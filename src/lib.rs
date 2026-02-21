use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::Read,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU16, Ordering},
        Arc,
    },
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::{
    net::{TcpListener, TcpStream},
    process::Command,
    sync::{broadcast, mpsc, oneshot, Mutex, Semaphore},
    task::JoinHandle,
};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

pub mod protocol;

use crate::protocol::{
    TerminalAttachParams, TerminalDetachParams, TerminalResizeParams, METHOD_PING,
    METHOD_PROJECT_LIST, METHOD_TERMINAL_ATTACH, METHOD_TERMINAL_DETACH, METHOD_TERMINAL_RESIZE,
    METHOD_THREAD_LIST,
};

pub const DEFAULT_ADDR: &str = "127.0.0.1:19990";
const MAX_IN_FLIGHT_REQUESTS: usize = 32;
const INPUT_CHANNEL_CAPACITY: usize = 256;
const SEND_KEYS_HEX_CHUNK_BYTES: usize = 512;

#[derive(Clone)]
pub struct AppState {
    events_tx: broadcast::Sender<ServerEvent>,
    next_channel_id: Arc<AtomicU16>,
}

#[derive(Clone, Debug)]
struct ServerEvent {
    method: String,
    params: Value,
}

impl AppState {
    pub fn new() -> Self {
        let (events_tx, _) = broadcast::channel(256);
        Self {
            events_tx,
            next_channel_id: Arc::new(AtomicU16::new(1)),
        }
    }

    fn alloc_channel_id(&self) -> u16 {
        loop {
            let channel_id = self.next_channel_id.fetch_add(1, Ordering::Relaxed);
            if channel_id != 0 {
                return channel_id;
            }
        }
    }

    fn emit_event(&self, method: impl Into<String>, params: Value) {
        let _ = self.events_tx.send(ServerEvent {
            method: method.into(),
            params,
        });
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub enum DaemonError {
    Io(std::io::Error),
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "io error: {err}"),
        }
    }
}

impl std::error::Error for DaemonError {}

impl From<std::io::Error> for DaemonError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

pub async fn serve(addr: &str, shutdown_rx: oneshot::Receiver<()>) -> Result<(), DaemonError> {
    let listener = TcpListener::bind(addr).await?;
    serve_listener(listener, shutdown_rx).await;
    Ok(())
}

pub async fn serve_listener(listener: TcpListener, mut shutdown_rx: oneshot::Receiver<()>) {
    let state = Arc::new(AppState::new());
    let local_addr = match listener.local_addr() {
        Ok(addr) => addr,
        Err(err) => {
            error!(error = %err, "failed to get listener address");
            return;
        }
    };

    info!(%local_addr, "threadmill daemon listening");

    loop {
        tokio::select! {
            _ = &mut shutdown_rx => {
                info!("shutdown signal received");
                break;
            }
            accept_res = listener.accept() => {
                match accept_res {
                    Ok((stream, peer_addr)) => {
                        let state = Arc::clone(&state);
                        tokio::spawn(async move {
                            if let Err(err) = handle_connection(stream, peer_addr, state).await {
                                warn!(%peer_addr, error = %err, "connection failed");
                            }
                        });
                    }
                    Err(err) => warn!(error = %err, "accept failed"),
                }
            }
        }
    }
}

pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .try_init();
}

#[derive(Debug)]
enum ConnectionError {
    WebSocket(tokio_tungstenite::tungstenite::Error),
}

impl std::fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WebSocket(err) => write!(f, "websocket error: {err}"),
        }
    }
}

impl std::error::Error for ConnectionError {}

impl From<tokio_tungstenite::tungstenite::Error> for ConnectionError {
    fn from(value: tokio_tungstenite::tungstenite::Error) -> Self {
        Self::WebSocket(value)
    }
}

#[derive(Default)]
struct ConnectionState {
    by_channel: HashMap<u16, Attachment>,
    by_target: HashMap<String, u16>,
    attaching_targets: HashMap<String, u16>,
}

struct Attachment {
    target: String,
    pane_target: String,
    window_target: String,
    fifo_path: String,
    input_tx: Option<mpsc::Sender<Vec<u8>>>,
    input_task: JoinHandle<()>,
    output_task: JoinHandle<()>,
}

#[derive(Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

async fn handle_connection(
    stream: TcpStream,
    peer_addr: SocketAddr,
    state: Arc<AppState>,
) -> Result<(), ConnectionError> {
    let ws_stream = accept_async(stream).await?;
    let connection_id = Uuid::new_v4();
    let (mut ws_writer, mut ws_reader) = ws_stream.split();
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<Message>();
    let connection_state = Arc::new(Mutex::new(ConnectionState::default()));
    let semaphore = Arc::new(Semaphore::new(MAX_IN_FLIGHT_REQUESTS));

    let writer_task = tokio::spawn(async move {
        while let Some(msg) = outbound_rx.recv().await {
            if ws_writer.send(msg).await.is_err() {
                break;
            }
        }
    });

    let events_task = {
        let outbound_tx = outbound_tx.clone();
        let mut events_rx = state.events_tx.subscribe();
        tokio::spawn(async move {
            loop {
                match events_rx.recv().await {
                    Ok(event) => {
                        let message = Message::Text(
                            json!({
                                "jsonrpc": "2.0",
                                "method": event.method,
                                "params": event.params,
                            })
                            .to_string(),
                        );
                        if outbound_tx.send(message).is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!(skipped, "event subscriber lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    };

    info!(%connection_id, %peer_addr, "client connected");

    while let Some(frame_res) = ws_reader.next().await {
        match frame_res {
            Ok(Message::Text(text)) => {
                let permit = match Arc::clone(&semaphore).acquire_owned().await {
                    Ok(permit) => permit,
                    Err(_) => break,
                };

                let outbound_tx = outbound_tx.clone();
                let connection_state = Arc::clone(&connection_state);
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Some(response) = handle_text_message(
                        text.to_string(),
                        state,
                        connection_state,
                        outbound_tx.clone(),
                    )
                    .await
                    {
                        let _ = outbound_tx.send(response);
                    }
                });
            }
            Ok(Message::Binary(data)) => {
                if let Err(err) =
                    handle_binary_message(data.to_vec(), Arc::clone(&connection_state)).await
                {
                    warn!(%connection_id, error = %err, "failed to route binary frame");
                }
            }
            Ok(Message::Ping(payload)) => {
                let _ = outbound_tx.send(Message::Pong(payload));
            }
            Ok(Message::Pong(_)) => {}
            Ok(Message::Close(_)) => break,
            Ok(Message::Frame(_)) => {}
            Err(err) => {
                warn!(%connection_id, error = %err, "read loop failed");
                break;
            }
        }
    }

    cleanup_connection(Arc::clone(&connection_state)).await;
    events_task.abort();
    writer_task.abort();
    let _ = events_task.await;
    let _ = writer_task.await;
    info!(%connection_id, "client disconnected");
    Ok(())
}

async fn handle_text_message(
    text: String,
    state: Arc<AppState>,
    connection_state: Arc<Mutex<ConnectionState>>,
    outbound_tx: mpsc::UnboundedSender<Message>,
) -> Option<Message> {
    let request: JsonRpcRequest = match serde_json::from_str(&text) {
        Ok(request) => request,
        Err(err) => {
            return Some(error_response(None, -1, format!("invalid JSON: {err}")));
        }
    };

    if request.jsonrpc != "2.0" {
        return Some(error_response(request.id, -1, "jsonrpc must be '2.0'"));
    }

    let id = request.id.clone();
    let result = dispatch_request(request, state, connection_state, outbound_tx).await;
    match (id, result) {
        (Some(id), Ok(result)) => Some(success_response(id, result)),
        (Some(id), Err(message)) => Some(error_response(Some(id), -1, message)),
        (None, Ok(_)) => None,
        (None, Err(message)) => {
            warn!(error = %message, "notification failed");
            None
        }
    }
}

async fn dispatch_request(
    request: JsonRpcRequest,
    state: Arc<AppState>,
    connection_state: Arc<Mutex<ConnectionState>>,
    outbound_tx: mpsc::UnboundedSender<Message>,
) -> Result<Value, String> {
    match request.method.as_str() {
        METHOD_PING => Ok(json!("pong")),
        METHOD_PROJECT_LIST => Ok(json!([])),
        METHOD_THREAD_LIST => Ok(json!([])),
        METHOD_TERMINAL_ATTACH => {
            let params: TerminalAttachParams = serde_json::from_value(request.params)
                .map_err(|err| format!("invalid terminal.attach params: {err}"))?;
            terminal_attach(params, state, connection_state, outbound_tx).await
        }
        METHOD_TERMINAL_DETACH => {
            let params: TerminalDetachParams = serde_json::from_value(request.params)
                .map_err(|err| format!("invalid terminal.detach params: {err}"))?;
            terminal_detach(params, state, connection_state).await
        }
        METHOD_TERMINAL_RESIZE => {
            let params: TerminalResizeParams = serde_json::from_value(request.params)
                .map_err(|err| format!("invalid terminal.resize params: {err}"))?;
            terminal_resize(params, connection_state).await
        }
        _ => Err(format!("unknown method '{}'", request.method)),
    }
}

async fn handle_binary_message(
    data: Vec<u8>,
    connection_state: Arc<Mutex<ConnectionState>>,
) -> Result<(), String> {
    if data.len() < 2 {
        return Err("binary frame too short".to_string());
    }

    let channel_id = u16::from_be_bytes([data[0], data[1]]);
    let payload = data[2..].to_vec();

    let input_tx = {
        let guard = connection_state.lock().await;
        let Some(attachment) = guard.by_channel.get(&channel_id) else {
            return Err(format!("unknown channel {channel_id}"));
        };

        let Some(input_tx) = &attachment.input_tx else {
            return Err(format!("channel {channel_id} is closed"));
        };

        input_tx.clone()
    };

    input_tx
        .try_send(payload)
        .map_err(|err| format!("failed to queue input for channel {channel_id}: {err}"))
}

async fn terminal_attach(
    params: TerminalAttachParams,
    state: Arc<AppState>,
    connection_state: Arc<Mutex<ConnectionState>>,
    outbound_tx: mpsc::UnboundedSender<Message>,
) -> Result<Value, String> {
    let target = tmux_target(&params.session, params.window, params.pane);
    let channel_id = {
        let mut guard = connection_state.lock().await;
        if let Some(channel_id) = guard.by_target.get(&target) {
            return Ok(json!({ "channel_id": channel_id }));
        }
        if guard.attaching_targets.contains_key(&target) {
            return Err(format!("target {target} is already attaching"));
        }

        let reserved_channel_id = loop {
            let candidate = state.alloc_channel_id();
            if !guard.by_channel.contains_key(&candidate)
                && !guard.attaching_targets.values().any(|existing| *existing == candidate)
            {
                break candidate;
            }
        };
        guard
            .attaching_targets
            .insert(target.clone(), reserved_channel_id);
        reserved_channel_id
    };

    let attach_result = async {
        let pane_target = resolve_pane_target(&params.session, params.window, params.pane).await?;
        let window_target = resolve_window_target(&pane_target).await?;
        let fifo_path = format!("/tmp/threadmill-pipe-{channel_id}-{}", Uuid::new_v4());

        match std::fs::remove_file(&fifo_path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(format!("failed to clear stale fifo {fifo_path}: {err}")),
        }

        let mkfifo_output = Command::new("mkfifo")
            .arg(&fifo_path)
            .output()
            .await
            .map_err(|err| format!("failed to run mkfifo for {fifo_path}: {err}"))?;
        if !mkfifo_output.status.success() {
            return Err(format!(
                "mkfifo failed for {fifo_path}: {}",
                String::from_utf8_lossy(&mkfifo_output.stderr).trim()
            ));
        }

        let clear_pipe_output = match Command::new("tmux")
            .args(["pipe-pane", "-t", &pane_target])
            .output()
            .await
        {
            Ok(output) => output,
            Err(err) => {
                let _ = std::fs::remove_file(&fifo_path);
                return Err(format!("failed to clear existing tmux pipe-pane: {err}"));
            }
        };
        if !clear_pipe_output.status.success() {
            let _ = std::fs::remove_file(&fifo_path);
            return Err(format!(
                "tmux pipe-pane clear failed for {target}: {}",
                String::from_utf8_lossy(&clear_pipe_output.stderr).trim()
            ));
        }

        let pipe_command = format!("cat > {fifo_path}");
        let pipe_output = match Command::new("tmux")
            .args(["pipe-pane", "-t", &pane_target, "-O", &pipe_command])
            .output()
            .await
        {
            Ok(output) => output,
            Err(err) => {
                let _ = std::fs::remove_file(&fifo_path);
                return Err(format!("failed to run tmux pipe-pane: {err}"));
            }
        };
        if !pipe_output.status.success() {
            let _ = std::fs::remove_file(&fifo_path);
            return Err(format!(
                "tmux pipe-pane failed for {target}: {}",
                String::from_utf8_lossy(&pipe_output.stderr).trim()
            ));
        }

        let output_tx = outbound_tx.clone();
        let fifo_path_for_task = fifo_path.clone();
        let output_task = tokio::task::spawn_blocking(move || {
            let mut reader = match OpenOptions::new().read(true).open(&fifo_path_for_task) {
                Ok(reader) => reader,
                Err(err) => {
                    warn!(fifo = %fifo_path_for_task, error = %err, "failed to open tmux fifo");
                    return;
                }
            };

            let mut buf = [0_u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(read_len) => {
                        let mut payload = Vec::with_capacity(read_len + 2);
                        payload.extend_from_slice(&channel_id.to_be_bytes());
                        payload.extend_from_slice(&buf[..read_len]);
                        if output_tx.send(Message::Binary(payload)).is_err() {
                            break;
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(err) => {
                        warn!(fifo = %fifo_path_for_task, error = %err, "failed to read tmux fifo");
                        break;
                    }
                }
            }
        });

        // Input injection: use `tmux send-keys -H` to send hex-encoded bytes.
        // Writing to the pane TTY slave only produces output, not input.
        // Batch keystrokes over a short window to avoid per-keystroke process spawns.
        let (input_tx, mut input_rx) = mpsc::channel::<Vec<u8>>(INPUT_CHANNEL_CAPACITY);
        let pane_target_for_input = pane_target.clone();
        let input_task = tokio::spawn(async move {
            'input: while let Some(mut batch) = input_rx.recv().await {
                while let Ok(more) = input_rx.try_recv() {
                    batch.extend_from_slice(&more);
                }

                for chunk in batch.chunks(SEND_KEYS_HEX_CHUNK_BYTES) {
                    let hex_args: Vec<String> = chunk.iter().map(|byte| format!("{byte:02x}")).collect();
                    let mut args = vec![
                        "send-keys".to_string(),
                        "-t".to_string(),
                        pane_target_for_input.clone(),
                        "-H".to_string(),
                    ];
                    args.extend(hex_args);

                    match Command::new("tmux").args(&args).output().await {
                        Ok(output) if !output.status.success() => {
                            warn!(
                                pane = %pane_target_for_input,
                                error = %String::from_utf8_lossy(&output.stderr).trim(),
                                "tmux send-keys failed"
                            );
                            break 'input;
                        }
                        Err(err) => {
                            warn!(
                                pane = %pane_target_for_input,
                                error = %err,
                                "failed to run tmux send-keys"
                            );
                            break 'input;
                        }
                        Ok(_) => {}
                    }
                }
            }
        });

        Ok::<Attachment, String>(Attachment {
            target: target.clone(),
            pane_target,
            window_target,
            fifo_path,
            input_tx: Some(input_tx),
            input_task,
            output_task,
        })
    }
    .await;

    let attachment = match attach_result {
        Ok(attachment) => attachment,
        Err(err) => {
            let mut guard = connection_state.lock().await;
            guard.attaching_targets.remove(&target);
            return Err(err);
        }
    };

    {
        let mut guard = connection_state.lock().await;
        guard.attaching_targets.remove(&target);
        guard.by_target.insert(target.clone(), channel_id);
        guard.by_channel.insert(channel_id, attachment);
    }

    state.emit_event(
        "thread.status_changed",
        json!({
            "target": target,
            "status": "terminal_attached",
            "channel_id": channel_id,
        }),
    );

    Ok(json!({ "channel_id": channel_id }))
}

async fn terminal_detach(
    params: TerminalDetachParams,
    state: Arc<AppState>,
    connection_state: Arc<Mutex<ConnectionState>>,
) -> Result<Value, String> {
    let target = tmux_target(&params.session, params.window, params.pane);
    let detached = detach_by_target(&target, connection_state).await?;

    if detached {
        state.emit_event(
            "thread.status_changed",
            json!({
                "target": target,
                "status": "terminal_detached",
            }),
        );
    }

    Ok(json!({ "detached": detached }))
}

async fn terminal_resize(
    params: TerminalResizeParams,
    connection_state: Arc<Mutex<ConnectionState>>,
) -> Result<Value, String> {
    let target = tmux_target(&params.session, params.window, params.pane);
    let window_target = {
        let guard = connection_state.lock().await;
        let channel_id = *guard
            .by_target
            .get(&target)
            .ok_or_else(|| format!("target {target} is not attached"))?;
        guard
            .by_channel
            .get(&channel_id)
            .ok_or_else(|| format!("channel {channel_id} not found"))?
            .window_target
            .clone()
    };

    // Resize the tmux window (which resizes the pane to fill it).
    // resize-pane alone won't shrink a single-pane window.
    let output = Command::new("tmux")
        .args([
            "resize-window",
            "-t",
            &window_target,
            "-x",
            &params.cols.to_string(),
            "-y",
            &params.rows.to_string(),
        ])
        .output()
        .await
        .map_err(|err| format!("failed to run tmux resize-window: {err}"))?;

    if !output.status.success() {
        return Err(format!(
            "tmux resize-window failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(json!({ "resized": true }))
}

async fn detach_by_target(
    target: &str,
    connection_state: Arc<Mutex<ConnectionState>>,
) -> Result<bool, String> {
    let maybe_attachment = {
        let mut guard = connection_state.lock().await;
        let Some(channel_id) = guard.by_target.remove(target) else {
            return Ok(false);
        };
        guard.by_channel.remove(&channel_id)
    };

    if let Some(mut attachment) = maybe_attachment {
        cleanup_attachment(&mut attachment).await;
    }

    Ok(true)
}

async fn cleanup_connection(connection_state: Arc<Mutex<ConnectionState>>) {
    let attachments = {
        let mut guard = connection_state.lock().await;
        guard.by_target.clear();
        guard.attaching_targets.clear();
        std::mem::take(&mut guard.by_channel)
    };

    for (_, mut attachment) in attachments {
        cleanup_attachment(&mut attachment).await;
    }
}

async fn cleanup_attachment(attachment: &mut Attachment) {
    debug!(target = %attachment.target, "cleaning attachment");
    if let Some(input_tx) = attachment.input_tx.take() {
        drop(input_tx);
    }

    let stop_pipe_output = Command::new("tmux")
        .args(["pipe-pane", "-t", &attachment.pane_target])
        .output()
        .await;
    match stop_pipe_output {
        Ok(output) if !output.status.success() => {
            warn!(
                target = %attachment.pane_target,
                error = %String::from_utf8_lossy(&output.stderr).trim(),
                "tmux pipe-pane stop failed"
            );
        }
        Err(err) => {
            warn!(target = %attachment.pane_target, error = %err, "failed to stop tmux pipe-pane");
        }
        Ok(_) => {}
    }

    if let Err(err) = std::fs::remove_file(&attachment.fifo_path) {
        if err.kind() != std::io::ErrorKind::NotFound {
            warn!(fifo = %attachment.fifo_path, error = %err, "failed to remove tmux fifo");
        }
    }

    attachment.input_task.abort();
    let _ = (&mut attachment.input_task).await;

    if tokio::time::timeout(Duration::from_millis(250), &mut attachment.output_task)
        .await
        .is_err()
    {
        attachment.output_task.abort();
    }
    let _ = (&mut attachment.output_task).await;
}

fn tmux_target(session: &str, window: u32, pane: u32) -> String {
    format!("{session}:{window}.{pane}")
}

async fn resolve_pane_target(session: &str, window: u32, pane: u32) -> Result<String, String> {
    let window_indexes = list_window_indexes(session).await?;
    let actual_window = if window_indexes.contains(&window) {
        window
    } else {
        *window_indexes.get(window as usize).ok_or_else(|| {
            format!(
                "window {window} is neither an existing tmux index nor a valid zero-based ordinal in session {session}"
            )
        })?
    };

    let window_target = format!("{session}:{actual_window}");
    let panes = list_panes(&window_target).await?;

    if let Some((_, pane_id)) = panes.iter().find(|(pane_index, _)| *pane_index == pane) {
        return Ok(pane_id.clone());
    }

    if let Some((_, pane_id)) = panes.get(pane as usize) {
        return Ok(pane_id.clone());
    }

    Err(format!(
        "pane {pane} is neither an existing tmux index nor a valid zero-based ordinal in {window_target}"
    ))
}

async fn resolve_window_target(pane_target: &str) -> Result<String, String> {
    let output = Command::new("tmux")
        .args([
            "display-message",
            "-p",
            "-t",
            pane_target,
            "#{session_name}:#{window_index}",
        ])
        .output()
        .await
        .map_err(|err| format!("failed to run tmux display-message: {err}"))?;

    if !output.status.success() {
        return Err(format!(
            "tmux display-message failed for {pane_target}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let window_target = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if window_target.is_empty() {
        return Err(format!("tmux returned empty window target for {pane_target}"));
    }

    Ok(window_target)
}

async fn list_window_indexes(session: &str) -> Result<Vec<u32>, String> {
    let output = Command::new("tmux")
        .args(["list-windows", "-t", session, "-F", "#{window_index}"])
        .output()
        .await
        .map_err(|err| format!("failed to run tmux list-windows: {err}"))?;

    if !output.status.success() {
        return Err(format!(
            "tmux list-windows failed for {session}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    parse_u32_lines(&output.stdout, "window")
}

async fn list_panes(window_target: &str) -> Result<Vec<(u32, String)>, String> {
    let output = Command::new("tmux")
        .args([
            "list-panes",
            "-t",
            window_target,
            "-F",
            "#{pane_index} #{pane_id}",
        ])
        .output()
        .await
        .map_err(|err| format!("failed to run tmux list-panes: {err}"))?;

    if !output.status.success() {
        return Err(format!(
            "tmux list-panes failed for {window_target}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    parse_pane_lines(&output.stdout)
}

fn parse_u32_lines(raw: &[u8], label: &str) -> Result<Vec<u32>, String> {
    String::from_utf8_lossy(raw)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            line.trim()
                .parse::<u32>()
                .map_err(|err| format!("invalid tmux {label} index '{line}': {err}"))
        })
        .collect()
}

fn parse_pane_lines(raw: &[u8]) -> Result<Vec<(u32, String)>, String> {
    String::from_utf8_lossy(raw)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut parts = line.split_whitespace();
            let pane_index = parts
                .next()
                .ok_or_else(|| format!("invalid tmux pane output '{line}'"))?
                .parse::<u32>()
                .map_err(|err| format!("invalid tmux pane index '{line}': {err}"))?;
            let pane_id = parts
                .next()
                .ok_or_else(|| format!("invalid tmux pane output '{line}'"))?
                .to_string();
            Ok((pane_index, pane_id))
        })
        .collect()
}


fn success_response(id: Value, result: Value) -> Message {
    Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        })
        .to_string(),
    )
}

fn error_response(id: Option<Value>, code: i64, message: impl Into<String>) -> Message {
    Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": id.unwrap_or(Value::Null),
            "error": {
                "code": code,
                "message": message.into(),
            }
        })
        .to_string(),
    )
}
