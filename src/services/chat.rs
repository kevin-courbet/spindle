use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};

use chrono::Utc;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{mpsc, oneshot, Mutex, Notify},
    time::{timeout, Duration},
};
use tokio_tungstenite::tungstenite::Message;
use tracing::warn;
use uuid::Uuid;

use crate::{
    protocol,
    services::{project::load_project_agents, terminal::TerminalConnectionState},
    AppState,
};

const CHAT_INPUT_CHANNEL_CAPACITY: usize = 256;
const CHAT_IO_CHUNK_SIZE: usize = 8192;
const CHAT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const CHAT_ATTACH_WAIT_TIMEOUT: Duration = Duration::from_secs(35);

pub struct ChatService;

#[derive(Default)]
pub struct ChatState {
    sessions: HashMap<String, ChatSessionRuntime>,
    sessions_by_thread: HashMap<String, Vec<String>>,
    channel_to_session: HashMap<u16, String>,
    channel_outbound: HashMap<u16, mpsc::UnboundedSender<Message>>,
}

struct ChatSessionRuntime {
    summary: protocol::ChatSessionSummary,
    thread_id: String,
    acp_session_id: Option<String>,
    attached_channels: HashSet<u16>,
    input_tx: Option<mpsc::Sender<Vec<u8>>>,
    stop_tx: Option<oneshot::Sender<()>>,
    status_notify: Arc<Notify>,
    ended_emitted: bool,
}

struct HandshakeResult {
    acp_session_id: String,
    modes: Option<Value>,
    models: Option<Value>,
    config_options: Option<Value>,
    title: Option<String>,
    model_id: Option<String>,
}

impl ChatService {
    pub async fn start(
        state: Arc<AppState>,
        params: protocol::ChatStartParams,
    ) -> Result<protocol::ChatStartResult, String> {
        let (project_path, command, cwd) = resolve_agent_launch(&state, &params.thread_id, &params.agent_name).await?;

        let session_id = Uuid::new_v4().to_string();
        let created_at = Utc::now().to_rfc3339();
        {
            let mut chat = state.chat.lock().await;
            let runtime = ChatSessionRuntime {
                summary: protocol::ChatSessionSummary {
                    session_id: session_id.clone(),
                    agent_type: params.agent_name.clone(),
                    status: protocol::ChatSessionStatus::Starting,
                    title: None,
                    model_id: None,
                    created_at,
                },
                thread_id: params.thread_id.clone(),
                acp_session_id: None,
                attached_channels: HashSet::new(),
                input_tx: None,
                stop_tx: None,
                status_notify: Arc::new(Notify::new()),
                ended_emitted: false,
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
        let (agent_type, acp_session_id, stop_tx) = {
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

            let acp_session_id = session
                .acp_session_id
                .clone()
                .ok_or_else(|| format!("chat session {} has no ACP session id", params.session_id))?;
            session.summary.status = protocol::ChatSessionStatus::Starting;
            session.input_tx = None;
            let stop_tx = session.stop_tx.take();
            session.ended_emitted = false;
            (session.summary.agent_type.clone(), acp_session_id, stop_tx)
        };

        if let Some(stop_tx) = stop_tx {
            let _ = stop_tx.send(());
        }

        emit_state_delta_updated(&state, &params.thread_id, &params.session_id).await;

        let (project_path, command, cwd) = resolve_agent_launch(&state, &params.thread_id, &agent_type).await?;
        spawn_session_task(
            Arc::clone(&state),
            params.thread_id,
            params.session_id.clone(),
            agent_type,
            project_path,
            command,
            cwd,
            Some(acp_session_id),
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
        stop_session_internal(state, &params.thread_id, &params.session_id, "stopped", false).await?;
        Ok(protocol::ChatStopResult { archived: true })
    }

    pub async fn list(
        state: Arc<AppState>,
        params: protocol::ChatListParams,
    ) -> Result<protocol::ChatListResult, String> {
        Ok(chat_session_summaries_for_thread(&state, &params.thread_id).await)
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
                    protocol::ChatSessionStatus::Starting => Some(Arc::clone(&session.status_notify)),
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
                    .map_err(|_| format!("timed out waiting for chat session {} to become ready", params.session_id))?;
                continue;
            }

            let mut conn = connection_state.lock().await;
            let mut chat = state.chat.lock().await;
            let channel_id = state.alloc_channel_id_with(|candidate| {
                conn.by_channel.contains_key(&candidate)
                    || conn.by_agent_channel.contains_key(&candidate)
                    || conn.by_chat_channel.contains_key(&candidate)
                    || conn
                        .attaching_targets
                        .values()
                        .any(|existing| *existing == candidate)
                    || chat.channel_to_session.contains_key(&candidate)
            });

            let session = chat
                .sessions
                .get_mut(&params.session_id)
                .ok_or_else(|| format!("chat session not found: {}", params.session_id))?;
            if session.summary.status != protocol::ChatSessionStatus::Ready {
                continue;
            }

            session.attached_channels.insert(channel_id);
            chat.channel_to_session
                .insert(channel_id, params.session_id.clone());
            chat.channel_outbound.insert(channel_id, outbound_tx.clone());
            conn.by_chat_channel
                .insert(channel_id, params.session_id.clone());

            return Ok(protocol::ChatAttachResult { channel_id });
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
        let input_tx = {
            let chat = state.chat.lock().await;
            let Some(session_id) = chat.channel_to_session.get(&channel_id) else {
                return Ok(false);
            };
            let session = chat
                .sessions
                .get(session_id)
                .ok_or_else(|| format!("chat session missing for channel {channel_id}"))?;
            session
                .input_tx
                .as_ref()
                .ok_or_else(|| format!("chat session {} is not running", session.summary.session_id))?
                .clone()
        };

        input_tx
            .try_send(payload)
            .map_err(|err| format!("failed to queue chat input for channel {channel_id}: {err}"))?;
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
            stop_session_internal(Arc::clone(&state), thread_id, &session_id, reason, purge).await?;
        }

        Ok(())
    }

    pub async fn thread_chat_sessions(state: Arc<AppState>, thread_id: &str) -> Vec<protocol::ChatSessionSummary> {
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
) {
    tokio::spawn(async move {
        if let Err(error) = run_session_task(
            Arc::clone(&state),
            thread_id,
            session_id,
            agent_type,
            project_path,
            command,
            cwd,
            load_session_id,
        )
        .await
        {
            warn!(error = %error, "chat session task failed");
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
) -> Result<(), String> {
    let mut child = Command::new("bash")
        .args(["-lc", &command])
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
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
    let handshake = timeout(
        CHAT_HANDSHAKE_TIMEOUT,
        perform_handshake(&input_tx, &mut stdout, load_session_id),
    )
    .await
    .map_err(|_| "chat handshake timed out after 30s".to_string())?;

    let handshake = match handshake {
        Ok(result) => result,
        Err(error) => {
            mark_session_failed(
                Arc::clone(&state),
                &thread_id,
                &session_id,
                error,
            )
            .await;
            let _ = child.kill().await;
            input_task.abort();
            return Ok(());
        }
    };

    mark_session_ready(
        Arc::clone(&state),
        &thread_id,
        &session_id,
        &handshake,
    )
    .await;

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
) -> Result<HandshakeResult, String> {
    let mut buffer = Vec::new();
    send_acp_request(input_tx, 1, "initialize", json!({
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
    }))
    .await?;

    let init = wait_for_acp_response(stdout, &mut buffer, 1).await?;
    ensure_acp_success(&init, "initialize")?;

    let (method, params) = if let Some(session_id) = load_session_id {
        ("session/load", json!({ "sessionId": session_id }))
    } else {
        ("session/new", json!({ "cwd": "." }))
    };

    send_acp_request(input_tx, 2, method, params).await?;
    let session_response = wait_for_acp_response(stdout, &mut buffer, 2).await?;
    let result = ensure_acp_success(&session_response, method)?;

    let acp_session_id = result
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{method} response missing sessionId"))?
        .to_string();

    let model_id = result
        .get("models")
        .and_then(|models| models.get("currentModelId"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    Ok(HandshakeResult {
        acp_session_id,
        modes: result.get("modes").cloned(),
        models: result.get("models").cloned(),
        config_options: result.get("configOptions").cloned(),
        title: result
            .get("title")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        model_id,
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

async fn fanout_output(state: Arc<AppState>, session_id: &str, payload: &[u8]) {
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
    for (channel_id, outbound) in targets {
        let mut frame = Vec::with_capacity(payload.len() + 2);
        frame.extend_from_slice(&channel_id.to_be_bytes());
        frame.extend_from_slice(payload);
        if outbound.send(Message::Binary(frame)).is_err() {
            dead_channels.push(channel_id);
        }
    }

    if !dead_channels.is_empty() {
        detach_channels(state, dead_channels).await;
    }
}

async fn mark_session_ready(
    state: Arc<AppState>,
    thread_id: &str,
    session_id: &str,
    handshake: &HandshakeResult,
) {
    {
        let mut chat = state.chat.lock().await;
        if let Some(session) = chat.sessions.get_mut(session_id) {
            session.summary.status = protocol::ChatSessionStatus::Ready;
            session.summary.title = handshake.title.clone();
            session.summary.model_id = handshake.model_id.clone();
            session.acp_session_id = Some(handshake.acp_session_id.clone());
            session.status_notify.notify_waiters();
        }
    }

    state.emit_chat_session_ready(protocol::ChatSessionReadyEvent {
        thread_id: thread_id.to_string(),
        session_id: session_id.to_string(),
        modes: handshake.modes.clone(),
        models: handshake.models.clone(),
        config_options: handshake.config_options.clone(),
    });
    emit_state_delta_updated(&state, thread_id, session_id).await;
}

async fn mark_session_failed(state: Arc<AppState>, thread_id: &str, session_id: &str, error: String) {
    {
        let mut chat = state.chat.lock().await;
        if let Some(session) = chat.sessions.get_mut(session_id) {
            session.summary.status = protocol::ChatSessionStatus::Failed;
            session.input_tx = None;
            session.stop_tx = None;
            session.status_notify.notify_waiters();
        }
    }

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
    let mut should_emit_ended = false;
    {
        let mut chat = state.chat.lock().await;
        if let Some(session) = chat.sessions.get_mut(session_id) {
            if !session.ended_emitted {
                session.summary.status = protocol::ChatSessionStatus::Ended;
                session.input_tx = None;
                session.stop_tx = None;
                session.ended_emitted = true;
                should_emit_ended = true;
            }
            session.status_notify.notify_waiters();
        }
    }

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
    let removed_channels = {
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
        channels
    };

    if !removed_channels.is_empty() {
        detach_channels(Arc::clone(&state), removed_channels).await;
    }

    state.emit_state_delta(vec![protocol::StateDeltaOperationPayload::ChatSessionRemoved {
        thread_id: thread_id.to_string(),
        session_id: session_id.to_string(),
    }]);
}

async fn emit_state_delta_added(state: &AppState, thread_id: &str, session_id: &str) {
    if let Some(chat_session) = find_summary(state, session_id).await {
        state.emit_state_delta(vec![protocol::StateDeltaOperationPayload::ChatSessionAdded {
            thread_id: thread_id.to_string(),
            chat_session,
        }]);
    }
}

async fn emit_state_delta_updated(state: &AppState, thread_id: &str, session_id: &str) {
    if let Some(chat_session) = find_summary(state, session_id).await {
        state.emit_state_delta(vec![protocol::StateDeltaOperationPayload::ChatSessionUpdated {
            thread_id: thread_id.to_string(),
            chat_session,
        }]);
    }
}

async fn find_summary(state: &AppState, session_id: &str) -> Option<protocol::ChatSessionSummary> {
    let chat = state.chat.lock().await;
    chat.sessions.get(session_id).map(|session| session.summary.clone())
}

async fn detach_channel(state: Arc<AppState>, channel_id: u16, hint_session_id: Option<String>) -> bool {
    let mut chat = state.chat.lock().await;

    let session_id = hint_session_id
        .or_else(|| chat.channel_to_session.get(&channel_id).cloned());
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
        .filter_map(|session_id| chat.sessions.get(&session_id).map(|session| session.summary.clone()))
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

async fn resolve_agent_launch(
    state: &Arc<AppState>,
    thread_id: &str,
    agent_name: &str,
) -> Result<(String, String, String), String> {
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

    let agents = load_project_agents(&project_path)?;
    let agent = agents
        .into_iter()
        .find(|candidate| candidate.name == agent_name)
        .ok_or_else(|| format!("agent not found: {agent_name}"))?;

    let cwd = resolve_agent_cwd(&worktree_path, agent.cwd.as_deref())?;
    Ok((project_path, agent.command, cwd))
}

fn resolve_agent_cwd(project_path: &str, cwd: Option<&str>) -> Result<String, String> {
    let Some(cwd) = cwd else {
        return Ok(project_path.to_string());
    };

    let cwd_path = Path::new(cwd);
    if cwd_path.is_absolute() {
        return Ok(cwd.to_string());
    }

    let project_root = std::fs::canonicalize(project_path)
        .map_err(|err| format!("failed to canonicalize project {project_path}: {err}"))?;
    let joined = project_root.join(cwd_path);
    let resolved = std::fs::canonicalize(&joined)
        .map_err(|err| format!("failed to resolve agent cwd {}: {err}", joined.display()))?;

    if !resolved.starts_with(&project_root) {
        return Err(format!("agent cwd escapes project root: {cwd}"));
    }

    resolved
        .to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("invalid utf-8 agent cwd: {}", resolved.display()))
}
