use std::{
    collections::HashMap,
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
    task::JoinHandle,
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
    next_operation_id: Arc<AtomicU64>,
    pub store: Arc<Mutex<StateStore>>,
    pub workflows: Arc<Mutex<services::workflow::WorkflowStore>>,
    pub chat: Arc<Mutex<services::chat::ChatState>>,
    pub create_tasks: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
}

#[derive(Clone, Debug)]
pub(crate) struct ServerEvent {
    pub(crate) method: String,
    pub(crate) params: Value,
}

impl AppState {
    pub fn new(store: StateStore) -> Self {
        let (events_tx, _) = broadcast::channel(256);
        let workflows = services::workflow::WorkflowStore::load_from_state_store(&store.path)
            .unwrap_or_else(|err| panic!("failed to load workflow store: {err}"));
        let history_root = store
            .path
            .parent()
            .map(|parent| parent.join("chat"))
            .unwrap_or_else(|| std::path::PathBuf::from("chat"));
        Self {
            events_tx,
            next_channel_id: Arc::new(AtomicU16::new(1)),
            state_version: Arc::new(AtomicU64::new(0)),
            next_operation_id: Arc::new(AtomicU64::new(1)),
            store: Arc::new(Mutex::new(store)),
            workflows: Arc::new(Mutex::new(workflows)),
            chat: Arc::new(Mutex::new(services::chat::ChatState::new(history_root))),
            create_tasks: Arc::new(Mutex::new(HashMap::new())),
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

    pub(crate) fn subscribe_events(&self) -> broadcast::Receiver<ServerEvent> {
        self.events_tx.subscribe()
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

    pub fn emit_preset_output(&self, event: protocol::PresetOutputEvent) {
        self.emit_event("preset.output", event);
    }

    pub fn emit_chat_session_created(&self, event: protocol::ChatSessionCreatedEvent) {
        self.emit_event("chat.session_created", event);
    }

    pub fn emit_chat_session_ready(&self, event: protocol::ChatSessionReadyEvent) {
        self.emit_event("chat.session_ready", event);
    }

    pub fn emit_chat_session_failed(&self, event: protocol::ChatSessionFailedEvent) {
        self.emit_event("chat.session_failed", event);
    }

    pub fn emit_chat_session_ended(&self, event: protocol::ChatSessionEndedEvent) {
        self.emit_event("chat.session_ended", event);
    }

    pub fn emit_chat_status_changed(&self, event: protocol::ChatStatusChangedEvent) {
        self.emit_event("chat.status_changed", event);
    }

    pub fn emit_state_delta(&self, operations: Vec<protocol::StateDeltaOperationPayload>) {
        if operations.is_empty() {
            return;
        }

        let operations = operations
            .into_iter()
            .map(|payload| protocol::StateDeltaOperation {
                op_id: format!(
                    "op-{}",
                    self.next_operation_id.fetch_add(1, Ordering::Relaxed)
                ),
                payload,
            })
            .collect();

        let state_version = self.state_version.fetch_add(1, Ordering::Relaxed) + 1;
        self.emit_event(
            "state.delta",
            protocol::StateDeltaEvent {
                state_version,
                operations,
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

    // Start opencode serve if not already running
    if let Err(err) = services::opencode::ensure_running().await {
        warn!(error = %err, "failed to start opencode serve at startup");
    }
    let state = Arc::new(AppState::new(store));
    if let Err(err) =
        services::chat::ChatService::recover_persisted_sessions(Arc::clone(&state)).await
    {
        warn!(error = %err, "failed to recover persisted chat sessions");
    }
    if let Err(err) = services::workflow::WorkflowService::reconcile_startup(Arc::clone(&state)).await
    {
        warn!(error = %err, "failed to reconcile workflows");
    }
    services::workflow::WorkflowService::start_event_listener(Arc::clone(&state));
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

#[derive(Debug, Clone, Default)]
pub struct ConnectionSessionState {
    pub session_id: Option<String>,
    pub protocol_version: Option<String>,
    pub capabilities: Vec<String>,
    pub hello_acknowledged: bool,
}

impl ConnectionSessionState {
    pub fn is_initialized(&self) -> bool {
        self.hello_acknowledged && self.session_id.is_some()
    }

    pub fn is_handshake_started(&self) -> bool {
        self.session_id.is_some()
    }

    pub fn mark_hello_acknowledged(&mut self) -> bool {
        if self.session_id.is_none() || self.hello_acknowledged {
            return false;
        }

        self.hello_acknowledged = true;
        true
    }
}

#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<protocol::RpcErrorData>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: protocol::RpcErrorData) -> Self {
        self.data = Some(data);
        self
    }

    pub fn parse_error(message: impl Into<String>) -> Self {
        Self::new(-32700, message).with_data(protocol::RpcErrorData {
            kind: Some("rpc.parse_error".to_string()),
            retryable: Some(false),
            details: None,
        })
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(-32600, message).with_data(protocol::RpcErrorData {
            kind: Some("rpc.invalid_request".to_string()),
            retryable: Some(false),
            details: None,
        })
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(-32602, message).with_data(protocol::RpcErrorData {
            kind: Some("rpc.invalid_params".to_string()),
            retryable: Some(false),
            details: None,
        })
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(-32601, format!("Method not found: {method}")).with_data(protocol::RpcErrorData {
            kind: Some("rpc.method_not_found".to_string()),
            retryable: Some(false),
            details: Some(json!({ "method": method })),
        })
    }

    pub fn session_not_initialized(method: &str) -> Self {
        Self::new(
            -32000,
            format!("session.hello required before calling {method}"),
        )
        .with_data(protocol::RpcErrorData {
            kind: Some("session.not_initialized".to_string()),
            retryable: Some(false),
            details: Some(json!({ "method": method })),
        })
    }

    pub fn session_already_initialized() -> Self {
        Self::new(
            -32600,
            "session.hello may only be called once per connection",
        )
        .with_data(protocol::RpcErrorData {
            kind: Some("session.already_initialized".to_string()),
            retryable: Some(false),
            details: None,
        })
    }

    pub fn session_protocol_mismatch(client_version: &str, expected_version: &str) -> Self {
        Self::new(
            -32602,
            format!(
                "unsupported protocol_version '{client_version}', expected '{expected_version}'"
            ),
        )
        .with_data(protocol::RpcErrorData {
            kind: Some("session.protocol_mismatch".to_string()),
            retryable: Some(false),
            details: Some(json!({
                "client_protocol_version": client_version,
                "expected_protocol_version": expected_version,
            })),
        })
    }

    pub fn session_missing_capabilities(missing: &[String]) -> Self {
        Self::new(
            -32602,
            format!("missing required capabilities: {}", missing.join(", ")),
        )
        .with_data(protocol::RpcErrorData {
            kind: Some("session.missing_capabilities".to_string()),
            retryable: Some(false),
            details: Some(json!({
                "missing": missing,
            })),
        })
    }

    pub fn not_found(kind: &str, message: impl Into<String>) -> Self {
        Self::new(-32004, message).with_data(protocol::RpcErrorData {
            kind: Some(kind.to_string()),
            retryable: Some(false),
            details: None,
        })
    }

    pub fn terminal_session_missing(
        message: impl Into<String>,
        details: Option<serde_json::Value>,
    ) -> Self {
        Self::new(-32041, message).with_data(protocol::RpcErrorData {
            kind: Some("terminal.session_missing".to_string()),
            retryable: Some(false),
            details,
        })
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(-32001, message).with_data(protocol::RpcErrorData {
            kind: Some("rpc.internal".to_string()),
            retryable: Some(false),
            details: None,
        })
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
    let session_state = Arc::new(Mutex::new(ConnectionSessionState::default()));
    let semaphore = Arc::new(Semaphore::new(MAX_IN_FLIGHT_REQUESTS));

    let writer_task = tokio::spawn(async move {
        while let Some(msg) = outbound_rx.recv().await {
            if ws_writer.send(msg).await.is_err() {
                break;
            }
        }
    });

    let deferred_events_rx = Arc::new(Mutex::new(Some(state.events_tx.subscribe())));
    let events_task = Arc::new(Mutex::new(None::<JoinHandle<()>>));

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
                let session_state = Arc::clone(&session_state);
                let state = Arc::clone(&state);
                let deferred_events_rx = Arc::clone(&deferred_events_rx);
                let events_task = Arc::clone(&events_task);
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Some(response) = handle_text_message(
                        text.to_string(),
                        state,
                        connection_state,
                        session_state.clone(),
                        outbound_tx.clone(),
                    )
                    .await
                    {
                        let activate_events = response.activate_events;
                        if outbound_tx.send(response.message).is_ok() && activate_events {
                            activate_session_events(
                                session_state,
                                deferred_events_rx,
                                events_task,
                                outbound_tx,
                            )
                            .await;
                        }
                    }
                });
            }
            Ok(Message::Binary(data)) => {
                if data.len() < 2 {
                    warn!(%connection_id, "received short binary frame");
                    continue;
                }

                let channel_id = u16::from_be_bytes([data[0], data[1]]);
                let payload = data[2..].to_vec();
                match services::chat::ChatService::handle_binary_frame(
                    Arc::clone(&state),
                    channel_id,
                    payload,
                )
                .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        if let Err(err) = terminal::handle_binary_frame(
                            data.to_vec(),
                            Arc::clone(&connection_state),
                        )
                        .await
                        {
                            warn!(%connection_id, error = %err, "failed to route binary frame");
                        }
                    }
                    Err(err) => {
                        warn!(%connection_id, error = %err, "failed to route chat binary frame");
                    }
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

    services::chat::ChatService::cleanup_connection_channels(
        Arc::clone(&state),
        Arc::clone(&connection_state),
    )
    .await;
    terminal::cleanup_connection(Arc::clone(&connection_state)).await;
    if let Some(events_task) = events_task.lock().await.take() {
        events_task.abort();
        let _ = events_task.await;
    }
    writer_task.abort();
    let _ = writer_task.await;
    info!(%connection_id, "client disconnected");
    Ok(())
}

struct TextMessageResponse {
    message: Message,
    activate_events: bool,
}

async fn activate_session_events(
    session_state: Arc<Mutex<ConnectionSessionState>>,
    deferred_events_rx: Arc<Mutex<Option<broadcast::Receiver<ServerEvent>>>>,
    events_task: Arc<Mutex<Option<JoinHandle<()>>>>,
    outbound_tx: mpsc::UnboundedSender<Message>,
) {
    let should_activate = {
        let mut guard = session_state.lock().await;
        guard.mark_hello_acknowledged()
    };
    if !should_activate {
        return;
    }

    let Some(mut events_rx) = deferred_events_rx.lock().await.take() else {
        return;
    };

    let task = tokio::spawn(async move {
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
    });

    let mut task_slot = events_task.lock().await;
    if let Some(existing_task) = task_slot.replace(task) {
        existing_task.abort();
        let _ = existing_task.await;
    }
}

async fn handle_text_message(
    text: String,
    state: Arc<AppState>,
    connection_state: Arc<Mutex<TerminalConnectionState>>,
    session_state: Arc<Mutex<ConnectionSessionState>>,
    outbound_tx: mpsc::UnboundedSender<Message>,
) -> Option<TextMessageResponse> {
    let request: JsonRpcRequest = match serde_json::from_str(&text) {
        Ok(request) => request,
        Err(err) => {
            return Some(TextMessageResponse {
                message: error_response(
                    None,
                    RpcError::parse_error(format!("invalid JSON: {err}")),
                ),
                activate_events: false,
            });
        }
    };

    if request.jsonrpc != "2.0" {
        return Some(TextMessageResponse {
            message: error_response(
                request.id,
                RpcError::invalid_request("jsonrpc must be '2.0'"),
            ),
            activate_events: false,
        });
    }

    let id = request.id.clone();
    let result = rpc_router::dispatch_request(
        &request.method,
        request.params,
        state,
        connection_state,
        session_state,
        outbound_tx,
    )
    .await;

    let activate_events = request.method == protocol::METHOD_SESSION_HELLO;
    match (id, result) {
        (Some(id), Ok(result)) => Some(TextMessageResponse {
            message: success_response(id, result),
            activate_events,
        }),
        (Some(id), Err(error)) => Some(TextMessageResponse {
            message: error_response(Some(id), error),
            activate_events: false,
        }),
        (None, Ok(_)) => None,
        (None, Err(error)) => {
            let kind = error
                .data
                .as_ref()
                .and_then(|data| data.kind.as_deref())
                .unwrap_or("unknown");
            warn!(code = error.code, kind, error = %error.message, "notification failed");
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
        .to_string(),
    )
}

fn error_response(id: Option<Value>, error: RpcError) -> Message {
    let mut error_payload = json!({
        "code": error.code,
        "message": error.message,
    });

    if let Some(data) = error.data {
        if let Some(object) = error_payload.as_object_mut() {
            object.insert(
                "data".to_string(),
                serde_json::to_value(data).unwrap_or(json!({})),
            );
        }
    }

    Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": id.unwrap_or(Value::Null),
            "error": error_payload,
        })
        .to_string(),
    )
}
