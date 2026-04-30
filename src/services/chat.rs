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

#[derive(Default)]
pub struct ChatState {
    sessions: HashMap<String, ChatSessionRuntime>,
    sessions_by_thread: HashMap<String, Vec<String>>,
    channel_to_session: HashMap<u16, String>,
    channel_outbound: HashMap<u16, mpsc::UnboundedSender<Message>>,
    history_root: PathBuf,
}

impl ChatState {
    pub fn new(history_root: PathBuf) -> Self {
        Self {
            sessions: HashMap::new(),
            sessions_by_thread: HashMap::new(),
            channel_to_session: HashMap::new(),
            channel_outbound: HashMap::new(),
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
            };
            let mut runtime =
                ChatSessionRuntime::new(summary, params.thread_id.clone(), history_path);
            runtime.display_name = params.display_name.clone();
            runtime.parent_session_id = params.parent_session_id.clone();
            runtime.agent_command = Some(command.clone());
            runtime.system_prompt = params.system_prompt.clone();
            runtime.initial_prompt = params.initial_prompt.clone();
            runtime.started_at = Some(Utc::now());
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
        Ok(runtime.summary.clone())
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

            let (acp_sid, modes, models, config_options) = {
                let session = chat.sessions.get(&params.session_id);
                (
                    session
                        .and_then(|s| s.acp_session_id.clone())
                        .unwrap_or_default(),
                    session.and_then(|s| s.modes.clone()),
                    session.and_then(|s| s.models.clone()),
                    session.and_then(|s| s.config_options.clone()),
                )
            };

            if let Some(session) = chat.sessions.get_mut(&params.session_id) {
                session.attached_channels.insert(channel_id);
            }
            chat.channel_to_session
                .insert(channel_id, params.session_id.clone());
            chat.channel_outbound
                .insert(channel_id, outbound_tx.clone());
            conn.by_chat_channel
                .insert(channel_id, params.session_id.clone());

            return Ok(protocol::ChatAttachResult {
                channel_id,
                acp_session_id: acp_sid,
                modes,
                models,
                config_options,
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
            )
        };

        emit_status_transitions(&state, transitions).await;

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
        outbound_payload_override,
    ) = {
        let mut chat = state.chat.lock().await;
        let Some(session) = chat.sessions.get_mut(session_id) else {
            return;
        };

        let raw_messages = extract_json_messages(&mut session.output_buffer, payload, "output");
        let (messages, outbound_payload_override) =
            filter_duplicate_user_echoes(session, raw_messages);

        let updates = collect_session_update_params(&messages);
        let outbound = apply_outbound_status_updates(&state, session, messages);
        let thread_id = session.thread_id.clone();
        let history_path = session.history_path.clone();
        let attached_channels = session
            .attached_channels
            .iter()
            .copied()
            .collect::<Vec<_>>();

        let targets = attached_channels
            .iter()
            .filter_map(|channel_id| {
                chat.channel_outbound
                    .get(channel_id)
                    .cloned()
                    .map(|outbound| (*channel_id, outbound))
            })
            .collect::<Vec<_>>();

        (
            targets,
            history_path,
            updates,
            outbound.transitions,
            outbound.injection_completed,
            thread_id,
            outbound_payload_override,
        )
    };

    emit_status_transitions(&state, transitions).await;

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

    // Rewrite ACP session ID to Spindle session ID in outbound payload
    let base_payload = outbound_payload_override.as_deref().unwrap_or(payload);
    if base_payload.is_empty() {
        return;
    }
    let rewritten_payload = {
        let chat = state.chat.lock().await;
        if let Some(session) = chat.sessions.get(session_id) {
            if let Some(ref acp_id) = session.acp_session_id {
                rewrite_session_id(base_payload, acp_id, session_id)
            } else {
                None
            }
        } else {
            None
        }
    };
    let out_payload = rewritten_payload.as_deref().unwrap_or(base_payload);

    if let Ok(s) = std::str::from_utf8(out_payload) {
        warn!(
            session_id,
            "chat_outbound_frame payload_len={} snippet={:?}",
            out_payload.len(),
            &s[..s.floor_char_boundary(s.len().min(200))]
        );
    }

    let mut dead_channels = Vec::new();
    for (channel_id, outbound) in targets {
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
    match update_kind_from_message(message) {
        Some("agent_message_chunk" | "agent_thought_chunk" | "tool_call") => {
            session.pending_user_echoes.clear();
        }
        _ => {}
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
        .map(|session| session.summary.clone())
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
                .map(|session| session.summary.clone())
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

    let command = project_agent_command(&project_path, agent_name)
        .or_else(|| agent_registry::agent_command(agent_name))
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
        let mut chat = state.chat.lock().await;
        chat.channel_to_session
            .insert(channel_id, session_id.clone());
        chat.sessions.insert(session_id, session);
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
