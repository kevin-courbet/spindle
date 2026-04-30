use serde_json::{json, Value};
use tokio::{io::AsyncReadExt, sync::mpsc};
use tracing::warn;

use super::CHAT_IO_CHUNK_SIZE;

/// Monotonic ID generator for Spindle-originated ACP requests (injections, etc.).
///
/// Starts at 1_000_000_000 to stay far above IDs the Mac uses for user-originated
/// prompts, avoiding collisions in `pending_prompt_ids` tracking.
pub(super) fn injection_request_id() -> u64 {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1_000_000_000);
    COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub(super) async fn send_acp_request(
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

pub(super) async fn wait_for_acp_response(
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

pub(super) fn ensure_acp_success<'a>(
    response: &'a Value,
    method: &str,
) -> Result<&'a Value, String> {
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

pub(super) fn request_id_key(id_value: Option<&Value>) -> Option<String> {
    let id_value = id_value?;
    id_value
        .as_str()
        .map(ToOwned::to_owned)
        .or_else(|| id_value.as_u64().map(|id| id.to_string()))
        .or_else(|| id_value.as_i64().map(|id| id.to_string()))
}

pub(super) fn extract_json_messages(
    buffer: &mut Vec<u8>,
    payload: &[u8],
    direction: &str,
) -> Vec<Value> {
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

/// Rewrite sessionId in a JSON payload. Spindle session IDs are the external
/// contract; ACP session IDs are internal to the agent. Spindle translates
/// at the relay boundary so clients never see ACP IDs.
pub(super) fn rewrite_session_id(payload: &[u8], from: &str, to: &str) -> Option<Vec<u8>> {
    if from.is_empty() || to.is_empty() || from == to {
        return None;
    }
    let payload_str = std::str::from_utf8(payload).ok()?;
    if !payload_str.contains(from) {
        return None;
    }
    Some(payload_str.replace(from, to).into_bytes())
}
