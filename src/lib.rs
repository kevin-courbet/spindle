use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicU16, AtomicU64, Ordering},
        Arc,
    },
};

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{broadcast, mpsc, oneshot, Mutex, Semaphore},
};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tracing::{error, info, warn};
use uuid::Uuid;

pub mod protocol;
pub mod rpc_router;
pub mod services;
pub mod state_store;
pub mod tmux;

use services::terminal::{self, TerminalConnectionState};
use state_store::StateStore;

pub const DEFAULT_ADDR: &str = "127.0.0.1:19990";
const MAX_IN_FLIGHT_REQUESTS: usize = 32;

#[derive(Clone)]
pub struct AppState {
    events_tx: broadcast::Sender<ServerEvent>,
    next_channel_id: Arc<AtomicU16>,
    state_version: Arc<AtomicU64>,
    pub store: Arc<Mutex<StateStore>>,
}

#[derive(Clone, Debug)]
struct ServerEvent {
    method: String,
    params: Value,
}

impl AppState {
    pub fn new(store: StateStore) -> Self {
        let (events_tx, _) = broadcast::channel(256);
        Self {
            events_tx,
            next_channel_id: Arc::new(AtomicU16::new(1)),
            state_version: Arc::new(AtomicU64::new(0)),
            store: Arc::new(Mutex::new(store)),
        }
    }

    pub fn alloc_channel_id_with<F>(&self, mut is_active: F) -> u16
    where
        F: FnMut(u16) -> bool,
    {
        loop {
            let channel_id = self.next_channel_id.fetch_add(1, Ordering::Relaxed);
            if channel_id == 0 {
                continue;
            }
            if is_active(channel_id) {
                continue;
            }
            return channel_id;
        }
    }

    pub fn state_version(&self) -> protocol::StateVersion {
        self.state_version.load(Ordering::Relaxed)
    }

    pub fn emit_event(&self, method: impl Into<String>, params: impl serde::Serialize) {
        let params = match serde_json::to_value(params) {
            Ok(params) => params,
            Err(err) => {
                warn!(error = %err, "failed to serialize event payload");
                return;
            }
        };

        let _ = self.events_tx.send(ServerEvent {
            method: method.into(),
            params,
        });
    }

    pub fn emit_thread_progress(&self, event: protocol::ThreadProgress) {
        self.emit_event("thread.progress", event);
    }

    pub fn emit_thread_status_changed(&self, event: protocol::ThreadStatusChanged) {
        self.emit_event("thread.status_changed", event);
    }

    pub fn emit_project_added(&self, event: protocol::ProjectAddedEvent) {
        self.emit_event("project.added", event);
    }

    pub fn emit_project_removed(&self, event: protocol::ProjectRemovedEvent) {
        self.emit_event("project.removed", event);
    }

    pub fn emit_thread_created(&self, event: protocol::ThreadCreatedEvent) {
        self.emit_event("thread.created", event);
    }

    pub fn emit_thread_removed(&self, event: protocol::ThreadRemovedEvent) {
        self.emit_event("thread.removed", event);
    }

    pub fn emit_preset_process_event(&self, event: protocol::PresetProcessEvent) {
        self.emit_event("preset.process_event", event);
    }

    pub fn emit_state_delta(&self, changes: Vec<protocol::StateDeltaChange>) {
        if changes.is_empty() {
            return;
        }

        let state_version = self.state_version.fetch_add(1, Ordering::Relaxed) + 1;
        self.emit_event(
            "state.delta",
            protocol::StateDeltaEvent {
                state_version,
                changes,
            },
        );
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
    let mut store = match StateStore::load() {
        Ok(store) => store,
        Err(err) => {
            error!(error = %err, "failed to load state store");
            return;
        }
    };

    if let Err(err) = store.reconcile().await {
        warn!(error = %err, "state reconciliation failed");
    }
    if let Err(err) = store.save() {
        warn!(error = %err, "failed to persist reconciled state");
    }

    let state = Arc::new(AppState::new(store));
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
    let connection_state = Arc::new(Mutex::new(TerminalConnectionState::default()));
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
                            .to_string()
                            .into(),
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
                    if let Some(response) =
                        handle_text_message(text.to_string(), state, connection_state, outbound_tx.clone()).await
                    {
                        let _ = outbound_tx.send(response);
                    }
                });
            }
            Ok(Message::Binary(data)) => {
                if let Err(err) = terminal::handle_binary_frame(data.to_vec(), Arc::clone(&connection_state)).await {
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

    terminal::cleanup_connection(Arc::clone(&connection_state)).await;
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
    connection_state: Arc<Mutex<TerminalConnectionState>>,
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
    let result = rpc_router::dispatch_request(
        &request.method,
        request.params,
        state,
        connection_state,
        outbound_tx,
    )
    .await;

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

fn success_response(id: Value, result: Value) -> Message {
    Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        })
        .to_string()
        .into(),
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
        .to_string()
        .into(),
    )
}
