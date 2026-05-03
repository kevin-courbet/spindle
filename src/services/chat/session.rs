use std::sync::Arc;

use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::oneshot,
    time::timeout,
};
use tracing::warn;

use crate::{config, protocol, AppState};

use super::*;

/// All the session identity + launch parameters needed to spawn and run one chat session.
/// Grouped into a single type so spawn/run signatures don't balloon.
pub(super) struct SessionLaunchContext {
    pub(super) thread_id: String,
    pub(super) session_id: String,
    pub(super) command: String,
    pub(super) cwd: String,
    pub(super) load_session_id: Option<String>,
    pub(super) preferred_model: Option<String>,
}

pub(super) fn spawn_session_task(state: Arc<AppState>, ctx: SessionLaunchContext) {
    let fail_state = Arc::clone(&state);
    let fail_thread_id = ctx.thread_id.clone();
    let fail_session_id = ctx.session_id.clone();

    // Build env vars for the agent process
    let env_vars = {
        let state_ref = state.clone();
        let tid = ctx.thread_id.clone();
        let sid = ctx.session_id.clone();
        tokio::spawn(async move { build_agent_env_vars(&state_ref, &tid, &sid).await })
    };

    let handle = tokio::spawn(async move {
        let env_vars = env_vars.await.unwrap_or_default();
        if let Err(error) = run_session_task(Arc::clone(&state), ctx, env_vars).await {
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

pub(super) async fn run_session_task(
    state: Arc<AppState>,
    ctx: SessionLaunchContext,
    env_vars: Vec<(String, String)>,
) -> Result<(), String> {
    let SessionLaunchContext {
        thread_id,
        session_id,
        command,
        cwd,
        load_session_id,
        preferred_model,
    } = ctx;
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

    let capture_blocked_requests = {
        let chat = state.chat.lock().await;
        chat.sessions
            .get(&session_id)
            .is_some_and(|session| session.blocked_request_capture_enabled)
    };

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
            capture_blocked_requests,
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
    let (had_conversation_context, is_fork_session) = {
        let chat = state.chat.lock().await;
        let session = chat.sessions.get(&session_id);
        (
            session.is_some_and(|session| session.had_conversation_context),
            session.is_some_and(|session| session.parent_session_id.is_some()),
        )
    };
    if should_send_initial_context_injection(
        is_new_session,
        had_conversation_context,
        is_fork_session,
    ) {
        let (system_prompt, initial_prompt) = {
            let chat = state.chat.lock().await;
            let session = chat.sessions.get(&session_id);
            let sp = session.and_then(|s| s.system_prompt.clone());
            let ip = session.and_then(|s| s.initial_prompt.clone());
            (sp, ip)
        };

        // Read platform system prompt from <config>/threadmill/system-prompt.md
        // (XDG_CONFIG_HOME if set, else platform default).
        let platform_prompt = {
            let config_dir = config::config_dir().unwrap_or_default();
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
        tracing::debug!(session_id = %session_id, "session load/restored context — skipping context injection");
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
        Ok(_) if explicit_stop => "stopped".to_string(),
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

pub(super) async fn mark_session_ready(
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

pub(super) async fn clear_pending_conversation_context(
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

pub(super) async fn mark_session_failed(
    state: Arc<AppState>,
    thread_id: &str,
    session_id: &str,
    error: String,
) {
    tracing::warn!(thread_id, session_id, %error, "chat_session_failed");
    let (transitions, removals) = {
        let mut transitions = Vec::new();
        let mut removals = Vec::new();
        let mut chat = state.chat.lock().await;
        if let Some(session) = chat.sessions.get_mut(session_id) {
            session.summary.status = protocol::ChatSessionStatus::Failed;
            session.input_tx = None;
            session.stop_tx = None;
            removals = drain_pending_blocked_request_removals(session);
            reset_status_tracking(session, &mut transitions);
            session.status_notify.notify_waiters();
        }
        (transitions, removals)
    };

    emit_status_transitions(&state, transitions).await;
    emit_blocked_request_removed_events(&state, removals);

    state.emit_chat_session_failed(protocol::ChatSessionFailedEvent {
        thread_id: thread_id.to_string(),
        session_id: session_id.to_string(),
        error,
    });
    emit_state_delta_updated(&state, thread_id, session_id).await;
}
pub(super) async fn mark_session_ended(
    state: Arc<AppState>,
    thread_id: &str,
    session_id: &str,
    reason: &str,
    purge: bool,
) {
    let (should_emit_ended, transitions, removals) = {
        let mut should_emit_ended = false;
        let mut transitions = Vec::new();
        let mut removals = Vec::new();
        let mut chat = state.chat.lock().await;
        if let Some(session) = chat.sessions.get_mut(session_id) {
            if !session.ended_emitted {
                session.summary.status = protocol::ChatSessionStatus::Ended;
                session.input_tx = None;
                session.stop_tx = None;
                session.ended_emitted = true;
                should_emit_ended = true;
            }
            removals = drain_pending_blocked_request_removals(session);
            reset_status_tracking(session, &mut transitions);
            session.status_notify.notify_waiters();
        }
        (should_emit_ended, transitions, removals)
    };

    emit_status_transitions(&state, transitions).await;
    emit_blocked_request_removed_events(&state, removals);

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
pub(super) async fn stop_session_internal(
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

pub(super) async fn purge_session(state: Arc<AppState>, thread_id: &str, session_id: &str) {
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
            chat.blocked_request_aware_channels.remove(channel_id);
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
