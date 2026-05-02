use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::Utc;
use serde_json::{json, Value};
use tokio::{
    sync::{mpsc, Mutex},
    time::{timeout, Duration},
};
use tokio_tungstenite::tungstenite::Message;
use tracing::warn;
use uuid::Uuid;

use crate::{
    protocol,
    services::{
        agent_registry,
        project::{load_project_default_chat_model, project_agent_command},
        terminal::TerminalConnectionState,
    },
    state_store, AppState,
};

const CHAT_INPUT_CHANNEL_CAPACITY: usize = 256;
const CHAT_IO_CHUNK_SIZE: usize = 8192;
const CHAT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const CHAT_ATTACH_WAIT_TIMEOUT: Duration = Duration::from_secs(35);
const CHAT_UPDATE_METHOD: &str = "session/update";
const CHAT_PROMPT_METHOD: &str = "session/prompt";
const CHAT_CANCEL_METHOD: &str = "session/cancel";
const CHAT_REQUEST_PERMISSION_METHOD: &str = "request_permission";
const CHAT_SESSION_REQUEST_PERMISSION_METHOD: &str = "session/request_permission";
const CHAT_REQUEST_QUESTION_METHOD: &str = "request_question";
const CHAT_SESSION_REQUEST_QUESTION_METHOD: &str = "session/request_question";
const CHAT_STALL_TIMEOUT: Duration = Duration::from_secs(60);

mod context;
mod history;
mod io;
mod runtime;
mod session;
mod status;

use context::build_conversation_context;
use history::*;
pub(crate) use history::{history_path_for_session, persist_session_metadata};
use io::*;
use runtime::ChatSessionRuntime;
use session::*;
use status::*;

pub struct ChatService;

#[derive(Debug, Clone, Copy, Default)]
pub struct ChatSessionOptions {
    pub capture_blocked_requests: bool,
}

#[derive(Default)]
pub struct ChatState {
    sessions: HashMap<String, ChatSessionRuntime>,
    sessions_by_thread: HashMap<String, Vec<String>>,
    channel_to_session: HashMap<u16, String>,
    channel_outbound: HashMap<u16, mpsc::UnboundedSender<Message>>,
    blocked_request_aware_channels: HashSet<u16>,
    history_root: PathBuf,
}

#[derive(Debug, Clone)]
struct PendingBlockedRequestRuntime {
    request: protocol::BlockedRequest,
    acp_request_id: Value,
}

#[derive(Debug, Clone)]
struct InvalidBlockedRequestRuntime {
    method: String,
    request_id: Value,
    reason: String,
}

#[derive(Debug, Clone)]
enum BlockedRequestFrame {
    NotBlockedRequest,
    Pending(Box<PendingBlockedRequestRuntime>),
    Invalid(InvalidBlockedRequestRuntime),
}

struct InvalidBlockedRequestDelivery {
    input_tx: mpsc::Sender<Vec<u8>>,
    payload: Vec<u8>,
    method: String,
    request_id: String,
}

#[derive(Debug, Clone)]
struct BlockedRequestCancellation {
    response: Vec<u8>,
    removal: protocol::BlockedRequestRemovedEvent,
}

struct BlockedRequestCancellationDelivery {
    removals: Vec<protocol::BlockedRequestRemovedEvent>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ChatBlockedRequestAnswerError {
    NotFound(String),
    Invalid(String),
    Delivery(String),
    AlreadyResolved(protocol::BlockedRequestAlreadyResolvedError),
}

impl std::fmt::Display for ChatBlockedRequestAnswerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(message) | Self::Invalid(message) | Self::Delivery(message) => {
                write!(f, "{message}")
            }
            Self::AlreadyResolved(error) => write!(
                f,
                "blocked request already resolved: {}/{}/{}",
                error.thread_id, error.session_id, error.request_id
            ),
        }
    }
}
impl ChatState {
    pub fn new(history_root: PathBuf) -> Self {
        Self {
            sessions: HashMap::new(),
            sessions_by_thread: HashMap::new(),
            channel_to_session: HashMap::new(),
            channel_outbound: HashMap::new(),
            blocked_request_aware_channels: HashSet::new(),
            history_root,
        }
    }
}

impl ChatState {
    pub(crate) fn history_root_path(&self) -> &Path {
        &self.history_root
    }
}

struct HandshakeResult {
    acp_session_id: String,
    modes: Option<Value>,
    models: Option<Value>,
    config_options: Option<Value>,
    title: Option<String>,
    model_id: Option<String>,
    /// Notifications collected during session/load replay. Empty for session/new.
    replay_notifications: Vec<Value>,
}

impl ChatService {
    pub(crate) async fn register_imported_session(
        state: Arc<AppState>,
        thread_id: String,
        session_id: String,
        summary: protocol::ChatSessionSummary,
        acp_session_id: Option<String>,
        history_path: PathBuf,
    ) {
        {
            let mut chat = state.chat.lock().await;
            if chat.sessions.contains_key(&session_id) {
                if let Some(existing) = chat.sessions.get_mut(&session_id) {
                    existing.summary = summary;
                    existing.thread_id = thread_id.clone();
                    existing.acp_session_id = acp_session_id;
                    existing.ended_emitted = true;
                    existing.history_path = history_path;
                }
                if !chat
                    .sessions_by_thread
                    .get(&thread_id)
                    .is_some_and(|sessions| sessions.iter().any(|id| id == &session_id))
                {
                    chat.sessions_by_thread
                        .entry(thread_id.clone())
                        .or_default()
                        .push(session_id.clone());
                }
                drop(chat);
                emit_state_delta_updated(&state, &thread_id, &session_id).await;
                return;
            }

            let mut runtime = ChatSessionRuntime::new(summary, thread_id.clone(), history_path);
            runtime.acp_session_id = acp_session_id;
            runtime.ended_emitted = true;
            chat.sessions.insert(session_id.clone(), runtime);
            chat.sessions_by_thread
                .entry(thread_id.clone())
                .or_default()
                .push(session_id.clone());
        }

        emit_state_delta_added(&state, &thread_id, &session_id).await;
    }

    pub async fn start(
        state: Arc<AppState>,
        params: protocol::ChatStartParams,
        options: ChatSessionOptions,
    ) -> Result<protocol::ChatStartResult, String> {
        tracing::info!(thread_id = %params.thread_id, agent = %params.agent_name, "chat_start");
        let (_project_path, command, cwd, project_preferred_model) =
            resolve_agent_launch(&state, &params.thread_id, &params.agent_name).await?;
        // Per-session override (from agent def's `model:` frontmatter) wins over the
        // project-level default.
        let preferred_model = params.preferred_model.clone().or(project_preferred_model);

        let session_id = Uuid::new_v4().to_string();
        let created_at = Utc::now().to_rfc3339();
        {
            let mut chat = state.chat.lock().await;
            let history_path =
                history_path_for_session(&chat.history_root, &params.thread_id, &session_id);
            let summary = protocol::ChatSessionSummary {
                session_id: session_id.clone(),
                agent_type: params.agent_name.clone(),
                status: protocol::ChatSessionStatus::Starting,
                agent_status: protocol::AgentStatus::Idle,
                worker_count: 0,
                title: None,
                model_id: None,
                created_at,
                display_name: params.display_name.clone(),
                parent_session_id: params.parent_session_id.clone(),
                pending_blocked_requests: Vec::new(),
            };
            let mut runtime =
                ChatSessionRuntime::new(summary, params.thread_id.clone(), history_path);
            runtime.display_name = params.display_name.clone();
            runtime.parent_session_id = params.parent_session_id.clone();
            runtime.agent_command = Some(command.clone());
            runtime.system_prompt = params.system_prompt.clone();
            runtime.initial_prompt = params.initial_prompt.clone();
            runtime.started_at = Some(Utc::now());
            runtime.blocked_request_capture_enabled = options.capture_blocked_requests;
            chat.sessions.insert(session_id.clone(), runtime);
            chat.sessions_by_thread
                .entry(params.thread_id.clone())
                .or_default()
                .push(session_id.clone());
        }

        state.emit_chat_session_created(protocol::ChatSessionCreatedEvent {
            thread_id: params.thread_id.clone(),
            session_id: session_id.clone(),
            agent_type: params.agent_name.clone(),
            display_name: params.display_name.clone(),
            parent_session_id: params.parent_session_id.clone(),
        });
        emit_state_delta_added(&state, &params.thread_id, &session_id).await;

        spawn_session_task(
            Arc::clone(&state),
            SessionLaunchContext {
                thread_id: params.thread_id,
                session_id: session_id.clone(),
                command,
                cwd,
                load_session_id: None,
                preferred_model: preferred_model.clone(),
            },
        );

        Ok(protocol::ChatStartResult {
            session_id,
            status: protocol::ChatSessionStatus::Starting,
        })
    }

    pub async fn load(
        state: Arc<AppState>,
        params: protocol::ChatLoadParams,
        options: ChatSessionOptions,
    ) -> Result<protocol::ChatLoadResult, String> {
        let history_root = {
            let chat = state.chat.lock().await;
            chat.history_root_path().to_path_buf()
        };
        let force_new_session = params.force_new_session
            || restored_session_marker_exists(
                &history_root,
                &params.thread_id,
                &params.session_id,
            )?;

        let (agent_type, load_session_id, stop_tx, history_path) = {
            let mut chat = state.chat.lock().await;
            let session = chat
                .sessions
                .get_mut(&params.session_id)
                .ok_or_else(|| format!("chat session not found: {}", params.session_id))?;
            if session.thread_id != params.thread_id {
                return Err(format!(
                    "chat session {} does not belong to thread {}",
                    params.session_id, params.thread_id
                ));
            }
            session.blocked_request_capture_enabled |= options.capture_blocked_requests;
            if session.summary.status == protocol::ChatSessionStatus::Starting {
                return Ok(protocol::ChatLoadResult {
                    session_id: params.session_id,
                    status: protocol::ChatSessionStatus::Starting,
                });
            }

            let load_session_id = if force_new_session {
                None
            } else {
                session.acp_session_id.clone()
            };
            session.summary.status = protocol::ChatSessionStatus::Starting;
            session.input_tx = None;
            let stop_tx = session.stop_tx.take();
            session.ended_emitted = false;
            (
                session.summary.agent_type.clone(),
                load_session_id,
                stop_tx,
                session.history_path.clone(),
            )
        };

        let conversation_context = if load_session_id.is_none() {
            build_conversation_context(&history_path, None)
        } else {
            None
        };

        {
            let mut chat = state.chat.lock().await;
            if let Some(session) = chat.sessions.get_mut(&params.session_id) {
                if conversation_context.is_some() {
                    session.had_conversation_context = true;
                }
                session.conversation_context = conversation_context;
            }
        }

        if let Some(stop_tx) = stop_tx {
            let _ = stop_tx.send(());
        }

        emit_state_delta_updated(&state, &params.thread_id, &params.session_id).await;

        // Recovered sessions have agent_type "unknown". Use client-supplied
        // agent_name as fallback so resolve_agent_launch can find the command.
        let effective_agent = if agent_type == "unknown" {
            params
                .agent_name
                .as_deref()
                .unwrap_or(&agent_type)
                .to_string()
        } else {
            agent_type.clone()
        };

        let (_project_path, command, cwd, preferred_model) =
            resolve_agent_launch(&state, &params.thread_id, &effective_agent).await?;
        {
            let mut chat = state.chat.lock().await;
            if let Some(session) = chat.sessions.get_mut(&params.session_id) {
                session.agent_command = Some(command.clone());
            }
        }
        spawn_session_task(
            Arc::clone(&state),
            SessionLaunchContext {
                thread_id: params.thread_id,
                session_id: params.session_id.clone(),
                command,
                cwd,
                load_session_id,
                preferred_model: preferred_model.clone(),
            },
        );

        Ok(protocol::ChatLoadResult {
            session_id: params.session_id,
            status: protocol::ChatSessionStatus::Starting,
        })
    }

    /// Fork a session at a given JSONL cursor, creating a new session with conversation
    /// context built from the source history up to that point.
    pub async fn fork(
        state: Arc<AppState>,
        params: protocol::ChatForkParams,
    ) -> Result<protocol::ChatForkResult, String> {
        let source_thread_id = params.thread_id.clone();
        let target_thread_id = params
            .target_thread_id
            .clone()
            .unwrap_or_else(|| source_thread_id.clone());
        let (source_agent_type, source_display_name, source_agent_command, history_root) = {
            let chat = state.chat.lock().await;
            let source = chat
                .sessions
                .get(&params.source_session_id)
                .ok_or_else(|| format!("source session not found: {}", params.source_session_id))?;
            if source.thread_id != source_thread_id {
                return Err(format!(
                    "source session {} does not belong to thread {}",
                    params.source_session_id, source_thread_id
                ));
            }
            (
                source.summary.agent_type.clone(),
                source.display_name.clone(),
                source.agent_command.clone(),
                chat.history_root_path().to_path_buf(),
            )
        };

        // Validate cursor against source JSONL
        let source_path =
            history_path_for_session(&history_root, &source_thread_id, &params.source_session_id);
        if source_path.exists() {
            let line_count = count_lines(&source_path)?;
            if params.message_cursor > line_count {
                return Err(format!(
                    "message_cursor {} exceeds source history line count {}",
                    params.message_cursor, line_count
                ));
            }
        }

        // Copy source JSONL before the selected cursor. Forking from a message
        // replaces that message in the next turn, so the selected line itself is excluded.
        let fork_session_id = Uuid::new_v4().to_string();
        let fork_path =
            history_path_for_session(&history_root, &target_thread_id, &fork_session_id);

        let copy_line_count = params.message_cursor.saturating_sub(1);
        if source_path.exists() && copy_line_count > 0 {
            if let Some(parent) = fork_path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("failed to create fork history dir: {e}"))?;
            }
            let src_file = fs::File::open(&source_path)
                .map_err(|e| format!("failed to open source history: {e}"))?;
            let reader = BufReader::new(src_file);
            let mut dst_file = fs::File::create(&fork_path)
                .map_err(|e| format!("failed to create fork history: {e}"))?;
            for (i, line) in reader.lines().enumerate() {
                if i as u64 >= copy_line_count {
                    break;
                }
                let line = line.map_err(|e| format!("failed to read source history line: {e}"))?;
                use std::io::Write;
                writeln!(dst_file, "{line}")
                    .map_err(|e| format!("failed to write fork history: {e}"))?;
            }
        }

        // Build conversation context from the copied JSONL
        let conversation_context = build_conversation_context(&fork_path, None);

        // Generate display name with fork count
        let fork_display_name = {
            let chat = state.chat.lock().await;
            let base = source_display_name.as_deref().unwrap_or(&source_agent_type);
            let existing_forks = chat
                .sessions
                .values()
                .filter(|s| s.parent_session_id.as_deref() == Some(&params.source_session_id))
                .count();
            Some(format!("{base} (fork #{})", existing_forks + 1))
        };

        // Create session runtime (status: Ended — Mac will call chat.load to start agent)
        let created_at = Utc::now().to_rfc3339();
        {
            let mut chat = state.chat.lock().await;
            let summary = protocol::ChatSessionSummary {
                session_id: fork_session_id.clone(),
                agent_type: source_agent_type.clone(),
                status: protocol::ChatSessionStatus::Ended,
                agent_status: protocol::AgentStatus::Idle,
                worker_count: 0,
                title: None,
                model_id: None,
                created_at,
                display_name: fork_display_name.clone(),
                parent_session_id: Some(params.source_session_id.clone()),
                pending_blocked_requests: Vec::new(),
            };
            let mut runtime = ChatSessionRuntime::new(summary, target_thread_id.clone(), fork_path);
            runtime.display_name = fork_display_name.clone();
            runtime.parent_session_id = Some(params.source_session_id.clone());
            runtime.agent_command = source_agent_command;
            runtime.conversation_context = conversation_context;
            runtime.had_conversation_context = true;
            chat.sessions.insert(fork_session_id.clone(), runtime);
            chat.sessions_by_thread
                .entry(target_thread_id.clone())
                .or_default()
                .push(fork_session_id.clone());
        }

        state.emit_chat_session_created(protocol::ChatSessionCreatedEvent {
            thread_id: target_thread_id.clone(),
            session_id: fork_session_id.clone(),
            agent_type: source_agent_type.clone(),
            display_name: fork_display_name.clone(),
            parent_session_id: Some(params.source_session_id.clone()),
        });
        emit_state_delta_added(&state, &target_thread_id, &fork_session_id).await;

        Ok(protocol::ChatForkResult {
            session_id: fork_session_id,
            agent_type: source_agent_type,
            display_name: fork_display_name,
        })
    }

    pub async fn stop(
        state: Arc<AppState>,
        params: protocol::ChatStopParams,
    ) -> Result<protocol::ChatStopResult, String> {
        stop_session_internal(
            state,
            &params.thread_id,
            &params.session_id,
            "stopped",
            false,
        )
        .await?;
        Ok(protocol::ChatStopResult { archived: true })
    }

    pub async fn list(
        state: Arc<AppState>,
        params: protocol::ChatListParams,
    ) -> Result<protocol::ChatListResult, String> {
        Ok(chat_session_summaries_for_thread(&state, &params.thread_id).await)
    }

    pub async fn history(
        state: Arc<AppState>,
        params: protocol::ChatHistoryParams,
    ) -> Result<protocol::ChatHistoryResult, String> {
        if !is_safe_history_component(&params.thread_id)
            || !is_safe_history_component(&params.session_id)
        {
            return Ok(protocol::ChatHistoryResult {
                updates: Vec::new(),
                next_cursor: None,
            });
        }

        let history_root = {
            let chat = state.chat.lock().await;
            chat.history_root.clone()
        };
        let history_path =
            history_path_for_session(&history_root, &params.thread_id, &params.session_id);
        read_history_page(&history_path, params.cursor)
    }

    pub(crate) async fn prepare_restored_session_context(
        state: Arc<AppState>,
        thread_id: &str,
        session_id: &str,
    ) -> Result<(), String> {
        let history_root = {
            let chat = state.chat.lock().await;
            chat.history_root_path().to_path_buf()
        };
        let history_path = history_path_for_session(&history_root, thread_id, session_id);
        let conversation_context = build_conversation_context(&history_path, None);

        let session_updated = {
            let mut chat = state.chat.lock().await;
            if let Some(session) = chat.sessions.get_mut(session_id) {
                session.acp_session_id = None;
                if conversation_context.is_some() {
                    session.had_conversation_context = true;
                }
                session.conversation_context = conversation_context.clone();
                true
            } else {
                false
            }
        };

        if !session_updated {
            return Ok(());
        }

        persist_restored_session_marker(&history_root, thread_id, session_id)?;
        persist_session_metadata(&history_root, thread_id, session_id, None)?;

        Ok(())
    }

    pub async fn status(
        state: Arc<AppState>,
        params: protocol::ChatStatusParams,
    ) -> Result<protocol::ChatStatusResult, String> {
        let chat = state.chat.lock().await;
        let runtime = chat
            .sessions
            .get(&params.session_id)
            .ok_or_else(|| format!("session not found: {}", params.session_id))?;
        Ok(summary_with_blocked_requests(runtime))
    }

    pub async fn answer_blocked_request(
        state: Arc<AppState>,
        params: protocol::ChatAnswerBlockedRequestParams,
    ) -> Result<protocol::ChatAnswerBlockedRequestResult, ChatBlockedRequestAnswerError> {
        let answered_at = Utc::now().to_rfc3339();
        let (result, removed) = {
            let mut chat = state.chat.lock().await;
            let session = chat.sessions.get_mut(&params.session_id).ok_or_else(|| {
                ChatBlockedRequestAnswerError::NotFound(format!(
                    "chat session not found: {}",
                    params.session_id
                ))
            })?;
            if session.thread_id != params.thread_id {
                return Err(ChatBlockedRequestAnswerError::Invalid(format!(
                    "chat session {} does not belong to thread {}",
                    params.session_id, params.thread_id
                )));
            }
            let Some(pending) = session
                .pending_blocked_requests
                .get(&params.request_id)
                .cloned()
            else {
                return Err(ChatBlockedRequestAnswerError::AlreadyResolved(
                    protocol::BlockedRequestAlreadyResolvedError {
                        thread_id: params.thread_id,
                        session_id: params.session_id,
                        request_id: params.request_id,
                    },
                ));
            };
            let result =
                build_blocked_request_answer_result(&pending.request, &params, answered_at)?;
            let response = agent_response_for_blocked_request(&pending, &result)?;
            let input_tx = session.input_tx.as_ref().ok_or_else(|| {
                ChatBlockedRequestAnswerError::Delivery(format!(
                    "chat session {} is not running",
                    session.summary.session_id
                ))
            })?;
            input_tx.try_send(response).map_err(|err| {
                ChatBlockedRequestAnswerError::Delivery(format!(
                    "failed to queue blocked request answer for {}: {err}",
                    result.request_id
                ))
            })?;
            let removed = session.pending_blocked_requests.remove(&params.request_id);
            (result, removed)
        };

        if removed.is_some() {
            state.emit_event(
                "chat.blocked_request.answered",
                protocol::BlockedRequestAnsweredEvent {
                    result: result.clone(),
                },
            );
            state.emit_event(
                "chat.blocked_request.removed",
                protocol::BlockedRequestRemovedEvent {
                    thread_id: result.thread_id.clone(),
                    session_id: result.session_id.clone(),
                    request_id: result.request_id.clone(),
                },
            );
            emit_state_delta_updated(&state, &result.thread_id, &result.session_id).await;
        }

        Ok(result)
    }

    /// Inject a `<system-context>` ACP prompt into a live session.
    ///
    /// The injected prompt is tracked in `pending_prompt_ids` so the session's status
    /// machine flips Busy → Idle when the agent responds, matching the behaviour of
    /// user-originated prompts. The injection is NOT tagged via `injection_prompt_id`
    /// (that singleton is reserved for the handshake-time injection that fires
    /// `chat.injection_complete`); mid-session injections complete silently.
    ///
    /// If the agent is mid-turn, ACP queues the prompt for delivery after the current
    /// turn. The underlying mpsc + single stdin writer task guarantees frame-level
    /// atomicity — no concurrent-write mutex is required.
    ///
    /// Returns an error if the session is unknown, has no active ACP channel yet
    /// (pre-handshake), or the stdin channel is closed.
    pub async fn inject_system_context(
        state: Arc<AppState>,
        session_id: &str,
        context: &str,
    ) -> Result<(), String> {
        if context.trim().is_empty() {
            return Err("inject_system_context: context must not be empty".to_string());
        }

        let (input_tx, acp_session_id) = {
            let chat = state.chat.lock().await;
            let session = chat
                .sessions
                .get(session_id)
                .ok_or_else(|| format!("session not found: {session_id}"))?;
            let input_tx = session
                .input_tx
                .clone()
                .ok_or_else(|| format!("session {session_id} has no active stdin channel"))?;
            let acp_session_id = session
                .acp_session_id
                .clone()
                .ok_or_else(|| format!("session {session_id} has not completed ACP handshake"))?;
            (input_tx, acp_session_id)
        };

        let wrapped = format!("<system-context>\n{}\n</system-context>", context.trim());
        let request_id = injection_request_id();
        let request_id_str = request_id.to_string();

        send_acp_request(
            &input_tx,
            request_id,
            "session/prompt",
            json!({
                "sessionId": acp_session_id,
                "prompt": [{"type": "text", "text": wrapped}],
            }),
        )
        .await?;

        // Register in the session's pending-prompt set so the Busy/Idle status machine
        // accounts for this in-flight ACP turn. Cleared by `apply_outbound_status_updates`
        // when the response arrives.
        let transitions = {
            let mut chat = state.chat.lock().await;
            let mut transitions = Vec::new();
            if let Some(session) = chat.sessions.get_mut(session_id) {
                session.pending_prompt_ids.insert(request_id_str);
                apply_status_transition(session, protocol::AgentStatus::Busy, &mut transitions);
            }
            transitions
        };
        emit_status_transitions(&state, transitions).await;

        tracing::info!(
            session_id = %session_id,
            request_id,
            len = wrapped.len(),
            "injected system context"
        );
        Ok(())
    }

    pub async fn recover_persisted_sessions(state: Arc<AppState>) -> Result<(), String> {
        let known_threads = {
            let store = state.store.lock().await;
            store
                .data
                .threads
                .iter()
                .map(|thread| thread.id.clone())
                .collect::<HashSet<_>>()
        };
        let history_root = {
            let chat = state.chat.lock().await;
            chat.history_root.clone()
        };

        let recovered = discover_history_sessions(&history_root, &known_threads)?;

        if recovered.is_empty() {
            return Ok(());
        }

        let mut chat = state.chat.lock().await;
        for recovered in recovered {
            if chat.sessions.contains_key(&recovered.summary.session_id) {
                continue;
            }

            let session_id = recovered.summary.session_id.clone();
            let thread_id = recovered.thread_id.clone();
            chat.sessions.insert(session_id.clone(), recovered);
            chat.sessions_by_thread
                .entry(thread_id)
                .or_default()
                .push(session_id);
        }

        Ok(())
    }

    pub async fn attach(
        params: protocol::ChatAttachParams,
        state: Arc<AppState>,
        connection_state: Arc<Mutex<TerminalConnectionState>>,
        outbound_tx: mpsc::UnboundedSender<Message>,
        supports_blocked_requests: bool,
    ) -> Result<protocol::ChatAttachResult, String> {
        loop {
            let wait_notify = {
                let chat = state.chat.lock().await;
                let session = chat
                    .sessions
                    .get(&params.session_id)
                    .ok_or_else(|| format!("chat session not found: {}", params.session_id))?;
                if session.thread_id != params.thread_id {
                    return Err(format!(
                        "chat session {} does not belong to thread {}",
                        params.session_id, params.thread_id
                    ));
                }

                match session.summary.status {
                    protocol::ChatSessionStatus::Ready => None,
                    protocol::ChatSessionStatus::Starting => {
                        Some(Arc::clone(&session.status_notify))
                    }
                    protocol::ChatSessionStatus::Failed | protocol::ChatSessionStatus::Ended => {
                        return Err(format!(
                            "chat session {} is {}",
                            params.session_id,
                            chat_status_name(&session.summary.status)
                        ))
                    }
                }
            };

            if let Some(notify) = wait_notify {
                timeout(CHAT_ATTACH_WAIT_TIMEOUT, notify.notified())
                    .await
                    .map_err(|_| {
                        format!(
                            "timed out waiting for chat session {} to become ready",
                            params.session_id
                        )
                    })?;
                continue;
            }

            let mut conn = connection_state.lock().await;
            let mut chat = state.chat.lock().await;
            let channel_id = state.alloc_channel_id_with(|candidate| {
                conn.by_channel.contains_key(&candidate)
                    || conn.by_chat_channel.contains_key(&candidate)
                    || conn
                        .attaching_targets
                        .values()
                        .any(|existing| *existing == candidate)
                    || chat.channel_to_session.contains_key(&candidate)
            });

            {
                let session = chat
                    .sessions
                    .get(&params.session_id)
                    .ok_or_else(|| format!("chat session not found: {}", params.session_id))?;
                if session.summary.status != protocol::ChatSessionStatus::Ready {
                    continue;
                }
            }

            let (acp_sid, modes, models, config_options, pending_blocked_requests) = {
                let session = chat.sessions.get(&params.session_id);
                (
                    session
                        .and_then(|s| s.acp_session_id.clone())
                        .unwrap_or_default(),
                    session.and_then(|s| s.modes.clone()),
                    session.and_then(|s| s.models.clone()),
                    session.and_then(|s| s.config_options.clone()),
                    session
                        .map(pending_blocked_requests_for_session)
                        .unwrap_or_default(),
                )
            };

            if let Some(session) = chat.sessions.get_mut(&params.session_id) {
                session.attached_channels.insert(channel_id);
                if supports_blocked_requests {
                    session.blocked_request_capture_enabled = true;
                }
            }
            chat.channel_to_session
                .insert(channel_id, params.session_id.clone());
            chat.channel_outbound
                .insert(channel_id, outbound_tx.clone());
            if supports_blocked_requests {
                chat.blocked_request_aware_channels.insert(channel_id);
            }
            conn.by_chat_channel
                .insert(channel_id, params.session_id.clone());

            return Ok(protocol::ChatAttachResult {
                channel_id,
                acp_session_id: acp_sid,
                modes,
                models,
                config_options,
                pending_blocked_requests,
            });
        }
    }

    pub async fn detach(
        params: protocol::ChatDetachParams,
        state: Arc<AppState>,
        connection_state: Arc<Mutex<TerminalConnectionState>>,
    ) -> Result<protocol::ChatDetachResult, String> {
        let session_id = {
            let mut conn = connection_state.lock().await;
            conn.by_chat_channel.remove(&params.channel_id)
        };

        let detached = detach_channel(state, params.channel_id, session_id).await;
        Ok(protocol::ChatDetachResult { detached })
    }

    pub async fn handle_binary_frame(
        state: Arc<AppState>,
        channel_id: u16,
        payload: Vec<u8>,
    ) -> Result<bool, String> {
        let (
            thread_id,
            session_id,
            input_tx,
            transitions,
            replacement_payload,
            prompt_updates,
            mut auto_checkpoints,
            title_prompt_to_generate,
            history_path,
            consumed_conversation_context,
            blocked_request_cancellations,
        ) = {
            let mut chat = state.chat.lock().await;
            let Some(session_id) = chat.channel_to_session.get(&channel_id).cloned() else {
                return Ok(false);
            };
            let session = chat
                .sessions
                .get_mut(&session_id)
                .ok_or_else(|| format!("chat session missing for channel {channel_id}"))?;
            let input_tx = session
                .input_tx
                .as_ref()
                .ok_or_else(|| {
                    format!("chat session {} is not running", session.summary.session_id)
                })?
                .clone();
            let history_path = session.history_path.clone();
            let messages = extract_json_messages(&mut session.input_buffer, &payload, "input");
            let SessionProcessingOutcome {
                transitions,
                replacement_payload,
                history_updates: prompt_updates,
                auto_checkpoints,
                title_prompt_to_generate,
                consumed_conversation_context,
                blocked_request_cancellations,
            } = apply_inbound_status_updates(&state, session, messages);
            (
                session.thread_id.clone(),
                session_id,
                input_tx,
                transitions,
                replacement_payload,
                prompt_updates,
                auto_checkpoints,
                title_prompt_to_generate,
                history_path,
                consumed_conversation_context,
                blocked_request_cancellations,
            )
        };

        emit_status_transitions(&state, transitions).await;

        let BlockedRequestCancellationDelivery {
            removals: blocked_request_removals,
            error: blocked_request_delivery_error,
        } = deliver_blocked_request_cancellations(
            &state,
            &session_id,
            channel_id,
            blocked_request_cancellations,
        )
        .await;

        let had_blocked_request_cancellations = !blocked_request_removals.is_empty();
        emit_blocked_request_removed_events(&state, blocked_request_removals);
        if had_blocked_request_cancellations {
            emit_state_delta_updated(&state, &thread_id, &session_id).await;
        }
        if let Some(error) = blocked_request_delivery_error {
            return Err(error);
        }

        if let Some(first_prompt) = title_prompt_to_generate {
            spawn_title_generation(
                Arc::clone(&state),
                session_id.clone(),
                thread_id.clone(),
                first_prompt,
            );
        }

        // Capture history cursor BEFORE writing user echoes to the JSONL.
        // Auto-checkpoints need the line count from before the echo so that
        // checkpoint.restore truncates to a consistent point (without orphan
        // user echoes that lack agent responses).
        if !auto_checkpoints.is_empty() && history_path.exists() {
            match count_lines(&history_path) {
                Ok(cursor) => {
                    for checkpoint in &mut auto_checkpoints {
                        checkpoint.history_cursor = Some(cursor);
                    }
                }
                Err(error) => {
                    warn!(error = %error, "failed to read history cursor for auto-checkpoint");
                }
            }
        }

        if !prompt_updates.is_empty() {
            match append_updates_to_history(&history_path, &prompt_updates) {
                Ok(()) => {
                    record_pending_user_echoes(&state, &session_id, &prompt_updates).await;
                    emit_session_update_notifications(
                        Arc::clone(&state),
                        &session_id,
                        &prompt_updates,
                    )
                    .await;
                }
                Err(error) => {
                    warn!(channel_id, error = %error, "failed to persist user prompt to chat history");
                }
            }
        }

        for checkpoint in auto_checkpoints {
            spawn_auto_checkpoint(Arc::clone(&state), checkpoint);
        }

        // Rewrite Spindle session ID to ACP session ID in the payload
        // so the agent receives its own session ID namespace
        let outbound_payload = replacement_payload.unwrap_or(payload);

        let rewritten_payload = {
            let chat = state.chat.lock().await;
            if let Some(sid) = chat.channel_to_session.get(&channel_id) {
                if let Some(session) = chat.sessions.get(sid) {
                    if let Some(ref acp_id) = session.acp_session_id {
                        rewrite_session_id(&outbound_payload, sid, acp_id)
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        };
        let final_payload = rewritten_payload.unwrap_or(outbound_payload);

        if let Ok(s) = std::str::from_utf8(&final_payload) {
            warn!(
                channel_id,
                "chat_inbound_frame payload_len={} snippet={:?}",
                final_payload.len(),
                &s[..s.floor_char_boundary(s.len().min(200))]
            );
        }

        input_tx
            .try_send(final_payload)
            .map_err(|err| format!("failed to queue chat input for channel {channel_id}: {err}"))?;

        if consumed_conversation_context {
            clear_pending_conversation_context(&state, &thread_id, &session_id).await?;
        }

        Ok(true)
    }
    pub async fn cleanup_connection_channels(
        state: Arc<AppState>,
        connection_state: Arc<Mutex<TerminalConnectionState>>,
    ) {
        let channels = {
            let mut conn = connection_state.lock().await;
            let channels = conn.by_chat_channel.keys().copied().collect::<Vec<_>>();
            conn.by_chat_channel.clear();
            channels
        };
        detach_channels(state, channels).await;
    }

    pub async fn stop_all_for_thread(
        state: Arc<AppState>,
        thread_id: &str,
        reason: &str,
        purge: bool,
    ) -> Result<(), String> {
        let session_ids = {
            let chat = state.chat.lock().await;
            chat.sessions_by_thread
                .get(thread_id)
                .cloned()
                .unwrap_or_default()
        };

        for session_id in session_ids {
            stop_session_internal(Arc::clone(&state), thread_id, &session_id, reason, purge)
                .await?;
        }

        if purge {
            let history_root = {
                let chat = state.chat.lock().await;
                chat.history_root.clone()
            };
            if let Err(error) = remove_thread_history_dir(&history_root, thread_id) {
                warn!(thread_id, error = %error, "failed to remove thread chat history directory");
            }
        }

        Ok(())
    }

    pub async fn thread_chat_sessions(
        state: Arc<AppState>,
        thread_id: &str,
    ) -> Vec<protocol::ChatSessionSummary> {
        chat_session_summaries_for_thread(&state, thread_id).await
    }
}

async fn perform_handshake(
    input_tx: &mpsc::Sender<Vec<u8>>,
    stdout: &mut tokio::process::ChildStdout,
    load_session_id: Option<String>,
    cwd: &str,
    preferred_model: Option<&str>,
) -> Result<HandshakeResult, String> {
    let mut buffer = Vec::new();
    let mut collected = Vec::new();

    send_acp_request(
        input_tx,
        1,
        "initialize",
        json!({
            "protocolVersion": 1,
            "clientCapabilities": {
                "fs": {
                    "readTextFile": false,
                    "writeTextFile": false
                },
                "terminal": false
            },
            "clientInfo": {
                "name": "Threadmill",
                "title": "Threadmill",
                "version": "dev"
            }
        }),
    )
    .await?;

    let init = wait_for_acp_response(stdout, &mut buffer, 1, &mut collected).await?;
    // Discard anything collected before initialize response (shouldn't be any)
    collected.clear();
    ensure_acp_success(&init, "initialize")?;

    let (method, params) = if let Some(session_id) = load_session_id {
        (
            "session/load",
            json!({ "sessionId": session_id, "cwd": cwd, "mcpServers": [] }),
        )
    } else {
        ("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
    };

    send_acp_request(input_tx, 2, method, params).await?;
    // For session/load, the agent replays the full conversation history as
    // session/update notifications before returning the response. These are
    // collected here instead of discarded.
    let session_response = wait_for_acp_response(stdout, &mut buffer, 2, &mut collected).await?;
    let result = ensure_acp_success(&session_response, method)?;

    if !collected.is_empty() {
        tracing::info!(
            "perform_handshake: captured {} notification(s) during {method}",
            collected.len()
        );
    }

    let acp_session_id = result
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{method} response missing sessionId"))?
        .to_string();

    let mut model_id = result
        .get("models")
        .and_then(|models| models.get("currentModelId"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let mut models = result.get("models").cloned();

    if let Some(preferred_model) = preferred_model {
        send_acp_request(
            input_tx,
            3,
            "session/set_model",
            json!({
                "sessionId": acp_session_id,
                "modelId": preferred_model,
            }),
        )
        .await?;
        let set_model_response =
            wait_for_acp_response(stdout, &mut buffer, 3, &mut collected).await?;
        ensure_acp_success(&set_model_response, "session/set_model")?;
        model_id = Some(preferred_model.to_string());
        if let Some(models_value) = models.as_mut().and_then(Value::as_object_mut) {
            models_value.insert(
                "currentModelId".to_string(),
                Value::String(preferred_model.to_string()),
            );
        }
    }

    Ok(HandshakeResult {
        acp_session_id,
        modes: result.get("modes").cloned(),
        models,
        config_options: result.get("configOptions").cloned(),
        title: result
            .get("title")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        model_id,
        replay_notifications: collected,
    })
}

fn blocked_request_from_message(
    session: &ChatSessionRuntime,
    message: &Value,
) -> BlockedRequestFrame {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return BlockedRequestFrame::NotBlockedRequest;
    };
    let is_permission_method = matches!(
        method,
        CHAT_REQUEST_PERMISSION_METHOD | CHAT_SESSION_REQUEST_PERMISSION_METHOD
    );
    let is_question_method = matches!(
        method,
        CHAT_REQUEST_QUESTION_METHOD | CHAT_SESSION_REQUEST_QUESTION_METHOD
    );
    if !is_permission_method && !is_question_method {
        return BlockedRequestFrame::NotBlockedRequest;
    }

    let request_id_value = message.get("id").cloned().unwrap_or(Value::Null);
    let Some(request_id) = request_id_key(message.get("id")) else {
        return BlockedRequestFrame::Invalid(InvalidBlockedRequestRuntime {
            method: method.to_string(),
            request_id: request_id_value,
            reason: "blocked request id must be a string or integer".to_string(),
        });
    };
    let acp_request_id = message
        .get("id")
        .cloned()
        .unwrap_or(Value::String(request_id.clone()));
    let params = message.get("params");
    let created_at = Utc::now().to_rfc3339();

    if is_permission_method {
        let message_text = params
            .and_then(|params| params.get("message"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let Some(permission) = blocked_permission_from_params(params) else {
            return BlockedRequestFrame::Invalid(InvalidBlockedRequestRuntime {
                method: method.to_string(),
                request_id: request_id_value,
                reason:
                    "permission blocked request requires at least one option with a non-empty id"
                        .to_string(),
            });
        };
        let title = permission_title(params, &message_text);
        return BlockedRequestFrame::Pending(Box::new(PendingBlockedRequestRuntime {
            acp_request_id,
            request: protocol::BlockedRequest {
                thread_id: session.thread_id.clone(),
                session_id: session.summary.session_id.clone(),
                request_id,
                kind: protocol::BlockedRequestKind::Permission,
                title,
                message: message_text,
                created_at,
                question: None,
                permission: Some(permission),
                raw_request: Some(message.clone()),
            },
        }));
    }

    if is_question_method {
        let Some(prompt) = params
            .and_then(|params| {
                params
                    .get("question")
                    .or_else(|| params.get("prompt"))
                    .or_else(|| params.get("message"))
            })
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|prompt| !prompt.is_empty())
        else {
            return BlockedRequestFrame::Invalid(InvalidBlockedRequestRuntime {
                method: method.to_string(),
                request_id: request_id_value,
                reason:
                    "question blocked request requires a non-empty question, prompt, or message"
                        .to_string(),
            });
        };
        let prompt = prompt.to_string();
        let title = params
            .and_then(|params| params.get("title"))
            .and_then(Value::as_str)
            .filter(|title| !title.is_empty())
            .unwrap_or("Question requested")
            .to_string();
        return BlockedRequestFrame::Pending(Box::new(PendingBlockedRequestRuntime {
            acp_request_id,
            request: protocol::BlockedRequest {
                thread_id: session.thread_id.clone(),
                session_id: session.summary.session_id.clone(),
                request_id,
                kind: protocol::BlockedRequestKind::Question,
                title,
                message: prompt.clone(),
                created_at,
                question: Some(protocol::BlockedQuestionRequest {
                    prompt,
                    actions: vec![
                        protocol::BlockedRequestAnswerAction::Accept,
                        protocol::BlockedRequestAnswerAction::Decline,
                        protocol::BlockedRequestAnswerAction::Cancel,
                    ],
                }),
                permission: None,
                raw_request: Some(message.clone()),
            },
        }));
    }

    BlockedRequestFrame::NotBlockedRequest
}

fn invalid_blocked_request_error_payload(error: &InvalidBlockedRequestRuntime) -> Vec<u8> {
    serialize_json_messages(&[json!({
        "jsonrpc": "2.0",
        "id": error.request_id.clone(),
        "error": {
            "code": -32602,
            "message": error.reason,
            "data": {
                "kind": "invalid_blocked_request",
                "retryable": false,
                "details": {
                    "method": error.method,
                    "reason": error.reason,
                }
            }
        }
    })])
    .unwrap_or_default()
}

fn blocked_permission_from_params(
    params: Option<&Value>,
) -> Option<protocol::BlockedPermissionRequest> {
    let options = params
        .and_then(|params| params.get("options"))
        .and_then(Value::as_array)
        .map(|options| {
            options
                .iter()
                .filter_map(|option| {
                    let id = option
                        .get("optionId")
                        .or_else(|| option.get("option_id"))
                        .or_else(|| option.get("id"))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|id| !id.is_empty())?;
                    let label = option
                        .get("name")
                        .or_else(|| option.get("label"))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|label| !label.is_empty())
                        .unwrap_or(id);
                    let kind = option
                        .get("kind")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|kind| !kind.is_empty())
                        .map(ToOwned::to_owned);
                    Some(protocol::BlockedPermissionOption {
                        id: id.to_string(),
                        label: label.to_string(),
                        kind,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if options.is_empty() {
        return None;
    }

    let tool_call = params.and_then(|params| params.get("toolCall"));
    Some(protocol::BlockedPermissionRequest {
        tool_call_id: tool_call.and_then(permission_tool_call_id),
        tool_name: tool_call
            .and_then(|tool_call| tool_call.get("name"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        options,
    })
}

fn permission_tool_call_id(tool_call: &Value) -> Option<String> {
    string_field(tool_call, "toolCallId")
        .or_else(|| string_field(tool_call, "tool_call_id"))
        .or_else(|| string_field(tool_call, "id"))
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn permission_title(params: Option<&Value>, fallback_message: &str) -> String {
    let raw_input = params
        .and_then(|params| params.get("toolCall"))
        .and_then(|tool_call| tool_call.get("rawInput"));
    if let Some(path) = raw_input
        .and_then(|raw| {
            raw.get("file_path")
                .or_else(|| raw.get("filePath"))
                .or_else(|| raw.get("path"))
        })
        .and_then(Value::as_str)
    {
        return format!("File: {path}");
    }
    if let Some(command) = raw_input
        .and_then(|raw| raw.get("command").or_else(|| raw.get("cmd")))
        .and_then(Value::as_str)
    {
        let mut command = command.to_string();
        if command.len() > 80 {
            command.truncate(command.floor_char_boundary(77));
            command.push('…');
        }
        return format!("Run: {command}");
    }
    if !fallback_message.is_empty() {
        return fallback_message.to_string();
    }
    "Permission requested".to_string()
}

fn build_blocked_request_answer_result(
    request: &protocol::BlockedRequest,
    params: &protocol::ChatAnswerBlockedRequestParams,
    answered_at: String,
) -> Result<protocol::BlockedRequestAnswerResult, ChatBlockedRequestAnswerError> {
    match request.kind {
        protocol::BlockedRequestKind::Question => {
            if params.option_id.is_some() {
                return Err(ChatBlockedRequestAnswerError::Invalid(
                    "question blocked requests must be answered with action".to_string(),
                ));
            }
            let action = params.action.clone().ok_or_else(|| {
                ChatBlockedRequestAnswerError::Invalid(
                    "question blocked requests require action".to_string(),
                )
            })?;
            Ok(protocol::BlockedRequestAnswerResult {
                thread_id: request.thread_id.clone(),
                session_id: request.session_id.clone(),
                request_id: request.request_id.clone(),
                action: Some(action),
                option_id: None,
                answered_at,
            })
        }
        protocol::BlockedRequestKind::Permission => {
            if params.action.is_some() {
                return Err(ChatBlockedRequestAnswerError::Invalid(
                    "permission blocked requests must be answered with option_id".to_string(),
                ));
            }
            let option_id = params.option_id.clone().ok_or_else(|| {
                ChatBlockedRequestAnswerError::Invalid(
                    "permission blocked requests require option_id".to_string(),
                )
            })?;
            let is_valid_option = request
                .permission
                .as_ref()
                .map(|permission| {
                    permission
                        .options
                        .iter()
                        .any(|option| option.id == option_id)
                })
                .unwrap_or(false);
            if !is_valid_option {
                return Err(ChatBlockedRequestAnswerError::Invalid(format!(
                    "permission option_id is not valid for blocked request {}",
                    request.request_id
                )));
            }
            Ok(protocol::BlockedRequestAnswerResult {
                thread_id: request.thread_id.clone(),
                session_id: request.session_id.clone(),
                request_id: request.request_id.clone(),
                action: None,
                option_id: Some(option_id),
                answered_at,
            })
        }
    }
}

fn agent_response_for_blocked_request(
    pending: &PendingBlockedRequestRuntime,
    result: &protocol::BlockedRequestAnswerResult,
) -> Result<Vec<u8>, ChatBlockedRequestAnswerError> {
    let result_value = match pending.request.kind {
        protocol::BlockedRequestKind::Permission => {
            let option_id = result.option_id.as_ref().ok_or_else(|| {
                ChatBlockedRequestAnswerError::Invalid(
                    "invariant violation: permission blocked request answer missing option_id"
                        .to_string(),
                )
            })?;
            json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": option_id,
                }
            })
        }
        protocol::BlockedRequestKind::Question => {
            let action = result.action.as_ref().ok_or_else(|| {
                ChatBlockedRequestAnswerError::Invalid(
                    "invariant violation: question blocked request answer missing action"
                        .to_string(),
                )
            })?;
            json!({
                "action": match action {
                    protocol::BlockedRequestAnswerAction::Accept => "accept",
                    protocol::BlockedRequestAnswerAction::Decline => "decline",
                    protocol::BlockedRequestAnswerAction::Cancel => "cancel",
                },
            })
        }
    };
    let mut payload = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": pending.acp_request_id,
        "result": result_value,
    }))
    .map_err(|err| {
        ChatBlockedRequestAnswerError::Invalid(format!(
            "failed to encode blocked request answer: {err}"
        ))
    })?;
    payload.push(b'\n');
    Ok(payload)
}

fn cancelled_agent_response_for_blocked_request(pending: &PendingBlockedRequestRuntime) -> Vec<u8> {
    let result_value = match pending.request.kind {
        protocol::BlockedRequestKind::Permission => json!({
            "outcome": {
                "outcome": "cancelled",
            }
        }),
        protocol::BlockedRequestKind::Question => json!({
            "action": "cancel",
        }),
    };
    let mut payload = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": pending.acp_request_id,
        "result": result_value,
    }))
    .expect("blocked request cancellation response must serialize");
    payload.push(b'\n');
    payload
}

fn blocked_request_removed_event(
    pending: &PendingBlockedRequestRuntime,
) -> protocol::BlockedRequestRemovedEvent {
    protocol::BlockedRequestRemovedEvent {
        thread_id: pending.request.thread_id.clone(),
        session_id: pending.request.session_id.clone(),
        request_id: pending.request.request_id.clone(),
    }
}

struct OutboundFrame {
    payload: Vec<u8>,
    message: Option<Value>,
}

fn extract_outbound_frames(buffer: &mut Vec<u8>, payload: &[u8]) -> Vec<OutboundFrame> {
    buffer.extend_from_slice(payload);
    let mut frames = Vec::new();

    while let Some(newline_idx) = buffer.iter().position(|byte| *byte == b'\n') {
        let mut line = buffer[..newline_idx].to_vec();
        buffer.drain(..=newline_idx);
        if line.is_empty() {
            continue;
        }

        let message = match serde_json::from_slice::<Value>(&line) {
            Ok(value) => Some(value),
            Err(error) => {
                warn!(error = %error, direction = "output", "failed to parse chat JSON frame");
                None
            }
        };
        line.push(b'\n');
        frames.push(OutboundFrame {
            payload: line,
            message,
        });
    }

    frames
}

fn collect_session_update_params(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .filter(|message| {
            message
                .get("method")
                .and_then(Value::as_str)
                .map(|method| method == CHAT_UPDATE_METHOD)
                .unwrap_or(false)
        })
        .filter_map(|message| message.get("params").cloned())
        .collect()
}

async fn fanout_output(state: Arc<AppState>, session_id: &str, payload: &[u8]) {
    let (
        targets,
        history_path,
        updates,
        transitions,
        injection_completed,
        thread_id,
        blocked_requests,
        daemon_aware_payload,
        legacy_payload,
        acp_session_id,
        invalid_blocked_request_deliveries,
        invalid_blocked_request_delivery_errors,
    ) = {
        let mut chat = state.chat.lock().await;
        let blocked_request_aware_channels = chat.blocked_request_aware_channels.clone();
        let Some(session) = chat.sessions.get_mut(session_id) else {
            return;
        };
        let frames = extract_outbound_frames(&mut session.output_buffer, payload);
        let attached_channels = session
            .attached_channels
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let should_capture_blocked_requests = session.blocked_request_capture_enabled;
        let mut blocked_requests = Vec::new();
        let mut pass_through_messages = Vec::new();
        let mut daemon_aware_payload = Vec::new();
        let mut legacy_payload = Vec::new();
        let mut invalid_blocked_request_deliveries = Vec::new();
        let mut invalid_blocked_request_delivery_errors = Vec::new();
        for frame in frames {
            match frame.message {
                Some(message) => {
                    if is_duplicate_user_echo(session, &message) {
                        continue;
                    }
                    clear_pending_user_echoes_after_agent_turn(session, &message);
                    match blocked_request_from_message(session, &message) {
                        BlockedRequestFrame::Pending(pending) => {
                            let pending = *pending;
                            if should_capture_blocked_requests {
                                session
                                    .pending_blocked_requests
                                    .insert(pending.request.request_id.clone(), pending.clone());
                                blocked_requests.push(pending.request);
                            } else {
                                legacy_payload.extend_from_slice(&frame.payload);
                                pass_through_messages.push(message);
                            }
                        }
                        BlockedRequestFrame::Invalid(error) => {
                            if should_capture_blocked_requests {
                                warn!(
                                    session_id,
                                    method = %error.method,
                                    request_id = %error.request_id,
                                    reason = %error.reason,
                                    "invalid blocked request from agent; sending error to agent stdin"
                                );
                                let payload = invalid_blocked_request_error_payload(&error);
                                if let Some(input_tx) = session.input_tx.clone() {
                                    invalid_blocked_request_deliveries.push(
                                        InvalidBlockedRequestDelivery {
                                            input_tx,
                                            payload,
                                            method: error.method,
                                            request_id: error.request_id.to_string(),
                                        },
                                    );
                                } else {
                                    invalid_blocked_request_delivery_errors.push(format!(
                                    "chat session {} cannot receive invalid blocked request error for {} request {}: agent stdin is not available",
                                    session.summary.session_id,
                                    error.method,
                                    error.request_id
                                ));
                                }
                            } else {
                                legacy_payload.extend_from_slice(&frame.payload);
                                pass_through_messages.push(message);
                            }
                        }
                        BlockedRequestFrame::NotBlockedRequest => {
                            pass_through_messages.push(message);
                            daemon_aware_payload.extend_from_slice(&frame.payload);
                            legacy_payload.extend_from_slice(&frame.payload);
                        }
                    }
                }
                None => {
                    daemon_aware_payload.extend_from_slice(&frame.payload);
                    legacy_payload.extend_from_slice(&frame.payload);
                }
            }
        }

        let updates = collect_session_update_params(&pass_through_messages);
        let outbound = apply_outbound_status_updates(&state, session, pass_through_messages);
        let thread_id = session.thread_id.clone();
        let history_path = session.history_path.clone();
        let acp_session_id = session.acp_session_id.clone();

        let targets = attached_channels
            .iter()
            .filter_map(|channel_id| {
                chat.channel_outbound
                    .get(channel_id)
                    .cloned()
                    .map(|outbound| {
                        (
                            *channel_id,
                            outbound,
                            blocked_request_aware_channels.contains(channel_id),
                        )
                    })
            })
            .collect::<Vec<_>>();

        (
            targets,
            history_path,
            updates,
            outbound.transitions,
            outbound.injection_completed,
            thread_id,
            blocked_requests,
            daemon_aware_payload,
            legacy_payload,
            acp_session_id,
            invalid_blocked_request_deliveries,
            invalid_blocked_request_delivery_errors,
        )
    };

    if let Some(error) = invalid_blocked_request_delivery_errors.into_iter().next() {
        tracing::error!(session_id, error = %error, "failed to deliver invalid blocked request error");
        mark_session_failed(Arc::clone(&state), &thread_id, session_id, error).await;
        return;
    }

    for delivery in invalid_blocked_request_deliveries {
        if let Err(error) = delivery.input_tx.try_send(delivery.payload) {
            let message = format!(
                "failed to queue invalid blocked request error for {} request {}: {error}",
                delivery.method, delivery.request_id
            );
            tracing::error!(session_id, error = %message, "failed to deliver invalid blocked request error");
            mark_session_failed(Arc::clone(&state), &thread_id, session_id, message).await;
            return;
        }
    }

    emit_status_transitions(&state, transitions).await;

    let has_blocked_requests = !blocked_requests.is_empty();
    for request in blocked_requests {
        state.emit_event(
            "chat.blocked_request.added",
            protocol::BlockedRequestAddedEvent { request },
        );
    }
    if has_blocked_requests {
        emit_state_delta_updated(&state, &thread_id, session_id).await;
    }

    if injection_completed {
        tracing::info!(
            session_id,
            "injection turn completed — emitting chat.injection_complete"
        );
        state.emit_event(
            "chat.injection_complete",
            json!({
                "thread_id": thread_id,
                "session_id": session_id,
            }),
        );
    }

    // Emit worker_update to parent session if this is a child worker
    emit_worker_update_to_parent(&state, session_id).await;

    if !updates.is_empty() {
        if let Err(error) = append_updates_to_history(&history_path, &updates) {
            warn!(
                session_id,
                path = %history_path.display(),
                error = %error,
                "failed to persist chat session/update payload"
            );
        }
    }

    let rewrite_outbound = |payload: Vec<u8>| -> Vec<u8> {
        if payload.is_empty() {
            return payload;
        }
        acp_session_id
            .as_deref()
            .and_then(|acp_id| rewrite_session_id(&payload, acp_id, session_id))
            .unwrap_or(payload)
    };
    let daemon_aware_payload = rewrite_outbound(daemon_aware_payload);
    let legacy_payload = rewrite_outbound(legacy_payload);

    if daemon_aware_payload.is_empty() && legacy_payload.is_empty() {
        return;
    }

    let mut dead_channels = Vec::new();
    for (channel_id, outbound, supports_blocked_requests) in targets {
        let out_payload = if supports_blocked_requests {
            daemon_aware_payload.as_slice()
        } else {
            legacy_payload.as_slice()
        };
        if out_payload.is_empty() {
            continue;
        }
        if let Ok(s) = std::str::from_utf8(out_payload) {
            warn!(
                session_id,
                "chat_outbound_frame payload_len={} snippet={:?}",
                out_payload.len(),
                &s[..s.floor_char_boundary(s.len().min(200))]
            );
        }
        let mut frame = Vec::with_capacity(out_payload.len() + 2);
        frame.extend_from_slice(&channel_id.to_be_bytes());
        frame.extend_from_slice(out_payload);
        if outbound.send(Message::Binary(frame.into())).is_err() {
            dead_channels.push(channel_id);
        }
    }

    if !dead_channels.is_empty() {
        detach_channels(Arc::clone(&state), dead_channels).await;
    }
}

async fn record_pending_user_echoes(state: &Arc<AppState>, session_id: &str, updates: &[Value]) {
    let pending = updates
        .iter()
        .filter_map(history_user_message_text)
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return;
    }

    let mut chat = state.chat.lock().await;
    if let Some(session) = chat.sessions.get_mut(session_id) {
        session.pending_user_echoes.extend(pending);
    }
}

async fn emit_session_update_notifications(
    state: Arc<AppState>,
    session_id: &str,
    updates: &[Value],
) {
    if updates.is_empty() {
        return;
    }

    let targets = {
        let chat = state.chat.lock().await;
        let Some(session) = chat.sessions.get(session_id) else {
            return;
        };
        session
            .attached_channels
            .iter()
            .filter_map(|channel_id| {
                chat.channel_outbound
                    .get(channel_id)
                    .cloned()
                    .map(|outbound| (*channel_id, outbound))
            })
            .collect::<Vec<_>>()
    };

    let mut dead_channels = Vec::new();
    for update in updates {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": CHAT_UPDATE_METHOD,
            "params": update,
        });
        let Ok(payload) = serde_json::to_vec(&notification) else {
            continue;
        };
        for (channel_id, outbound) in &targets {
            let mut frame = Vec::with_capacity(payload.len() + 2);
            frame.extend_from_slice(&channel_id.to_be_bytes());
            frame.extend_from_slice(&payload);
            if outbound.send(Message::Binary(frame.into())).is_err() {
                dead_channels.push(*channel_id);
            }
        }
    }

    if !dead_channels.is_empty() {
        dead_channels.sort_unstable();
        dead_channels.dedup();
        detach_channels(state, dead_channels).await;
    }
}

#[cfg(test)]
fn filter_duplicate_user_echoes(
    session: &mut ChatSessionRuntime,
    messages: Vec<Value>,
) -> (Vec<Value>, Option<Vec<u8>>) {
    let original_count = messages.len();
    let mut filtered = Vec::with_capacity(original_count);
    for message in messages {
        if is_duplicate_user_echo(session, &message) {
            continue;
        }
        clear_pending_user_echoes_after_agent_turn(session, &message);
        filtered.push(message);
    }
    let payload_override = if filtered.len() == original_count {
        None
    } else {
        Some(serialize_json_messages(&filtered).unwrap_or_default())
    };

    (filtered, payload_override)
}

fn is_duplicate_user_echo(session: &mut ChatSessionRuntime, message: &Value) -> bool {
    if message
        .get("method")
        .and_then(Value::as_str)
        .map(|method| method != CHAT_UPDATE_METHOD)
        .unwrap_or(true)
    {
        return false;
    }

    let Some(text) = message.get("params").and_then(history_user_message_text) else {
        return false;
    };

    if session
        .pending_user_echoes
        .front()
        .is_some_and(|pending| pending == text)
    {
        session.pending_user_echoes.pop_front();
        return true;
    }

    false
}

fn update_kind_from_message(message: &Value) -> Option<&str> {
    let update = message.get("params")?.get("update")?;
    update
        .get("sessionUpdate")
        .or_else(|| update.get("kind"))
        .and_then(Value::as_str)
}

fn clear_pending_user_echoes_after_agent_turn(session: &mut ChatSessionRuntime, message: &Value) {
    if let Some("agent_message_chunk" | "agent_thought_chunk" | "tool_call") =
        update_kind_from_message(message)
    {
        session.pending_user_echoes.clear();
    }
}

fn spawn_title_generation(
    state: Arc<AppState>,
    session_id: String,
    thread_id: String,
    first_prompt: String,
) {
    if first_prompt.trim().is_empty() {
        return;
    }

    tokio::spawn(async move {
        match state.title.generate_title(&first_prompt).await {
            Ok(title) => {
                tracing::info!(%session_id, %title, "title generated");
                {
                    let mut chat = state.chat.lock().await;
                    if let Some(session) = chat.sessions.get_mut(&session_id) {
                        session.summary.title = Some(title.clone());
                    }
                }
                {
                    let mut store = state.store.lock().await;
                    if let Some(thread) = store.data.threads.iter_mut().find(|t| t.id == thread_id)
                    {
                        thread.display_name = Some(title);
                    }
                    store.save().ok();
                }
                emit_state_delta_updated(&state, &thread_id, &session_id).await;
            }
            Err(err) => {
                tracing::warn!(%session_id, error = %err, "title generation failed");
            }
        }
    });
}

async fn emit_worker_update_to_parent(state: &Arc<AppState>, worker_session_id: &str) {
    let event = {
        let chat = state.chat.lock().await;
        let Some(worker) = chat.sessions.get(worker_session_id) else {
            return;
        };
        let Some(ref parent_id) = worker.parent_session_id else {
            return;
        };

        let duration_ms = worker.started_at.map(|started| {
            Utc::now()
                .signed_duration_since(started)
                .num_milliseconds()
                .max(0) as u64
        });

        let latest_tool =
            worker
                .latest_tool_name
                .as_ref()
                .map(|name| protocol::WorkerToolSummary {
                    name: name.clone(),
                    title: worker.latest_tool_title.clone(),
                });

        protocol::ChatWorkerUpdateEvent {
            parent_session_id: parent_id.clone(),
            worker_session_id: worker_session_id.to_string(),
            agent_status: worker.summary.agent_status.clone(),
            display_name: worker.display_name.clone(),
            latest_tool,
            tool_count: worker.total_tool_count,
            duration_ms,
        }
    };

    state.emit_event("chat.worker_update", &event);
}

fn drain_pending_blocked_request_removals(
    session: &mut ChatSessionRuntime,
) -> Vec<protocol::BlockedRequestRemovedEvent> {
    session
        .pending_blocked_requests
        .drain()
        .map(|(_, pending)| protocol::BlockedRequestRemovedEvent {
            thread_id: pending.request.thread_id,
            session_id: pending.request.session_id,
            request_id: pending.request.request_id,
        })
        .collect()
}

fn collect_pending_blocked_request_cancellations(
    session: &mut ChatSessionRuntime,
) -> Vec<BlockedRequestCancellation> {
    let mut pending = session
        .pending_blocked_requests
        .values()
        .cloned()
        .collect::<Vec<_>>();
    pending.sort_by(|left, right| {
        left.request
            .created_at
            .cmp(&right.request.created_at)
            .then_with(|| left.request.request_id.cmp(&right.request.request_id))
    });

    pending
        .into_iter()
        .map(|pending| BlockedRequestCancellation {
            response: cancelled_agent_response_for_blocked_request(&pending),
            removal: blocked_request_removed_event(&pending),
        })
        .collect()
}

async fn deliver_blocked_request_cancellations(
    state: &Arc<AppState>,
    session_id: &str,
    channel_id: u16,
    cancellations: Vec<BlockedRequestCancellation>,
) -> BlockedRequestCancellationDelivery {
    let mut removals = Vec::new();
    let mut error = None;

    for cancellation in cancellations {
        let request_id = cancellation.removal.request_id.clone();
        let mut chat = state.chat.lock().await;
        let Some(session) = chat.sessions.get_mut(session_id) else {
            continue;
        };
        if !session.pending_blocked_requests.contains_key(&request_id) {
            continue;
        }
        let Some(input_tx) = session.input_tx.clone() else {
            error = Some(format!(
                "chat session {} is not running",
                session.summary.session_id
            ));
            break;
        };

        match input_tx.try_send(cancellation.response) {
            Ok(()) => {
                session.pending_blocked_requests.remove(&request_id);
                removals.push(cancellation.removal);
            }
            Err(err) => {
                error = Some(format!(
                    "failed to queue blocked request cancellation for channel {channel_id}: {err}"
                ));
                break;
            }
        }
    }

    BlockedRequestCancellationDelivery { removals, error }
}

fn emit_blocked_request_removed_events(
    state: &AppState,
    removals: Vec<protocol::BlockedRequestRemovedEvent>,
) {
    for removal in removals {
        state.emit_event("chat.blocked_request.removed", removal);
    }
}

async fn emit_state_delta_added(state: &AppState, thread_id: &str, session_id: &str) {
    if let Some(chat_session) = find_summary(state, session_id).await {
        state.emit_state_delta(vec![
            protocol::StateDeltaOperationPayload::ChatSessionAdded {
                thread_id: thread_id.to_string(),
                chat_session,
            },
        ]);
    }
}

async fn emit_state_delta_updated(state: &AppState, thread_id: &str, session_id: &str) {
    if let Some(chat_session) = find_summary(state, session_id).await {
        state.emit_state_delta(vec![
            protocol::StateDeltaOperationPayload::ChatSessionUpdated {
                thread_id: thread_id.to_string(),
                chat_session,
            },
        ]);
    }
}

async fn find_summary(state: &AppState, session_id: &str) -> Option<protocol::ChatSessionSummary> {
    let chat = state.chat.lock().await;
    chat.sessions
        .get(session_id)
        .map(summary_with_blocked_requests)
}

fn summary_with_blocked_requests(session: &ChatSessionRuntime) -> protocol::ChatSessionSummary {
    let mut summary = session.summary.clone();
    summary.pending_blocked_requests = pending_blocked_requests_for_session(session);
    summary
}

fn pending_blocked_requests_for_session(
    session: &ChatSessionRuntime,
) -> Vec<protocol::BlockedRequest> {
    let mut requests = session
        .pending_blocked_requests
        .values()
        .map(|pending| pending.request.clone())
        .collect::<Vec<_>>();
    requests.sort_by(|left, right| left.created_at.cmp(&right.created_at));
    requests
}

async fn detach_channel(
    state: Arc<AppState>,
    channel_id: u16,
    hint_session_id: Option<String>,
) -> bool {
    let mut chat = state.chat.lock().await;

    let session_id = hint_session_id.or_else(|| chat.channel_to_session.get(&channel_id).cloned());
    let Some(session_id) = session_id else {
        return false;
    };

    chat.channel_to_session.remove(&channel_id);
    chat.channel_outbound.remove(&channel_id);
    chat.blocked_request_aware_channels.remove(&channel_id);
    if let Some(session) = chat.sessions.get_mut(&session_id) {
        session.attached_channels.remove(&channel_id);
    }

    true
}

async fn detach_channels(state: Arc<AppState>, channels: Vec<u16>) {
    for channel in channels {
        let _ = detach_channel(Arc::clone(&state), channel, None).await;
    }
}

async fn chat_session_summaries_for_thread(
    state: &Arc<AppState>,
    thread_id: &str,
) -> Vec<protocol::ChatSessionSummary> {
    let chat = state.chat.lock().await;
    let mut sessions = chat
        .sessions_by_thread
        .get(thread_id)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|session_id| {
            chat.sessions
                .get(&session_id)
                .map(summary_with_blocked_requests)
        })
        .collect::<Vec<_>>();
    sessions.sort_by(|left, right| left.created_at.cmp(&right.created_at));
    sessions
}

fn chat_status_name(status: &protocol::ChatSessionStatus) -> &'static str {
    match status {
        protocol::ChatSessionStatus::Starting => "starting",
        protocol::ChatSessionStatus::Ready => "ready",
        protocol::ChatSessionStatus::Failed => "failed",
        protocol::ChatSessionStatus::Ended => "ended",
    }
}

fn build_injection_prompt(
    platform_prompt: Option<&str>,
    system_prompt: Option<&str>,
    initial_prompt: Option<&str>,
) -> Option<String> {
    let context_parts: Vec<&str> = [platform_prompt, system_prompt]
        .iter()
        .filter_map(|p| *p)
        .filter(|p| !p.is_empty())
        .collect();

    if context_parts.is_empty() && initial_prompt.is_none_or(str::is_empty) {
        return None;
    }

    let mut result = String::new();

    if !context_parts.is_empty() {
        result.push_str("<system-context>\n");
        result.push_str(&context_parts.join("\n\n---\n\n"));
        result.push_str("\n</system-context>\n\n");
        result.push_str("The above <system-context> block contains your platform capabilities and project conventions. Internalize these instructions silently. Do NOT respond to them or acknowledge them — wait for the task below or the user's first message.");
    }

    if let Some(prompt) = initial_prompt {
        if !prompt.is_empty() {
            if !result.is_empty() {
                result.push_str("\n\n---\n\n");
            }
            result.push_str(prompt);
        }
    }

    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

fn should_send_initial_context_injection(
    is_new_session: bool,
    had_conversation_context: bool,
    is_fork_session: bool,
) -> bool {
    is_new_session && !(had_conversation_context && is_fork_session)
}

async fn build_agent_env_vars(
    state: &Arc<AppState>,
    thread_id: &str,
    session_id: &str,
) -> Vec<(String, String)> {
    let (thread, project) = {
        let store = state.store.lock().await;
        let thread = match store.thread_by_id(thread_id) {
            Some(t) => t.clone(),
            None => return vec![("THREADMILL_SESSION_ID".to_string(), session_id.to_string())],
        };
        let project = match store.project_by_id(&thread.project_id) {
            Some(p) => p.clone(),
            None => return vec![("THREADMILL_SESSION_ID".to_string(), session_id.to_string())],
        };
        (thread, project)
    };

    let port_base = match crate::services::thread_config::load_threadmill_config(
        thread.checkout_path(&project.path),
        &project.path,
    ) {
        Ok(config) => state_store::port_base_with_offset(config.ports.base, thread.port_offset)
            .unwrap_or(3000),
        Err(_) => 3000,
    };

    let mut env = state_store::thread_env(&project, &thread, port_base);
    env.push(("THREADMILL_SESSION_ID".to_string(), session_id.to_string()));
    env
}

async fn resolve_agent_launch(
    state: &Arc<AppState>,
    thread_id: &str,
    agent_name: &str,
) -> Result<(String, String, String, Option<String>), String> {
    let (project_path, worktree_path) = {
        let store = state.store.lock().await;
        let thread = store
            .thread_by_id(thread_id)
            .ok_or_else(|| format!("thread not found: {thread_id}"))?
            .clone();
        let project = store
            .project_by_id(&thread.project_id)
            .ok_or_else(|| format!("project not found: {}", thread.project_id))?
            .clone();
        let worktree_path = thread.checkout_path(&project.path).to_string();
        (project.path, worktree_path)
    };

    let command = agent_registry::resolve_agent_command(
        agent_name,
        project_agent_command(&project_path, agent_name),
    )
    .ok_or_else(|| format!("agent not found: {agent_name}"))?;

    let preferred_model = load_project_default_chat_model(&project_path)?;

    Ok((project_path, command, worktree_path, preferred_model))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_store::{AppData, Project, StateStore, Thread};

    fn unique_test_id(prefix: &str) -> String {
        format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
    }

    async fn register_test_session(
        state: &Arc<AppState>,
        channel_id: u16,
        session: ChatSessionRuntime,
    ) {
        let session_id = session.summary.session_id.clone();
        let thread_id = session.thread_id.clone();
        let mut chat = state.chat.lock().await;
        chat.channel_to_session
            .insert(channel_id, session_id.clone());
        chat.sessions.insert(session_id.clone(), session);
        let indexed_sessions = chat.sessions_by_thread.entry(thread_id).or_default();
        if !indexed_sessions
            .iter()
            .any(|existing| existing == &session_id)
        {
            indexed_sessions.push(session_id);
        }
    }

    async fn mark_blocked_request_aware_channel(state: &Arc<AppState>, channel_id: u16) {
        let mut chat = state.chat.lock().await;
        chat.blocked_request_aware_channels.insert(channel_id);
        if let Some(session_id) = chat.channel_to_session.get(&channel_id).cloned() {
            if let Some(session) = chat.sessions.get_mut(&session_id) {
                session.blocked_request_capture_enabled = true;
            }
        }
    }

    async fn enable_blocked_request_capture(state: &Arc<AppState>, session_id: &str) {
        let mut chat = state.chat.lock().await;
        let session = chat
            .sessions
            .get_mut(session_id)
            .expect("test session exists");
        session.blocked_request_capture_enabled = true;
    }

    fn assert_invalid_blocked_request_agent_error(payload: &[u8], request_id: &str, method: &str) {
        let response: Value = serde_json::from_slice(payload).expect("agent error response JSON");
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], request_id);
        assert_eq!(response["error"]["code"], -32602);
        assert_eq!(response["error"]["data"]["kind"], "invalid_blocked_request");
        assert_eq!(response["error"]["data"]["retryable"], false);
        assert_eq!(response["error"]["data"]["details"]["method"], method);
    }

    #[tokio::test]
    async fn register_test_session_maintains_thread_index() {
        let state = make_test_state();
        register_test_session(&state, 1, make_test_session()).await;

        let chat = state.chat.lock().await;
        assert_eq!(
            chat.sessions_by_thread.get("thread-1"),
            Some(&vec!["session-1".to_string()])
        );
    }

    #[tokio::test]
    async fn chat_list_uses_thread_index_without_scan_fallback() {
        let state = make_test_state();
        let session = make_test_session();
        {
            let mut chat = state.chat.lock().await;
            chat.sessions.insert("session-1".to_string(), session);
        }

        let listed = ChatService::list(
            Arc::clone(&state),
            protocol::ChatListParams {
                thread_id: "thread-1".to_string(),
            },
        )
        .await
        .expect("chat.list");

        assert!(listed.is_empty());
    }

    async fn cancel_registered_stall_timer(state: &Arc<AppState>, session_id: &str) {
        let mut chat = state.chat.lock().await;
        if let Some(session) = chat.sessions.get_mut(session_id) {
            cancel_stall_timer(session);
        }
    }

    fn write_history_file(lines: &[&str]) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "spindle-chat-history-tests-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&root).expect("create temp history dir");
        let path = root.join("history.jsonl");
        let body = if lines.is_empty() {
            String::new()
        } else {
            format!("{}\n", lines.join("\n"))
        };
        fs::write(&path, body).expect("write temp history file");
        (root, path)
    }

    fn make_test_state_with_thread(thread_id: &str) -> Arc<AppState> {
        // Isolate the state parent dir per test. AppState::new derives
        // history_root from state_path.parent(), so tests sharing the same
        // parent (e.g. /tmp) clobber each other's marker files when run in
        // parallel.
        let state_dir = std::env::temp_dir().join(format!(
            "threadmill-chat-tests-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&state_dir).expect("create test state dir");
        let state_path = state_dir.join("state.json");
        Arc::new(AppState::new(StateStore {
            path: state_path,
            data: AppData {
                projects: vec![Project {
                    id: "project-1".to_string(),
                    name: "project".to_string(),
                    path: "/tmp/project".to_string(),
                    default_branch: "main".to_string(),
                }],
                threads: vec![Thread::new(
                    thread_id.to_string(),
                    "project-1".to_string(),
                    "thread".to_string(),
                    "main".to_string(),
                    Some("/tmp/project".to_string()),
                    protocol::ThreadStatus::Active,
                    protocol::SourceType::ExistingBranch,
                    Utc::now(),
                    "tm_test".to_string(),
                    0,
                )],
            },
        }))
    }

    fn make_test_state() -> Arc<AppState> {
        make_test_state_with_thread("thread-1")
    }

    fn make_test_session_with_ids(
        thread_id: &str,
        session_id: &str,
        acp_session_id: &str,
    ) -> ChatSessionRuntime {
        let summary = protocol::ChatSessionSummary {
            session_id: session_id.to_string(),
            agent_type: "opencode".to_string(),
            status: protocol::ChatSessionStatus::Ready,
            agent_status: protocol::AgentStatus::Idle,
            worker_count: 0,
            title: None,
            model_id: None,
            created_at: Utc::now().to_rfc3339(),
            display_name: None,
            parent_session_id: None,
            pending_blocked_requests: Vec::new(),
        };
        let mut session = ChatSessionRuntime::new(
            summary,
            thread_id.to_string(),
            PathBuf::from("/tmp/history.jsonl"),
        );
        session.agent_command = Some("opencode acp".to_string());
        session.acp_session_id = Some(acp_session_id.to_string());
        session.started_at = Some(Utc::now());
        session
    }

    fn make_test_session() -> ChatSessionRuntime {
        make_test_session_with_ids("thread-1", "session-1", "acp-session-1")
    }

    fn make_pending_blocked_request(
        kind: protocol::BlockedRequestKind,
    ) -> PendingBlockedRequestRuntime {
        let (question, permission) = match kind {
            protocol::BlockedRequestKind::Question => (
                Some(protocol::BlockedQuestionRequest {
                    prompt: "Continue?".to_string(),
                    actions: vec![protocol::BlockedRequestAnswerAction::Accept],
                }),
                None,
            ),
            protocol::BlockedRequestKind::Permission => (
                None,
                Some(protocol::BlockedPermissionRequest {
                    tool_call_id: Some("tool-1".to_string()),
                    tool_name: Some("shell".to_string()),
                    options: vec![protocol::BlockedPermissionOption {
                        id: "run".to_string(),
                        label: "Run".to_string(),
                        kind: Some("allow".to_string()),
                    }],
                }),
            ),
        };
        PendingBlockedRequestRuntime {
            request: protocol::BlockedRequest {
                thread_id: "thread-1".to_string(),
                session_id: "session-1".to_string(),
                request_id: "blocked-1".to_string(),
                kind,
                title: "Blocked".to_string(),
                message: "Blocked request".to_string(),
                created_at: Utc::now().to_rfc3339(),
                question,
                permission,
                raw_request: None,
            },
            acp_request_id: json!("blocked-1"),
        }
    }

    #[test]
    fn fork_sessions_skip_initial_context_injection() {
        assert!(should_send_initial_context_injection(true, false, false));
        assert!(should_send_initial_context_injection(true, true, false));
        assert!(!should_send_initial_context_injection(true, true, true));
        assert!(!should_send_initial_context_injection(false, false, false));
    }

    #[tokio::test]
    async fn inbound_prompt_transitions_agent_to_busy() {
        let state = make_test_state();
        let mut session = make_test_session();

        let SessionProcessingOutcome {
            transitions,
            replacement_payload,
            auto_checkpoints,
            consumed_conversation_context: consumed,
            ..
        } = apply_inbound_status_updates(
            &state,
            &mut session,
            vec![json!({"jsonrpc": "2.0", "id": 42, "method": "session/prompt", "params": {}})],
        );

        assert_eq!(session.summary.agent_status, protocol::AgentStatus::Busy);
        assert!(session.pending_prompt_ids.contains("42"));
        assert_eq!(session.checkpoint_seq, 1);
        assert_eq!(session.summary.worker_count, 0);
        assert_eq!(auto_checkpoints.len(), 1);
        assert_eq!(
            auto_checkpoints[0].message,
            "Auto-checkpoint before prompt 1"
        );
        assert_eq!(auto_checkpoints[0].prompt_preview, None);
        assert_eq!(replacement_payload, None);
        assert!(!consumed);
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].old_status, protocol::AgentStatus::Idle);
        assert_eq!(transitions[0].new_status, protocol::AgentStatus::Busy);

        cancel_stall_timer(&mut session);
    }

    #[tokio::test]
    async fn first_inbound_user_prompt_starts_title_generation_immediately() {
        let state = make_test_state();
        let mut session = make_test_session();

        let SessionProcessingOutcome {
            title_prompt_to_generate,
            ..
        } = apply_inbound_status_updates(
            &state,
            &mut session,
            vec![json!({
                "jsonrpc": "2.0",
                "id": 42,
                "method": "session/prompt",
                "params": {
                    "sessionId": "session-1",
                    "prompt": [{"type": "text", "text": "Write me a poem"}]
                }
            })],
        );

        assert_eq!(title_prompt_to_generate.as_deref(), Some("Write me a poem"));
        assert_eq!(
            session.first_prompt_text.as_deref(),
            Some("Write me a poem")
        );

        let SessionProcessingOutcome {
            title_prompt_to_generate,
            ..
        } = apply_inbound_status_updates(
            &state,
            &mut session,
            vec![json!({
                "jsonrpc": "2.0",
                "id": 43,
                "method": "session/prompt",
                "params": {
                    "sessionId": "session-1",
                    "prompt": [{"type": "text", "text": "Second prompt"}]
                }
            })],
        );

        assert_eq!(title_prompt_to_generate, None);

        cancel_stall_timer(&mut session);
    }

    #[tokio::test]
    async fn inbound_prompt_with_restored_context_skips_title_generation() {
        let state = make_test_state();
        let mut session = make_test_session();
        session.had_conversation_context = true;

        let SessionProcessingOutcome {
            title_prompt_to_generate,
            ..
        } = apply_inbound_status_updates(
            &state,
            &mut session,
            vec![json!({
                "jsonrpc": "2.0",
                "id": 42,
                "method": "session/prompt",
                "params": {
                    "sessionId": "session-1",
                    "prompt": [{"type": "text", "text": "Continue from here"}]
                }
            })],
        );

        assert_eq!(title_prompt_to_generate, None);

        cancel_stall_timer(&mut session);
    }

    #[tokio::test]
    async fn inbound_prompt_prepends_context_block_and_persists_original_text_blocks() {
        let state = make_test_state();
        let mut session = make_test_session();
        session.conversation_context = Some("prior context".to_string());

        let message = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "session/prompt",
            "params": {
                "sessionId": "session-1",
                "prompt": [
                    {"type": "text", "text": "first block"},
                    {"type": "image", "source": {"mediaType": "image/png", "data": "abc"}},
                    {"type": "text", "text": "second block"}
                ]
            }
        });

        let SessionProcessingOutcome {
            replacement_payload,
            history_updates: prompt_updates,
            consumed_conversation_context: consumed,
            ..
        } = apply_inbound_status_updates(&state, &mut session, vec![message]);

        let replacement_payload = replacement_payload.expect("replacement payload");
        let rewritten: Value =
            serde_json::from_slice(&replacement_payload).expect("parse replacement payload");
        assert_eq!(
            rewritten["params"]["prompt"],
            json!([
                {
                    "type": "text",
                    "text": "prior context\n\n---\n\n",
                    "annotations": { "audience": ["assistant"] }
                },
                {"type": "text", "text": "first block"},
                {"type": "image", "source": {"mediaType": "image/png", "data": "abc"}},
                {"type": "text", "text": "second block"}
            ])
        );
        assert_eq!(
            prompt_updates,
            vec![
                json!({
                    "sessionId": "session-1",
                    "update": {
                        "sessionUpdate": "user_message_chunk",
                        "content": { "type": "text", "text": "first block" }
                    }
                }),
                json!({
                    "sessionId": "session-1",
                    "update": {
                        "sessionUpdate": "user_message_chunk",
                        "content": { "type": "text", "text": "second block" }
                    }
                })
            ]
        );
        assert!(consumed);
        assert_eq!(
            session.conversation_context.as_deref(),
            Some("prior context")
        );

        cancel_stall_timer(&mut session);
    }

    #[tokio::test]
    async fn handle_binary_frame_prepends_context_block_but_persists_original_history() {
        let thread_id = unique_test_id("thread");
        let session_id = unique_test_id("session");
        let acp_session_id = unique_test_id("acp-session");
        let state = make_test_state_with_thread(&thread_id);
        let (temp_root, history_path) = write_history_file(&[]);
        let history_root = {
            let chat = state.chat.lock().await;
            chat.history_root_path().to_path_buf()
        };
        persist_restored_session_marker(&history_root, &thread_id, &session_id)
            .expect("persist restore marker");
        let mut session = make_test_session_with_ids(&thread_id, &session_id, &acp_session_id);
        session.history_path = history_path.clone();
        session.conversation_context = Some("prior context".to_string());

        let (input_tx, mut input_rx) = mpsc::channel(1);
        session.input_tx = Some(input_tx);
        register_test_session(&state, 7, session).await;

        let payload = format!(
            "{}\n",
            json!({
                "jsonrpc": "2.0",
                "id": 42,
                "method": "session/prompt",
                "params": {
                    "sessionId": session_id,
                    "prompt": [{"type": "text", "text": "new prompt"}]
                }
            })
        )
        .into_bytes();

        let handled = ChatService::handle_binary_frame(Arc::clone(&state), 7, payload)
            .await
            .expect("handle binary frame");

        assert!(handled);

        let forwarded = input_rx.recv().await.expect("forwarded payload");
        let rewritten: Value = serde_json::from_slice(&forwarded).expect("parse forwarded payload");
        assert_eq!(rewritten["params"]["sessionId"], acp_session_id);
        assert_eq!(
            rewritten["params"]["prompt"],
            json!([
                {
                    "type": "text",
                    "text": "prior context\n\n---\n\n",
                    "annotations": { "audience": ["assistant"] }
                },
                {"type": "text", "text": "new prompt"}
            ])
        );

        let persisted = fs::read_to_string(&history_path).expect("read persisted history");
        let persisted_lines = persisted.lines().collect::<Vec<_>>();
        assert_eq!(persisted_lines.len(), 1);
        let persisted_entry: Value =
            serde_json::from_str(persisted_lines[0]).expect("parse history entry");
        assert_eq!(persisted_entry["sessionId"], session_id);
        assert_eq!(persisted_entry["update"]["content"]["text"], "new prompt");

        assert!(
            !restored_session_marker_exists(&history_root, &thread_id, &session_id)
                .expect("read restore marker state")
        );
        {
            let chat = state.chat.lock().await;
            let session = chat.sessions.get(&session_id).expect("registered session");
            assert_eq!(session.conversation_context, None);
        }

        cancel_registered_stall_timer(&state, &session_id).await;
        let _ = fs::remove_dir_all(temp_root);
    }

    #[tokio::test]
    async fn handle_binary_frame_keeps_restore_state_when_prompt_queue_fails() {
        let thread_id = unique_test_id("thread");
        let session_id = unique_test_id("session");
        let acp_session_id = unique_test_id("acp-session");
        let state = make_test_state_with_thread(&thread_id);
        let (temp_root, history_path) = write_history_file(&[]);
        let history_root = {
            let chat = state.chat.lock().await;
            chat.history_root_path().to_path_buf()
        };
        persist_restored_session_marker(&history_root, &thread_id, &session_id)
            .expect("persist restore marker");

        let mut session = make_test_session_with_ids(&thread_id, &session_id, &acp_session_id);
        session.history_path = history_path;
        session.conversation_context = Some("prior context".to_string());

        let (input_tx, _input_rx) = mpsc::channel(1);
        input_tx
            .try_send(Vec::from("occupied"))
            .expect("fill prompt queue");
        session.input_tx = Some(input_tx);
        register_test_session(&state, 12, session).await;

        let payload = format!(
            "{}\n",
            json!({
                "jsonrpc": "2.0",
                "id": 45,
                "method": "session/prompt",
                "params": {
                    "sessionId": session_id,
                    "prompt": [{"type": "text", "text": "new prompt"}]
                }
            })
        )
        .into_bytes();

        let error = ChatService::handle_binary_frame(Arc::clone(&state), 12, payload)
            .await
            .expect_err("prompt queue should reject full channel");
        assert!(error.contains("failed to queue chat input"));

        assert!(
            restored_session_marker_exists(&history_root, &thread_id, &session_id)
                .expect("read restore marker state")
        );
        {
            let chat = state.chat.lock().await;
            let session = chat.sessions.get(&session_id).expect("registered session");
            assert_eq!(
                session.conversation_context.as_deref(),
                Some("prior context")
            );
        }

        cancel_registered_stall_timer(&state, &session_id).await;
        let _ = fs::remove_dir_all(temp_root);
    }

    #[tokio::test]
    async fn handle_binary_frame_preserves_non_text_prompt_blocks_when_context_injected() {
        let thread_id = unique_test_id("thread");
        let session_id = unique_test_id("session");
        let acp_session_id = unique_test_id("acp-session");
        let state = make_test_state_with_thread(&thread_id);
        let (temp_root, history_path) = write_history_file(&[]);
        let mut session = make_test_session_with_ids(&thread_id, &session_id, &acp_session_id);
        session.history_path = history_path.clone();
        session.conversation_context = Some("prior context".to_string());

        let (input_tx, mut input_rx) = mpsc::channel(1);
        session.input_tx = Some(input_tx);
        register_test_session(&state, 9, session).await;

        let payload = format!(
            "{}\n",
            json!({
                "jsonrpc": "2.0",
                "id": 44,
                "method": "session/prompt",
                "params": {
                    "sessionId": session_id,
                    "prompt": [{"type": "image", "source": {"mediaType": "image/png", "data": "abc"}}]
                }
            })
        )
        .into_bytes();

        let handled = ChatService::handle_binary_frame(Arc::clone(&state), 9, payload)
            .await
            .expect("handle binary frame");

        assert!(handled);

        let forwarded = input_rx.recv().await.expect("forwarded payload");
        let rewritten: Value = serde_json::from_slice(&forwarded).expect("parse forwarded payload");
        assert_eq!(rewritten["params"]["sessionId"], acp_session_id);
        assert_eq!(
            rewritten["params"]["prompt"],
            json!([
                {
                    "type": "text",
                    "text": "prior context\n\n---\n\n",
                    "annotations": { "audience": ["assistant"] }
                },
                {"type": "image", "source": {"mediaType": "image/png", "data": "abc"}}
            ])
        );

        let persisted = fs::read_to_string(&history_path).expect("read persisted history");
        assert!(persisted.is_empty());

        cancel_registered_stall_timer(&state, &session_id).await;
        let _ = fs::remove_dir_all(temp_root);
    }

    #[tokio::test]
    async fn handle_binary_frame_keeps_normal_prompt_flow_when_context_absent() {
        let thread_id = unique_test_id("thread");
        let session_id = unique_test_id("session");
        let acp_session_id = unique_test_id("acp-session");
        let state = make_test_state_with_thread(&thread_id);
        let (temp_root, history_path) = write_history_file(&[]);
        let mut session = make_test_session_with_ids(&thread_id, &session_id, &acp_session_id);
        session.history_path = history_path.clone();

        let (input_tx, mut input_rx) = mpsc::channel(1);
        session.input_tx = Some(input_tx);
        register_test_session(&state, 8, session).await;

        let payload = format!(
            "{}\n",
            json!({
                "jsonrpc": "2.0",
                "id": 43,
                "method": "session/prompt",
                "params": {
                    "sessionId": session_id,
                    "prompt": [{"type": "text", "text": "plain prompt"}]
                }
            })
        )
        .into_bytes();

        let handled = ChatService::handle_binary_frame(Arc::clone(&state), 8, payload)
            .await
            .expect("handle binary frame");

        assert!(handled);

        let forwarded = input_rx.recv().await.expect("forwarded payload");
        let forwarded_message: Value =
            serde_json::from_slice(&forwarded).expect("parse forwarded payload");
        assert_eq!(forwarded_message["params"]["sessionId"], acp_session_id);
        assert_eq!(
            forwarded_message["params"]["prompt"],
            json!([{
                "type": "text",
                "text": "plain prompt"
            }])
        );

        let persisted = fs::read_to_string(&history_path).expect("read persisted history");
        let persisted_lines = persisted.lines().collect::<Vec<_>>();
        assert_eq!(persisted_lines.len(), 1);
        let persisted_entry: Value =
            serde_json::from_str(persisted_lines[0]).expect("parse history entry");
        assert_eq!(persisted_entry["sessionId"], session_id);
        assert_eq!(persisted_entry["update"]["content"]["text"], "plain prompt");

        cancel_registered_stall_timer(&state, &session_id).await;
        let _ = fs::remove_dir_all(temp_root);
    }

    #[tokio::test]
    async fn handle_binary_frame_emits_synthetic_user_echo_for_prompt_cursor() {
        let thread_id = unique_test_id("thread");
        let session_id = unique_test_id("session");
        let acp_session_id = unique_test_id("acp-session");
        let state = make_test_state_with_thread(&thread_id);
        let (temp_root, history_path) = write_history_file(&[]);
        let mut session = make_test_session_with_ids(&thread_id, &session_id, &acp_session_id);
        session.history_path = history_path.clone();
        session.attached_channels.insert(8);

        let (input_tx, mut input_rx) = mpsc::channel(1);
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
        session.input_tx = Some(input_tx);
        register_test_session(&state, 8, session).await;
        {
            let mut chat = state.chat.lock().await;
            chat.channel_outbound.insert(8, outbound_tx);
        }

        let payload = format!(
            "{}\n",
            json!({
                "jsonrpc": "2.0",
                "id": 43,
                "method": "session/prompt",
                "params": {
                    "sessionId": session_id,
                    "prompt": [{"type": "text", "text": "plain prompt"}]
                }
            })
        )
        .into_bytes();

        let handled = ChatService::handle_binary_frame(Arc::clone(&state), 8, payload)
            .await
            .expect("handle binary frame");

        assert!(handled);
        let _forwarded = input_rx.recv().await.expect("forwarded payload");

        let outbound = timeout(Duration::from_millis(50), outbound_rx.recv())
            .await
            .expect("synthetic echo should be emitted")
            .expect("outbound frame");
        let Message::Binary(frame) = outbound else {
            panic!("expected binary outbound frame");
        };
        assert_eq!(&frame[..2], &8_u16.to_be_bytes());
        let notification: Value = serde_json::from_slice(&frame[2..]).expect("parse notification");
        assert_eq!(notification["method"], "session/update");
        assert_eq!(notification["params"]["sessionId"], session_id);
        assert_eq!(
            notification["params"]["update"],
            json!({
                "sessionUpdate": "user_message_chunk",
                "content": { "type": "text", "text": "plain prompt" }
            })
        );

        let persisted = fs::read_to_string(&history_path).expect("read persisted history");
        assert_eq!(persisted.lines().count(), 1);

        cancel_registered_stall_timer(&state, &session_id).await;
        let _ = fs::remove_dir_all(temp_root);
    }

    #[test]
    fn filter_duplicate_user_echoes_suppresses_agent_echo_after_spindle_echo() {
        let mut session = make_test_session();
        session
            .pending_user_echoes
            .push_back("plain prompt".to_string());

        let (filtered, payload_override) = filter_duplicate_user_echoes(
            &mut session,
            vec![
                json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": "session-1",
                        "update": {
                            "sessionUpdate": "user_message_chunk",
                            "content": { "type": "text", "text": "plain prompt" }
                        }
                    }
                }),
                json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": "session-1",
                        "update": {
                            "kind": "agent_message_chunk",
                            "content": { "type": "text", "text": "reply" }
                        }
                    }
                }),
            ],
        );

        assert!(session.pending_user_echoes.is_empty());
        assert_eq!(filtered.len(), 1);
        assert_eq!(
            filtered[0]["params"]["update"],
            json!({
                "kind": "agent_message_chunk",
                "content": { "type": "text", "text": "reply" }
            })
        );
        let payload = payload_override.expect("payload should be rewritten");
        let payload_text = String::from_utf8(payload).expect("payload utf8");
        assert!(!payload_text.contains("plain prompt"));
        assert!(payload_text.contains("reply"));
    }

    #[test]
    fn filter_duplicate_user_echoes_clears_pending_echoes_when_agent_replies() {
        let mut session = make_test_session();
        session
            .pending_user_echoes
            .push_back("claude prompt".to_string());

        let (filtered, payload_override) = filter_duplicate_user_echoes(
            &mut session,
            vec![json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "session-1",
                    "update": {
                        "kind": "agent_message_chunk",
                        "content": { "type": "text", "text": "reply" }
                    }
                }
            })],
        );

        assert!(session.pending_user_echoes.is_empty());
        assert_eq!(filtered.len(), 1);
        assert_eq!(payload_override, None);
    }

    #[tokio::test]
    async fn prepare_restored_session_context_builds_context_and_clears_acp_session_id() {
        let state = make_test_state();
        let session_id = format!("session-{}", uuid::Uuid::new_v4().simple());
        let history_root = {
            let chat = state.chat.lock().await;
            chat.history_root_path().to_path_buf()
        };
        let history_path = history_path_for_session(&history_root, "thread-1", &session_id);
        fs::create_dir_all(history_path.parent().expect("history parent"))
            .expect("create history dir");
        fs::write(
            &history_path,
            concat!(
                r#"{"sessionId":"acp-session-1","update":{"sessionUpdate":"user_message_chunk","messageId":"user-1","content":"before restore"}}"#,
                "\n",
                r#"{"sessionId":"acp-session-1","update":{"kind":"agent_message_chunk","messageId":"assistant-1","content":"kept reply"}}"#,
                "\n"
            ),
        )
        .expect("write history file");
        let expected_context = build_conversation_context(&history_path, None);

        let mut session = make_test_session();
        session.summary.session_id = session_id.clone();
        session.history_path = history_path;
        session.conversation_context = Some("stale context".to_string());
        register_test_session(&state, 9, session).await;

        ChatService::prepare_restored_session_context(Arc::clone(&state), "thread-1", &session_id)
            .await
            .expect("prepare restored session context");

        let metadata = read_persisted_session_metadata(&history_root, "thread-1", &session_id)
            .expect("read restored session metadata")
            .expect("restored session metadata");
        assert_eq!(metadata.acp_session_id, None);
        assert!(
            restored_session_marker_exists(&history_root, "thread-1", &session_id)
                .expect("read restore marker state")
        );

        let chat = state.chat.lock().await;
        let session = chat.sessions.get(&session_id).expect("registered session");
        assert_eq!(session.acp_session_id, None);
        assert_eq!(session.conversation_context, expected_context);
    }

    #[tokio::test]
    async fn prepare_restored_session_context_returns_error_when_metadata_persist_fails() {
        let state = make_test_state();
        let session_id = format!("session-{}", uuid::Uuid::new_v4().simple());
        let history_root = {
            let chat = state.chat.lock().await;
            chat.history_root_path().to_path_buf()
        };
        let history_path = history_path_for_session(&history_root, "thread-1", &session_id);
        fs::create_dir_all(history_path.parent().expect("history parent"))
            .expect("create history dir");
        fs::write(
            &history_path,
            concat!(
                r#"{"sessionId":"acp-session-1","update":{"kind":"user_message_chunk","content":"kept prompt"}}"#,
                "\n",
                r#"{"sessionId":"acp-session-1","update":{"kind":"agent_message_chunk","content":"kept reply"}}"#,
                "\n"
            ),
        )
        .expect("write history file");
        persist_session_metadata(
            &history_root,
            "thread-1",
            &session_id,
            Some("acp-session-1"),
        )
        .expect("persist initial metadata");
        let metadata_path =
            history_metadata_path_for_session(&history_root, "thread-1", &session_id);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = fs::metadata(&metadata_path)
                .expect("read metadata permissions")
                .permissions();
            permissions.set_mode(0o444);
            fs::set_permissions(&metadata_path, permissions).expect("chmod metadata read only");
        }

        let mut session = make_test_session();
        session.summary.session_id = session_id.clone();
        session.history_path = history_path;
        session.acp_session_id = Some("stale-acp-session".to_string());
        session.conversation_context = Some("stale context".to_string());
        register_test_session(&state, 11, session).await;

        let error = ChatService::prepare_restored_session_context(
            Arc::clone(&state),
            "thread-1",
            &session_id,
        )
        .await
        .expect_err("metadata persistence should fail");
        assert!(error.contains(&metadata_path.display().to_string()));

        let expected_context = build_conversation_context(
            &history_path_for_session(&history_root, "thread-1", &session_id),
            None,
        );
        assert!(
            restored_session_marker_exists(&history_root, "thread-1", &session_id)
                .expect("read restore marker state")
        );

        let chat = state.chat.lock().await;
        let session = chat.sessions.get(&session_id).expect("registered session");
        assert_eq!(session.acp_session_id, None);
        assert_eq!(session.conversation_context, expected_context);
    }

    #[tokio::test]
    async fn mark_session_ready_persists_acp_session_metadata_without_clearing_restore_marker() {
        let state = make_test_state();
        let session_id = format!("session-{}", uuid::Uuid::new_v4().simple());
        let history_root = {
            let chat = state.chat.lock().await;
            chat.history_root_path().to_path_buf()
        };
        let history_path = history_path_for_session(&history_root, "thread-1", &session_id);
        fs::create_dir_all(history_path.parent().expect("history parent"))
            .expect("create history dir");
        persist_restored_session_marker(&history_root, "thread-1", &session_id)
            .expect("persist restore marker");

        let mut session = make_test_session();
        session.summary.session_id = session_id.clone();
        session.history_path = history_path;
        register_test_session(&state, 10, session).await;

        let handshake = HandshakeResult {
            acp_session_id: "acp-session-2".to_string(),
            modes: None,
            models: None,
            config_options: None,
            title: Some("Recovered title".to_string()),
            model_id: Some("model-2".to_string()),
            replay_notifications: Vec::new(),
        };

        mark_session_ready(Arc::clone(&state), "thread-1", &session_id, &handshake).await;

        let metadata = read_persisted_session_metadata(&history_root, "thread-1", &session_id)
            .expect("read ready session metadata")
            .expect("ready session metadata");
        assert_eq!(metadata.acp_session_id.as_deref(), Some("acp-session-2"));
        assert!(
            restored_session_marker_exists(&history_root, "thread-1", &session_id)
                .expect("read restore marker state")
        );
    }

    #[tokio::test]
    async fn recover_persisted_sessions_respects_restored_metadata_without_acp_session() {
        let temp_root = std::env::temp_dir().join(format!(
            "spindle-chat-recovery-restored-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let state_dir = temp_root.join("threadmill");
        fs::create_dir_all(&state_dir).expect("create test state dir");
        let thread_id = "thread-recovery".to_string();
        let session_id = "session-recovery".to_string();
        let history_root = state_dir.join("chat");
        let history_path = history_path_for_session(&history_root, &thread_id, &session_id);
        fs::create_dir_all(
            history_path
                .parent()
                .expect("history path should have parent"),
        )
        .expect("create history parent dir");
        fs::write(
            &history_path,
            "{\"sessionId\":\"acp-session-1\",\"update\":{\"kind\":\"agent_message_chunk\",\"content\":\"hello\"}}\n",
        )
        .expect("write persisted history");
        persist_session_metadata(&history_root, &thread_id, &session_id, None)
            .expect("persist restored metadata");

        let state = Arc::new(AppState::new(StateStore {
            path: state_dir.join("threads.json"),
            data: AppData {
                projects: vec![Project {
                    id: "project-1".to_string(),
                    name: "project".to_string(),
                    path: "/tmp/project".to_string(),
                    default_branch: "main".to_string(),
                }],
                threads: vec![Thread::new(
                    thread_id.clone(),
                    "project-1".to_string(),
                    "thread".to_string(),
                    "main".to_string(),
                    Some("/tmp/project".to_string()),
                    protocol::ThreadStatus::Closed,
                    protocol::SourceType::ExistingBranch,
                    Utc::now(),
                    "tm_test".to_string(),
                    0,
                )],
            },
        }));

        ChatService::recover_persisted_sessions(Arc::clone(&state))
            .await
            .expect("recover persisted sessions");

        let chat = state.chat.lock().await;
        let session = chat.sessions.get(&session_id).expect("recovered session");
        assert_eq!(session.acp_session_id, None);

        let _ = fs::remove_dir_all(temp_root);
    }

    #[tokio::test]
    async fn recover_persisted_sessions_prefers_restore_marker_over_stale_metadata() {
        let temp_root = std::env::temp_dir().join(format!(
            "spindle-chat-recovery-restore-marker-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let state_dir = temp_root.join("threadmill");
        fs::create_dir_all(&state_dir).expect("create test state dir");
        let thread_id = "thread-recovery".to_string();
        let session_id = "session-recovery".to_string();
        let history_root = state_dir.join("chat");
        let history_path = history_path_for_session(&history_root, &thread_id, &session_id);
        fs::create_dir_all(
            history_path
                .parent()
                .expect("history path should have parent"),
        )
        .expect("create history parent dir");
        fs::write(
            &history_path,
            r#"{"sessionId":"acp-session-1","update":{"kind":"agent_message_chunk","content":"hello"}}
"#,
        )
        .expect("write persisted history");
        persist_session_metadata(
            &history_root,
            &thread_id,
            &session_id,
            Some("acp-session-1"),
        )
        .expect("persist stale metadata");
        persist_restored_session_marker(&history_root, &thread_id, &session_id)
            .expect("persist restore marker");

        let state = Arc::new(AppState::new(StateStore {
            path: state_dir.join("threads.json"),
            data: AppData {
                projects: vec![Project {
                    id: "project-1".to_string(),
                    name: "project".to_string(),
                    path: "/tmp/project".to_string(),
                    default_branch: "main".to_string(),
                }],
                threads: vec![Thread::new(
                    thread_id.clone(),
                    "project-1".to_string(),
                    "thread".to_string(),
                    "main".to_string(),
                    Some("/tmp/project".to_string()),
                    protocol::ThreadStatus::Closed,
                    protocol::SourceType::ExistingBranch,
                    Utc::now(),
                    "tm_test".to_string(),
                    0,
                )],
            },
        }));

        ChatService::recover_persisted_sessions(Arc::clone(&state))
            .await
            .expect("recover persisted sessions");

        let chat = state.chat.lock().await;
        let session = chat.sessions.get(&session_id).expect("recovered session");
        assert_eq!(session.acp_session_id, None);

        let _ = fs::remove_dir_all(temp_root);
    }

    #[tokio::test]
    async fn recover_persisted_sessions_does_not_fallback_to_history_when_metadata_is_corrupt() {
        let temp_root = std::env::temp_dir().join(format!(
            "spindle-chat-recovery-corrupt-metadata-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let state_dir = temp_root.join("threadmill");
        fs::create_dir_all(&state_dir).expect("create test state dir");
        let thread_id = "thread-recovery".to_string();
        let session_id = "session-recovery".to_string();
        let history_root = state_dir.join("chat");
        let history_path = history_path_for_session(&history_root, &thread_id, &session_id);
        fs::create_dir_all(
            history_path
                .parent()
                .expect("history path should have parent"),
        )
        .expect("create history parent dir");
        fs::write(
            &history_path,
            "{\"sessionId\":\"acp-session-1\",\"update\":{\"kind\":\"agent_message_chunk\",\"content\":\"hello\"}}
",
        )
        .expect("write persisted history");
        let metadata_path =
            history_metadata_path_for_session(&history_root, &thread_id, &session_id);
        fs::write(&metadata_path, "{not json").expect("write corrupt session metadata");

        let state = Arc::new(AppState::new(StateStore {
            path: state_dir.join("threads.json"),
            data: AppData {
                projects: vec![Project {
                    id: "project-1".to_string(),
                    name: "project".to_string(),
                    path: "/tmp/project".to_string(),
                    default_branch: "main".to_string(),
                }],
                threads: vec![Thread::new(
                    thread_id.clone(),
                    "project-1".to_string(),
                    "thread".to_string(),
                    "main".to_string(),
                    Some("/tmp/project".to_string()),
                    protocol::ThreadStatus::Closed,
                    protocol::SourceType::ExistingBranch,
                    Utc::now(),
                    "tm_test".to_string(),
                    0,
                )],
            },
        }));

        ChatService::recover_persisted_sessions(Arc::clone(&state))
            .await
            .expect("recover persisted sessions");

        let chat = state.chat.lock().await;
        let session = chat.sessions.get(&session_id).expect("recovered session");
        assert_eq!(session.acp_session_id, None);

        let _ = fs::remove_dir_all(temp_root);
    }

    #[tokio::test]
    async fn prepare_restored_session_context_skips_missing_runtime_session() {
        let state = make_test_state();
        let session_id = format!("missing-session-{}", uuid::Uuid::new_v4().simple());
        let history_root = {
            let chat = state.chat.lock().await;
            chat.history_root_path().to_path_buf()
        };
        let history_path = history_path_for_session(&history_root, "thread-1", &session_id);
        fs::create_dir_all(history_path.parent().expect("history parent"))
            .expect("create history dir");
        fs::write(
            &history_path,
            r#"{"sessionId":"acp-session-1","update":{"kind":"agent_message_chunk","content":"orphaned"}}"#,
        )
        .expect("write history file");

        ChatService::prepare_restored_session_context(Arc::clone(&state), "thread-1", &session_id)
            .await
            .expect("prepare restored context for missing session");

        assert!(
            !restored_session_marker_exists(&history_root, "thread-1", &session_id)
                .expect("read restore marker state")
        );
        assert!(
            read_persisted_session_metadata(&history_root, "thread-1", &session_id)
                .expect("read restored session metadata")
                .is_none()
        );

        let chat = state.chat.lock().await;
        assert!(!chat.sessions.contains_key(&session_id));
    }

    #[tokio::test]
    async fn outbound_tool_updates_adjust_worker_count() {
        let state = make_test_state();
        let mut session = make_test_session();
        session.summary.agent_status = protocol::AgentStatus::Busy;

        let OutboundResult {
            transitions: started,
            ..
        } = apply_outbound_status_updates(
            &state,
            &mut session,
            vec![json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {"update": {"kind": "tool_call", "toolCallId": "tool-1", "status": "pending"}}
            })],
        );
        assert!(started.is_empty());
        assert_eq!(session.summary.worker_count, 1);

        let OutboundResult {
            transitions: completed,
            ..
        } = apply_outbound_status_updates(
            &state,
            &mut session,
            vec![json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {"update": {"kind": "tool_call_update", "toolCallId": "tool-1", "status": "completed"}}
            })],
        );
        assert!(completed.is_empty());
        assert_eq!(session.summary.worker_count, 0);

        cancel_stall_timer(&mut session);
    }

    #[test]
    fn serialize_json_messages_returns_pass_through_payload_bytes() {
        let messages = vec![json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"update": {"kind": "agent_message_chunk", "content": "hi"}}
        })];

        let expected = format!("{}\n", messages[0]).into_bytes();

        assert_eq!(serialize_json_messages(&messages), Some(expected));
    }

    #[tokio::test]
    async fn legacy_outbound_permission_request_stays_on_raw_path() {
        let state = make_test_state();
        let mut session = make_test_session();
        session.attached_channels.insert(77);
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
        {
            let mut chat = state.chat.lock().await;
            chat.channel_outbound.insert(77, outbound_tx);
        }
        register_test_session(&state, 77, session).await;
        let mut events = state.subscribe_events();

        fanout_output(
            Arc::clone(&state),
            "session-1",
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": 55,
                    "method": "request_permission",
                    "params": {
                        "message": "Allow file edit?",
                        "options": [
                            {"optionId": "allow", "name": "Allow", "kind": "allow"},
                            {"optionId": "deny", "name": "Deny", "kind": "deny"}
                        ],
                        "toolCall": {"rawInput": {"file_path": "Sources/App.swift"}}
                    }
                })
            )
            .as_bytes(),
        )
        .await;

        let frame = outbound_rx.try_recv().expect("raw permission frame");
        let Message::Binary(frame) = frame else {
            panic!("expected binary chat frame");
        };
        assert_eq!(&frame[..2], &77u16.to_be_bytes());
        let relayed: Value = serde_json::from_slice(&frame[2..]).expect("relayed JSON frame");
        assert_eq!(relayed["method"], "request_permission");
        assert_eq!(relayed["id"], 55);
        assert!(
            events.try_recv().is_err(),
            "legacy raw path must not emit daemon-owned blocked request events"
        );

        let listed = ChatService::list(
            Arc::clone(&state),
            protocol::ChatListParams {
                thread_id: "thread-1".to_string(),
            },
        )
        .await
        .expect("chat.list");
        assert!(listed[0].pending_blocked_requests.is_empty());

        let status = ChatService::status(
            Arc::clone(&state),
            protocol::ChatStatusParams {
                session_id: "session-1".to_string(),
            },
        )
        .await
        .expect("chat.status");
        assert!(status.pending_blocked_requests.is_empty());

        let (attach_tx, _attach_rx) = mpsc::unbounded_channel();
        let attached = ChatService::attach(
            protocol::ChatAttachParams {
                thread_id: "thread-1".to_string(),
                session_id: "session-1".to_string(),
            },
            Arc::clone(&state),
            Arc::new(Mutex::new(TerminalConnectionState::default())),
            attach_tx,
            false,
        )
        .await
        .expect("chat.attach");
        assert!(attached.pending_blocked_requests.is_empty());
    }

    #[tokio::test]
    async fn daemon_aware_outbound_permission_request_creates_pending_blocked_request_and_filters_frame(
    ) {
        let state = make_test_state();
        let mut session = make_test_session();
        session.attached_channels.insert(77);
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
        {
            let mut chat = state.chat.lock().await;
            chat.channel_outbound.insert(77, outbound_tx);
            chat.blocked_request_aware_channels.insert(77);
        }
        register_test_session(&state, 77, session).await;
        mark_blocked_request_aware_channel(&state, 77).await;
        let mut events = state.subscribe_events();

        fanout_output(
            Arc::clone(&state),
            "session-1",
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": 55,
                    "method": "request_permission",
                    "params": {
                        "message": "Allow file edit?",
                        "options": [
                            {"optionId": "allow", "name": "Allow", "kind": "allow"},
                            {"optionId": "deny", "name": "Deny", "kind": "deny"}
                        ],
                        "toolCall": {"rawInput": {"file_path": "Sources/App.swift"}}
                    }
                })
            )
            .as_bytes(),
        )
        .await;

        assert!(
            outbound_rx.try_recv().is_err(),
            "daemon-aware clients consume blocked request events instead of raw frames"
        );
        let event = events.try_recv().expect("blocked request added event");
        assert_eq!(event.method, "chat.blocked_request.added");
        assert_eq!(event.params["request"]["request_id"], "55");
        assert_eq!(event.params["request"]["kind"], "permission");
        assert_eq!(event.params["request"]["title"], "File: Sources/App.swift");
        assert_eq!(
            event.params["request"]["permission"]["options"][0]["id"],
            "allow"
        );

        let listed = ChatService::list(
            Arc::clone(&state),
            protocol::ChatListParams {
                thread_id: "thread-1".to_string(),
            },
        )
        .await
        .expect("chat.list");
        assert_eq!(listed[0].pending_blocked_requests.len(), 1);
        assert_eq!(listed[0].pending_blocked_requests[0].request_id, "55");
    }

    #[tokio::test]
    async fn mixed_legacy_and_daemon_aware_permission_request_uses_daemon_owned_answer_path() {
        let state = make_test_state();
        let (input_tx, mut input_rx) = mpsc::channel(1);
        let mut session = make_test_session();
        session.input_tx = Some(input_tx);
        session.blocked_request_capture_enabled = true;
        session.attached_channels.insert(77);
        session.attached_channels.insert(78);
        let (legacy_tx, mut legacy_rx) = mpsc::unbounded_channel();
        let (aware_tx, mut aware_rx) = mpsc::unbounded_channel();
        {
            let mut chat = state.chat.lock().await;
            chat.channel_outbound.insert(77, legacy_tx);
            chat.channel_outbound.insert(78, aware_tx);
            chat.channel_to_session.insert(78, "session-1".to_string());
            chat.blocked_request_aware_channels.insert(78);
        }
        register_test_session(&state, 77, session).await;
        let mut events = state.subscribe_events();

        fanout_output(
            Arc::clone(&state),
            "session-1",
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": "permission-mixed",
                    "method": "request_permission",
                    "params": {
                        "message": "Allow shell command?",
                        "options": [{"optionId": "run", "name": "Run", "kind": "allow"}]
                    }
                })
            )
            .as_bytes(),
        )
        .await;

        assert!(
            legacy_rx.try_recv().is_err(),
            "legacy clients must not receive raw blocked request frames once daemon-owned capture is active"
        );
        assert!(
            aware_rx.try_recv().is_err(),
            "daemon-aware clients consume blocked request events instead of raw frames"
        );
        let added = events.try_recv().expect("blocked request added event");
        assert_eq!(added.method, "chat.blocked_request.added");
        assert_eq!(added.params["request"]["request_id"], "permission-mixed");
        let state_delta = events.try_recv().expect("blocked request state delta");
        assert_eq!(state_delta.method, "state.delta");

        let result = ChatService::answer_blocked_request(
            Arc::clone(&state),
            protocol::ChatAnswerBlockedRequestParams {
                thread_id: "thread-1".to_string(),
                session_id: "session-1".to_string(),
                request_id: "permission-mixed".to_string(),
                action: None,
                option_id: Some("run".to_string()),
            },
        )
        .await
        .expect("daemon-owned answer wins");

        assert_eq!(result.request_id, "permission-mixed");
        let forwarded = input_rx.try_recv().expect("agent response frame");
        let response: Value = serde_json::from_slice(&forwarded).expect("parse agent response");
        assert_eq!(response["id"], "permission-mixed");
        assert_eq!(response["result"]["outcome"]["outcome"], "selected");
        assert_eq!(response["result"]["outcome"]["optionId"], "run");

        let answered = events.try_recv().expect("answered event");
        assert_eq!(answered.method, "chat.blocked_request.answered");
        let removed = events.try_recv().expect("removed event");
        assert_eq!(removed.method, "chat.blocked_request.removed");
        let status = ChatService::status(
            Arc::clone(&state),
            protocol::ChatStatusParams {
                session_id: "session-1".to_string(),
            },
        )
        .await
        .expect("chat.status");
        assert!(status.pending_blocked_requests.is_empty());

        let stale = ChatService::answer_blocked_request(
            Arc::clone(&state),
            protocol::ChatAnswerBlockedRequestParams {
                thread_id: "thread-1".to_string(),
                session_id: "session-1".to_string(),
                request_id: "permission-mixed".to_string(),
                action: None,
                option_id: Some("run".to_string()),
            },
        )
        .await
        .expect_err("second answer must lose first-answer-wins race");
        assert!(matches!(
            stale,
            ChatBlockedRequestAnswerError::AlreadyResolved(_)
        ));
    }

    #[tokio::test]
    async fn attach_with_blocked_request_support_enables_daemon_owned_suppression() {
        let state = make_test_state();
        {
            let mut chat = state.chat.lock().await;
            chat.sessions
                .insert("session-1".to_string(), make_test_session());
            chat.sessions_by_thread
                .entry("thread-1".to_string())
                .or_default()
                .push("session-1".to_string());
        }
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
        let attached = ChatService::attach(
            protocol::ChatAttachParams {
                thread_id: "thread-1".to_string(),
                session_id: "session-1".to_string(),
            },
            Arc::clone(&state),
            Arc::new(Mutex::new(TerminalConnectionState::default())),
            outbound_tx,
            true,
        )
        .await
        .expect("chat.attach");
        assert!(attached.pending_blocked_requests.is_empty());
        let mut events = state.subscribe_events();

        fanout_output(
            Arc::clone(&state),
            "session-1",
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": "attached-permission",
                    "method": "request_permission",
                    "params": {
                        "message": "Allow shell command?",
                        "options": [{"optionId": "run", "name": "Run", "kind": "allow"}]
                    }
                })
            )
            .as_bytes(),
        )
        .await;

        assert!(
            outbound_rx.try_recv().is_err(),
            "capability-aware attached channel must not receive raw permission frame"
        );
        let event = events.try_recv().expect("blocked request added event");
        assert_eq!(event.method, "chat.blocked_request.added");
        assert_eq!(event.params["request"]["request_id"], "attached-permission");
        let status = ChatService::status(
            Arc::clone(&state),
            protocol::ChatStatusParams {
                session_id: "session-1".to_string(),
            },
        )
        .await
        .expect("chat.status");
        assert_eq!(status.pending_blocked_requests.len(), 1);
    }

    #[tokio::test]
    async fn session_owned_pre_attach_permission_request_is_captured_for_recovery() {
        let state = make_test_state();
        {
            let mut chat = state.chat.lock().await;
            chat.sessions
                .insert("session-1".to_string(), make_test_session());
            chat.sessions_by_thread
                .entry("thread-1".to_string())
                .or_default()
                .push("session-1".to_string());
        }
        enable_blocked_request_capture(&state, "session-1").await;
        let mut events = state.subscribe_events();

        fanout_output(
            Arc::clone(&state),
            "session-1",
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": "pre-attach-permission",
                    "method": "request_permission",
                    "params": {
                        "message": "Allow shell command?",
                        "options": [{"optionId": "run", "name": "Run", "kind": "allow"}]
                    }
                })
            )
            .as_bytes(),
        )
        .await;

        let event = events.try_recv().expect("blocked request added event");
        assert_eq!(event.method, "chat.blocked_request.added");
        assert_eq!(
            event.params["request"]["request_id"],
            "pre-attach-permission"
        );

        let listed = ChatService::list(
            Arc::clone(&state),
            protocol::ChatListParams {
                thread_id: "thread-1".to_string(),
            },
        )
        .await
        .expect("chat.list");
        assert_eq!(listed[0].pending_blocked_requests.len(), 1);

        let snapshot_sessions =
            ChatService::thread_chat_sessions(Arc::clone(&state), "thread-1").await;
        assert_eq!(snapshot_sessions[0].pending_blocked_requests.len(), 1);

        let (attach_tx, _attach_rx) = mpsc::unbounded_channel();
        let attached = ChatService::attach(
            protocol::ChatAttachParams {
                thread_id: "thread-1".to_string(),
                session_id: "session-1".to_string(),
            },
            Arc::clone(&state),
            Arc::new(Mutex::new(TerminalConnectionState::default())),
            attach_tx,
            true,
        )
        .await
        .expect("chat.attach");
        assert_eq!(attached.pending_blocked_requests.len(), 1);
        assert_eq!(
            attached.pending_blocked_requests[0].request_id,
            "pre-attach-permission"
        );
    }

    #[tokio::test]
    async fn chat_load_with_blocked_request_support_marks_starting_session_owned() {
        let state = make_test_state();
        let mut session = make_test_session();
        session.summary.status = protocol::ChatSessionStatus::Starting;
        {
            let mut chat = state.chat.lock().await;
            chat.sessions.insert("session-1".to_string(), session);
            chat.sessions_by_thread
                .entry("thread-1".to_string())
                .or_default()
                .push("session-1".to_string());
        }

        let loaded = ChatService::load(
            Arc::clone(&state),
            protocol::ChatLoadParams {
                thread_id: "thread-1".to_string(),
                session_id: "session-1".to_string(),
                agent_name: None,
                force_new_session: false,
            },
            ChatSessionOptions {
                capture_blocked_requests: true,
            },
        )
        .await
        .expect("chat.load starting session");

        assert_eq!(loaded.status, protocol::ChatSessionStatus::Starting);
        let chat = state.chat.lock().await;
        assert!(
            chat.sessions
                .get("session-1")
                .expect("session")
                .blocked_request_capture_enabled
        );
    }

    #[tokio::test]
    async fn disconnected_daemon_aware_session_captures_blocked_request_for_recovery() {
        let state = make_test_state();
        {
            let mut chat = state.chat.lock().await;
            chat.sessions
                .insert("session-1".to_string(), make_test_session());
            chat.sessions_by_thread
                .entry("thread-1".to_string())
                .or_default()
                .push("session-1".to_string());
        }

        let connection_state = Arc::new(Mutex::new(TerminalConnectionState::default()));
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
        let attached = ChatService::attach(
            protocol::ChatAttachParams {
                thread_id: "thread-1".to_string(),
                session_id: "session-1".to_string(),
            },
            Arc::clone(&state),
            Arc::clone(&connection_state),
            outbound_tx,
            true,
        )
        .await
        .expect("chat.attach");
        let detached = ChatService::detach(
            protocol::ChatDetachParams {
                channel_id: attached.channel_id,
            },
            Arc::clone(&state),
            Arc::clone(&connection_state),
        )
        .await
        .expect("chat.detach");
        assert!(detached.detached);
        let mut events = state.subscribe_events();

        fanout_output(
            Arc::clone(&state),
            "session-1",
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": "permission-after-disconnect",
                    "method": "request_permission",
                    "params": {
                        "message": "Allow shell command?",
                        "options": [{"optionId": "run", "name": "Run", "kind": "allow"}]
                    }
                })
            )
            .as_bytes(),
        )
        .await;

        assert!(
            outbound_rx.try_recv().is_err(),
            "disconnected channel must not receive raw permission frame"
        );
        let event = events.try_recv().expect("blocked request added event");
        assert_eq!(event.method, "chat.blocked_request.added");
        assert_eq!(
            event.params["request"]["request_id"],
            "permission-after-disconnect"
        );

        let listed = ChatService::list(
            Arc::clone(&state),
            protocol::ChatListParams {
                thread_id: "thread-1".to_string(),
            },
        )
        .await
        .expect("chat.list");
        assert_eq!(listed[0].pending_blocked_requests.len(), 1);
        assert_eq!(
            listed[0].pending_blocked_requests[0].request_id,
            "permission-after-disconnect"
        );

        let snapshot_sessions =
            ChatService::thread_chat_sessions(Arc::clone(&state), "thread-1").await;
        assert_eq!(snapshot_sessions[0].pending_blocked_requests.len(), 1);
        assert_eq!(
            snapshot_sessions[0].pending_blocked_requests[0].request_id,
            "permission-after-disconnect"
        );

        let (reattach_tx, _reattach_rx) = mpsc::unbounded_channel();
        let reattached = ChatService::attach(
            protocol::ChatAttachParams {
                thread_id: "thread-1".to_string(),
                session_id: "session-1".to_string(),
            },
            Arc::clone(&state),
            connection_state,
            reattach_tx,
            true,
        )
        .await
        .expect("reattach chat session");
        assert_eq!(reattached.pending_blocked_requests.len(), 1);
        assert_eq!(
            reattached.pending_blocked_requests[0].request_id,
            "permission-after-disconnect"
        );
    }

    #[tokio::test]
    async fn daemon_owned_session_captures_after_capable_detach_with_legacy_channel_remaining() {
        let state = make_test_state();
        {
            let mut chat = state.chat.lock().await;
            chat.sessions
                .insert("session-1".to_string(), make_test_session());
            chat.sessions_by_thread
                .entry("thread-1".to_string())
                .or_default()
                .push("session-1".to_string());
        }

        let connection_state = Arc::new(Mutex::new(TerminalConnectionState::default()));
        let (capable_tx, mut capable_rx) = mpsc::unbounded_channel();
        let capable = ChatService::attach(
            protocol::ChatAttachParams {
                thread_id: "thread-1".to_string(),
                session_id: "session-1".to_string(),
            },
            Arc::clone(&state),
            Arc::clone(&connection_state),
            capable_tx,
            true,
        )
        .await
        .expect("capable chat.attach");
        let (legacy_tx, mut legacy_rx) = mpsc::unbounded_channel();
        let _legacy = ChatService::attach(
            protocol::ChatAttachParams {
                thread_id: "thread-1".to_string(),
                session_id: "session-1".to_string(),
            },
            Arc::clone(&state),
            Arc::clone(&connection_state),
            legacy_tx,
            false,
        )
        .await
        .expect("legacy chat.attach");
        let detached = ChatService::detach(
            protocol::ChatDetachParams {
                channel_id: capable.channel_id,
            },
            Arc::clone(&state),
            Arc::clone(&connection_state),
        )
        .await
        .expect("capable chat.detach");
        assert!(detached.detached);
        let mut events = state.subscribe_events();

        fanout_output(
            Arc::clone(&state),
            "session-1",
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": "permission-after-aware-detach",
                    "method": "request_permission",
                    "params": {
                        "message": "Allow shell command?",
                        "options": [{"optionId": "run", "name": "Run", "kind": "allow"}]
                    }
                })
            )
            .as_bytes(),
        )
        .await;

        assert!(
            capable_rx.try_recv().is_err(),
            "detached capable channel must not receive frames"
        );
        assert!(
            legacy_rx.try_recv().is_err(),
            "legacy channel must not receive raw blocked request after session opted in"
        );
        let event = events.try_recv().expect("blocked request added event");
        assert_eq!(event.method, "chat.blocked_request.added");
        assert_eq!(
            event.params["request"]["request_id"],
            "permission-after-aware-detach"
        );

        let listed = ChatService::list(
            Arc::clone(&state),
            protocol::ChatListParams {
                thread_id: "thread-1".to_string(),
            },
        )
        .await
        .expect("chat.list");
        assert_eq!(listed[0].pending_blocked_requests.len(), 1);

        let snapshot_sessions =
            ChatService::thread_chat_sessions(Arc::clone(&state), "thread-1").await;
        assert_eq!(snapshot_sessions[0].pending_blocked_requests.len(), 1);
    }

    #[tokio::test]
    async fn split_outbound_permission_request_is_buffered_until_complete_and_filtered() {
        let state = make_test_state();
        let mut session = make_test_session();
        session.attached_channels.insert(77);
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
        {
            let mut chat = state.chat.lock().await;
            chat.channel_outbound.insert(77, outbound_tx);
        }
        register_test_session(&state, 77, session).await;
        mark_blocked_request_aware_channel(&state, 77).await;
        let mut events = state.subscribe_events();

        let blocked_frame = format!(
            "{}\n",
            json!({
                "jsonrpc": "2.0",
                "id": "permission-split",
                "method": "request_permission",
                "params": {
                    "message": "Allow shell command?",
                    "options": [{"optionId": "run", "name": "Run", "kind": "allow"}]
                }
            })
        );
        let split_at = blocked_frame
            .find("request_permission")
            .expect("method marker");

        fanout_output(
            Arc::clone(&state),
            "session-1",
            &blocked_frame.as_bytes()[..split_at],
        )
        .await;

        assert!(
            outbound_rx.try_recv().is_err(),
            "partial blocked request bytes must not be relayed before newline"
        );
        assert!(
            events.try_recv().is_err(),
            "partial blocked request must not emit event before complete frame"
        );

        fanout_output(
            Arc::clone(&state),
            "session-1",
            &blocked_frame.as_bytes()[split_at..],
        )
        .await;

        assert!(
            outbound_rx.try_recv().is_err(),
            "captured split blocked request must not be relayed"
        );
        let event = events.try_recv().expect("blocked request added event");
        assert_eq!(event.method, "chat.blocked_request.added");
        assert_eq!(event.params["request"]["request_id"], "permission-split");
    }

    #[tokio::test]
    async fn blocked_request_followed_by_partial_nonblocked_frame_keeps_partial_bytes() {
        let state = make_test_state();
        let mut session = make_test_session();
        session.attached_channels.insert(77);
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
        {
            let mut chat = state.chat.lock().await;
            chat.channel_outbound.insert(77, outbound_tx);
        }
        register_test_session(&state, 77, session).await;
        mark_blocked_request_aware_channel(&state, 77).await;
        let mut events = state.subscribe_events();

        let blocked_frame = format!(
            "{}\n",
            json!({
                "jsonrpc": "2.0",
                "id": "permission-before-partial",
                "method": "request_permission",
                "params": {
                    "message": "Allow file edit?",
                    "options": [{"optionId": "allow", "name": "Allow", "kind": "allow"}]
                }
            })
        );
        let passthrough_frame = format!(
            "{}\n",
            json!({
                "jsonrpc": "2.0",
                "method": "session/notify",
                "params": {"text": "still relayed"}
            })
        );
        let split_at = passthrough_frame
            .find("still relayed")
            .expect("payload marker");
        let first_chunk = format!("{}{}", blocked_frame, &passthrough_frame[..split_at]);

        fanout_output(Arc::clone(&state), "session-1", first_chunk.as_bytes()).await;

        assert!(
            outbound_rx.try_recv().is_err(),
            "partial pass-through frame after blocked request must stay buffered"
        );
        let event = events.try_recv().expect("blocked request added event");
        assert_eq!(event.method, "chat.blocked_request.added");
        assert_eq!(
            event.params["request"]["request_id"],
            "permission-before-partial"
        );

        fanout_output(
            Arc::clone(&state),
            "session-1",
            &passthrough_frame.as_bytes()[split_at..],
        )
        .await;

        let frame = outbound_rx
            .try_recv()
            .expect("completed pass-through frame");
        let Message::Binary(frame) = frame else {
            panic!("expected binary chat frame");
        };
        assert_eq!(&frame[..2], &77u16.to_be_bytes());
        let relayed: Value = serde_json::from_slice(&frame[2..]).expect("relayed JSON frame");
        assert_eq!(relayed["method"], "session/notify");
        assert_eq!(relayed["params"]["text"], "still relayed");
        assert!(
            outbound_rx.try_recv().is_err(),
            "only completed non-blocked frame should be relayed"
        );
    }

    #[test]
    fn blocked_permission_uses_acp_tool_call_id() {
        let params = json!({
            "message": "Allow file edit?",
            "options": [
                {"optionId": "allow", "name": "Allow", "kind": "allow"},
                {"optionId": "deny", "name": "Deny", "kind": "deny"}
            ],
            "toolCall": {
                "toolCallId": "tool-call-1",
                "title": "Edit file",
                "kind": "edit",
                "rawInput": {"file_path": "Sources/App.swift"}
            }
        });

        let permission =
            blocked_permission_from_params(Some(&params)).expect("valid permission payload");

        assert_eq!(permission.tool_call_id.as_deref(), Some("tool-call-1"));
    }

    #[tokio::test]
    async fn blocked_request_insertion_emits_session_update_state_delta() {
        let state = make_test_state();
        let mut session = make_test_session();
        session.attached_channels.insert(77);
        register_test_session(&state, 77, session).await;
        mark_blocked_request_aware_channel(&state, 77).await;
        let mut events = state.subscribe_events();

        fanout_output(
            Arc::clone(&state),
            "session-1",
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": "permission-1",
                    "method": "request_permission",
                    "params": {
                        "message": "Allow shell command?",
                        "options": [{"optionId": "run", "name": "Run", "kind": "allow"}]
                    }
                })
            )
            .as_bytes(),
        )
        .await;

        let event = events.try_recv().expect("blocked request added event");
        assert_eq!(event.method, "chat.blocked_request.added");

        let state_delta = events.try_recv().expect("blocked request state delta");
        assert_eq!(state_delta.method, "state.delta");
        let operation = state_delta.params["operations"]
            .as_array()
            .and_then(|operations| operations.first())
            .expect("state delta operation");
        assert_eq!(operation["type"], "chat.session_updated");
        assert_eq!(operation["thread_id"], "thread-1");
        assert_eq!(
            operation["chat_session"]["pending_blocked_requests"][0]["request_id"],
            "permission-1"
        );
    }

    #[tokio::test]
    async fn malformed_permission_request_without_valid_options_stays_on_raw_path() {
        let state = make_test_state();
        let mut session = make_test_session();
        session.attached_channels.insert(77);
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
        {
            let mut chat = state.chat.lock().await;
            chat.channel_outbound.insert(77, outbound_tx);
        }
        register_test_session(&state, 77, session).await;
        let mut events = state.subscribe_events();

        fanout_output(
            Arc::clone(&state),
            "session-1",
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": "permission-without-options",
                    "method": "request_permission",
                    "params": {
                        "message": "Allow file edit?",
                        "options": [
                            {"name": "Missing option id"},
                            {"optionId": "", "name": "Blank option id"}
                        ]
                    }
                })
            )
            .as_bytes(),
        )
        .await;

        let frame = outbound_rx.try_recv().expect("raw permission frame");
        let Message::Binary(frame) = frame else {
            panic!("expected binary chat frame");
        };
        assert_eq!(&frame[..2], &77u16.to_be_bytes());
        let relayed: Value = serde_json::from_slice(&frame[2..]).expect("relayed JSON frame");
        assert_eq!(relayed["method"], "request_permission");
        assert_eq!(relayed["id"], "permission-without-options");
        assert!(
            events.try_recv().is_err(),
            "malformed permission requests must not become blocked requests"
        );
        let status = ChatService::status(
            Arc::clone(&state),
            protocol::ChatStatusParams {
                session_id: "session-1".to_string(),
            },
        )
        .await
        .expect("chat.status");
        assert!(status.pending_blocked_requests.is_empty());
    }

    #[tokio::test]
    async fn daemon_aware_malformed_permission_request_sends_agent_error_without_client_relay() {
        let state = make_test_state();
        let (input_tx, mut input_rx) = mpsc::channel(1);
        let mut session = make_test_session();
        session.input_tx = Some(input_tx);
        session.attached_channels.insert(77);
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
        {
            let mut chat = state.chat.lock().await;
            chat.channel_outbound.insert(77, outbound_tx);
        }
        register_test_session(&state, 77, session).await;
        mark_blocked_request_aware_channel(&state, 77).await;
        let mut events = state.subscribe_events();

        fanout_output(
            Arc::clone(&state),
            "session-1",
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": "permission-without-options",
                    "method": "request_permission",
                    "params": {
                        "message": "Allow file edit?",
                        "options": [
                            {"name": "Missing option id"},
                            {"optionId": "", "name": "Blank option id"}
                        ]
                    }
                })
            )
            .as_bytes(),
        )
        .await;

        assert!(
            outbound_rx.try_recv().is_err(),
            "daemon-aware clients must not receive malformed blocked request raw method or error"
        );
        let agent_error = input_rx.try_recv().expect("agent stdin error frame");
        assert_invalid_blocked_request_agent_error(
            &agent_error,
            "permission-without-options",
            "request_permission",
        );
        assert!(
            events.try_recv().is_err(),
            "malformed permission requests must not become blocked requests"
        );
        let status = ChatService::status(
            Arc::clone(&state),
            protocol::ChatStatusParams {
                session_id: "session-1".to_string(),
            },
        )
        .await
        .expect("chat.status");
        assert!(status.pending_blocked_requests.is_empty());
    }

    #[tokio::test]
    async fn session_owned_malformed_permission_without_attached_clients_errors_agent_only() {
        let state = make_test_state();
        let (input_tx, mut input_rx) = mpsc::channel(1);
        let mut session = make_test_session();
        session.input_tx = Some(input_tx);
        session.blocked_request_capture_enabled = true;
        {
            let mut chat = state.chat.lock().await;
            chat.sessions.insert("session-1".to_string(), session);
            chat.sessions_by_thread
                .entry("thread-1".to_string())
                .or_default()
                .push("session-1".to_string());
        }
        let mut events = state.subscribe_events();

        fanout_output(
            Arc::clone(&state),
            "session-1",
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": "disconnected-malformed-permission",
                    "method": "request_permission",
                    "params": {
                        "message": "Allow file edit?",
                        "options": []
                    }
                })
            )
            .as_bytes(),
        )
        .await;

        let agent_error = input_rx.try_recv().expect("agent stdin error frame");
        assert_invalid_blocked_request_agent_error(
            &agent_error,
            "disconnected-malformed-permission",
            "request_permission",
        );
        assert!(
            events.try_recv().is_err(),
            "owned malformed request must not emit client events"
        );
        let status = ChatService::status(
            Arc::clone(&state),
            protocol::ChatStatusParams {
                session_id: "session-1".to_string(),
            },
        )
        .await
        .expect("chat.status");
        assert!(status.pending_blocked_requests.is_empty());
    }

    #[tokio::test]
    async fn session_owned_malformed_question_with_only_legacy_channel_errors_agent_only() {
        let state = make_test_state();
        let (input_tx, mut input_rx) = mpsc::channel(1);
        let mut session = make_test_session();
        session.input_tx = Some(input_tx);
        session.blocked_request_capture_enabled = true;
        session.attached_channels.insert(78);
        let (legacy_tx, mut legacy_rx) = mpsc::unbounded_channel();
        {
            let mut chat = state.chat.lock().await;
            chat.channel_outbound.insert(78, legacy_tx);
        }
        register_test_session(&state, 78, session).await;
        let mut events = state.subscribe_events();

        fanout_output(
            Arc::clone(&state),
            "session-1",
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": "legacy-only-malformed-question",
                    "method": "request_question",
                    "params": {"question": "   "}
                })
            )
            .as_bytes(),
        )
        .await;

        assert!(
            legacy_rx.try_recv().is_err(),
            "legacy clients must not receive malformed blocked request raw method or error after session opts in"
        );
        let agent_error = input_rx.try_recv().expect("agent stdin error frame");
        assert_invalid_blocked_request_agent_error(
            &agent_error,
            "legacy-only-malformed-question",
            "request_question",
        );
        assert!(
            events.try_recv().is_err(),
            "owned malformed request must not emit client events"
        );
        let status = ChatService::status(
            Arc::clone(&state),
            protocol::ChatStatusParams {
                session_id: "session-1".to_string(),
            },
        )
        .await
        .expect("chat.status");
        assert!(status.pending_blocked_requests.is_empty());
    }

    #[tokio::test]
    async fn malformed_question_request_without_prompt_stays_on_raw_path() {
        let state = make_test_state();
        let mut session = make_test_session();
        session.attached_channels.insert(78);
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
        {
            let mut chat = state.chat.lock().await;
            chat.channel_outbound.insert(78, outbound_tx);
        }
        register_test_session(&state, 78, session).await;
        let mut events = state.subscribe_events();

        fanout_output(
            Arc::clone(&state),
            "session-1",
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": "question-without-prompt",
                    "method": "request_question",
                    "params": {"question": "   "}
                })
            )
            .as_bytes(),
        )
        .await;

        let frame = outbound_rx.try_recv().expect("raw question frame");
        let Message::Binary(frame) = frame else {
            panic!("expected binary chat frame");
        };
        assert_eq!(&frame[..2], &78u16.to_be_bytes());
        let relayed: Value = serde_json::from_slice(&frame[2..]).expect("relayed JSON frame");
        assert_eq!(relayed["method"], "request_question");
        assert_eq!(relayed["id"], "question-without-prompt");
        assert!(
            events.try_recv().is_err(),
            "malformed question requests must not become blocked requests"
        );
        let status = ChatService::status(
            Arc::clone(&state),
            protocol::ChatStatusParams {
                session_id: "session-1".to_string(),
            },
        )
        .await
        .expect("chat.status");
        assert!(status.pending_blocked_requests.is_empty());
    }

    #[tokio::test]
    async fn daemon_aware_malformed_question_request_sends_agent_error_without_client_relay() {
        let state = make_test_state();
        let (input_tx, mut input_rx) = mpsc::channel(1);
        let mut session = make_test_session();
        session.input_tx = Some(input_tx);
        session.attached_channels.insert(78);
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
        {
            let mut chat = state.chat.lock().await;
            chat.channel_outbound.insert(78, outbound_tx);
        }
        register_test_session(&state, 78, session).await;
        mark_blocked_request_aware_channel(&state, 78).await;
        let mut events = state.subscribe_events();

        fanout_output(
            Arc::clone(&state),
            "session-1",
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": "question-without-prompt",
                    "method": "request_question",
                    "params": {"question": "   "}
                })
            )
            .as_bytes(),
        )
        .await;

        assert!(
            outbound_rx.try_recv().is_err(),
            "daemon-aware clients must not receive malformed blocked request raw method or error"
        );
        let agent_error = input_rx.try_recv().expect("agent stdin error frame");
        assert_invalid_blocked_request_agent_error(
            &agent_error,
            "question-without-prompt",
            "request_question",
        );
        assert!(
            events.try_recv().is_err(),
            "malformed question requests must not become blocked requests"
        );
        let status = ChatService::status(
            Arc::clone(&state),
            protocol::ChatStatusParams {
                session_id: "session-1".to_string(),
            },
        )
        .await
        .expect("chat.status");
        assert!(status.pending_blocked_requests.is_empty());
    }

    #[tokio::test]
    async fn answer_blocked_permission_sends_result_and_removes_pending_request() {
        let state = make_test_state();
        let (input_tx, mut input_rx) = mpsc::channel(1);
        let mut session = make_test_session();
        session.input_tx = Some(input_tx);
        session.attached_channels.insert(77);
        register_test_session(&state, 77, session).await;
        mark_blocked_request_aware_channel(&state, 77).await;
        fanout_output(
            Arc::clone(&state),
            "session-1",
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": "permission-1",
                    "method": "session/request_permission",
                    "params": {
                        "message": "Allow shell command?",
                        "options": [{"optionId": "run", "name": "Run", "kind": "allow"}]
                    }
                })
            )
            .as_bytes(),
        )
        .await;
        let mut events = state.subscribe_events();

        let result = ChatService::answer_blocked_request(
            Arc::clone(&state),
            protocol::ChatAnswerBlockedRequestParams {
                thread_id: "thread-1".to_string(),
                session_id: "session-1".to_string(),
                request_id: "permission-1".to_string(),
                action: None,
                option_id: Some("run".to_string()),
            },
        )
        .await
        .expect("answer blocked permission");

        assert_eq!(result.request_id, "permission-1");
        assert_eq!(result.option_id.as_deref(), Some("run"));
        let forwarded = input_rx.try_recv().expect("agent response frame");
        let response: Value = serde_json::from_slice(&forwarded).expect("parse agent response");
        assert_eq!(response["id"], "permission-1");
        assert_eq!(response["result"]["outcome"]["outcome"], "selected");
        assert_eq!(response["result"]["outcome"]["optionId"], "run");

        let answered = events.try_recv().expect("answered event");
        assert_eq!(answered.method, "chat.blocked_request.answered");
        let removed = events.try_recv().expect("removed event");
        assert_eq!(removed.method, "chat.blocked_request.removed");

        let status = ChatService::status(
            Arc::clone(&state),
            protocol::ChatStatusParams {
                session_id: "session-1".to_string(),
            },
        )
        .await
        .expect("chat.status");
        assert!(status.pending_blocked_requests.is_empty());

        let second = ChatService::answer_blocked_request(
            Arc::clone(&state),
            protocol::ChatAnswerBlockedRequestParams {
                thread_id: "thread-1".to_string(),
                session_id: "session-1".to_string(),
                request_id: "permission-1".to_string(),
                action: None,
                option_id: Some("run".to_string()),
            },
        )
        .await
        .expect_err("stale answer should fail");
        match second {
            ChatBlockedRequestAnswerError::AlreadyResolved(details) => {
                assert_eq!(details.thread_id, "thread-1");
                assert_eq!(details.session_id, "session-1");
                assert_eq!(details.request_id, "permission-1");
            }
            other => panic!("expected already resolved error, got {other}"),
        }
    }

    #[tokio::test]
    async fn answer_blocked_question_sends_selected_action() {
        let state = make_test_state();
        let (input_tx, mut input_rx) = mpsc::channel(1);
        let mut session = make_test_session();
        session.input_tx = Some(input_tx);
        session.attached_channels.insert(77);
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
        {
            let mut chat = state.chat.lock().await;
            chat.channel_outbound.insert(77, outbound_tx);
        }
        register_test_session(&state, 77, session).await;
        mark_blocked_request_aware_channel(&state, 77).await;
        fanout_output(
            Arc::clone(&state),
            "session-1",
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": "question-1",
                    "method": "request_question",
                    "params": {"title": "Confirm", "question": "Continue?"}
                })
            )
            .as_bytes(),
        )
        .await;

        assert!(
            outbound_rx.try_recv().is_err(),
            "captured blocked question must not be relayed to legacy clients"
        );

        let result = ChatService::answer_blocked_request(
            Arc::clone(&state),
            protocol::ChatAnswerBlockedRequestParams {
                thread_id: "thread-1".to_string(),
                session_id: "session-1".to_string(),
                request_id: "question-1".to_string(),
                action: Some(protocol::BlockedRequestAnswerAction::Decline),
                option_id: None,
            },
        )
        .await
        .expect("answer blocked question");

        assert_eq!(
            result.action,
            Some(protocol::BlockedRequestAnswerAction::Decline)
        );
        let forwarded = input_rx.try_recv().expect("agent response frame");
        let response: Value = serde_json::from_slice(&forwarded).expect("parse agent response");
        assert_eq!(response["id"], "question-1");
        assert_eq!(response["result"]["action"], "decline");
    }

    #[tokio::test]
    async fn session_cancel_send_failure_keeps_blocked_request_pending() {
        let state = make_test_state();
        let (input_tx, _input_rx) = mpsc::channel(1);
        input_tx.try_send(vec![b'x']).expect("fill input queue");
        let mut session = make_test_session();
        session.input_tx = Some(input_tx);
        session.attached_channels.insert(77);
        register_test_session(&state, 77, session).await;
        mark_blocked_request_aware_channel(&state, 77).await;

        fanout_output(
            Arc::clone(&state),
            "session-1",
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": "permission-cancel-failure",
                    "method": "request_permission",
                    "params": {
                        "message": "Allow shell command?",
                        "options": [{"optionId": "run", "name": "Run", "kind": "allow"}]
                    }
                })
            )
            .as_bytes(),
        )
        .await;
        let mut events = state.subscribe_events();

        let error = ChatService::handle_binary_frame(
            Arc::clone(&state),
            77,
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": "cancel-queue-full",
                    "method": "session/cancel",
                    "params": {"sessionId": "session-1"}
                })
            )
            .into_bytes(),
        )
        .await
        .expect_err("full input queue must fail cancel delivery");

        assert!(error.contains("failed to queue blocked request cancellation"));
        assert!(
            events.try_recv().is_err(),
            "failed cancellation send must not emit removal"
        );
        let status = ChatService::status(
            Arc::clone(&state),
            protocol::ChatStatusParams {
                session_id: "session-1".to_string(),
            },
        )
        .await
        .expect("chat.status");
        assert_eq!(status.pending_blocked_requests.len(), 1);
        assert_eq!(
            status.pending_blocked_requests[0].request_id,
            "permission-cancel-failure"
        );
    }

    #[tokio::test]
    async fn session_cancel_cancels_pending_blocked_requests_and_recovers_cleanly() {
        let state = make_test_state();
        let (input_tx, mut input_rx) = mpsc::channel(8);
        let mut session = make_test_session();
        session.input_tx = Some(input_tx);
        session.attached_channels.insert(77);
        register_test_session(&state, 77, session).await;
        mark_blocked_request_aware_channel(&state, 77).await;

        fanout_output(
            Arc::clone(&state),
            "session-1",
            format!(
                "{}\n{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": "permission-1",
                    "method": "request_permission",
                    "params": {
                        "message": "Allow shell command?",
                        "options": [{"optionId": "run", "name": "Run", "kind": "allow"}]
                    }
                }),
                json!({
                    "jsonrpc": "2.0",
                    "id": "question-1",
                    "method": "request_question",
                    "params": {"title": "Confirm", "question": "Continue?"}
                })
            )
            .as_bytes(),
        )
        .await;
        let mut events = state.subscribe_events();

        let handled = ChatService::handle_binary_frame(
            Arc::clone(&state),
            77,
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": "cancel-1",
                    "method": "session/cancel",
                    "params": {"sessionId": "session-1"}
                })
            )
            .into_bytes(),
        )
        .await
        .expect("handle cancel frame");
        assert!(handled);

        let permission = input_rx.recv().await.expect("permission cancellation");
        let permission_response: Value =
            serde_json::from_slice(&permission).expect("parse permission response");
        assert_eq!(permission_response["id"], "permission-1");
        assert_eq!(
            permission_response["result"]["outcome"]["outcome"],
            "cancelled"
        );

        let question = input_rx.recv().await.expect("question cancellation");
        let question_response: Value =
            serde_json::from_slice(&question).expect("parse question response");
        assert_eq!(question_response["id"], "question-1");
        assert_eq!(question_response["result"]["action"], "cancel");

        let cancel = input_rx.recv().await.expect("forwarded cancel");
        let forwarded_cancel: Value = serde_json::from_slice(&cancel).expect("parse cancel frame");
        assert_eq!(forwarded_cancel["method"], "session/cancel");
        assert_eq!(forwarded_cancel["params"]["sessionId"], "acp-session-1");

        let removed_permission = events.try_recv().expect("permission removed event");
        assert_eq!(removed_permission.method, "chat.blocked_request.removed");
        assert_eq!(removed_permission.params["request_id"], "permission-1");
        let removed_question = events.try_recv().expect("question removed event");
        assert_eq!(removed_question.method, "chat.blocked_request.removed");
        assert_eq!(removed_question.params["request_id"], "question-1");
        let state_delta = events.try_recv().expect("cancel state delta");
        assert_eq!(state_delta.method, "state.delta");
        let operation = state_delta.params["operations"]
            .as_array()
            .and_then(|operations| operations.first())
            .expect("state delta operation");
        assert_eq!(operation["type"], "chat.session_updated");
        assert!(operation["chat_session"]
            .get("pending_blocked_requests")
            .and_then(Value::as_array)
            .map(Vec::is_empty)
            .unwrap_or(true));

        let listed = ChatService::list(
            Arc::clone(&state),
            protocol::ChatListParams {
                thread_id: "thread-1".to_string(),
            },
        )
        .await
        .expect("chat.list");
        assert_eq!(listed.len(), 1);
        assert!(listed[0].pending_blocked_requests.is_empty());

        let connection_state = Arc::new(Mutex::new(TerminalConnectionState::default()));
        let (outbound_tx, _outbound_rx) = mpsc::unbounded_channel();
        let attached = ChatService::attach(
            protocol::ChatAttachParams {
                thread_id: "thread-1".to_string(),
                session_id: "session-1".to_string(),
            },
            Arc::clone(&state),
            connection_state,
            outbound_tx,
            false,
        )
        .await
        .expect("chat.attach");
        assert!(attached.pending_blocked_requests.is_empty());
    }

    #[test]
    fn permission_agent_response_requires_answer_option_id() {
        let pending = make_pending_blocked_request(protocol::BlockedRequestKind::Permission);
        let result = protocol::BlockedRequestAnswerResult {
            thread_id: "thread-1".to_string(),
            session_id: "session-1".to_string(),
            request_id: "blocked-1".to_string(),
            action: None,
            option_id: None,
            answered_at: Utc::now().to_rfc3339(),
        };

        let error = agent_response_for_blocked_request(&pending, &result)
            .expect_err("missing permission option_id must fail");

        assert!(
            matches!(error, ChatBlockedRequestAnswerError::Invalid(message) if message == "invariant violation: permission blocked request answer missing option_id")
        );
    }

    #[test]
    fn question_agent_response_requires_answer_action() {
        let pending = make_pending_blocked_request(protocol::BlockedRequestKind::Question);
        let result = protocol::BlockedRequestAnswerResult {
            thread_id: "thread-1".to_string(),
            session_id: "session-1".to_string(),
            request_id: "blocked-1".to_string(),
            action: None,
            option_id: None,
            answered_at: Utc::now().to_rfc3339(),
        };

        let error = agent_response_for_blocked_request(&pending, &result)
            .expect_err("missing question action must fail");

        assert!(
            matches!(error, ChatBlockedRequestAnswerError::Invalid(message) if message == "invariant violation: question blocked request answer missing action")
        );
    }

    #[tokio::test]
    async fn invalid_permission_option_keeps_blocked_request_pending() {
        let state = make_test_state();
        let (input_tx, mut input_rx) = mpsc::channel(1);
        let mut session = make_test_session();
        session.input_tx = Some(input_tx);
        session.attached_channels.insert(77);
        register_test_session(&state, 77, session).await;
        mark_blocked_request_aware_channel(&state, 77).await;
        fanout_output(
            Arc::clone(&state),
            "session-1",
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": "permission-2",
                    "method": "request_permission",
                    "params": {
                        "message": "Allow shell command?",
                        "options": [{"optionId": "run", "name": "Run", "kind": "allow"}]
                    }
                })
            )
            .as_bytes(),
        )
        .await;

        let error = ChatService::answer_blocked_request(
            Arc::clone(&state),
            protocol::ChatAnswerBlockedRequestParams {
                thread_id: "thread-1".to_string(),
                session_id: "session-1".to_string(),
                request_id: "permission-2".to_string(),
                action: None,
                option_id: Some("not-advertised".to_string()),
            },
        )
        .await
        .expect_err("invalid option must fail");

        assert!(matches!(error, ChatBlockedRequestAnswerError::Invalid(_)));
        assert!(input_rx.try_recv().is_err());
        let status = ChatService::status(
            Arc::clone(&state),
            protocol::ChatStatusParams {
                session_id: "session-1".to_string(),
            },
        )
        .await
        .expect("chat.status");
        assert_eq!(status.pending_blocked_requests.len(), 1);
    }

    #[tokio::test]
    async fn send_failure_keeps_blocked_request_pending() {
        let state = make_test_state();
        let (input_tx, _input_rx) = mpsc::channel(1);
        input_tx.try_send(vec![b'x']).expect("fill input queue");
        let mut session = make_test_session();
        session.input_tx = Some(input_tx);
        session.attached_channels.insert(77);
        register_test_session(&state, 77, session).await;
        mark_blocked_request_aware_channel(&state, 77).await;
        fanout_output(
            Arc::clone(&state),
            "session-1",
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": "permission-3",
                    "method": "request_permission",
                    "params": {
                        "message": "Allow shell command?",
                        "options": [{"optionId": "run", "name": "Run", "kind": "allow"}]
                    }
                })
            )
            .as_bytes(),
        )
        .await;
        let mut events = state.subscribe_events();

        let error = ChatService::answer_blocked_request(
            Arc::clone(&state),
            protocol::ChatAnswerBlockedRequestParams {
                thread_id: "thread-1".to_string(),
                session_id: "session-1".to_string(),
                request_id: "permission-3".to_string(),
                action: None,
                option_id: Some("run".to_string()),
            },
        )
        .await
        .expect_err("full input queue must fail");

        assert!(matches!(error, ChatBlockedRequestAnswerError::Delivery(_)));
        assert!(
            events.try_recv().is_err(),
            "failed send must not emit removal"
        );
        let status = ChatService::status(
            Arc::clone(&state),
            protocol::ChatStatusParams {
                session_id: "session-1".to_string(),
            },
        )
        .await
        .expect("chat.status");
        assert_eq!(status.pending_blocked_requests.len(), 1);
    }

    #[tokio::test]
    async fn stopped_session_delivery_failure_keeps_blocked_request_pending() {
        let state = make_test_state();
        let mut session = make_test_session();
        session.attached_channels.insert(77);
        register_test_session(&state, 77, session).await;
        mark_blocked_request_aware_channel(&state, 77).await;
        fanout_output(
            Arc::clone(&state),
            "session-1",
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": "permission-4",
                    "method": "request_permission",
                    "params": {
                        "message": "Allow shell command?",
                        "options": [{"optionId": "run", "name": "Run", "kind": "allow"}]
                    }
                })
            )
            .as_bytes(),
        )
        .await;
        let mut events = state.subscribe_events();

        let error = ChatService::answer_blocked_request(
            Arc::clone(&state),
            protocol::ChatAnswerBlockedRequestParams {
                thread_id: "thread-1".to_string(),
                session_id: "session-1".to_string(),
                request_id: "permission-4".to_string(),
                action: None,
                option_id: Some("run".to_string()),
            },
        )
        .await
        .expect_err("stopped session must fail as delivery error");

        assert!(matches!(error, ChatBlockedRequestAnswerError::Delivery(_)));
        assert!(
            events.try_recv().is_err(),
            "stopped session must not emit removal"
        );
        let status = ChatService::status(
            Arc::clone(&state),
            protocol::ChatStatusParams {
                session_id: "session-1".to_string(),
            },
        )
        .await
        .expect("chat.status");
        assert_eq!(status.pending_blocked_requests.len(), 1);
    }

    #[tokio::test]
    async fn session_exit_removes_pending_blocked_requests() {
        let state = make_test_state();
        let mut session = make_test_session();
        session.attached_channels.insert(77);
        register_test_session(&state, 77, session).await;
        mark_blocked_request_aware_channel(&state, 77).await;
        fanout_output(
            Arc::clone(&state),
            "session-1",
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": "question-1",
                    "method": "session/request_question",
                    "params": {"question": "Continue?"}
                })
            )
            .as_bytes(),
        )
        .await;
        let mut events = state.subscribe_events();

        mark_session_ended(
            Arc::clone(&state),
            "thread-1",
            "session-1",
            "agent exited",
            false,
        )
        .await;

        let removed = events.try_recv().expect("removed event");
        assert_eq!(removed.method, "chat.blocked_request.removed");
        assert_eq!(removed.params["request_id"], "question-1");
        let status = ChatService::status(
            Arc::clone(&state),
            protocol::ChatStatusParams {
                session_id: "session-1".to_string(),
            },
        )
        .await
        .expect("chat.status");
        assert!(status.pending_blocked_requests.is_empty());
    }

    #[tokio::test]
    async fn prompt_response_transitions_agent_back_to_idle() {
        let state = make_test_state();
        let mut session = make_test_session();
        session.summary.agent_status = protocol::AgentStatus::Busy;
        session.pending_prompt_ids.insert("7".to_string());
        session.active_tools.insert("tool-1".to_string());
        session.summary.worker_count = 1;

        let OutboundResult { transitions, .. } = apply_outbound_status_updates(
            &state,
            &mut session,
            vec![json!({"jsonrpc": "2.0", "id": 7, "result": {"ok": true}})],
        );

        assert_eq!(session.summary.agent_status, protocol::AgentStatus::Idle);
        assert_eq!(session.summary.worker_count, 0);
        assert!(session.active_tools.is_empty());
        assert!(session.pending_prompt_ids.is_empty());
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].old_status, protocol::AgentStatus::Busy);
        assert_eq!(transitions[0].new_status, protocol::AgentStatus::Idle);
    }

    #[tokio::test]
    async fn stalled_session_update_transitions_back_to_busy() {
        let state = make_test_state();
        let mut session = make_test_session();
        session.summary.agent_status = protocol::AgentStatus::Stalled;

        let OutboundResult { transitions, .. } = apply_outbound_status_updates(
            &state,
            &mut session,
            vec![
                json!({"jsonrpc": "2.0", "method": "session/update", "params": {"update": {"kind": "agent_message_chunk"}}}),
            ],
        );

        assert_eq!(session.summary.agent_status, protocol::AgentStatus::Busy);
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].old_status, protocol::AgentStatus::Stalled);
        assert_eq!(transitions[0].new_status, protocol::AgentStatus::Busy);

        cancel_stall_timer(&mut session);
    }

    #[test]
    fn build_conversation_context_formats_transcript_from_history_jsonl() {
        let (temp_root, history_path) = write_history_file(&[
            "not json",
            r#"{"sessionId":"acp-session-1","update":{"sessionUpdate":"user_message_chunk","messageId":"user-1","content":{"type":"text","text":"Hello"}}}"#,
            r#"{"sessionId":"acp-session-1","update":{"sessionUpdate":"user_message_chunk","messageId":"user-1","content":{"type":"text","text":", world"}}}"#,
            r#"{"sessionId":"acp-session-1","update":{"kind":"agent_message_chunk","messageId":"assistant-1","content":"Hi there"}}"#,
            r#"{"sessionId":"acp-session-1","update":{"kind":"agent_thought_chunk","messageId":"thought-1","content":{"type":"text","text":"Think 1"}}}"#,
            r#"{"sessionId":"acp-session-1","update":{"kind":"tool_call","toolCallId":"tool-1","title":"List files","status":"pending"}}"#,
            r#"{"sessionId":"acp-session-1","update":{"kind":"tool_call_update","toolCallId":"tool-1","title":"List files","status":"completed","rawInput":{"command":"ls"},"content":[{"type":"text","text":"stdout"},{"type":"text","text":"[Old tool result content cleared]"}]}}"#,
            r#"{"sessionId":"acp-session-1","update":{"kind":"tool_call_update","toolCallId":"tool-2","title":"Delete file","status":"failed","rawInput":"rm bad","error":{"message":"boom"}}}"#,
            r#"{"sessionId":"acp-session-1","update":{"kind":"plan","entries":[{"content":"First step","status":"completed"},{"content":"Second step","status":"in_progress"},{"content":"Third step","status":"pending"}]}}"#,
            r#"{"sessionId":"acp-session-1","update":{"kind":"usage_update","used":10,"size":100}}"#,
            r#"{"sessionId":"acp-session-1","update":{"kind":"config_option_update","configId":"mode"}}"#,
        ]);

        let context = build_conversation_context(&history_path, None);

        assert_eq!(
            context,
            Some(
                "<conversation-history>\nThe following is a conversation you were having with the user. All tool calls\nhave already been executed and their results are reflected in the current working\ndirectory state. Continue this conversation naturally — the user's message\nfollows after this context block.\n\n[User]\nHello, world\n\n[Assistant]\nHi there\n\n[Assistant - Thinking]\nThink 1\n\n[Tool Call: List files (completed)]\nInput: {\"command\":\"ls\"}\nOutput: stdout\n[Old tool result content cleared]\n\n[Tool Call: Delete file (failed)]\nInput: rm bad\nError: boom\n\n[Plan Update]\n[x] First step\n[-] Second step\n[ ] Third step\n</conversation-history>".to_string()
            )
        );

        let _ = fs::remove_dir_all(temp_root);
    }

    #[test]
    fn build_conversation_context_respects_cursor_and_keeps_standalone_chunks() {
        let (temp_root, history_path) = write_history_file(&[
            r#"{"sessionId":"acp-session-1","update":{"sessionUpdate":"user_message_chunk","messageId":"user-1","content":"First"}}"#,
            r#"{"sessionId":"acp-session-1","update":{"kind":"agent_message_chunk","content":"Standalone"}}"#,
            r#"{"sessionId":"acp-session-1","update":{"kind":"agent_message_chunk","messageId":"assistant-1","content":"Ignored"}}"#,
        ]);

        let context = build_conversation_context(&history_path, Some(2));

        assert_eq!(
            context,
            Some(
                "<conversation-history>\nThe following is a conversation you were having with the user. All tool calls\nhave already been executed and their results are reflected in the current working\ndirectory state. Continue this conversation naturally — the user's message\nfollows after this context block.\n\n[User]\nFirst\n\n[Assistant]\nStandalone\n</conversation-history>".to_string()
            )
        );

        let _ = fs::remove_dir_all(temp_root);
    }

    #[test]
    fn build_conversation_context_renders_pending_tools_without_updates() {
        let (temp_root, history_path) = write_history_file(&[
            r#"{"sessionId":"acp-session-1","update":{"kind":"tool_call","toolCallId":"tool-1","title":"Search repo","status":"pending"}}"#,
        ]);

        let context = build_conversation_context(&history_path, None);

        assert_eq!(
            context,
            Some(
                "<conversation-history>\nThe following is a conversation you were having with the user. All tool calls\nhave already been executed and their results are reflected in the current working\ndirectory state. Continue this conversation naturally — the user's message\nfollows after this context block.\n\n[Tool Call: Search repo (pending)]\n</conversation-history>".to_string()
            )
        );

        let _ = fs::remove_dir_all(temp_root);
    }

    #[test]
    fn build_conversation_context_renders_cancelled_tool_updates_as_terminal() {
        let (temp_root, history_path) = write_history_file(&[
            r#"{"sessionId":"acp-session-1","update":{"kind":"tool_call","toolCallId":"tool-1","title":"Search repo","status":"pending"}}"#,
            r#"{"sessionId":"acp-session-1","update":{"kind":"tool_call_update","toolCallId":"tool-1","title":"Search repo","status":"cancelled","rawInput":{"pattern":"TODO"}}}"#,
        ]);

        let context = build_conversation_context(&history_path, None);

        assert_eq!(
            context,
            Some(
                "<conversation-history>\nThe following is a conversation you were having with the user. All tool calls\nhave already been executed and their results are reflected in the current working\ndirectory state. Continue this conversation naturally — the user's message\nfollows after this context block.\n\n[Tool Call: Search repo (cancelled)]\nInput: {\"pattern\":\"TODO\"}\n</conversation-history>".to_string()
            )
        );

        let _ = fs::remove_dir_all(temp_root);
    }

    #[test]
    fn build_conversation_context_returns_none_for_empty_or_non_meaningful_history() {
        let (empty_root, empty_history_path) = write_history_file(&[]);
        assert_eq!(build_conversation_context(&empty_history_path, None), None);
        let _ = fs::remove_dir_all(empty_root);

        let (temp_root, history_path) = write_history_file(&[
            "not json",
            r#"{"sessionId":"acp-session-1","update":{"kind":"usage_update","used":10,"size":100}}"#,
            r#"{"sessionId":"acp-session-1","update":{"kind":"config_option_update","configId":"mode"}}"#,
        ]);

        assert_eq!(build_conversation_context(&history_path, Some(0)), None);
        assert_eq!(build_conversation_context(&history_path, None), None);

        let _ = fs::remove_dir_all(temp_root);
    }

    #[tokio::test]
    async fn recovers_sessions_from_persisted_history_files() {
        let temp_root = std::env::temp_dir().join(format!(
            "spindle-chat-recovery-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let state_dir = temp_root.join("threadmill");
        fs::create_dir_all(&state_dir).expect("create test state dir");
        let thread_id = "thread-recovery".to_string();
        let session_id = "session-recovery".to_string();

        let history_path =
            history_path_for_session(&state_dir.join("chat"), &thread_id, &session_id);
        fs::create_dir_all(
            history_path
                .parent()
                .expect("history path should have parent"),
        )
        .expect("create history parent dir");
        fs::write(
            &history_path,
            "{\"sessionId\":\"acp-session-1\",\"update\":{\"kind\":\"agent_message_chunk\",\"content\":\"hello\"}}\n",
        )
        .expect("write persisted history");

        let state = Arc::new(AppState::new(StateStore {
            path: state_dir.join("threads.json"),
            data: AppData {
                projects: vec![Project {
                    id: "project-1".to_string(),
                    name: "project".to_string(),
                    path: "/tmp/project".to_string(),
                    default_branch: "main".to_string(),
                }],
                threads: vec![Thread::new(
                    thread_id.clone(),
                    "project-1".to_string(),
                    "thread".to_string(),
                    "main".to_string(),
                    Some("/tmp/project".to_string()),
                    protocol::ThreadStatus::Closed,
                    protocol::SourceType::ExistingBranch,
                    Utc::now(),
                    "tm_test".to_string(),
                    0,
                )],
            },
        }));

        ChatService::recover_persisted_sessions(Arc::clone(&state))
            .await
            .expect("recover persisted sessions");

        let listed = ChatService::list(
            Arc::clone(&state),
            protocol::ChatListParams {
                thread_id: thread_id.clone(),
            },
        )
        .await
        .expect("chat.list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session_id, session_id);
        assert_eq!(listed[0].status, protocol::ChatSessionStatus::Ended);

        let chat = state.chat.lock().await;
        let session = chat.sessions.get(&session_id).expect("recovered session");
        assert_eq!(session.acp_session_id.as_deref(), Some("acp-session-1"));
        drop(chat);

        let history = ChatService::history(
            Arc::clone(&state),
            protocol::ChatHistoryParams {
                thread_id: thread_id.clone(),
                session_id: listed[0].session_id.clone(),
                cursor: None,
            },
        )
        .await
        .expect("chat.history");
        assert_eq!(history.updates.len(), 1);
        assert!(history.next_cursor.is_none());

        let _ = fs::remove_dir_all(temp_root);
    }

    #[tokio::test]
    async fn fork_copies_history_and_creates_session() {
        let state = make_test_state();
        let thread_id = "thread-1";
        let source_session_id = "source-session";

        // Set up history root and source session
        let history_root = {
            let chat = state.chat.lock().await;
            chat.history_root_path().to_path_buf()
        };
        let source_history = history_path_for_session(&history_root, thread_id, source_session_id);
        fs::create_dir_all(source_history.parent().unwrap()).unwrap();
        fs::write(
            &source_history,
            concat!(
                "{\"sessionId\":\"acp-1\",\"update\":{\"sessionUpdate\":\"user_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"hello\"}}}\n",
                "{\"sessionId\":\"acp-1\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"hi there\"}}}\n",
                "{\"sessionId\":\"acp-1\",\"update\":{\"sessionUpdate\":\"user_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"do something\"}}}\n",
                "{\"sessionId\":\"acp-1\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"done\"}}}\n",
            ),
        )
        .unwrap();

        // Register source session in chat state
        {
            let mut chat = state.chat.lock().await;
            let summary = protocol::ChatSessionSummary {
                session_id: source_session_id.to_string(),
                agent_type: "opencode".to_string(),
                status: protocol::ChatSessionStatus::Ready,
                agent_status: protocol::AgentStatus::Idle,
                worker_count: 0,
                title: None,
                model_id: None,
                created_at: Utc::now().to_rfc3339(),
                display_name: Some("Test Session".to_string()),
                parent_session_id: None,
                pending_blocked_requests: Vec::new(),
            };
            let mut runtime =
                ChatSessionRuntime::new(summary, thread_id.to_string(), source_history.clone());
            runtime.display_name = Some("Test Session".to_string());
            runtime.agent_command = Some("opencode acp".to_string());
            runtime.acp_session_id = Some("acp-1".to_string());
            runtime.started_at = Some(Utc::now());
            chat.sessions.insert(source_session_id.to_string(), runtime);
            chat.sessions_by_thread
                .entry(thread_id.to_string())
                .or_default()
                .push(source_session_id.to_string());
        }

        // Fork from cursor=3 (second user prompt) and exclude the selected prompt.
        let result = ChatService::fork(
            Arc::clone(&state),
            protocol::ChatForkParams {
                thread_id: thread_id.to_string(),
                target_thread_id: None,
                source_session_id: source_session_id.to_string(),
                message_cursor: 3,
            },
        )
        .await
        .expect("fork should succeed");

        // Verify result
        assert_eq!(result.agent_type, "opencode");
        assert!(result.display_name.as_ref().unwrap().contains("fork #1"));
        assert!(!result.session_id.is_empty());

        // Verify forked JSONL has exactly 2 lines and excludes the selected line.
        let fork_history = history_path_for_session(&history_root, thread_id, &result.session_id);
        assert!(fork_history.exists(), "fork history file should exist");
        let fork_history_contents = fs::read_to_string(&fork_history).unwrap();
        let fork_line_count = fork_history_contents
            .lines()
            .filter(|l| !l.is_empty())
            .count();
        assert_eq!(
            fork_line_count, 2,
            "fork should have exactly 2 lines before selected cursor=3"
        );
        assert!(!fork_history_contents.contains("do something"));

        // Verify session registered in state
        {
            let chat = state.chat.lock().await;
            let fork_session = chat.sessions.get(&result.session_id).unwrap();
            assert_eq!(
                fork_session.summary.status,
                protocol::ChatSessionStatus::Ended
            );
            assert_eq!(
                fork_session.parent_session_id.as_deref(),
                Some(source_session_id)
            );
            assert!(
                fork_session.conversation_context.is_some(),
                "fork should have conversation_context built"
            );
        }

        // Verify cursor validation — out of range
        let err = ChatService::fork(
            Arc::clone(&state),
            protocol::ChatForkParams {
                thread_id: thread_id.to_string(),
                target_thread_id: None,
                source_session_id: source_session_id.to_string(),
                message_cursor: 99,
            },
        )
        .await;
        assert!(err.is_err(), "cursor beyond line count should fail");
        assert!(err.unwrap_err().contains("exceeds"));

        // Fork again to verify incrementing fork count
        let result2 = ChatService::fork(
            Arc::clone(&state),
            protocol::ChatForkParams {
                thread_id: thread_id.to_string(),
                target_thread_id: None,
                source_session_id: source_session_id.to_string(),
                message_cursor: 1,
            },
        )
        .await
        .expect("second fork should succeed");
        assert!(result2.display_name.as_ref().unwrap().contains("fork #2"));

        let target_thread_id = "thread-2";
        let result3 = ChatService::fork(
            Arc::clone(&state),
            protocol::ChatForkParams {
                thread_id: thread_id.to_string(),
                target_thread_id: Some(target_thread_id.to_string()),
                source_session_id: source_session_id.to_string(),
                message_cursor: 3,
            },
        )
        .await
        .expect("cross-thread fork should succeed");

        let target_fork_history =
            history_path_for_session(&history_root, target_thread_id, &result3.session_id);
        assert!(
            target_fork_history.exists(),
            "cross-thread fork history should be written under target thread"
        );
        {
            let chat = state.chat.lock().await;
            let target_fork_session = chat.sessions.get(&result3.session_id).unwrap();
            assert_eq!(target_fork_session.thread_id, target_thread_id);
            assert!(chat
                .sessions_by_thread
                .get(target_thread_id)
                .unwrap()
                .contains(&result3.session_id));
        }
    }
}
