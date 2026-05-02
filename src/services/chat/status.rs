use std::sync::Arc;

use chrono::Utc;
use serde_json::{json, Value};
use tracing::warn;

use super::{
    collect_pending_blocked_request_cancellations, emit_state_delta_updated, io::request_id_key,
    runtime::ChatSessionRuntime, BlockedRequestCancellation, CHAT_CANCEL_METHOD,
    CHAT_PROMPT_METHOD, CHAT_STALL_TIMEOUT, CHAT_UPDATE_METHOD,
};
use crate::{protocol, services::checkpoint::CheckpointService, AppState};

#[derive(Debug)]
pub(super) struct StatusTransition {
    pub(super) thread_id: String,
    pub(super) session_id: String,
    pub(super) old_status: protocol::AgentStatus,
    pub(super) new_status: protocol::AgentStatus,
    pub(super) worker_count: usize,
}

pub(super) struct AutoCheckpointRequest {
    pub(super) thread_id: String,
    pub(super) session_id: String,
    pub(super) message: String,
    pub(super) prompt_preview: Option<String>,
    /// Pre-computed history cursor captured BEFORE the user echo is appended
    /// to the JSONL. Without this, the auto-checkpoint reads the line count
    /// after the echo is already written, so checkpoint.restore truncates to
    /// a point that includes the user echo but not the agent response.
    pub(super) history_cursor: Option<u64>,
}

pub(super) async fn emit_status_transitions(
    state: &Arc<AppState>,
    transitions: Vec<StatusTransition>,
) {
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

pub(super) fn apply_status_transition(
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

pub(super) fn reset_status_tracking(
    session: &mut ChatSessionRuntime,
    transitions: &mut Vec<StatusTransition>,
) {
    session.active_tools.clear();
    session.pending_prompt_ids.clear();
    session.summary.worker_count = 0;
    cancel_stall_timer(session);
    apply_status_transition(session, protocol::AgentStatus::Idle, transitions);
}

pub(super) fn cancel_stall_timer(session: &mut ChatSessionRuntime) {
    session.stall_generation = session.stall_generation.wrapping_add(1);
    session.last_update_time = None;
    if let Some(task) = session.stall_task.take() {
        task.abort();
    }
}

pub(super) fn restart_stall_timer(state: &Arc<AppState>, session: &mut ChatSessionRuntime) {
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

pub(super) async fn handle_stall_timeout(
    state: Arc<AppState>,
    session_id: String,
    generation: u64,
) {
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

/// Output of `apply_inbound_status_updates`.
pub(super) struct SessionProcessingOutcome {
    pub(super) transitions: Vec<StatusTransition>,
    /// If set, the original inbound payload was rewritten and should be
    /// forwarded to the agent in place of the raw bytes.
    pub(super) replacement_payload: Option<Vec<u8>>,
    pub(super) history_updates: Vec<Value>,
    pub(super) auto_checkpoints: Vec<AutoCheckpointRequest>,
    pub(super) title_prompt_to_generate: Option<String>,
    /// Whether the per-session conversation context was consumed during this batch.
    pub(super) consumed_conversation_context: bool,
    pub(super) blocked_request_cancellations: Vec<BlockedRequestCancellation>,
}

pub(super) fn apply_inbound_status_updates(
    state: &Arc<AppState>,
    session: &mut ChatSessionRuntime,
    messages: Vec<Value>,
) -> SessionProcessingOutcome {
    let mut transitions = Vec::new();
    let mut messages = messages;
    let mut replacement_payload = None;
    let mut payload_rewritten = false;
    let mut history_updates = Vec::new();
    let mut auto_checkpoints = Vec::new();
    let mut title_prompt_to_generate = None;
    let mut consumed_conversation_context = false;
    let mut blocked_request_cancellations = Vec::new();

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

            if session.user_prompt_count == 0 {
                if let Some(text) = params
                    .and_then(|p| p.get("prompt"))
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|p| {
                                if p.get("type")?.as_str() == Some("text") {
                                    p.get("text")?.as_str()
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .filter(|t| !t.is_empty())
                {
                    session.first_prompt_text = Some(text);
                    if session.summary.parent_session_id.is_none()
                        && !session.had_conversation_context
                        && session.summary.title.is_none()
                    {
                        title_prompt_to_generate = session.first_prompt_text.clone();
                    }
                }
            }
            session.user_prompt_count += 1;

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
            blocked_request_cancellations
                .extend(collect_pending_blocked_request_cancellations(session));
        }
    }

    if payload_rewritten {
        replacement_payload = serialize_json_messages(&messages);
    }

    SessionProcessingOutcome {
        transitions,
        replacement_payload,
        history_updates,
        auto_checkpoints,
        title_prompt_to_generate,
        consumed_conversation_context,
        blocked_request_cancellations,
    }
}

pub(super) fn spawn_auto_checkpoint(state: Arc<AppState>, request: AutoCheckpointRequest) {
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

pub(super) fn prompt_preview_from_params(params: Option<&Value>) -> Option<String> {
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

pub(super) fn user_prompt_history_updates(
    session_id_value: Value,
    params: Option<&Value>,
) -> Vec<Value> {
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

pub(super) fn prepend_prompt_text_block(message: &mut Value, prompt_text: &str) -> bool {
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

    let context_block = json!({
        "type": "text",
        "text": prompt_text,
        "annotations": { "audience": ["assistant"] }
    });
    if let Some(prompt_array) = prompt.as_array_mut() {
        prompt_array.insert(0, context_block);
    } else {
        *prompt = Value::Array(vec![context_block]);
    }
    true
}

pub(super) fn serialize_json_messages(messages: &[Value]) -> Option<Vec<u8>> {
    let mut payload = Vec::new();
    for message in messages {
        serde_json::to_writer(&mut payload, message).ok()?;
        payload.push(b'\n');
    }
    Some(payload)
}

pub(super) fn user_prompt_history_update(session_id_value: Value, text: &str) -> Value {
    json!({
        "sessionId": session_id_value,
        "update": {
            "sessionUpdate": "user_message_chunk",
            "content": { "type": "text", "text": text }
        }
    })
}

pub(super) fn history_user_message_text(value: &Value) -> Option<&str> {
    let update = value.get("update")?;
    let kind = update
        .get("sessionUpdate")
        .or_else(|| update.get("kind"))
        .and_then(Value::as_str)?;
    if kind != "user_message_chunk" {
        return None;
    }
    update
        .get("content")
        .and_then(|content| content.get("text").or(Some(content)))
        .and_then(Value::as_str)
}

pub(super) struct OutboundResult {
    pub(super) transitions: Vec<StatusTransition>,
    pub(super) injection_completed: bool,
}

/// Returns status transitions and whether the handshake-time injection completed.
pub(super) fn apply_outbound_status_updates(
    state: &Arc<AppState>,
    session: &mut ChatSessionRuntime,
    messages: Vec<Value>,
) -> OutboundResult {
    let mut result = OutboundResult {
        transitions: Vec::new(),
        injection_completed: false,
    };

    for message in messages {
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            if method == CHAT_UPDATE_METHOD {
                apply_worker_update(session, message.get("params"));
                if session.summary.agent_status == protocol::AgentStatus::Busy {
                    restart_stall_timer(state, session);
                } else if session.summary.agent_status == protocol::AgentStatus::Stalled {
                    apply_status_transition(
                        session,
                        protocol::AgentStatus::Busy,
                        &mut result.transitions,
                    );
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
            result.injection_completed = true;
        }

        reset_status_tracking(session, &mut result.transitions);
    }

    result
}

pub(super) fn apply_worker_update(session: &mut ChatSessionRuntime, params: Option<&Value>) {
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

pub(super) fn extract_tool_call_id(update: &Value) -> Option<String> {
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

pub(super) fn extract_tool_status(update: &Value) -> Option<&str> {
    update.get("status").and_then(Value::as_str).or_else(|| {
        update
            .get("toolCall")
            .and_then(|tool_call| tool_call.get("status"))
            .and_then(Value::as_str)
    })
}
