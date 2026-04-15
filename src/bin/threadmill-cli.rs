use std::{env, fs, process::ExitCode};

use clap::{Parser, Subcommand, ValueEnum};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::{connect_async, tungstenite::Message};

const DEFAULT_WS_URL: &str = "ws://127.0.0.1:19990";
const REQUEST_ID: u64 = 1;
const HELLO_REQUEST_ID: u64 = 0;
const PROTOCOL_VERSION: &str = "2026-03-17";
const PROTOCOL_CAPABILITIES: &[&str] = &[
    "state.delta.operations.v1",
    "preset.output.v1",
    "rpc.errors.structured.v1",
];

#[derive(Parser, Debug)]
#[command(name = "threadmill-cli")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    Thread {
        #[command(subcommand)]
        command: ThreadCommand,
    },
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    Chat {
        #[command(subcommand)]
        command: ChatCommand,
    },
    Todo {
        #[command(subcommand)]
        command: TodoCommand,
    },
    Status {
        #[arg(long)]
        pretty: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ThreadCommand {
    List {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        pretty: bool,
    },
    Create {
        project: String,
        name: String,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        pretty: bool,
    },
    Close {
        thread_id: String,
        #[arg(long)]
        pretty: bool,
    },
    Info {
        #[arg(long)]
        pretty: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ProjectCommand {
    List {
        #[arg(long)]
        pretty: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ChatCommand {
    /// Start a new chat session
    Start {
        /// Agent binary name (e.g. "claude", "gemini", "opencode")
        #[arg(long)]
        agent: Option<String>,
        /// Agent definition name (reads from .threadmill/agents/<name>.md)
        #[arg(long)]
        agent_def: Option<String>,
        /// Initial prompt to send after handshake
        #[arg(long)]
        prompt: Option<String>,
        /// Path to file containing system prompt
        #[arg(long)]
        system_prompt_file: Option<String>,
        /// Display name for the session tab
        #[arg(long)]
        display_name: Option<String>,
        /// Parent session ID (links as child worker)
        #[arg(long)]
        parent_session: Option<String>,
        #[arg(long)]
        pretty: bool,
    },
    /// Wait for a session to reach idle state
    Wait {
        /// Session ID to wait for
        #[arg(long)]
        session: String,
        /// Timeout in seconds (default 3600)
        #[arg(long, default_value = "3600")]
        timeout: u64,
    },
    /// Get status of a single session
    Status {
        /// Session ID
        #[arg(long)]
        session: String,
        #[arg(long)]
        pretty: bool,
    },
    /// List chat sessions in the current thread
    List {
        #[arg(long)]
        pretty: bool,
    },
}

#[derive(Subcommand, Debug)]
enum TodoCommand {
    List {
        #[arg(long, value_enum, default_value_t = TodoFilterArg::Active)]
        filter: TodoFilterArg,
    },
    Add {
        content: String,
        #[arg(long, value_enum)]
        priority: Option<TodoPriorityArg>,
    },
    Update {
        id: String,
        #[arg(long)]
        content: Option<String>,
        #[arg(long, value_enum)]
        priority: Option<TodoPriorityArg>,
    },
    Done {
        id: String,
    },
    Reopen {
        id: String,
    },
    Remove {
        id: String,
    },
    Reorder {
        ids: Vec<String>,
    },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum TodoFilterArg {
    All,
    Active,
    Completed,
}

impl TodoFilterArg {
    fn as_rpc_value(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Active => "active",
            Self::Completed => "completed",
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum TodoPriorityArg {
    Low,
    Medium,
    High,
}

impl TodoPriorityArg {
    fn as_rpc_value(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[derive(Debug)]
struct CliError {
    message: String,
    is_connection: bool,
}

impl CliError {
    fn error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            is_connection: false,
        }
    }

    fn connection(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            is_connection: true,
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::from(0),
        Err(err) => {
            eprintln!("{}", err.message);
            if err.is_connection {
                ExitCode::from(2)
            } else {
                ExitCode::from(1)
            }
        }
    }
}

async fn run(cli: Cli) -> Result<(), CliError> {
    let ws_url = env::var("THREADMILL_CLI_WS_URL").unwrap_or_else(|_| DEFAULT_WS_URL.to_string());
    let auth_token = read_auth_token();

    match cli.command {
        Command::Thread { command } => match command {
            ThreadCommand::List { project, pretty } => {
                let params = if let Some(project_filter) = project {
                    let project_id =
                        resolve_project_id(&ws_url, auth_token.as_deref(), &project_filter).await?;
                    json!({ "project_id": project_id })
                } else {
                    json!({})
                };
                let result =
                    rpc_request(&ws_url, auth_token.as_deref(), "thread.list", params).await?;
                print_json(&result, pretty)
            }
            ThreadCommand::Create {
                project,
                name,
                branch,
                pretty,
            } => {
                let project_id =
                    resolve_project_id(&ws_url, auth_token.as_deref(), &project).await?;
                let mut params = json!({
                    "project_id": project_id,
                    "name": name,
                    "source_type": "new_feature",
                });
                if let Some(branch) = branch {
                    params["branch"] = json!(branch);
                }
                let result =
                    rpc_request(&ws_url, auth_token.as_deref(), "thread.create", params).await?;
                print_json(&result, pretty)
            }
            ThreadCommand::Close { thread_id, pretty } => {
                let result = rpc_request(
                    &ws_url,
                    auth_token.as_deref(),
                    "thread.close",
                    json!({ "thread_id": thread_id, "mode": "close" }),
                )
                .await?;
                print_json(&result, pretty)
            }
            ThreadCommand::Info { pretty } => {
                let current = env::var("THREADMILL_THREAD")
                    .map_err(|_| CliError::error("THREADMILL_THREAD is not set"))?;
                let list =
                    rpc_request(&ws_url, auth_token.as_deref(), "thread.list", json!({})).await?;
                let threads = list
                    .as_array()
                    .ok_or_else(|| CliError::error("thread.list returned non-array result"))?;

                let id_matches: Vec<Value> = threads
                    .iter()
                    .filter(|thread| {
                        thread.get("id").and_then(Value::as_str) == Some(current.as_str())
                    })
                    .cloned()
                    .collect();
                if id_matches.len() == 1 {
                    return print_json(&id_matches[0], pretty);
                }

                let name_matches: Vec<Value> = threads
                    .iter()
                    .filter(|thread| {
                        thread.get("name").and_then(Value::as_str) == Some(current.as_str())
                    })
                    .cloned()
                    .collect();

                if name_matches.is_empty() {
                    return Err(CliError::error(format!(
                        "no thread found for THREADMILL_THREAD={current}"
                    )));
                }

                if name_matches.len() == 1 {
                    return print_json(&name_matches[0], pretty);
                }

                print_json(&Value::Array(name_matches), pretty)
            }
        },
        Command::Project { command } => match command {
            ProjectCommand::List { pretty } => {
                let result =
                    rpc_request(&ws_url, auth_token.as_deref(), "project.list", json!({})).await?;
                print_json(&result, pretty)
            }
        },
        Command::Chat { command } => handle_chat(command, &ws_url, auth_token.as_deref()).await,
        Command::Todo { command } => handle_todo(command, &ws_url, auth_token.as_deref()).await,
        Command::Status { pretty } => {
            let ping = rpc_request(&ws_url, auth_token.as_deref(), "ping", json!({})).await?;
            let output = json!({
                "status": "ok",
                "daemon": "reachable",
                "ping": ping,
                "url": ws_url,
            });
            print_json(&output, pretty)
        }
    }
}

struct AgentDef {
    agent: String,
    display_name: Option<String>,
    system_prompt: String,
}

fn parse_agent_def(path: &std::path::Path) -> Result<AgentDef, CliError> {
    let content = fs::read_to_string(path)
        .map_err(|err| CliError::error(format!("failed to read {}: {err}", path.display())))?;

    // Parse YAML frontmatter between --- delimiters
    let (frontmatter, body) = if let Some(after_open) = content.strip_prefix("---\n") {
        if let Some(end) = after_open.find("\n---\n") {
            let fm = &after_open[..end];
            let body = &after_open[end + 5..];
            (fm, body.trim())
        } else {
            ("", content.as_str())
        }
    } else {
        ("", content.as_str())
    };

    let mut agent = String::new();
    let mut display_name = None;

    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("agent:") {
            agent = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("display_name:") {
            display_name = Some(value.trim().to_string());
        }
    }

    if agent.is_empty() {
        return Err(CliError::error(format!(
            "agent definition {} missing 'agent' field in frontmatter",
            path.display()
        )));
    }

    Ok(AgentDef {
        agent,
        display_name,
        system_prompt: body.to_string(),
    })
}

fn resolve_agent_def_path(name: &str) -> Result<std::path::PathBuf, CliError> {
    // Try project-level first: .threadmill/agents/<name>.md
    let project_path = std::path::Path::new(".threadmill/agents").join(format!("{name}.md"));
    if project_path.exists() {
        return Ok(project_path);
    }

    // Try global: ~/.config/threadmill/agents/<name>.md
    if let Some(config_dir) = dirs::config_dir() {
        let global_path = config_dir
            .join("threadmill/agents")
            .join(format!("{name}.md"));
        if global_path.exists() {
            return Ok(global_path);
        }
    }

    Err(CliError::error(format!(
        "agent definition '{name}' not found in .threadmill/agents/ or ~/.config/threadmill/agents/"
    )))
}

async fn handle_chat(
    command: ChatCommand,
    ws_url: &str,
    auth_token: Option<&str>,
) -> Result<(), CliError> {
    match command {
        ChatCommand::Start {
            agent,
            agent_def,
            prompt,
            system_prompt_file,
            display_name,
            parent_session,
            pretty,
        } => {
            // Resolve thread_id from THREADMILL_THREAD env var
            let thread_id = resolve_current_thread_id(ws_url, auth_token).await?;

            // Resolve agent definition if provided
            let (agent_name, sys_prompt, disp_name) = if let Some(def_name) = agent_def {
                let def_path = resolve_agent_def_path(&def_name)?;
                let def = parse_agent_def(&def_path)?;
                let dn = display_name.or(def.display_name);
                (def.agent, Some(def.system_prompt), dn)
            } else if let Some(agent_name) = agent {
                let sys = system_prompt_file
                    .map(|path| {
                        fs::read_to_string(&path)
                            .map_err(|err| CliError::error(format!("failed to read {path}: {err}")))
                    })
                    .transpose()?;
                (agent_name, sys, display_name)
            } else {
                return Err(CliError::error("either --agent or --agent-def is required"));
            };

            let mut params = json!({
                "thread_id": thread_id,
                "agent_name": agent_name,
            });
            if let Some(sp) = sys_prompt {
                params["system_prompt"] = json!(sp);
            }
            if let Some(ip) = prompt {
                params["initial_prompt"] = json!(ip);
            }
            if let Some(dn) = disp_name {
                params["display_name"] = json!(dn);
            }
            if let Some(ps) = parent_session {
                params["parent_session_id"] = json!(ps);
            }

            let result = rpc_request(ws_url, auth_token, "chat.start", params).await?;
            print_json(&result, pretty)
        }
        ChatCommand::Wait {
            session,
            timeout: timeout_secs,
        } => {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

            // Event-streaming wait: connect, send session.hello, then listen for events
            let (mut socket, _) = tokio_tungstenite::connect_async(ws_url)
                .await
                .map_err(|err| CliError::connection(format!("failed to connect: {err}")))?;

            // Send session.hello
            let hello = json!({
                "jsonrpc": "2.0",
                "id": HELLO_REQUEST_ID,
                "method": "session.hello",
                "params": {
                    "client": { "name": "threadmill-cli", "version": env!("CARGO_PKG_VERSION") },
                    "protocol_version": PROTOCOL_VERSION,
                    "capabilities": PROTOCOL_CAPABILITIES,
                },
            });
            socket
                .send(Message::Text(hello.to_string()))
                .await
                .map_err(|err| CliError::connection(format!("failed to send hello: {err}")))?;
            let _ = read_response_by_id(&mut socket, HELLO_REQUEST_ID, "session.hello").await?;

            // Listen for events until session ends or goes idle after being busy.
            // Print periodic status to stdout so calling processes (agent bash tools)
            // see activity and don't kill us for appearing hung.
            let mut was_busy = false;
            let mut last_heartbeat = std::time::Instant::now();
            let heartbeat_interval = std::time::Duration::from_secs(10);
            let started = std::time::Instant::now();

            eprintln!("waiting for session {session}...");

            loop {
                if std::time::Instant::now() > deadline {
                    eprintln!("timeout waiting for session {session}");
                    return Err(CliError::error("timeout"));
                }

                // Heartbeat: print elapsed time so calling process sees stdout activity
                if last_heartbeat.elapsed() >= heartbeat_interval {
                    let elapsed = started.elapsed().as_secs();
                    eprintln!("still waiting... ({elapsed}s elapsed)");
                    last_heartbeat = std::time::Instant::now();
                }

                let frame =
                    tokio::time::timeout(std::time::Duration::from_secs(5), socket.next()).await;

                match frame {
                    Ok(Some(Ok(Message::Text(text)))) => {
                        let value: Value = match serde_json::from_str(text.as_ref()) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };

                        // Skip RPC responses
                        if value.get("id").is_some() {
                            continue;
                        }

                        let method = value.get("method").and_then(Value::as_str).unwrap_or("");
                        let params = value.get("params").cloned().unwrap_or(json!({}));

                        match method {
                            "chat.status_changed" => {
                                let sid = params
                                    .get("session_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                if sid != session {
                                    continue;
                                }
                                let new_status = params
                                    .get("new_status")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                eprintln!("worker status: {new_status}");
                                match new_status {
                                    "busy" => {
                                        was_busy = true;
                                    }
                                    "idle" if was_busy => {
                                        eprintln!("worker finished (idle after busy)");
                                        return Ok(());
                                    }
                                    _ => {}
                                }
                            }
                            "chat.worker_update" => {
                                let sid = params
                                    .get("worker_session_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                if sid == session {
                                    if let Some(tool) = params.get("latest_tool") {
                                        let name =
                                            tool.get("name").and_then(Value::as_str).unwrap_or("?");
                                        let title =
                                            tool.get("title").and_then(Value::as_str).unwrap_or("");
                                        if title.is_empty() {
                                            eprintln!("worker: {name}");
                                        } else {
                                            eprintln!("worker: {name} {title}");
                                        }
                                    }
                                }
                            }
                            "chat.session_ended" | "chat.session_failed" => {
                                let sid = params
                                    .get("session_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                if sid == session {
                                    let reason = params
                                        .get("reason")
                                        .or_else(|| params.get("error"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("unknown");
                                    eprintln!("session ended: {reason}");
                                    return Err(CliError::error(format!(
                                        "session ended: {reason}"
                                    )));
                                }
                            }
                            "state.delta" => {
                                if let Some(ops) =
                                    params.get("operations").and_then(Value::as_array)
                                {
                                    for op in ops {
                                        let op_type =
                                            op.get("type").and_then(Value::as_str).unwrap_or("");
                                        if op_type == "chat.session_removed" {
                                            let sid = op
                                                .get("session_id")
                                                .and_then(Value::as_str)
                                                .unwrap_or("");
                                            if sid == session {
                                                eprintln!("session removed");
                                                return Err(CliError::error("session removed"));
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    Ok(Some(Ok(Message::Ping(payload)))) => {
                        let _ = socket.send(Message::Pong(payload)).await;
                    }
                    Ok(Some(Ok(Message::Close(_)))) => {
                        return Err(CliError::connection("connection closed while waiting"));
                    }
                    Ok(Some(Err(err))) => {
                        return Err(CliError::connection(format!("websocket error: {err}")));
                    }
                    Ok(None) => {
                        return Err(CliError::connection("connection closed while waiting"));
                    }
                    Err(_) => {
                        // 5s read timeout — loop continues, check deadline + heartbeat
                        continue;
                    }
                    _ => {}
                }
            }
        }
        ChatCommand::Status { session, pretty } => {
            let result = rpc_request(
                ws_url,
                auth_token,
                "chat.status",
                json!({ "session_id": session }),
            )
            .await?;
            print_json(&result, pretty)
        }
        ChatCommand::List { pretty } => {
            let thread_id = resolve_current_thread_id(ws_url, auth_token).await?;
            let result = rpc_request(
                ws_url,
                auth_token,
                "chat.list",
                json!({ "thread_id": thread_id }),
            )
            .await?;
            print_json(&result, pretty)
        }
    }
}

async fn handle_todo(
    command: TodoCommand,
    ws_url: &str,
    auth_token: Option<&str>,
) -> Result<(), CliError> {
    let thread_id = resolve_current_thread_id(ws_url, auth_token).await?;

    match command {
        TodoCommand::List { filter } => {
            let result = rpc_request(
                ws_url,
                auth_token,
                "todo.list",
                json!({ "thread_id": thread_id, "filter": filter.as_rpc_value() }),
            )
            .await?;
            print_json(&result, false)
        }
        TodoCommand::Add { content, priority } => {
            if content.trim().is_empty() {
                return Err(CliError::error("todo content must not be empty"));
            }

            let mut params = json!({ "thread_id": thread_id, "content": content });
            if let Some(priority) = priority {
                params["priority"] = json!(priority.as_rpc_value());
            }

            let result = rpc_request(ws_url, auth_token, "todo.add", params).await?;
            print_json(&result, false)
        }
        TodoCommand::Update {
            id,
            content,
            priority,
        } => {
            let mut params = json!({ "thread_id": thread_id, "todo_id": id });
            let mut changed = false;
            if let Some(content) = content {
                if content.trim().is_empty() {
                    return Err(CliError::error("todo content must not be empty"));
                }
                params["content"] = json!(content);
                changed = true;
            }
            if let Some(priority) = priority {
                params["priority"] = json!(priority.as_rpc_value());
                changed = true;
            }
            if !changed {
                return Err(CliError::error(
                    "todo update requires --content and/or --priority",
                ));
            }

            let result = rpc_request(ws_url, auth_token, "todo.update", params).await?;
            print_json(&result, false)
        }
        TodoCommand::Done { id } => {
            let result = rpc_request(
                ws_url,
                auth_token,
                "todo.update",
                json!({ "thread_id": thread_id, "todo_id": id, "completed": true }),
            )
            .await?;
            print_json(&result, false)
        }
        TodoCommand::Reopen { id } => {
            let result = rpc_request(
                ws_url,
                auth_token,
                "todo.update",
                json!({ "thread_id": thread_id, "todo_id": id, "completed": false }),
            )
            .await?;
            print_json(&result, false)
        }
        TodoCommand::Remove { id } => {
            let result = rpc_request(
                ws_url,
                auth_token,
                "todo.remove",
                json!({ "thread_id": thread_id, "todo_id": id }),
            )
            .await?;
            print_json(&result, false)
        }
        TodoCommand::Reorder { ids } => {
            if ids.is_empty() {
                return Err(CliError::error("todo reorder requires at least one id"));
            }

            let result = rpc_request(
                ws_url,
                auth_token,
                "todo.reorder",
                json!({ "thread_id": thread_id, "todo_ids": ids }),
            )
            .await?;
            print_json(&result, false)
        }
    }
}

async fn resolve_current_thread_id(
    ws_url: &str,
    auth_token: Option<&str>,
) -> Result<String, CliError> {
    let current = env::var("THREADMILL_THREAD").map_err(|_| {
        CliError::error(
            "THREADMILL_THREAD is not set — run from a threadmill tmux session or agent process",
        )
    })?;

    let list = rpc_request(ws_url, auth_token, "thread.list", json!({})).await?;
    let threads = list
        .as_array()
        .ok_or_else(|| CliError::error("thread.list returned non-array"))?;

    // Try by ID first
    if let Some(thread) = threads
        .iter()
        .find(|t| t.get("id").and_then(Value::as_str) == Some(&current))
    {
        return thread
            .get("id")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| CliError::error("thread missing id"));
    }

    // Try by name
    if let Some(thread) = threads
        .iter()
        .find(|t| t.get("name").and_then(Value::as_str) == Some(&current))
    {
        return thread
            .get("id")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| CliError::error("thread missing id"));
    }

    Err(CliError::error(format!(
        "no thread found for THREADMILL_THREAD={current}"
    )))
}

async fn resolve_project_id(
    ws_url: &str,
    auth_token: Option<&str>,
    input: &str,
) -> Result<String, CliError> {
    let projects = rpc_request(ws_url, auth_token, "project.list", json!({})).await?;
    let projects = projects
        .as_array()
        .ok_or_else(|| CliError::error("project.list returned non-array result"))?;

    if let Some(project) = projects
        .iter()
        .find(|project| project.get("id").and_then(Value::as_str) == Some(input))
    {
        return project
            .get("id")
            .and_then(Value::as_str)
            .map(|id| id.to_string())
            .ok_or_else(|| CliError::error("project entry missing id"));
    }

    if let Some(project) = projects
        .iter()
        .find(|project| project.get("name").and_then(Value::as_str) == Some(input))
    {
        return project
            .get("id")
            .and_then(Value::as_str)
            .map(|id| id.to_string())
            .ok_or_else(|| CliError::error("project entry missing id"));
    }

    Err(CliError::error(format!("project not found: {input}")))
}

async fn rpc_request(
    ws_url: &str,
    auth_token: Option<&str>,
    method: &str,
    params: Value,
) -> Result<Value, CliError> {
    let (mut socket, _) = connect_async(ws_url)
        .await
        .map_err(|err| CliError::connection(format!("failed to connect to {ws_url}: {err}")))?;

    let hello_request = json!({
        "jsonrpc": "2.0",
        "id": HELLO_REQUEST_ID,
        "method": "session.hello",
        "params": {
            "client": {
                "name": "threadmill-cli",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "protocol_version": PROTOCOL_VERSION,
            "capabilities": PROTOCOL_CAPABILITIES,
        },
    });

    socket
        .send(Message::Text(hello_request.to_string()))
        .await
        .map_err(|err| CliError::connection(format!("failed to send session.hello: {err}")))?;

    let _ = read_response_by_id(&mut socket, HELLO_REQUEST_ID, "session.hello").await?;

    let mut request = json!({
        "jsonrpc": "2.0",
        "id": REQUEST_ID,
        "method": method,
        "params": params,
    });
    if let Some(auth_token) = auth_token {
        request["auth_token"] = json!(auth_token);
    }

    socket
        .send(Message::Text(request.to_string()))
        .await
        .map_err(|err| CliError::connection(format!("failed to send {method}: {err}")))?;

    read_response_by_id(&mut socket, REQUEST_ID, method).await
}

async fn read_response_by_id(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    expected_id: u64,
    method: &str,
) -> Result<Value, CliError> {
    while let Some(frame) = socket.next().await {
        let frame =
            frame.map_err(|err| CliError::connection(format!("failed to read response: {err}")))?;

        match frame {
            Message::Text(text) => {
                let value: Value = serde_json::from_str(text.as_ref())
                    .map_err(|err| CliError::error(format!("invalid JSON response: {err}")))?;

                if value.get("id") != Some(&json!(expected_id)) {
                    continue;
                }

                if let Some(error) = value.get("error") {
                    let message = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown rpc error");
                    return Err(CliError::error(format!("{method} failed: {message}")));
                }

                let result = value
                    .get("result")
                    .cloned()
                    .ok_or_else(|| CliError::error("missing result in JSON-RPC response"))?;
                return Ok(result);
            }
            Message::Ping(payload) => {
                socket.send(Message::Pong(payload)).await.map_err(|err| {
                    CliError::connection(format!("failed to send websocket pong: {err}"))
                })?;
            }
            Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
            Message::Close(_) => {
                return Err(CliError::connection(
                    "connection closed before response was received",
                ));
            }
        }
    }

    Err(CliError::connection(
        "connection closed before response was received",
    ))
}

fn print_json(value: &Value, pretty: bool) -> Result<(), CliError> {
    let rendered = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    }
    .map_err(|err| CliError::error(format!("failed to serialize output: {err}")))?;

    println!("{rendered}");
    Ok(())
}

fn read_auth_token() -> Option<String> {
    let config_dir = dirs::config_dir()?;
    let path = config_dir.join("threadmill").join("auth_token");
    let token = fs::read_to_string(path).ok()?;
    let token = token.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}
