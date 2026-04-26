use std::process::Stdio;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tracing::info;

use super::agent_registry;

const TITLE_SYSTEM_PROMPT: &str = "\
You are a title generator. Output ONLY a thread title. Nothing else.\n\
Generate a brief title for this conversation.\n\
- Single line, ≤50 characters, no explanations\n\
- Same language as the user message\n\
- Keep technical terms, filenames, numbers exact\n\
- Remove articles (the, a, an)\n\
- Focus on WHAT the user wants to do\n\
- Never use tools";

/// Preferred agents for title gen, in priority order.
/// Lightweight agents with their own auth — no OpenCode overhead.
const PREFERRED_AGENTS: &[&str] = &["claude", "codex", "gemini", "opencode"];

const MAX_TITLES_BEFORE_ROTATE: u32 = 50;

pub struct TitleService {
    inner: Mutex<Option<TitleSession>>,
}

#[allow(dead_code)]
struct TitleSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    acp_session_id: String,
    request_counter: u64,
    titles_generated: u32,
    agent_id: String,
}

impl TitleService {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }
}

impl Default for TitleService {
    fn default() -> Self {
        Self::new()
    }
}

impl TitleService {
    pub async fn generate_title(&self, user_text: &str) -> Result<String, String> {
        let mut guard = self.inner.lock().await;

        // Start or rotate session if needed
        if guard.is_none()
            || guard
                .as_ref()
                .is_some_and(|s| s.titles_generated >= MAX_TITLES_BEFORE_ROTATE)
        {
            if let Some(mut old) = guard.take() {
                let _ = old.child.kill().await;
            }
            *guard = Some(start_title_session().await?);
        }

        let session = guard.as_mut().unwrap();
        session.request_counter += 1;
        let req_id = session.request_counter;

        let prompt = format!("{TITLE_SYSTEM_PROMPT}\n\nUser message:\n{user_text}");

        // Send prompt
        let request = json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "method": "session/prompt",
            "params": {
                "sessionId": session.acp_session_id,
                "prompt": [{ "type": "text", "text": prompt }]
            }
        });
        send_line(&mut session.stdin, &request).await?;

        // Collect agent_message_chunk text until we get the response
        let mut title_chunks = Vec::new();
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(15);

        loop {
            if tokio::time::Instant::now() > deadline {
                return Err("title generation timed out".to_string());
            }

            let line = match tokio::time::timeout_at(deadline, read_line(&mut session.stdout)).await
            {
                Ok(Ok(line)) => line,
                Ok(Err(e)) => return Err(format!("read error: {e}")),
                Err(_) => return Err("title generation timed out".to_string()),
            };

            let msg: Value =
                serde_json::from_str(&line).map_err(|e| format!("invalid JSON from agent: {e}"))?;

            // Collect text chunks
            if msg
                .pointer("/params/update/sessionUpdate")
                .and_then(Value::as_str)
                == Some("agent_message_chunk")
            {
                if let Some(text) = msg
                    .pointer("/params/update/content/text")
                    .and_then(Value::as_str)
                {
                    title_chunks.push(text.to_owned());
                }
                continue;
            }

            // Check for the response
            if msg.get("id").and_then(Value::as_u64) == Some(req_id) && msg.get("result").is_some()
            {
                break;
            }

            // Skip other notifications (thought chunks, tool calls, usage updates)
        }

        session.titles_generated += 1;

        let raw = title_chunks.join("");
        let title = raw
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim();
        let title = if title.len() > 100 {
            &title[..title.floor_char_boundary(100)]
        } else {
            title
        };

        if title.is_empty() {
            Err("agent returned empty title".to_string())
        } else {
            Ok(title.to_owned())
        }
    }
}

async fn start_title_session() -> Result<TitleSession, String> {
    let agents = agent_registry::discover_agents();
    let agent = PREFERRED_AGENTS
        .iter()
        .find_map(|id| agents.iter().find(|a| a.id == *id && a.installed))
        .ok_or("no installed agent available for title generation")?;

    let command = agent.resolved_path.as_deref().unwrap_or(&agent.command);

    info!(agent = %agent.id, command = %command, "starting title generation agent");

    let mut cmd = Command::new(command);
    for arg in &agent.launch_args {
        cmd.arg(arg);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn title agent: {e}"))?;

    let stdin = child.stdin.take().ok_or("title agent has no stdin")?;
    let stdout = child.stdout.take().ok_or("title agent has no stdout")?;
    let mut stdout = BufReader::new(stdout);
    let mut stdin = stdin;

    // ACP handshake: initialize
    send_line(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": 1,
                "clientInfo": { "name": "Threadmill-TitleService", "version": "1" }
            }
        }),
    )
    .await?;
    let init_resp = read_response(&mut stdout, 1).await?;
    if init_resp.get("error").is_some() {
        return Err(format!(
            "title agent initialize failed: {}",
            init_resp["error"]
        ));
    }

    // ACP handshake: session/new
    send_line(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": { "cwd": "/tmp", "mcpServers": [] }
        }),
    )
    .await?;
    let session_resp = read_response(&mut stdout, 2).await?;
    let session_id = session_resp
        .pointer("/result/sessionId")
        .and_then(Value::as_str)
        .ok_or("title agent session/new returned no sessionId")?
        .to_owned();

    // Pick small model if available
    if let Some(models) = session_resp.pointer("/result/models/availableModels") {
        if let Some(small) = pick_small_model(models, &agent.id) {
            info!(model = %small, "setting title model");
            send_line(
                &mut stdin,
                &json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "session/setModel",
                    "params": { "sessionId": session_id, "modelId": small }
                }),
            )
            .await?;
            let _ = read_response(&mut stdout, 3).await;
        }
    }

    info!(session_id = %session_id, agent = %agent.id, "title session ready");

    Ok(TitleSession {
        child,
        stdin,
        stdout,
        acp_session_id: session_id,
        request_counter: 10, // start after handshake IDs
        titles_generated: 0,
        agent_id: agent.id.clone(),
    })
}

fn pick_small_model(models: &Value, agent_type: &str) -> Option<String> {
    let models = models.as_array()?;
    let patterns: &[&str] = match agent_type {
        "claude" => &["haiku"],
        "codex" => &["-mini", "-nano"],
        "gemini" => &["flash"],
        _ => &["-nano", "-mini", "flash"],
    };
    for pattern in patterns {
        for model in models {
            if let Some(id) = model.get("modelId").and_then(Value::as_str) {
                if id.to_lowercase().contains(pattern) {
                    return Some(id.to_owned());
                }
            }
        }
    }
    None
}

async fn send_line(stdin: &mut ChildStdin, msg: &Value) -> Result<(), String> {
    let mut data = serde_json::to_vec(msg).map_err(|e| format!("serialize: {e}"))?;
    data.push(b'\n');
    stdin
        .write_all(&data)
        .await
        .map_err(|e| format!("write to title agent: {e}"))?;
    stdin
        .flush()
        .await
        .map_err(|e| format!("flush title agent stdin: {e}"))
}

async fn read_line(stdout: &mut BufReader<ChildStdout>) -> Result<String, String> {
    let mut line = String::new();
    let n = stdout
        .read_line(&mut line)
        .await
        .map_err(|e| format!("read from title agent: {e}"))?;
    if n == 0 {
        return Err("title agent stdout closed".to_string());
    }
    Ok(line)
}

/// Read lines until we get a JSON-RPC response matching the given ID.
async fn read_response(
    stdout: &mut BufReader<ChildStdout>,
    expected_id: u64,
) -> Result<Value, String> {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(30);
    loop {
        if tokio::time::Instant::now() > deadline {
            return Err("timeout waiting for title agent response".to_string());
        }
        let line = match tokio::time::timeout_at(deadline, read_line(stdout)).await {
            Ok(Ok(line)) => line,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err("timeout waiting for title agent response".to_string()),
        };
        if let Ok(msg) = serde_json::from_str::<Value>(&line) {
            if msg.get("id").and_then(Value::as_u64) == Some(expected_id) {
                return Ok(msg);
            }
        }
    }
}
