use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{mpsc, oneshot, Mutex, Notify},
    task::JoinHandle,
    time::{timeout, Duration},
};
use tokio_tungstenite::tungstenite::Message;
use tracing::warn;
use uuid::Uuid;

use crate::{
    protocol,
    services::{
        agent_registry,
        checkpoint::CheckpointService,
        project::{load_project_default_chat_model, project_agent_command},
        terminal::TerminalConnectionState,
    },
    state_store, AppState,
};

const CHAT_INPUT_CHANNEL_CAPACITY: usize = 256;
const CHAT_IO_CHUNK_SIZE: usize = 8192;
const CHAT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const CHAT_ATTACH_WAIT_TIMEOUT: Duration = Duration::from_secs(35);
const CHAT_HISTORY_BATCH_SIZE: usize = 100;
const CHAT_UPDATE_METHOD: &str = "session/update";
const CHAT_PROMPT_METHOD: &str = "session/prompt";
const CHAT_CANCEL_METHOD: &str = "session/cancel";
const CHAT_STALL_TIMEOUT: Duration = Duration::from_secs(60);

pub struct ChatService;

#[derive(Default)]
pub struct ChatState {
    sessions: HashMap<String, ChatSessionRuntime>,
    sessions_by_thread: HashMap<String, Vec<String>>,
    channel_to_session: HashMap<u16, String>,
    channel_outbound: HashMap<u16, mpsc::UnboundedSender<Message>>,
    history_root: PathBuf,
}

struct ChatSessionRuntime {
    summary: protocol::ChatSessionSummary,
    thread_id: String,
    display_name: Option<String>,
    parent_session_id: Option<String>,
    system_prompt: Option<String>,
    initial_prompt: Option<String>,
    conversation_context: Option<String>,
    acp_session_id: Option<String>,
    attached_channels: HashSet<u16>,
    input_tx: Option<mpsc::Sender<Vec<u8>>>,
    stop_tx: Option<oneshot::Sender<()>>,
    status_notify: Arc<Notify>,
    ended_emitted: bool,
    history_path: PathBuf,
    input_buffer: Vec<u8>,
    output_buffer: Vec<u8>,
    active_tools: HashSet<String>,
    total_tool_count: usize,
    latest_tool_name: Option<String>,
    latest_tool_title: Option<String>,
    started_at: Option<chrono::DateTime<Utc>>,
    pending_prompt_ids: HashSet<String>,
    checkpoint_seq: u64,
    last_update_time: Option<chrono::DateTime<Utc>>,
    stall_generation: u64,
    stall_task: Option<JoinHandle<()>>,
    modes: Option<Value>,
    models: Option<Value>,
    config_options: Option<Value>,
    /// Tracks the injection prompt request ID so we can detect when the injection turn completes.
    /// Set when injection is sent, cleared when the response arrives in apply_outbound_status_updates.
    injection_prompt_id: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedChatSessionMetadata {
    acp_session_id: Option<String>,
}

impl ChatService {
    pub async fn start(
        state: Arc<AppState>,
        params: protocol::ChatStartParams,
    ) -> Result<protocol::ChatStartResult, String> {
        tracing::info!(thread_id = %params.thread_id, agent = %params.agent_name, "chat_start");
        let (project_path, command, cwd, preferred_model) =
            resolve_agent_launch(&state, &params.thread_id, &params.agent_name).await?;

        let session_id = Uuid::new_v4().to_string();
        let created_at = Utc::now().to_rfc3339();
        {
            let mut chat = state.chat.lock().await;
            let history_path =
                history_path_for_session(&chat.history_root, &params.thread_id, &session_id);
            let runtime = ChatSessionRuntime {
                summary: protocol::ChatSessionSummary {
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
                },
                thread_id: params.thread_id.clone(),
                display_name: params.display_name.clone(),
                parent_session_id: params.parent_session_id.clone(),
                system_prompt: params.system_prompt.clone(),
                initial_prompt: params.initial_prompt.clone(),
                conversation_context: None,
                acp_session_id: None,
                attached_channels: HashSet::new(),
                input_tx: None,
                stop_tx: None,
                status_notify: Arc::new(Notify::new()),
                ended_emitted: false,
                history_path,
                input_buffer: Vec::new(),
                output_buffer: Vec::new(),
                active_tools: HashSet::new(),
                total_tool_count: 0,
                latest_tool_name: None,
                latest_tool_title: None,
                started_at: Some(Utc::now()),
                pending_prompt_ids: HashSet::new(),
                checkpoint_seq: 0,
                last_update_time: None,
                stall_generation: 0,
                stall_task: None,
                modes: None,
                models: None,
                config_options: None,
                injection_prompt_id: None,
            };
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
            params.thread_id,
            session_id.clone(),
            params.agent_name,
            project_path,
            command,
            cwd,
            None,
            preferred_model.clone(),
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
        let force_new_session =
            restored_session_marker_exists(&history_root, &params.thread_id, &params.session_id)?;

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

        let (project_path, command, cwd, preferred_model) =
            resolve_agent_launch(&state, &params.thread_id, &effective_agent).await?;
        spawn_session_task(
            Arc::clone(&state),
            params.thread_id,
            params.session_id.clone(),
            agent_type,
            project_path,
            command,
            cwd,
            load_session_id,
            preferred_model.clone(),
        );

        Ok(protocol::ChatLoadResult {
            session_id: params.session_id,
            status: protocol::ChatSessionStatus::Starting,
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
            let (
                transitions,
                replacement_payload,
                prompt_updates,
                auto_checkpoints,
                consumed_conversation_context,
            ) = apply_inbound_status_updates(&state, session, messages);
            (
                session.thread_id.clone(),
                session_id,
                input_tx,
                transitions,
                replacement_payload,
                prompt_updates,
                auto_checkpoints,
                history_path,
                consumed_conversation_context,
            )
        };

        emit_status_transitions(&state, transitions).await;

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
            if let Err(error) = append_updates_to_history(&history_path, &prompt_updates) {
                warn!(channel_id, error = %error, "failed to persist user prompt to chat history");
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

fn spawn_session_task(
    state: Arc<AppState>,
    thread_id: String,
    session_id: String,
    agent_type: String,
    project_path: String,
    command: String,
    cwd: String,
    load_session_id: Option<String>,
    preferred_model: Option<String>,
) {
    let fail_state = Arc::clone(&state);
    let fail_thread_id = thread_id.clone();
    let fail_session_id = session_id.clone();

    // Build env vars for the agent process
    let env_vars = {
        let state_ref = state.clone();
        let tid = thread_id.clone();
        let sid = session_id.clone();
        tokio::spawn(async move { build_agent_env_vars(&state_ref, &tid, &sid).await })
    };

    let handle = tokio::spawn(async move {
        let env_vars = match env_vars.await {
            Ok(vars) => vars,
            Err(_) => Vec::new(),
        };
        if let Err(error) = run_session_task(
            Arc::clone(&state),
            thread_id,
            session_id,
            agent_type,
            project_path,
            command,
            cwd,
            load_session_id,
            preferred_model,
            env_vars,
        )
        .await
        {
            warn!(error = %error, "chat session task failed");
        }
    });

    // Monitor the spawned task — if it panics, emit a session_failed event
    tokio::spawn(async move {
        if let Err(join_error) = handle.await {
            if join_error.is_panic() {
                let panic_msg = match join_error.into_panic().downcast::<String>() {
                    Ok(msg) => *msg,
                    Err(payload) => match payload.downcast::<&str>() {
                        Ok(s) => s.to_string(),
                        Err(_) => "task panicked (unknown payload)".to_string(),
                    },
                };
                tracing::error!(
                    thread_id = %fail_thread_id,
                    session_id = %fail_session_id,
                    panic = %panic_msg,
                    "chat session task panicked"
                );
                mark_session_failed(
                    fail_state,
                    &fail_thread_id,
                    &fail_session_id,
                    format!("internal error: session task panicked: {panic_msg}"),
                )
                .await;
            }
        }
    });
}

async fn run_session_task(
    state: Arc<AppState>,
    thread_id: String,
    session_id: String,
    _agent_type: String,
    _project_path: String,
    command: String,
    cwd: String,
    load_session_id: Option<String>,
    preferred_model: Option<String>,
    env_vars: Vec<(String, String)>,
) -> Result<(), String> {
    let mut cmd = Command::new("bash");
    cmd.args(["-lc", &command])
        .current_dir(&cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    for (key, value) in &env_vars {
        cmd.env(key, value);
    }
    let mut child = cmd
        .spawn()
        .map_err(|err| format!("failed to spawn chat agent process: {err}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to capture chat agent stdout".to_string())?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to capture chat agent stdin".to_string())?;

    let (input_tx, mut input_rx) = mpsc::channel::<Vec<u8>>(CHAT_INPUT_CHANNEL_CAPACITY);
    let input_task = tokio::spawn(async move {
        let mut writer = stdin;
        while let Some(mut batch) = input_rx.recv().await {
            while let Ok(more) = input_rx.try_recv() {
                batch.extend_from_slice(&more);
            }

            if let Err(err) = writer.write_all(&batch).await {
                warn!(error = %err, "failed to write chat agent stdin");
                break;
            }

            if let Err(err) = writer.flush().await {
                warn!(error = %err, "failed to flush chat agent stdin");
                break;
            }
        }
    });

    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    {
        let mut chat = state.chat.lock().await;
        let Some(session) = chat.sessions.get_mut(&session_id) else {
            let _ = child.kill().await;
            input_task.abort();
            return Ok(());
        };
        session.input_tx = Some(input_tx.clone());
        session.stop_tx = Some(stop_tx);
    }

    let mut stdout = stdout;
    let is_new_session = load_session_id.is_none();
    let handshake = timeout(
        CHAT_HANDSHAKE_TIMEOUT,
        perform_handshake(
            &input_tx,
            &mut stdout,
            load_session_id,
            &cwd,
            preferred_model.as_deref(),
        ),
    )
    .await
    .map_err(|_| "chat handshake timed out after 30s".to_string())?;

    let handshake = match handshake {
        Ok(result) => result,
        Err(error) => {
            mark_session_failed(Arc::clone(&state), &thread_id, &session_id, error).await;
            let _ = child.kill().await;
            input_task.abort();
            return Ok(());
        }
    };

    // Persist replay notifications from session/load to JSONL so that
    // chat.history returns the canonical conversation history from the agent.
    // Done BEFORE mark_session_ready so the JSONL is ready when the Mac
    // calls chat.history after attaching.
    if !handshake.replay_notifications.is_empty() {
        // Skip overwrite_history — the JSONL already contains the full history
        // from live fanout_output. The agent's session/load replay is often
        // incomplete (only the latest turn), so overwriting would destroy data.
        // New updates after load will append normally via fanout_output.
    }

    mark_session_ready(Arc::clone(&state), &thread_id, &session_id, &handshake).await;

    // Post-handshake context injection: platform prompt + system_prompt + initial_prompt.
    // Only inject on session/new — session/load already has the context from the
    // prior conversation. Re-injecting would show the injection text again.
    //
    // Project AGENTS.md is NOT injected here — each agent binary handles its own
    // project context discovery (OpenCode reads AGENTS.md, Claude reads CLAUDE.md, etc.).
    // We only inject platform-level awareness (Threadmill env vars, threadmill-cli,
    // worker orchestration) which no agent can discover on its own.
    if is_new_session {
        let (system_prompt, initial_prompt) = {
            let chat = state.chat.lock().await;
            let session = chat.sessions.get(&session_id);
            let sp = session.and_then(|s| s.system_prompt.clone());
            let ip = session.and_then(|s| s.initial_prompt.clone());
            (sp, ip)
        };

        // Read platform system prompt from ~/.config/threadmill/system-prompt.md
        let platform_prompt = {
            let config_dir = dirs::config_dir().unwrap_or_else(std::path::PathBuf::new);
            let platform_path = config_dir.join("threadmill").join("system-prompt.md");
            match std::fs::read_to_string(&platform_path) {
                Ok(content) => {
                    tracing::info!(session_id = %session_id, "injecting platform system-prompt.md ({} bytes)", content.len());
                    Some(content)
                }
                Err(_) => {
                    tracing::debug!(session_id = %session_id, "no system-prompt.md found, skipping platform prompt");
                    None
                }
            }
        };

        let combined = build_injection_prompt(
            platform_prompt.as_deref(),
            system_prompt.as_deref(),
            initial_prompt.as_deref(),
        );
        if let Some(prompt_text) = combined {
            tracing::info!(session_id = %session_id, len = prompt_text.len(), "sending injection prompt");
            let injection_id: u64 = 100;
            if let Err(err) = send_acp_request(
                &input_tx,
                injection_id,
                "session/prompt",
                json!({
                    "sessionId": handshake.acp_session_id,
                    "prompt": [{"type": "text", "text": prompt_text}]
                }),
            )
            .await
            {
                tracing::warn!(session_id = %session_id, error = %err, "failed to send injection prompt");
            } else {
                // Track injection id so the outbound status machinery fires Busy→Idle
                // when the agent responds, giving the Mac a clear "injection complete" signal.
                let transitions = {
                    let mut chat = state.chat.lock().await;
                    let mut transitions = Vec::new();
                    if let Some(session) = chat.sessions.get_mut(&session_id) {
                        let id_str = injection_id.to_string();
                        session.pending_prompt_ids.insert(id_str.clone());
                        session.injection_prompt_id = Some(id_str);
                        apply_status_transition(
                            session,
                            protocol::AgentStatus::Busy,
                            &mut transitions,
                        );
                    }
                    transitions
                };
                emit_status_transitions(&state, transitions).await;
            }
            // Don't wait for response — let it stream in the I/O loop.
            // The response will be detected by apply_outbound_status_updates which
            // fires Busy→Idle and emits chat.injection_complete.
        } else {
            // No injection content — signal immediately so the Mac doesn't wait
            state.emit_event(
                "chat.injection_complete",
                json!({
                    "thread_id": thread_id,
                    "session_id": session_id,
                }),
            );
        }
    } else {
        tracing::debug!(session_id = %session_id, "session/load — skipping context injection (already in conversation history)");
        // Loaded sessions don't inject — signal immediately
        state.emit_event(
            "chat.injection_complete",
            json!({
                "thread_id": thread_id,
                "session_id": session_id,
            }),
        );
    }

    let mut buf = [0_u8; CHAT_IO_CHUNK_SIZE];
    let mut explicit_stop = false;

    let final_status = loop {
        tokio::select! {
            _ = &mut stop_rx => {
                explicit_stop = true;
                let _ = child.kill().await;
                break child.wait().await;
            }
            read_result = stdout.read(&mut buf) => {
                match read_result {
                    Ok(0) => {
                        break child.wait().await;
                    }
                    Ok(read_len) => {
                        fanout_output(Arc::clone(&state), &session_id, &buf[..read_len]).await;
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(err) => {
                        warn!(error = %err, "failed to read chat agent stdout");
                        break child.wait().await;
                    }
                }
            }
        }
    };

    input_task.abort();
    let _ = input_task.await;

    let reason = match final_status {
        Ok(status) if explicit_stop => "stopped".to_string(),
        Ok(status) if status.success() => "exited".to_string(),
        Ok(status) => format!(
            "crashed{}",
            status
                .code()
                .map(|code| format!(" (code {code})"))
                .unwrap_or_default()
        ),
        Err(err) => format!("crashed ({err})"),
    };

    mark_session_ended(Arc::clone(&state), &thread_id, &session_id, &reason, false).await;
    Ok(())
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

async fn send_acp_request(
    input_tx: &mpsc::Sender<Vec<u8>>,
    id: u64,
    method: &str,
    params: Value,
) -> Result<(), String> {
    let payload = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    }))
    .map_err(|err| format!("failed to encode ACP request {method}: {err}"))?;

    let mut frame = payload;
    frame.push(b'\n');
    input_tx
        .send(frame)
        .await
        .map_err(|err| format!("failed to queue ACP request {method}: {err}"))
}

async fn wait_for_acp_response(
    stdout: &mut tokio::process::ChildStdout,
    buffer: &mut Vec<u8>,
    response_id: u64,
    collected: &mut Vec<Value>,
) -> Result<Value, String> {
    loop {
        let Some(line) = next_json_line(stdout, buffer).await? else {
            return Err("agent closed stdout during handshake".to_string());
        };
        let id_matches = line
            .get("id")
            .and_then(Value::as_u64)
            .map(|id| id == response_id)
            .unwrap_or(false)
            || line
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| id.parse::<u64>().ok())
                .map(|id| id == response_id)
                .unwrap_or(false);

        if id_matches {
            return Ok(line);
        }
        collected.push(line);
    }
}

fn ensure_acp_success<'a>(response: &'a Value, method: &str) -> Result<&'a Value, String> {
    if let Some(error) = response.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown ACP error");
        return Err(format!("ACP {method} failed: {message}"));
    }

    response
        .get("result")
        .ok_or_else(|| format!("ACP {method} response missing result"))
}

async fn next_json_line(
    stdout: &mut tokio::process::ChildStdout,
    buffer: &mut Vec<u8>,
) -> Result<Option<Value>, String> {
    loop {
        if let Some(newline_idx) = buffer.iter().position(|byte| *byte == b'\n') {
            let line = buffer[..newline_idx].to_vec();
            buffer.drain(..=newline_idx);
            if line.is_empty() {
                continue;
            }

            let parsed = serde_json::from_slice::<Value>(&line)
                .map_err(|err| format!("failed to parse ACP line: {err}"))?;
            return Ok(Some(parsed));
        }

        let mut chunk = [0_u8; CHAT_IO_CHUNK_SIZE];
        let read_len = stdout
            .read(&mut chunk)
            .await
            .map_err(|err| format!("failed to read ACP stdout: {err}"))?;
        if read_len == 0 {
            return Ok(None);
        }

        buffer.extend_from_slice(&chunk[..read_len]);
    }
}

#[derive(Debug)]
struct StatusTransition {
    thread_id: String,
    session_id: String,
    old_status: protocol::AgentStatus,
    new_status: protocol::AgentStatus,
    worker_count: usize,
}

struct AutoCheckpointRequest {
    thread_id: String,
    session_id: String,
    message: String,
    prompt_preview: Option<String>,
    /// Pre-computed history cursor captured BEFORE the user echo is appended
    /// to the JSONL. Without this, the auto-checkpoint reads the line count
    /// after the echo is already written, so checkpoint.restore truncates to
    /// a point that includes the user echo but not the agent response.
    history_cursor: Option<u64>,
}

async fn emit_status_transitions(state: &Arc<AppState>, transitions: Vec<StatusTransition>) {
    for transition in transitions {
        state.emit_chat_status_changed(protocol::ChatStatusChangedEvent {
            thread_id: transition.thread_id.clone(),
            session_id: transition.session_id.clone(),
            old_status: transition.old_status,
            new_status: transition.new_status,
            worker_count: transition.worker_count,
        });
        emit_state_delta_updated(state, &transition.thread_id, &transition.session_id).await;
    }
}

fn apply_status_transition(
    session: &mut ChatSessionRuntime,
    new_status: protocol::AgentStatus,
    transitions: &mut Vec<StatusTransition>,
) {
    let old_status = session.summary.agent_status.clone();
    if old_status == new_status {
        return;
    }

    session.summary.agent_status = new_status.clone();
    transitions.push(StatusTransition {
        thread_id: session.thread_id.clone(),
        session_id: session.summary.session_id.clone(),
        old_status,
        new_status,
        worker_count: session.summary.worker_count,
    });
}

fn reset_status_tracking(
    session: &mut ChatSessionRuntime,
    transitions: &mut Vec<StatusTransition>,
) {
    session.active_tools.clear();
    session.pending_prompt_ids.clear();
    session.summary.worker_count = 0;
    cancel_stall_timer(session);
    apply_status_transition(session, protocol::AgentStatus::Idle, transitions);
}

fn cancel_stall_timer(session: &mut ChatSessionRuntime) {
    session.stall_generation = session.stall_generation.wrapping_add(1);
    session.last_update_time = None;
    if let Some(task) = session.stall_task.take() {
        task.abort();
    }
}

fn restart_stall_timer(state: &Arc<AppState>, session: &mut ChatSessionRuntime) {
    session.stall_generation = session.stall_generation.wrapping_add(1);
    session.last_update_time = Some(Utc::now());
    if let Some(task) = session.stall_task.take() {
        task.abort();
    }

    let generation = session.stall_generation;
    let session_id = session.summary.session_id.clone();
    let state = Arc::clone(state);
    session.stall_task = Some(tokio::spawn(async move {
        tokio::time::sleep(CHAT_STALL_TIMEOUT).await;
        handle_stall_timeout(state, session_id, generation).await;
    }));
}

async fn handle_stall_timeout(state: Arc<AppState>, session_id: String, generation: u64) {
    let transitions = {
        let mut transitions = Vec::new();
        let mut chat = state.chat.lock().await;
        let Some(session) = chat.sessions.get_mut(&session_id) else {
            return;
        };

        if session.stall_generation != generation
            || session.summary.agent_status != protocol::AgentStatus::Busy
        {
            return;
        }

        session.stall_task = None;
        apply_status_transition(session, protocol::AgentStatus::Stalled, &mut transitions);
        transitions
    };

    if transitions.is_empty() {
        return;
    }

    emit_status_transitions(&state, transitions).await;
}

fn apply_inbound_status_updates(
    state: &Arc<AppState>,
    session: &mut ChatSessionRuntime,
    messages: Vec<Value>,
) -> (
    Vec<StatusTransition>,
    Option<Vec<u8>>,
    Vec<Value>,
    Vec<AutoCheckpointRequest>,
    bool,
) {
    let mut transitions = Vec::new();
    let mut messages = messages;
    let mut replacement_payload = None;
    let mut payload_rewritten = false;
    let mut history_updates = Vec::new();
    let mut auto_checkpoints = Vec::new();
    let mut consumed_conversation_context = false;

    for message in &mut messages {
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();

        if method == CHAT_PROMPT_METHOD {
            let params = message.get("params");
            let session_id_value = params
                .and_then(|params| params.get("sessionId"))
                .cloned()
                .unwrap_or_else(|| Value::String(String::new()));
            let prompt_preview = prompt_preview_from_params(params);
            let prompt_updates = user_prompt_history_updates(session_id_value.clone(), params);

            if let Some(id) = request_id_key(message.get("id")) {
                session.pending_prompt_ids.insert(id);
            }

            if let Some(conversation_context) = session.conversation_context.as_deref() {
                let context_block = format!("{conversation_context}\n\n---\n\n");
                if prepend_prompt_text_block(message, &context_block) {
                    payload_rewritten = true;
                    consumed_conversation_context = true;
                }
            }

            history_updates.extend(prompt_updates);

            session.checkpoint_seq += 1;
            auto_checkpoints.push(AutoCheckpointRequest {
                thread_id: session.thread_id.clone(),
                session_id: session.summary.session_id.clone(),
                message: format!("Auto-checkpoint before prompt {}", session.checkpoint_seq),
                prompt_preview,
                history_cursor: None, // filled in by handle_inbound_data before JSONL write
            });
            session.active_tools.clear();
            session.summary.worker_count = 0;
            apply_status_transition(session, protocol::AgentStatus::Busy, &mut transitions);
            restart_stall_timer(state, session);
            continue;
        }

        if method == CHAT_CANCEL_METHOD {
            reset_status_tracking(session, &mut transitions);
        }
    }

    if payload_rewritten {
        replacement_payload = serialize_json_messages(&messages);
    }

    (
        transitions,
        replacement_payload,
        history_updates,
        auto_checkpoints,
        consumed_conversation_context,
    )
}

fn spawn_auto_checkpoint(state: Arc<AppState>, request: AutoCheckpointRequest) {
    tokio::spawn(async move {
        if let Err(error) = CheckpointService::save_with_cursor(
            state,
            protocol::CheckpointSaveParams {
                thread_id: request.thread_id,
                session_id: Some(request.session_id),
                message: Some(request.message),
                prompt_preview: request.prompt_preview,
            },
            request.history_cursor,
        )
        .await
        {
            warn!(error = %error, "failed to save auto-checkpoint");
        }
    });
}

fn prompt_preview_from_params(params: Option<&Value>) -> Option<String> {
    let prompt = params
        .and_then(|params| params.get("prompt"))
        .and_then(Value::as_array)?;
    let preview = prompt
        .iter()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    if preview.is_empty() {
        return None;
    }

    let mut preview = preview;
    if preview.len() > 160 {
        preview.truncate(preview.floor_char_boundary(160));
        preview.push('…');
    }
    Some(preview)
}

fn user_prompt_history_updates(session_id_value: Value, params: Option<&Value>) -> Vec<Value> {
    params
        .and_then(|params| params.get("prompt"))
        .and_then(Value::as_array)
        .map(|prompt| {
            prompt
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .map(|text| user_prompt_history_update(session_id_value.clone(), text))
                .collect()
        })
        .unwrap_or_default()
}

fn prepend_prompt_text_block(message: &mut Value, prompt_text: &str) -> bool {
    let Some(message_object) = message.as_object_mut() else {
        return false;
    };
    let params = message_object
        .entry("params".to_string())
        .or_insert_with(|| json!({}));
    let Some(params_object) = params.as_object_mut() else {
        return false;
    };
    let prompt = params_object
        .entry("prompt".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));

    if let Some(prompt_array) = prompt.as_array_mut() {
        prompt_array.insert(0, json!({ "type": "text", "text": prompt_text }));
    } else {
        *prompt = json!([{ "type": "text", "text": prompt_text }]);
    }
    true
}

fn serialize_json_messages(messages: &[Value]) -> Option<Vec<u8>> {
    let mut payload = Vec::new();
    for message in messages {
        serde_json::to_writer(&mut payload, message).ok()?;
        payload.push(b'\n');
    }
    Some(payload)
}

fn user_prompt_history_update(session_id_value: Value, text: &str) -> Value {
    json!({
        "sessionId": session_id_value,
        "update": {
            "sessionUpdate": "user_message_chunk",
            "content": { "type": "text", "text": text }
        }
    })
}

/// Returns (status_transitions, injection_just_completed).
fn apply_outbound_status_updates(
    state: &Arc<AppState>,
    session: &mut ChatSessionRuntime,
    messages: Vec<Value>,
) -> (Vec<StatusTransition>, bool) {
    let mut transitions = Vec::new();
    let mut injection_completed = false;

    for message in messages {
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            if method == CHAT_UPDATE_METHOD {
                apply_worker_update(session, message.get("params"));
                if session.summary.agent_status == protocol::AgentStatus::Busy {
                    restart_stall_timer(state, session);
                } else if session.summary.agent_status == protocol::AgentStatus::Stalled {
                    apply_status_transition(session, protocol::AgentStatus::Busy, &mut transitions);
                    restart_stall_timer(state, session);
                }
            }
        }

        if !(message.get("result").is_some() || message.get("error").is_some()) {
            continue;
        }

        let Some(id) = request_id_key(message.get("id")) else {
            continue;
        };
        if !session.pending_prompt_ids.remove(&id) {
            continue;
        }

        // Detect injection turn completion
        if session.injection_prompt_id.as_deref() == Some(&id) {
            session.injection_prompt_id = None;
            injection_completed = true;
        }

        reset_status_tracking(session, &mut transitions);
    }

    (transitions, injection_completed)
}

fn apply_worker_update(session: &mut ChatSessionRuntime, params: Option<&Value>) {
    let Some(update) = params.and_then(|params| params.get("update")) else {
        return;
    };

    let kind = update
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let status = extract_tool_status(update).unwrap_or_default();
    let tool_call_id = extract_tool_call_id(update);

    if kind == "tool_call" {
        if matches!(status, "pending" | "in_progress") {
            if let Some(tool_call_id) = tool_call_id {
                session.active_tools.insert(tool_call_id);
            }
            session.total_tool_count += 1;

            // Extract tool name and title for worker update relay
            let tool_name = update
                .get("toolCall")
                .and_then(|tc| tc.get("name"))
                .and_then(Value::as_str)
                .or_else(|| update.get("name").and_then(Value::as_str))
                .map(ToOwned::to_owned);
            let tool_title = update
                .get("toolCall")
                .and_then(|tc| tc.get("state"))
                .and_then(|s| s.get("title"))
                .and_then(Value::as_str)
                .or_else(|| update.get("title").and_then(Value::as_str))
                .map(ToOwned::to_owned);
            if let Some(name) = tool_name {
                session.latest_tool_name = Some(name);
            }
            if let Some(title) = tool_title {
                session.latest_tool_title = Some(title);
            }
        }
    } else if kind == "tool_call_update" {
        // Update title if available (tools stream their title as they progress)
        let tool_title = update
            .get("toolCall")
            .and_then(|tc| tc.get("state"))
            .and_then(|s| s.get("title"))
            .and_then(Value::as_str)
            .or_else(|| update.get("title").and_then(Value::as_str))
            .map(ToOwned::to_owned);
        if let Some(title) = tool_title {
            session.latest_tool_title = Some(title);
        }

        if matches!(status, "completed" | "cancelled" | "error") {
            if let Some(tool_call_id) = tool_call_id {
                session.active_tools.remove(&tool_call_id);
            }
        }
    }

    session.summary.worker_count = session.active_tools.len();
}

fn extract_tool_call_id(update: &Value) -> Option<String> {
    update
        .get("toolCallId")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            update
                .get("tool_call_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .or_else(|| {
            update
                .get("toolCall")
                .and_then(|tool_call| tool_call.get("id"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
}

fn extract_tool_status(update: &Value) -> Option<&str> {
    update.get("status").and_then(Value::as_str).or_else(|| {
        update
            .get("toolCall")
            .and_then(|tool_call| tool_call.get("status"))
            .and_then(Value::as_str)
    })
}

fn request_id_key(id_value: Option<&Value>) -> Option<String> {
    let id_value = id_value?;
    id_value
        .as_str()
        .map(ToOwned::to_owned)
        .or_else(|| id_value.as_u64().map(|id| id.to_string()))
        .or_else(|| id_value.as_i64().map(|id| id.to_string()))
}

fn extract_json_messages(buffer: &mut Vec<u8>, payload: &[u8], direction: &str) -> Vec<Value> {
    buffer.extend_from_slice(payload);
    let mut messages = Vec::new();

    while let Some(newline_idx) = buffer.iter().position(|byte| *byte == b"\n"[0]) {
        let line = buffer[..newline_idx].to_vec();
        buffer.drain(..=newline_idx);
        if line.is_empty() {
            continue;
        }

        match serde_json::from_slice::<Value>(&line) {
            Ok(value) => messages.push(value),
            Err(error) => {
                warn!(error = %error, direction, "failed to parse chat JSON frame");
            }
        }
    }

    messages
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

/// Rewrite sessionId in a JSON payload. Spindle session IDs are the external
/// contract; ACP session IDs are internal to the agent. Spindle translates
/// at the relay boundary so clients never see ACP IDs.
fn rewrite_session_id(payload: &[u8], from: &str, to: &str) -> Option<Vec<u8>> {
    if from.is_empty() || to.is_empty() || from == to {
        return None;
    }
    let payload_str = std::str::from_utf8(payload).ok()?;
    if !payload_str.contains(from) {
        return None;
    }
    Some(payload_str.replace(from, to).into_bytes())
}

async fn fanout_output(state: Arc<AppState>, session_id: &str, payload: &[u8]) {
    let (targets, history_path, updates, transitions, injection_completed, thread_id) = {
        let mut chat = state.chat.lock().await;
        let Some(session) = chat.sessions.get_mut(session_id) else {
            return;
        };

        let messages = extract_json_messages(&mut session.output_buffer, payload, "output");
        let updates = collect_session_update_params(&messages);
        let (transitions, injection_completed) =
            apply_outbound_status_updates(&state, session, messages);
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
            transitions,
            injection_completed,
            thread_id,
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
    let rewritten_payload = {
        let chat = state.chat.lock().await;
        if let Some(session) = chat.sessions.get(session_id) {
            if let Some(ref acp_id) = session.acp_session_id {
                rewrite_session_id(payload, acp_id, session_id)
            } else {
                None
            }
        } else {
            None
        }
    };
    let out_payload = rewritten_payload.as_deref().unwrap_or(payload);

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
        if outbound.send(Message::Binary(frame)).is_err() {
            dead_channels.push(channel_id);
        }
    }

    if !dead_channels.is_empty() {
        detach_channels(state, dead_channels).await;
    }
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

async fn mark_session_ready(
    state: Arc<AppState>,
    thread_id: &str,
    session_id: &str,
    handshake: &HandshakeResult,
) {
    tracing::info!(thread_id, session_id, model = ?handshake.model_id, "chat_session_ready");
    let history_root = {
        let chat = state.chat.lock().await;
        chat.history_root.clone()
    };
    match persist_session_metadata(
        &history_root,
        thread_id,
        session_id,
        Some(handshake.acp_session_id.as_str()),
    ) {
        Ok(()) => {}
        Err(error) => {
            warn!(
                thread_id,
                session_id,
                error = %error,
                "failed to persist chat session metadata"
            );
        }
    }
    {
        let mut chat = state.chat.lock().await;
        if let Some(session) = chat.sessions.get_mut(session_id) {
            session.summary.status = protocol::ChatSessionStatus::Ready;
            session.summary.title = handshake.title.clone();
            session.summary.model_id = handshake.model_id.clone();
            session.acp_session_id = Some(handshake.acp_session_id.clone());
            session.modes = handshake.modes.clone();
            session.models = handshake.models.clone();
            session.config_options = handshake.config_options.clone();
            session.status_notify.notify_waiters();
        }
    }

    state.emit_chat_session_ready(protocol::ChatSessionReadyEvent {
        acp_session_id: handshake.acp_session_id.clone(),
        thread_id: thread_id.to_string(),
        session_id: session_id.to_string(),
        modes: handshake.modes.clone(),
        models: handshake.models.clone(),
        config_options: handshake.config_options.clone(),
    });
    emit_state_delta_updated(&state, thread_id, session_id).await;
}

async fn clear_pending_conversation_context(
    state: &Arc<AppState>,
    thread_id: &str,
    session_id: &str,
) -> Result<(), String> {
    let history_root = {
        let chat = state.chat.lock().await;
        chat.history_root_path().to_path_buf()
    };
    clear_restored_session_marker(&history_root, thread_id, session_id)?;

    let mut chat = state.chat.lock().await;
    if let Some(session) = chat.sessions.get_mut(session_id) {
        session.conversation_context = None;
    }

    Ok(())
}

async fn mark_session_failed(
    state: Arc<AppState>,
    thread_id: &str,
    session_id: &str,
    error: String,
) {
    tracing::warn!(thread_id, session_id, %error, "chat_session_failed");
    let transitions = {
        let mut transitions = Vec::new();
        let mut chat = state.chat.lock().await;
        if let Some(session) = chat.sessions.get_mut(session_id) {
            session.summary.status = protocol::ChatSessionStatus::Failed;
            session.input_tx = None;
            session.stop_tx = None;
            reset_status_tracking(session, &mut transitions);
            session.status_notify.notify_waiters();
        }
        transitions
    };

    emit_status_transitions(&state, transitions).await;

    state.emit_chat_session_failed(protocol::ChatSessionFailedEvent {
        thread_id: thread_id.to_string(),
        session_id: session_id.to_string(),
        error,
    });
    emit_state_delta_updated(&state, thread_id, session_id).await;
}
async fn mark_session_ended(
    state: Arc<AppState>,
    thread_id: &str,
    session_id: &str,
    reason: &str,
    purge: bool,
) {
    let (should_emit_ended, transitions) = {
        let mut should_emit_ended = false;
        let mut transitions = Vec::new();
        let mut chat = state.chat.lock().await;
        if let Some(session) = chat.sessions.get_mut(session_id) {
            if !session.ended_emitted {
                session.summary.status = protocol::ChatSessionStatus::Ended;
                session.input_tx = None;
                session.stop_tx = None;
                session.ended_emitted = true;
                should_emit_ended = true;
            }
            reset_status_tracking(session, &mut transitions);
            session.status_notify.notify_waiters();
        }
        (should_emit_ended, transitions)
    };

    emit_status_transitions(&state, transitions).await;

    if should_emit_ended {
        state.emit_chat_session_ended(protocol::ChatSessionEndedEvent {
            thread_id: thread_id.to_string(),
            session_id: session_id.to_string(),
            reason: reason.to_string(),
        });
        emit_state_delta_updated(&state, thread_id, session_id).await;
    }

    if purge {
        purge_session(state, thread_id, session_id).await;
    }
}
async fn stop_session_internal(
    state: Arc<AppState>,
    thread_id: &str,
    session_id: &str,
    reason: &str,
    purge: bool,
) -> Result<(), String> {
    let stop_tx = {
        let mut chat = state.chat.lock().await;
        let session = chat
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| format!("chat session not found: {session_id}"))?;
        if session.thread_id != thread_id {
            return Err(format!(
                "chat session {session_id} does not belong to thread {thread_id}"
            ));
        }
        session.stop_tx.take()
    };

    if let Some(stop_tx) = stop_tx {
        let _ = stop_tx.send(());
    }

    mark_session_ended(state, thread_id, session_id, reason, purge).await;
    Ok(())
}

async fn purge_session(state: Arc<AppState>, thread_id: &str, session_id: &str) {
    let (removed_channels, history_path) = {
        let mut chat = state.chat.lock().await;
        let Some(mut session) = chat.sessions.remove(session_id) else {
            return;
        };

        if let Some(thread_sessions) = chat.sessions_by_thread.get_mut(thread_id) {
            thread_sessions.retain(|existing| existing != session_id);
            if thread_sessions.is_empty() {
                chat.sessions_by_thread.remove(thread_id);
            }
        }

        let channels = session.attached_channels.drain().collect::<Vec<_>>();
        for channel_id in &channels {
            chat.channel_to_session.remove(channel_id);
            chat.channel_outbound.remove(channel_id);
        }
        (channels, session.history_path)
    };

    if !removed_channels.is_empty() {
        detach_channels(Arc::clone(&state), removed_channels).await;
    }

    if let Err(error) = remove_history_file(&history_path) {
        warn!(
            session_id,
            path = %history_path.display(),
            error = %error,
            "failed to remove chat history file during purge"
        );
    }

    state.emit_state_delta(vec![
        protocol::StateDeltaOperationPayload::ChatSessionRemoved {
            thread_id: thread_id.to_string(),
            session_id: session_id.to_string(),
        },
    ]);
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

pub(crate) fn history_path_for_session(
    history_root: &Path,
    thread_id: &str,
    session_id: &str,
) -> PathBuf {
    history_root
        .join(thread_id)
        .join(format!("{session_id}.jsonl"))
}

fn history_metadata_path_for_session(
    history_root: &Path,
    thread_id: &str,
    session_id: &str,
) -> PathBuf {
    history_root
        .join(thread_id)
        .join(format!("{session_id}.metadata.json"))
}

fn restored_session_marker_path_for_session(
    history_root: &Path,
    thread_id: &str,
    session_id: &str,
) -> PathBuf {
    history_root
        .join(thread_id)
        .join(format!("{session_id}.restore-session-new"))
}

fn persist_restored_session_marker(
    history_root: &Path,
    thread_id: &str,
    session_id: &str,
) -> Result<(), String> {
    let marker_path = restored_session_marker_path_for_session(history_root, thread_id, session_id);
    let parent = marker_path.parent().ok_or_else(|| {
        format!(
            "invalid restored session marker path: {}",
            marker_path.display()
        )
    })?;
    fs::create_dir_all(parent)
        .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;

    let mut file = fs::File::create(&marker_path)
        .map_err(|err| format!("failed to create {}: {err}", marker_path.display()))?;
    file.write_all(b"session/new\n")
        .map_err(|err| format!("failed to write {}: {err}", marker_path.display()))?;
    file.sync_all()
        .map_err(|err| format!("failed to fsync {}: {err}", marker_path.display()))
}

fn restored_session_marker_exists(
    history_root: &Path,
    thread_id: &str,
    session_id: &str,
) -> Result<bool, String> {
    restored_session_marker_path_for_session(history_root, thread_id, session_id)
        .try_exists()
        .map_err(|err| {
            format!(
                "failed to stat restored session marker for {}: {err}",
                session_id
            )
        })
}

fn clear_restored_session_marker(
    history_root: &Path,
    thread_id: &str,
    session_id: &str,
) -> Result<(), String> {
    let marker_path = restored_session_marker_path_for_session(history_root, thread_id, session_id);
    match fs::remove_file(&marker_path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("failed to remove {}: {err}", marker_path.display())),
    }
}

fn persist_session_metadata(
    history_root: &Path,
    thread_id: &str,
    session_id: &str,
    acp_session_id: Option<&str>,
) -> Result<(), String> {
    let metadata_path = history_metadata_path_for_session(history_root, thread_id, session_id);
    let parent = metadata_path.parent().ok_or_else(|| {
        format!(
            "invalid chat session metadata path: {}",
            metadata_path.display()
        )
    })?;
    fs::create_dir_all(parent)
        .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;

    let metadata = PersistedChatSessionMetadata {
        acp_session_id: acp_session_id.map(ToOwned::to_owned),
    };
    let mut file = fs::File::create(&metadata_path)
        .map_err(|err| format!("failed to create {}: {err}", metadata_path.display()))?;
    serde_json::to_writer(&mut file, &metadata)
        .map_err(|err| format!("failed to encode {}: {err}", metadata_path.display()))?;
    file.write_all(b"\n")
        .map_err(|err| format!("failed to finalize {}: {err}", metadata_path.display()))?;
    file.sync_all()
        .map_err(|err| format!("failed to fsync {}: {err}", metadata_path.display()))
}

fn read_persisted_session_metadata(
    history_root: &Path,
    thread_id: &str,
    session_id: &str,
) -> Result<Option<PersistedChatSessionMetadata>, String> {
    let metadata_path = history_metadata_path_for_session(history_root, thread_id, session_id);
    if !metadata_path.exists() {
        return Ok(None);
    }

    let file = fs::File::open(&metadata_path)
        .map_err(|err| format!("failed to open {}: {err}", metadata_path.display()))?;
    let reader = BufReader::new(file);
    serde_json::from_reader(reader)
        .map(Some)
        .map_err(|err| format!("failed to parse {}: {err}", metadata_path.display()))
}

fn read_history_acp_session_id(history_path: &Path) -> Option<String> {
    fs::File::open(history_path).ok().and_then(|file| {
        let reader = BufReader::new(file);
        reader
            .lines()
            .next()?
            .ok()
            .and_then(|line| serde_json::from_str::<Value>(&line).ok())
            .and_then(|value| value.get("sessionId")?.as_str().map(ToOwned::to_owned))
    })
}

fn is_safe_history_component(value: &str) -> bool {
    !value.is_empty() && !value.contains('/') && !value.contains('\\') && !value.contains("..")
}

fn count_lines(path: &Path) -> Result<u64, String> {
    let file =
        fs::File::open(path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    let reader = BufReader::new(file);
    let mut count = 0_u64;
    for line in reader.lines() {
        line.map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        count += 1;
    }
    Ok(count)
}

fn append_updates_to_history(history_path: &Path, updates: &[Value]) -> Result<(), String> {
    if updates.is_empty() {
        return Ok(());
    }

    let parent = history_path
        .parent()
        .ok_or_else(|| format!("invalid chat history path: {}", history_path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(history_path)
        .map_err(|err| format!("failed to open {}: {err}", history_path.display()))?;

    for update in updates {
        serde_json::to_writer(&mut file, update)
            .map_err(|err| format!("failed to encode history update: {err}"))?;
        file.write_all(b"\n").map_err(|err| {
            format!(
                "failed to append newline to {}: {err}",
                history_path.display()
            )
        })?;
    }

    file.sync_data()
        .map_err(|err| format!("failed to fsync {}: {err}", history_path.display()))
}

fn read_history_page(
    history_path: &Path,
    cursor: Option<u64>,
) -> Result<protocol::ChatHistoryResult, String> {
    if !history_path.exists() {
        return Ok(protocol::ChatHistoryResult {
            updates: Vec::new(),
            next_cursor: None,
        });
    }

    let start_idx = cursor.unwrap_or(0) as usize;
    let file = fs::File::open(history_path)
        .map_err(|err| format!("failed to open {}: {err}", history_path.display()))?;
    let reader = BufReader::new(file);

    let mut updates = Vec::new();
    let mut line_index = 0_usize;
    let mut has_more = false;

    for line in reader.lines() {
        let line =
            line.map_err(|err| format!("failed to read {}: {err}", history_path.display()))?;
        if line_index < start_idx {
            line_index += 1;
            continue;
        }

        if updates.len() >= CHAT_HISTORY_BATCH_SIZE {
            has_more = true;
            break;
        }

        match serde_json::from_str::<Value>(&line) {
            Ok(value) => updates.push(value),
            Err(error) => {
                warn!(
                    path = %history_path.display(),
                    line_index,
                    error = %error,
                    "failed to parse chat history line"
                );
            }
        }
        line_index += 1;
    }

    let next_cursor = if has_more {
        Some(start_idx as u64 + CHAT_HISTORY_BATCH_SIZE as u64)
    } else {
        None
    };

    Ok(protocol::ChatHistoryResult {
        updates,
        next_cursor,
    })
}

pub(crate) fn build_conversation_context(
    history_path: &Path,
    cursor: Option<u64>,
) -> Option<String> {
    if !history_path.exists() {
        return None;
    }

    let file = fs::File::open(history_path).ok()?;
    let reader = BufReader::new(file);
    let max_lines = cursor.unwrap_or(u64::MAX).min(usize::MAX as u64) as usize;
    let mut entries = Vec::new();
    let mut message_indices = HashMap::new();
    let mut tool_indices = HashMap::new();

    for (line_index, line) in reader.lines().take(max_lines).enumerate() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                warn!(
                    path = %history_path.display(),
                    line_index,
                    error = %error,
                    "failed to read chat history line while building conversation context"
                );
                continue;
            }
        };

        let value = match serde_json::from_str::<Value>(&line) {
            Ok(value) => value,
            Err(error) => {
                warn!(
                    path = %history_path.display(),
                    line_index,
                    error = %error,
                    "failed to parse chat history line while building conversation context"
                );
                continue;
            }
        };

        let Some(update) = value.get("update") else {
            continue;
        };

        match session_update_kind(update) {
            Some("user_message_chunk") => append_message_entry(
                &mut entries,
                &mut message_indices,
                "[User]",
                extract_message_id(update),
                extract_inline_text(update.get("content")),
            ),
            Some("agent_message_chunk") => append_message_entry(
                &mut entries,
                &mut message_indices,
                "[Assistant]",
                extract_message_id(update),
                extract_inline_text(update.get("content")),
            ),
            Some("agent_thought_chunk") => append_message_entry(
                &mut entries,
                &mut message_indices,
                "[Assistant - Thinking]",
                extract_message_id(update),
                extract_inline_text(update.get("content")),
            ),
            Some("tool_call") => upsert_tool_call_entry(&mut entries, &mut tool_indices, update),
            Some("tool_call_update") => {
                apply_tool_call_update_entry(&mut entries, &mut tool_indices, update)
            }
            Some("plan") => {
                if let Some(plan) = format_plan_update(update) {
                    entries.push(ConversationContextEntry::Plan(plan));
                }
            }
            Some("usage_update") | Some("config_option_update") => {}
            _ => {}
        }
    }

    let transcript = entries
        .into_iter()
        .filter_map(|entry| entry.render())
        .collect::<Vec<_>>();

    if transcript.is_empty() {
        return None;
    }

    Some(format!(
        "<conversation-history>\nThe following is a conversation you were having with the user. All tool calls\nhave already been executed and their results are reflected in the current working\ndirectory state. Continue this conversation naturally — the user's message\nfollows after this context block.\n\n{}\n</conversation-history>",
        transcript.join("\n\n")
    ))
}

enum ConversationContextEntry {
    Message {
        label: &'static str,
        content: String,
    },
    ToolCall {
        title: String,
        status: ConversationToolStatus,
        input: Option<String>,
        output: Option<String>,
        error: Option<String>,
    },
    Plan(String),
}

impl ConversationContextEntry {
    fn render(self) -> Option<String> {
        match self {
            Self::Message { label, content } => {
                if content.trim().is_empty() {
                    None
                } else {
                    Some(format!("{label}\n{content}"))
                }
            }
            Self::ToolCall {
                title,
                status,
                input,
                output,
                error,
            } => {
                let mut lines = vec![format!("[Tool Call: {title} ({})]", status.as_str())];
                if let Some(input) = input.filter(|input| !input.trim().is_empty()) {
                    lines.push(format!("Input: {input}"));
                }
                match status {
                    ConversationToolStatus::Pending => {}
                    ConversationToolStatus::Completed => {
                        if let Some(output) = output.filter(|output| !output.trim().is_empty()) {
                            lines.push(format!("Output: {output}"));
                        }
                    }
                    ConversationToolStatus::Cancelled => {}
                    ConversationToolStatus::Failed => {
                        if let Some(error) = error.filter(|error| !error.trim().is_empty()) {
                            lines.push(format!("Error: {error}"));
                        }
                    }
                }
                Some(lines.join("\n"))
            }
            Self::Plan(plan) => {
                if plan.trim().is_empty() {
                    None
                } else {
                    Some(format!("[Plan Update]\n{plan}"))
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum ConversationToolStatus {
    Pending,
    Completed,
    Cancelled,
    Failed,
}

impl ConversationToolStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

fn session_update_kind(update: &Value) -> Option<&str> {
    update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .or_else(|| update.get("kind").and_then(Value::as_str))
}

fn extract_message_id(update: &Value) -> Option<&str> {
    update
        .get("messageId")
        .and_then(Value::as_str)
        .or_else(|| update.get("message_id").and_then(Value::as_str))
        .filter(|message_id| !message_id.is_empty())
}

fn append_message_entry(
    entries: &mut Vec<ConversationContextEntry>,
    message_indices: &mut HashMap<String, usize>,
    label: &'static str,
    message_id: Option<&str>,
    content: Option<String>,
) {
    let Some(content) = content.filter(|content| !content.trim().is_empty()) else {
        return;
    };

    if let Some(message_id) = message_id {
        let key = format!("{label}:{message_id}");
        if let Some(index) = message_indices.get(&key).copied() {
            if let Some(ConversationContextEntry::Message {
                content: existing, ..
            }) = entries.get_mut(index)
            {
                existing.push_str(&content);
                return;
            }
        }

        message_indices.insert(key, entries.len());
    }

    entries.push(ConversationContextEntry::Message { label, content });
}

fn upsert_tool_call_entry(
    entries: &mut Vec<ConversationContextEntry>,
    tool_indices: &mut HashMap<String, usize>,
    update: &Value,
) {
    let title = extract_tool_title(update);
    let input = extract_raw_input(update);
    if let Some(tool_call_id) = extract_tool_call_id(update) {
        if let Some(index) = tool_indices.get(&tool_call_id).copied() {
            if let Some(ConversationContextEntry::ToolCall {
                title: existing_title,
                status,
                input: existing_input,
                ..
            }) = entries.get_mut(index)
            {
                *existing_title = title;
                *status = ConversationToolStatus::Pending;
                if input.is_some() {
                    *existing_input = input;
                }
                return;
            }
        }

        tool_indices.insert(tool_call_id, entries.len());
    }

    entries.push(ConversationContextEntry::ToolCall {
        title,
        status: ConversationToolStatus::Pending,
        input,
        output: None,
        error: None,
    });
}

fn apply_tool_call_update_entry(
    entries: &mut Vec<ConversationContextEntry>,
    tool_indices: &mut HashMap<String, usize>,
    update: &Value,
) {
    let title = extract_tool_title(update);
    let status = extract_tool_context_status(update);
    let input = extract_raw_input(update);
    let output = extract_tool_output(update);
    let error = extract_tool_error(update);

    if let Some(tool_call_id) = extract_tool_call_id(update) {
        if let Some(index) = tool_indices.get(&tool_call_id).copied() {
            if let Some(ConversationContextEntry::ToolCall {
                title: existing_title,
                status: existing_status,
                input: existing_input,
                output: existing_output,
                error: existing_error,
            }) = entries.get_mut(index)
            {
                *existing_title = title;
                *existing_status = status;
                if input.is_some() {
                    *existing_input = input;
                }
                if output.is_some() {
                    *existing_output = output;
                }
                if error.is_some() {
                    *existing_error = error;
                }
                return;
            }
        }

        tool_indices.insert(tool_call_id, entries.len());
    }

    entries.push(ConversationContextEntry::ToolCall {
        title,
        status,
        input,
        output,
        error,
    });
}

fn extract_tool_context_status(update: &Value) -> ConversationToolStatus {
    match extract_tool_status(update).unwrap_or_default() {
        "completed" => ConversationToolStatus::Completed,
        "cancelled" => ConversationToolStatus::Cancelled,
        "failed" | "error" => ConversationToolStatus::Failed,
        _ => ConversationToolStatus::Pending,
    }
}

fn extract_tool_title(update: &Value) -> String {
    update
        .get("title")
        .and_then(Value::as_str)
        .or_else(|| {
            update
                .get("toolCall")
                .and_then(|tool_call| tool_call.get("state"))
                .and_then(|state| state.get("title"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            update
                .get("toolCall")
                .and_then(|tool_call| tool_call.get("title"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            update
                .get("toolCall")
                .and_then(|tool_call| tool_call.get("name"))
                .and_then(Value::as_str)
        })
        .or_else(|| update.get("name").and_then(Value::as_str))
        .unwrap_or("Tool")
        .to_string()
}

fn extract_raw_input(update: &Value) -> Option<String> {
    update
        .get("rawInput")
        .and_then(render_scalar_or_json)
        .or_else(|| update.get("input").and_then(render_scalar_or_json))
        .or_else(|| {
            update
                .get("toolCall")
                .and_then(|tool_call| tool_call.get("rawInput"))
                .and_then(render_scalar_or_json)
        })
}

fn extract_tool_output(update: &Value) -> Option<String> {
    extract_block_text(update.get("content"))
        .or_else(|| extract_block_text(update.get("output")))
        .or_else(|| {
            update
                .get("toolCall")
                .and_then(|tool_call| tool_call.get("content"))
                .and_then(|content| extract_block_text(Some(content)))
        })
}

fn extract_tool_error(update: &Value) -> Option<String> {
    extract_block_text(update.get("error"))
        .or_else(|| update.get("error").and_then(render_scalar_or_json))
}

fn format_plan_update(update: &Value) -> Option<String> {
    let entries = update
        .get("entries")
        .and_then(Value::as_array)
        .or_else(|| {
            update
                .get("plan")
                .and_then(|plan| plan.get("entries"))
                .and_then(Value::as_array)
        })?;

    let mut lines = Vec::new();
    for entry in entries {
        let Some(text) = entry
            .get("content")
            .and_then(Value::as_str)
            .or_else(|| entry.get("description").and_then(Value::as_str))
            .or_else(|| entry.get("text").and_then(Value::as_str))
            .or_else(|| entry.get("title").and_then(Value::as_str))
            .map(str::trim)
            .filter(|text| !text.is_empty())
        else {
            continue;
        };
        let marker = match entry
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending")
        {
            "completed" => "[x]",
            "in_progress" | "inProgress" => "[-]",
            "cancelled" => "[/]",
            "failed" | "error" => "[!]",
            _ => "[ ]",
        };
        lines.push(format!("{marker} {text}"));
    }

    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

fn extract_inline_text(value: Option<&Value>) -> Option<String> {
    extract_text_fragments(value).map(|fragments| fragments.join(""))
}

fn extract_block_text(value: Option<&Value>) -> Option<String> {
    extract_text_fragments(value).map(|fragments| fragments.join("\n"))
}

fn extract_text_fragments(value: Option<&Value>) -> Option<Vec<String>> {
    let value = value?;
    let mut fragments = Vec::new();
    collect_text_fragments(value, &mut fragments);
    if fragments.is_empty() {
        None
    } else {
        Some(fragments)
    }
}

fn collect_text_fragments(value: &Value, fragments: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            if !text.is_empty() {
                fragments.push(text.clone());
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_text_fragments(item, fragments);
            }
        }
        Value::Object(map) => {
            if let Some(text) = map.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    fragments.push(text.to_string());
                }
                return;
            }
            if let Some(message) = map.get("message").and_then(Value::as_str) {
                if !message.is_empty() {
                    fragments.push(message.to_string());
                }
                return;
            }
            for key in ["content", "parts", "value", "error"] {
                if let Some(nested) = map.get(key) {
                    collect_text_fragments(nested, fragments);
                    if !fragments.is_empty() {
                        return;
                    }
                }
            }
        }
        _ => {}
    }
}

fn render_scalar_or_json(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        other => serde_json::to_string(other).ok(),
    }
}

fn discover_history_sessions(
    history_root: &Path,
    known_threads: &HashSet<String>,
) -> Result<Vec<ChatSessionRuntime>, String> {
    if !history_root.exists() {
        return Ok(Vec::new());
    }

    let mut recovered = Vec::new();
    let thread_dirs = fs::read_dir(history_root)
        .map_err(|err| format!("failed to read {}: {err}", history_root.display()))?;

    for thread_dir in thread_dirs {
        let thread_dir =
            thread_dir.map_err(|err| format!("failed to read chat thread dir entry: {err}"))?;
        let file_type = thread_dir
            .file_type()
            .map_err(|err| format!("failed to inspect {}: {err}", thread_dir.path().display()))?;
        if !file_type.is_dir() {
            continue;
        }

        let thread_id = thread_dir.file_name().to_string_lossy().to_string();
        if !known_threads.contains(&thread_id) {
            continue;
        }

        let entries = fs::read_dir(thread_dir.path())
            .map_err(|err| format!("failed to read {}: {err}", thread_dir.path().display()))?;
        for entry in entries {
            let entry =
                entry.map_err(|err| format!("failed to read chat history file entry: {err}"))?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }

            let Some(stem) = path.file_stem().and_then(|name| name.to_str()) else {
                continue;
            };
            let created_at = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .map(chrono::DateTime::<Utc>::from)
                .unwrap_or_else(Utc::now)
                .to_rfc3339();

            let acp_session_id =
                match restored_session_marker_exists(history_root, &thread_id, stem) {
                    Ok(true) => None,
                    Ok(false) => {
                        match read_persisted_session_metadata(history_root, &thread_id, stem) {
                            Ok(Some(metadata)) => metadata.acp_session_id,
                            Ok(None) => {
                                // Legacy sessions derive ACP session ID from first JSONL entry.
                                // Restored sessions persist metadata with acp_session_id = null so
                                // recovery durably falls back to session/new after daemon restart.
                                read_history_acp_session_id(&path)
                            }
                            Err(error) => {
                                warn!(
                                    thread_id,
                                    session_id = stem,
                                    error = %error,
                                    "failed to read persisted chat session metadata"
                                );
                                None
                            }
                        }
                    }
                    Err(error) => {
                        warn!(
                            thread_id,
                            session_id = stem,
                            error = %error,
                            "failed to read restored chat session marker"
                        );
                        None
                    }
                };

            recovered.push(ChatSessionRuntime {
                summary: protocol::ChatSessionSummary {
                    session_id: stem.to_string(),
                    agent_type: "unknown".to_string(),
                    status: protocol::ChatSessionStatus::Ended,
                    agent_status: protocol::AgentStatus::Idle,
                    worker_count: 0,
                    title: None,
                    model_id: None,
                    created_at,
                    display_name: None,
                    parent_session_id: None,
                },
                thread_id: thread_id.clone(),
                display_name: None,
                parent_session_id: None,
                system_prompt: None,
                initial_prompt: None,
                conversation_context: None,
                acp_session_id,
                attached_channels: HashSet::new(),
                input_tx: None,
                stop_tx: None,
                status_notify: Arc::new(Notify::new()),
                ended_emitted: true,
                history_path: path,
                input_buffer: Vec::new(),
                output_buffer: Vec::new(),
                active_tools: HashSet::new(),
                total_tool_count: 0,
                latest_tool_name: None,
                latest_tool_title: None,
                started_at: None,
                pending_prompt_ids: HashSet::new(),
                checkpoint_seq: 0,
                last_update_time: None,
                stall_generation: 0,
                stall_task: None,
                modes: None,
                models: None,
                config_options: None,
                injection_prompt_id: None,
            });
        }
    }

    Ok(recovered)
}

fn remove_history_file(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }

    fs::remove_file(path).map_err(|err| format!("failed to remove {}: {err}", path.display()))
}

fn remove_thread_history_dir(history_root: &Path, thread_id: &str) -> Result<(), String> {
    let thread_dir = history_root.join(thread_id);
    if !thread_dir.exists() {
        return Ok(());
    }

    fs::remove_dir_all(&thread_dir)
        .map_err(|err| format!("failed to remove {}: {err}", thread_dir.display()))
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

    if context_parts.is_empty() && initial_prompt.map_or(true, str::is_empty) {
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

    let port_base =
        match crate::services::thread::load_threadmill_config(&thread.worktree_path, &project.path)
        {
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
        (project.path, thread.worktree_path)
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

    fn make_test_state() -> Arc<AppState> {
        let state_path = std::env::temp_dir().join(format!(
            "threadmill-chat-tests-{}.json",
            uuid::Uuid::new_v4().simple()
        ));
        Arc::new(AppState::new(StateStore {
            path: state_path,
            data: AppData {
                projects: vec![Project {
                    id: "project-1".to_string(),
                    name: "project".to_string(),
                    path: "/tmp/project".to_string(),
                    default_branch: "main".to_string(),
                }],
                threads: vec![Thread {
                    id: "thread-1".to_string(),
                    project_id: "project-1".to_string(),
                    name: "thread".to_string(),
                    branch: "main".to_string(),
                    worktree_path: "/tmp/project".to_string(),
                    status: protocol::ThreadStatus::Active,
                    source_type: protocol::SourceType::ExistingBranch,
                    created_at: Utc::now(),
                    tmux_session: "tm_test".to_string(),
                    port_offset: 0,
                }],
            },
        }))
    }

    fn make_test_session() -> ChatSessionRuntime {
        ChatSessionRuntime {
            summary: protocol::ChatSessionSummary {
                session_id: "session-1".to_string(),
                agent_type: "opencode".to_string(),
                status: protocol::ChatSessionStatus::Ready,
                agent_status: protocol::AgentStatus::Idle,
                worker_count: 0,
                title: None,
                model_id: None,
                created_at: Utc::now().to_rfc3339(),
                display_name: None,
                parent_session_id: None,
            },
            thread_id: "thread-1".to_string(),
            display_name: None,
            parent_session_id: None,
            system_prompt: None,
            initial_prompt: None,
            conversation_context: None,
            acp_session_id: Some("acp-session-1".to_string()),
            attached_channels: HashSet::new(),
            input_tx: None,
            stop_tx: None,
            status_notify: Arc::new(Notify::new()),
            ended_emitted: false,
            history_path: PathBuf::from("/tmp/history.jsonl"),
            input_buffer: Vec::new(),
            output_buffer: Vec::new(),
            active_tools: HashSet::new(),
            total_tool_count: 0,
            latest_tool_name: None,
            latest_tool_title: None,
            started_at: Some(Utc::now()),
            pending_prompt_ids: HashSet::new(),
            checkpoint_seq: 0,
            modes: None,
            models: None,
            config_options: None,
            last_update_time: None,
            injection_prompt_id: None,
            stall_generation: 0,
            stall_task: None,
        }
    }

    #[tokio::test]
    async fn inbound_prompt_transitions_agent_to_busy() {
        let state = make_test_state();
        let mut session = make_test_session();

        let (transitions, replacement_payload, _prompt_updates, auto_checkpoints, consumed) =
            apply_inbound_status_updates(
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

        let (_transitions, replacement_payload, prompt_updates, _auto_checkpoints, consumed) =
            apply_inbound_status_updates(&state, &mut session, vec![message]);

        let replacement_payload = replacement_payload.expect("replacement payload");
        let rewritten: Value =
            serde_json::from_slice(&replacement_payload).expect("parse replacement payload");
        assert_eq!(
            rewritten["params"]["prompt"],
            json!([
                {
                    "type": "text",
                    "text": "prior context\n\n---\n\n"
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
        let state = make_test_state();
        let (temp_root, history_path) = write_history_file(&[]);
        let history_root = {
            let chat = state.chat.lock().await;
            chat.history_root_path().to_path_buf()
        };
        persist_restored_session_marker(&history_root, "thread-1", "session-1")
            .expect("persist restore marker");
        let mut session = make_test_session();
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
                    "sessionId": "session-1",
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
        assert_eq!(rewritten["params"]["sessionId"], "acp-session-1");
        assert_eq!(
            rewritten["params"]["prompt"],
            json!([
                {
                    "type": "text",
                    "text": "prior context\n\n---\n\n"
                },
                {"type": "text", "text": "new prompt"}
            ])
        );

        let persisted = fs::read_to_string(&history_path).expect("read persisted history");
        let persisted_lines = persisted.lines().collect::<Vec<_>>();
        assert_eq!(persisted_lines.len(), 1);
        let persisted_entry: Value =
            serde_json::from_str(persisted_lines[0]).expect("parse history entry");
        assert_eq!(persisted_entry["sessionId"], "session-1");
        assert_eq!(persisted_entry["update"]["content"]["text"], "new prompt");

        assert!(
            !restored_session_marker_exists(&history_root, "thread-1", "session-1")
                .expect("read restore marker state")
        );
        {
            let chat = state.chat.lock().await;
            let session = chat.sessions.get("session-1").expect("registered session");
            assert_eq!(session.conversation_context, None);
        }

        cancel_registered_stall_timer(&state, "session-1").await;
        let _ = fs::remove_dir_all(temp_root);
    }

    #[tokio::test]
    async fn handle_binary_frame_keeps_restore_state_when_prompt_queue_fails() {
        let state = make_test_state();
        let (temp_root, history_path) = write_history_file(&[]);
        let history_root = {
            let chat = state.chat.lock().await;
            chat.history_root_path().to_path_buf()
        };
        persist_restored_session_marker(&history_root, "thread-1", "session-1")
            .expect("persist restore marker");

        let mut session = make_test_session();
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
                    "sessionId": "session-1",
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
            restored_session_marker_exists(&history_root, "thread-1", "session-1")
                .expect("read restore marker state")
        );
        {
            let chat = state.chat.lock().await;
            let session = chat.sessions.get("session-1").expect("registered session");
            assert_eq!(
                session.conversation_context.as_deref(),
                Some("prior context")
            );
        }

        cancel_registered_stall_timer(&state, "session-1").await;
        let _ = fs::remove_dir_all(temp_root);
    }

    #[tokio::test]
    async fn handle_binary_frame_preserves_non_text_prompt_blocks_when_context_injected() {
        let state = make_test_state();
        let (temp_root, history_path) = write_history_file(&[]);
        let mut session = make_test_session();
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
                    "sessionId": "session-1",
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
        assert_eq!(rewritten["params"]["sessionId"], "acp-session-1");
        assert_eq!(
            rewritten["params"]["prompt"],
            json!([
                {
                    "type": "text",
                    "text": "prior context\n\n---\n\n"
                },
                {"type": "image", "source": {"mediaType": "image/png", "data": "abc"}}
            ])
        );

        let persisted = fs::read_to_string(&history_path).expect("read persisted history");
        assert!(persisted.is_empty());

        cancel_registered_stall_timer(&state, "session-1").await;
        let _ = fs::remove_dir_all(temp_root);
    }

    #[tokio::test]
    async fn handle_binary_frame_keeps_normal_prompt_flow_when_context_absent() {
        let state = make_test_state();
        let (temp_root, history_path) = write_history_file(&[]);
        let mut session = make_test_session();
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
                    "sessionId": "session-1",
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
        assert_eq!(forwarded_message["params"]["sessionId"], "acp-session-1");
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
        assert_eq!(persisted_entry["sessionId"], "session-1");
        assert_eq!(persisted_entry["update"]["content"]["text"], "plain prompt");

        cancel_registered_stall_timer(&state, "session-1").await;
        let _ = fs::remove_dir_all(temp_root);
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
                threads: vec![Thread {
                    id: thread_id.clone(),
                    project_id: "project-1".to_string(),
                    name: "thread".to_string(),
                    branch: "main".to_string(),
                    worktree_path: "/tmp/project".to_string(),
                    status: protocol::ThreadStatus::Closed,
                    source_type: protocol::SourceType::ExistingBranch,
                    created_at: Utc::now(),
                    tmux_session: "tm_test".to_string(),
                    port_offset: 0,
                }],
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
                threads: vec![Thread {
                    id: thread_id.clone(),
                    project_id: "project-1".to_string(),
                    name: "thread".to_string(),
                    branch: "main".to_string(),
                    worktree_path: "/tmp/project".to_string(),
                    status: protocol::ThreadStatus::Closed,
                    source_type: protocol::SourceType::ExistingBranch,
                    created_at: Utc::now(),
                    tmux_session: "tm_test".to_string(),
                    port_offset: 0,
                }],
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
                threads: vec![Thread {
                    id: thread_id.clone(),
                    project_id: "project-1".to_string(),
                    name: "thread".to_string(),
                    branch: "main".to_string(),
                    worktree_path: "/tmp/project".to_string(),
                    status: protocol::ThreadStatus::Closed,
                    source_type: protocol::SourceType::ExistingBranch,
                    created_at: Utc::now(),
                    tmux_session: "tm_test".to_string(),
                    port_offset: 0,
                }],
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
        assert!(chat.sessions.get(&session_id).is_none());
    }

    #[tokio::test]
    async fn outbound_tool_updates_adjust_worker_count() {
        let state = make_test_state();
        let mut session = make_test_session();
        session.summary.agent_status = protocol::AgentStatus::Busy;

        let (started, _) = apply_outbound_status_updates(
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

        let (completed, _) = apply_outbound_status_updates(
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

        let (transitions, _) = apply_outbound_status_updates(
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

        let (transitions, _) = apply_outbound_status_updates(
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
                threads: vec![Thread {
                    id: thread_id.clone(),
                    project_id: "project-1".to_string(),
                    name: "thread".to_string(),
                    branch: "main".to_string(),
                    worktree_path: "/tmp/project".to_string(),
                    status: protocol::ThreadStatus::Closed,
                    source_type: protocol::SourceType::ExistingBranch,
                    created_at: Utc::now(),
                    tmux_session: "tm_test".to_string(),
                    port_offset: 0,
                }],
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
}
