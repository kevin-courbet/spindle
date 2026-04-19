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
    Workflow {
        #[command(subcommand)]
        command: WorkflowCommand,
    },
    Issue {
        #[command(subcommand)]
        command: IssueCommand,
    },
    Status {
        #[arg(long)]
        pretty: bool,
    },
    /// Run as a stdio MCP server. Exposes the workflow.* surface (plus read-only
    /// thread / chat helpers) as MCP tools that any MCP-capable agent harness
    /// — Claude Code, Codex, OpenCode, Gemini CLI, … — can consume. Reads
    /// newline-delimited JSON-RPC from stdin, writes responses to stdout. Logs
    /// go to stderr.
    Mcp,
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
        /// Agent definition name (reads from `.threadmill/agents/<name>.md`)
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
enum WorkflowCommand {
    /// Create a new workflow for a thread (PLANNING phase)
    Create {
        /// Thread ID or name (defaults to THREADMILL_THREAD)
        #[arg(long)]
        thread_id: Option<String>,
        /// PRD issue URL
        #[arg(long)]
        prd_issue: Option<String>,
        /// Implementation issue URL (repeatable)
        #[arg(long = "implementation-issue")]
        implementation_issues: Vec<String>,
        #[arg(long)]
        pretty: bool,
    },
    /// Show current workflow state (phase, workers, reviewers, findings)
    Status {
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        pretty: bool,
    },
    /// List workflows, optionally filtered by thread
    List {
        /// Thread ID or name (defaults to THREADMILL_THREAD if set)
        #[arg(long)]
        thread_id: Option<String>,
        /// List all workflows across threads
        #[arg(long, conflicts_with = "thread_id")]
        all: bool,
        #[arg(long)]
        pretty: bool,
    },
    /// Transition workflow to a new phase
    Transition {
        #[arg(long)]
        workflow_id: String,
        #[arg(long, value_enum)]
        phase: WorkflowPhaseArg,
        /// Force transition bypassing state-machine validation
        #[arg(long)]
        force: bool,
        #[arg(long)]
        pretty: bool,
    },
    /// Spawn a worker chat session tracked by the workflow
    SpawnWorker {
        #[arg(long)]
        workflow_id: String,
        /// Agent binary name (e.g. "claude", "gemini", "opencode")
        #[arg(long)]
        agent: Option<String>,
        /// Agent definition name (reads from `.threadmill/agents/<name>.md`)
        #[arg(long)]
        agent_def: Option<String>,
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long)]
        system_prompt_file: Option<String>,
        #[arg(long)]
        display_name: Option<String>,
        #[arg(long)]
        parent_session: Option<String>,
        #[arg(long)]
        pretty: bool,
    },
    /// Record a worker handoff (completion, blocker, context-exhaustion, ...)
    RecordHandoff {
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        worker_id: String,
        #[arg(long, value_enum)]
        stop_reason: StopReasonArg,
        /// Context/summary. Literal text, or "@path" to read from file
        #[arg(long)]
        context: String,
        /// Progress bullet (repeatable)
        #[arg(long = "progress")]
        progress: Vec<String>,
        /// Next-step bullet (repeatable)
        #[arg(long = "next-step")]
        next_steps: Vec<String>,
        #[arg(long)]
        blockers: Option<String>,
        #[arg(long)]
        pretty: bool,
    },
    /// Transition workflow to REVIEWING and spawn reviewers
    StartReview {
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        force: bool,
        /// Reviewer specs as JSON array, or "@path" to read from file.
        /// Each entry: {"agent_name": "...", "system_prompt": "...", "initial_prompt": "...", "display_name": "...", "parent_session_id": "..."}
        #[arg(long)]
        reviewers: Option<String>,
        #[arg(long)]
        pretty: bool,
    },
    /// Spawn a single reviewer session
    SpawnReviewer {
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        agent_def: Option<String>,
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long)]
        system_prompt_file: Option<String>,
        #[arg(long)]
        display_name: Option<String>,
        #[arg(long)]
        parent_session: Option<String>,
        #[arg(long)]
        pretty: bool,
    },
    /// List reviewer sessions attached to a workflow
    ListReviewers {
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        pretty: bool,
    },
    /// Record findings synthesized from the review swarm
    RecordFindings {
        #[arg(long)]
        workflow_id: String,
        /// Findings as JSON array, or "@path" to read from file.
        /// Each entry: {"severity": "LOW|MEDIUM|HIGH", "summary": "...", "details": "...", "source_reviewers": [...], "file_path": "...", "line": 42}
        #[arg(long)]
        findings: String,
        #[arg(long)]
        pretty: bool,
    },
    /// Mark workflow COMPLETE
    Complete {
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        pretty: bool,
    },
    /// Toggle a finding's resolved flag (default: mark resolved)
    ResolveFinding {
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        finding_id: String,
        /// Pass `--unresolve` to flip an accidentally-resolved finding back
        #[arg(long)]
        unresolve: bool,
        #[arg(long)]
        pretty: bool,
    },
    /// Append an implementation issue URL to a workflow's linked issues list
    AddLinkedIssue {
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        url: String,
        #[arg(long)]
        pretty: bool,
    },
    /// Resolve live state for the workflow's PRD + all implementation issues
    ResolveLinkedIssues {
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        pretty: bool,
    },
}

#[derive(Subcommand, Debug)]
enum IssueCommand {
    /// List issues on a project (filter by label, state, scope)
    List {
        #[arg(long)]
        project_id: String,
        #[arg(long)]
        label: Option<String>,
        #[arg(long, value_enum)]
        state: Option<IssueStateArg>,
        /// Scope selector: `all`, `prds`, or `linked-to` (requires --workflow-id)
        #[arg(long, value_enum)]
        scope: Option<IssueScopeArg>,
        /// Required when `--scope linked-to`
        #[arg(long, required_if_eq("scope", "linked-to"))]
        workflow_id: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
        #[arg(long)]
        bypass_cache: bool,
        #[arg(long)]
        pretty: bool,
    },
    /// Resolve live state for a single issue by URL
    Resolve {
        #[arg(long)]
        project_id: String,
        #[arg(long)]
        url: String,
        #[arg(long)]
        pretty: bool,
    },
    /// Close an issue (optionally posts reason as comment first)
    Close {
        #[arg(long)]
        project_id: String,
        #[arg(long)]
        url: String,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long)]
        by_worker_id: Option<String>,
        #[arg(long)]
        pretty: bool,
    },
    /// Post a comment on an issue
    Comment {
        #[arg(long)]
        project_id: String,
        #[arg(long)]
        url: String,
        #[arg(long)]
        body: String,
        #[arg(long)]
        by_worker_id: Option<String>,
        #[arg(long)]
        pretty: bool,
    },
    /// Create a new issue (optionally link to an existing workflow)
    Create {
        #[arg(long)]
        project_id: String,
        #[arg(long)]
        title: String,
        #[arg(long)]
        body: String,
        #[arg(long = "label")]
        labels: Vec<String>,
        #[arg(long = "assignee")]
        assignees: Vec<String>,
        #[arg(long)]
        link_to_workflow_id: Option<String>,
        #[arg(long)]
        pretty: bool,
    },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum IssueStateArg {
    Open,
    Closed,
    All,
}

impl IssueStateArg {
    fn as_rpc_value(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::All => "all",
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum IssueScopeArg {
    All,
    Prds,
    LinkedTo,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum WorkflowPhaseArg {
    Planning,
    Implementing,
    Testing,
    Reviewing,
    Fixing,
    Complete,
    Blocked,
    Failed,
}

impl WorkflowPhaseArg {
    fn as_rpc_value(self) -> &'static str {
        match self {
            Self::Planning => "PLANNING",
            Self::Implementing => "IMPLEMENTING",
            Self::Testing => "TESTING",
            Self::Reviewing => "REVIEWING",
            Self::Fixing => "FIXING",
            Self::Complete => "COMPLETE",
            Self::Blocked => "BLOCKED",
            Self::Failed => "FAILED",
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum StopReasonArg {
    Done,
    ContextExhausted,
    BlockedNeedInfo,
    BlockedTechnical,
    QualityConcern,
    ScopeCreep,
}

impl StopReasonArg {
    fn as_rpc_value(self) -> &'static str {
        match self {
            Self::Done => "DONE",
            Self::ContextExhausted => "CONTEXT_EXHAUSTED",
            Self::BlockedNeedInfo => "BLOCKED_NEED_INFO",
            Self::BlockedTechnical => "BLOCKED_TECHNICAL",
            Self::QualityConcern => "QUALITY_CONCERN",
            Self::ScopeCreep => "SCOPE_CREEP",
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
        Command::Workflow { command } => {
            handle_workflow(command, &ws_url, auth_token.as_deref()).await
        }
        Command::Issue { command } => {
            handle_issue(command, &ws_url, auth_token.as_deref()).await
        }
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
        Command::Mcp => run_mcp_server(&ws_url, auth_token).await,
    }
}

struct AgentDef {
    agent: String,
    display_name: Option<String>,
    system_prompt: String,
    /// Optional ACP model override (e.g. `claude-opus-4-7`). Wired through to
    /// `ChatStartParams.preferred_model` so the persona pins its LLM.
    model: Option<String>,
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
    let mut model: Option<String> = None;

    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("agent:") {
            agent = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("display_name:") {
            display_name = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("model:") {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                model = Some(trimmed.to_string());
            }
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
        model,
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
            let (agent_name, sys_prompt, disp_name, pref_model) = if let Some(def_name) = agent_def
            {
                let def_path = resolve_agent_def_path(&def_name)?;
                let def = parse_agent_def(&def_path)?;
                let dn = display_name.or(def.display_name);
                (def.agent, Some(def.system_prompt), dn, def.model)
            } else if let Some(agent_name) = agent {
                let sys = system_prompt_file
                    .map(|path| {
                        fs::read_to_string(&path)
                            .map_err(|err| CliError::error(format!("failed to read {path}: {err}")))
                    })
                    .transpose()?;
                (agent_name, sys, display_name, None)
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
            if let Some(model) = pref_model {
                params["preferred_model"] = json!(model);
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
                .send(Message::Text(hello.to_string().into()))
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

/// Read a value that is either literal text or `@<path>` pointing at a file.
fn resolve_text_or_file(input: &str) -> Result<String, CliError> {
    if let Some(path) = input.strip_prefix('@') {
        fs::read_to_string(path)
            .map(|s| s.trim_end_matches('\n').to_string())
            .map_err(|err| CliError::error(format!("failed to read {path}: {err}")))
    } else {
        Ok(input.to_string())
    }
}

struct AgentPayload {
    agent_name: String,
    system_prompt: Option<String>,
    initial_prompt: Option<String>,
    display_name: Option<String>,
    preferred_model: Option<String>,
}

/// Resolve an optional agent / agent_def pair into the worker spawn payload fields.
fn resolve_agent_payload(
    agent: Option<String>,
    agent_def: Option<String>,
    prompt: Option<String>,
    system_prompt_file: Option<String>,
    display_name: Option<String>,
) -> Result<AgentPayload, CliError> {
    if let Some(def_name) = agent_def {
        if system_prompt_file.is_some() {
            return Err(CliError::error(
                "--system-prompt-file cannot be combined with --agent-def \
                 (agent definitions already embed the system prompt)",
            ));
        }
        let def_path = resolve_agent_def_path(&def_name)?;
        let def = parse_agent_def(&def_path)?;
        Ok(AgentPayload {
            agent_name: def.agent,
            system_prompt: Some(def.system_prompt),
            initial_prompt: prompt,
            display_name: display_name.or(def.display_name),
            preferred_model: def.model,
        })
    } else if let Some(agent_name) = agent {
        let system_prompt = system_prompt_file
            .map(|path| {
                fs::read_to_string(&path)
                    .map(|s| s.trim_end_matches('\n').to_string())
                    .map_err(|err| CliError::error(format!("failed to read {path}: {err}")))
            })
            .transpose()?;
        Ok(AgentPayload {
            agent_name,
            system_prompt,
            initial_prompt: prompt,
            display_name,
            preferred_model: None,
        })
    } else {
        Err(CliError::error("either --agent or --agent-def is required"))
    }
}

fn build_spawn_params(
    workflow_id: String,
    payload: AgentPayload,
    parent_session: Option<String>,
) -> Value {
    let mut params = json!({
        "workflow_id": workflow_id,
        "agent_name": payload.agent_name,
    });
    if let Some(sp) = payload.system_prompt {
        params["system_prompt"] = json!(sp);
    }
    if let Some(p) = payload.initial_prompt {
        params["initial_prompt"] = json!(p);
    }
    if let Some(d) = payload.display_name {
        params["display_name"] = json!(d);
    }
    if let Some(ps) = parent_session {
        params["parent_session_id"] = json!(ps);
    }
    if let Some(model) = payload.preferred_model {
        params["preferred_model"] = json!(model);
    }
    params
}

async fn resolve_thread_arg(
    ws_url: &str,
    auth_token: Option<&str>,
    input: &str,
) -> Result<String, CliError> {
    let list = rpc_request(ws_url, auth_token, "thread.list", json!({})).await?;
    let threads = list
        .as_array()
        .ok_or_else(|| CliError::error("thread.list returned non-array"))?;

    if let Some(t) = threads
        .iter()
        .find(|t| t.get("id").and_then(Value::as_str) == Some(input))
    {
        return t
            .get("id")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| CliError::error("thread missing id"));
    }
    if let Some(t) = threads
        .iter()
        .find(|t| t.get("name").and_then(Value::as_str) == Some(input))
    {
        return t
            .get("id")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| CliError::error("thread missing id"));
    }
    Err(CliError::error(format!("no thread found for {input}")))
}

async fn handle_workflow(
    command: WorkflowCommand,
    ws_url: &str,
    auth_token: Option<&str>,
) -> Result<(), CliError> {
    match command {
        WorkflowCommand::Create {
            thread_id,
            prd_issue,
            implementation_issues,
            pretty,
        } => {
            let resolved_thread_id = match thread_id {
                Some(ref t) => resolve_thread_arg(ws_url, auth_token, t).await?,
                None => resolve_current_thread_id(ws_url, auth_token).await?,
            };
            let mut params = json!({ "thread_id": resolved_thread_id });
            if let Some(url) = prd_issue {
                params["prd_issue_url"] = json!(url);
            }
            if !implementation_issues.is_empty() {
                params["implementation_issue_urls"] = json!(implementation_issues);
            }
            let result = rpc_request(ws_url, auth_token, "workflow.create", params).await?;
            print_json(&result, pretty)
        }
        WorkflowCommand::Status {
            workflow_id,
            pretty,
        } => {
            let result = rpc_request(
                ws_url,
                auth_token,
                "workflow.status",
                json!({ "workflow_id": workflow_id }),
            )
            .await?;
            print_json(&result, pretty)
        }
        WorkflowCommand::List {
            thread_id,
            all,
            pretty,
        } => {
            let params = if all {
                json!({})
            } else if let Some(t) = thread_id {
                let resolved = resolve_thread_arg(ws_url, auth_token, &t).await?;
                json!({ "thread_id": resolved })
            } else if let Ok(current) = env::var("THREADMILL_THREAD") {
                let resolved = resolve_thread_arg(ws_url, auth_token, &current).await?;
                json!({ "thread_id": resolved })
            } else {
                json!({})
            };
            let result = rpc_request(ws_url, auth_token, "workflow.list", params).await?;
            print_json(&result, pretty)
        }
        WorkflowCommand::Transition {
            workflow_id,
            phase,
            force,
            pretty,
        } => {
            let result = rpc_request(
                ws_url,
                auth_token,
                "workflow.transition",
                json!({
                    "workflow_id": workflow_id,
                    "phase": phase.as_rpc_value(),
                    "force": force,
                }),
            )
            .await?;
            print_json(&result, pretty)
        }
        WorkflowCommand::SpawnWorker {
            workflow_id,
            agent,
            agent_def,
            prompt,
            system_prompt_file,
            display_name,
            parent_session,
            pretty,
        } => {
            let payload =
                resolve_agent_payload(agent, agent_def, prompt, system_prompt_file, display_name)?;
            let params = build_spawn_params(workflow_id, payload, parent_session);
            let result = rpc_request(ws_url, auth_token, "workflow.spawn_worker", params).await?;
            print_json(&result, pretty)
        }
        WorkflowCommand::RecordHandoff {
            workflow_id,
            worker_id,
            stop_reason,
            context,
            progress,
            next_steps,
            blockers,
            pretty,
        } => {
            let context_text = resolve_text_or_file(&context)?;
            let mut params = json!({
                "workflow_id": workflow_id,
                "worker_id": worker_id,
                "stop_reason": stop_reason.as_rpc_value(),
                "context": context_text,
            });
            if !progress.is_empty() {
                params["progress"] = json!(progress);
            }
            if !next_steps.is_empty() {
                params["next_steps"] = json!(next_steps);
            }
            if let Some(b) = blockers {
                params["blockers"] = json!(b);
            }
            let result = rpc_request(ws_url, auth_token, "workflow.record_handoff", params).await?;
            print_json(&result, pretty)
        }
        WorkflowCommand::StartReview {
            workflow_id,
            force,
            reviewers,
            pretty,
        } => {
            let mut params = json!({
                "workflow_id": workflow_id,
                "force": force,
            });
            if let Some(raw) = reviewers {
                let json_text = resolve_text_or_file(&raw)?;
                let parsed: Value = serde_json::from_str(&json_text)
                    .map_err(|err| CliError::error(format!("invalid --reviewers JSON: {err}")))?;
                if !parsed.is_array() {
                    return Err(CliError::error("--reviewers must be a JSON array"));
                }
                params["reviewers"] = parsed;
            }
            let result = rpc_request(ws_url, auth_token, "workflow.start_review", params).await?;
            print_json(&result, pretty)
        }
        WorkflowCommand::SpawnReviewer {
            workflow_id,
            agent,
            agent_def,
            prompt,
            system_prompt_file,
            display_name,
            parent_session,
            pretty,
        } => {
            let payload =
                resolve_agent_payload(agent, agent_def, prompt, system_prompt_file, display_name)?;
            let params = build_spawn_params(workflow_id, payload, parent_session);
            let result = rpc_request(ws_url, auth_token, "workflow.spawn_reviewer", params).await?;
            print_json(&result, pretty)
        }
        WorkflowCommand::ListReviewers {
            workflow_id,
            pretty,
        } => {
            let result = rpc_request(
                ws_url,
                auth_token,
                "workflow.list_reviewers",
                json!({ "workflow_id": workflow_id }),
            )
            .await?;
            print_json(&result, pretty)
        }
        WorkflowCommand::RecordFindings {
            workflow_id,
            findings,
            pretty,
        } => {
            let json_text = resolve_text_or_file(&findings)?;
            let parsed: Value = serde_json::from_str(&json_text)
                .map_err(|err| CliError::error(format!("invalid --findings JSON: {err}")))?;
            if !parsed.is_array() {
                return Err(CliError::error("--findings must be a JSON array"));
            }
            let result = rpc_request(
                ws_url,
                auth_token,
                "workflow.record_findings",
                json!({
                    "workflow_id": workflow_id,
                    "findings": parsed,
                }),
            )
            .await?;
            print_json(&result, pretty)
        }
        WorkflowCommand::Complete {
            workflow_id,
            force,
            pretty,
        } => {
            let result = rpc_request(
                ws_url,
                auth_token,
                "workflow.complete",
                json!({ "workflow_id": workflow_id, "force": force }),
            )
            .await?;
            print_json(&result, pretty)
        }
        WorkflowCommand::ResolveFinding {
            workflow_id,
            finding_id,
            unresolve,
            pretty,
        } => {
            let result = rpc_request(
                ws_url,
                auth_token,
                "workflow.resolve_finding",
                json!({
                    "workflow_id": workflow_id,
                    "finding_id": finding_id,
                    "resolved": !unresolve,
                }),
            )
            .await?;
            print_json(&result, pretty)
        }
        WorkflowCommand::AddLinkedIssue {
            workflow_id,
            url,
            pretty,
        } => {
            let result = rpc_request(
                ws_url,
                auth_token,
                "workflow.add_linked_issue",
                json!({ "workflow_id": workflow_id, "url": url }),
            )
            .await?;
            print_json(&result, pretty)
        }
        WorkflowCommand::ResolveLinkedIssues {
            workflow_id,
            pretty,
        } => {
            let result = rpc_request(
                ws_url,
                auth_token,
                "workflow.resolve_linked_issues",
                json!({ "workflow_id": workflow_id }),
            )
            .await?;
            print_json(&result, pretty)
        }
    }
}

async fn handle_issue(
    command: IssueCommand,
    ws_url: &str,
    auth_token: Option<&str>,
) -> Result<(), CliError> {
    match command {
        IssueCommand::List {
            project_id,
            label,
            state,
            scope,
            workflow_id,
            limit,
            bypass_cache,
            pretty,
        } => {
            let mut params = json!({ "project_id": project_id });
            if let Some(label) = label {
                params["label"] = json!(label);
            }
            if let Some(state) = state {
                params["state"] = json!(state.as_rpc_value());
            }
            if let Some(scope) = scope {
                let scope_value = match scope {
                    IssueScopeArg::All => json!({ "kind": "all" }),
                    IssueScopeArg::Prds => json!({ "kind": "prds" }),
                    IssueScopeArg::LinkedTo => {
                        // clap's required_if_eq guarantees workflow_id is Some here.
                        let wid = workflow_id.ok_or_else(|| {
                            CliError::error("--workflow-id is required when --scope linked-to")
                        })?;
                        json!({ "kind": "linked_to", "workflow_id": wid })
                    }
                };
                params["scope"] = scope_value;
            }
            if let Some(limit) = limit {
                params["limit"] = json!(limit);
            }
            if bypass_cache {
                params["bypass_cache"] = json!(true);
            }
            let result = rpc_request(ws_url, auth_token, "issue.list", params).await?;
            print_json(&result, pretty)
        }
        IssueCommand::Resolve {
            project_id,
            url,
            pretty,
        } => {
            let result = rpc_request(
                ws_url,
                auth_token,
                "issue.resolve",
                json!({ "project_id": project_id, "url": url }),
            )
            .await?;
            print_json(&result, pretty)
        }
        IssueCommand::Close {
            project_id,
            url,
            reason,
            by_worker_id,
            pretty,
        } => {
            let mut params = json!({ "project_id": project_id, "url": url });
            if let Some(reason) = reason {
                params["reason"] = json!(reason);
            }
            if let Some(wid) = by_worker_id {
                params["by_worker_id"] = json!(wid);
            }
            let result = rpc_request(ws_url, auth_token, "issue.close", params).await?;
            print_json(&result, pretty)
        }
        IssueCommand::Comment {
            project_id,
            url,
            body,
            by_worker_id,
            pretty,
        } => {
            let mut params = json!({ "project_id": project_id, "url": url, "body": body });
            if let Some(wid) = by_worker_id {
                params["by_worker_id"] = json!(wid);
            }
            let result = rpc_request(ws_url, auth_token, "issue.comment", params).await?;
            print_json(&result, pretty)
        }
        IssueCommand::Create {
            project_id,
            title,
            body,
            labels,
            assignees,
            link_to_workflow_id,
            pretty,
        } => {
            let mut params = json!({
                "project_id": project_id,
                "draft": {
                    "title": title,
                    "body": body,
                    "labels": labels,
                    "assignees": assignees,
                },
            });
            if let Some(wid) = link_to_workflow_id {
                params["link_to_workflow_id"] = json!(wid);
            }
            let result = rpc_request(ws_url, auth_token, "issue.create", params).await?;
            print_json(&result, pretty)
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
        .send(Message::Text(hello_request.to_string().into()))
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
        .send(Message::Text(request.to_string().into()))
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

// ===================================================================
//   MCP stdio server
// ===================================================================
//
// Implements the Model Context Protocol (2024-11-05 wire version) over stdio.
// Newline-delimited JSON-RPC 2.0. The server exposes the workflow.* CLI
// surface (plus read-only thread / chat helpers) as MCP tools that any
// MCP-capable agent harness — Claude Code, Codex, OpenCode, Gemini CLI —
// can dial by spawning `threadmill-cli mcp` as a subprocess.
//
// All tool calls are routed through the existing `rpc_request` helper, so
// the MCP server is a thin translation layer in front of Spindle's
// WebSocket JSON-RPC API. Logs intentionally go to stderr; stdout is
// reserved exclusively for MCP protocol frames.

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Protocol versions we're willing to negotiate. If the client asks for one of
/// these we echo it back; otherwise we fall back to `MCP_PROTOCOL_VERSION` and
/// let the client decide whether to continue or disconnect.
const SUPPORTED_MCP_VERSIONS: &[&str] = &["2024-11-05"];

struct McpTool {
    name: &'static str,
    description: &'static str,
    input_schema: Value,
    dispatch: McpDispatch,
}

/// How to turn a `tools/call.arguments` object into a Spindle JSON-RPC call.
enum McpDispatch {
    /// Direct call: forward the given method name with the raw arguments object as params.
    Direct(&'static str),
    /// Needs thread context: reads `thread_id` from `THREADMILL_THREAD` env var, injects
    /// it into the params under the given key, then forwards to `method`.
    WithCurrentThread {
        method: &'static str,
        inject_key: &'static str,
    },
}

fn mcp_tool_catalog() -> Vec<McpTool> {
    vec![
        // --- workflow.* (mirror the CLI surface) ---
        McpTool {
            name: "workflow_create",
            description: "Create a new Sisyphus workflow for the current thread. Optionally link a PRD issue URL \
                          and implementation-issue URLs. Returns the new workflow_id.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prd_issue_url": {"type": "string", "description": "PRD issue URL (optional)."},
                    "implementation_issue_urls": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Implementation issue URLs (optional)."
                    },
                    "orchestrator_session_id": {"type": "string", "description": "Chat session that drives the workflow. Defaults to the caller's session if omitted."}
                }
            }),
            dispatch: McpDispatch::WithCurrentThread {
                method: "workflow.create",
                inject_key: "thread_id",
            },
        },
        McpTool {
            name: "workflow_status",
            description: "Fetch the current state of a workflow (phase, workers, reviewers, findings, linked issues).",
            input_schema: json!({
                "type": "object",
                "required": ["workflow_id"],
                "properties": {"workflow_id": {"type": "string"}}
            }),
            dispatch: McpDispatch::Direct("workflow.status"),
        },
        McpTool {
            name: "workflow_list",
            description: "List workflows. With no arguments, lists workflows for the current thread.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "thread_id": {"type": "string", "description": "Filter by thread. Defaults to current thread if omitted."}
                }
            }),
            dispatch: McpDispatch::WithCurrentThread {
                method: "workflow.list",
                inject_key: "thread_id",
            },
        },
        McpTool {
            name: "workflow_transition",
            description: "Transition a workflow to a new phase. Pass force=true to bypass state-machine validation \
                          (user override). Valid phases: PLANNING, IMPLEMENTING, TESTING, REVIEWING, FIXING, COMPLETE, BLOCKED, FAILED.",
            input_schema: json!({
                "type": "object",
                "required": ["workflow_id", "phase"],
                "properties": {
                    "workflow_id": {"type": "string"},
                    "phase": {"type": "string", "enum": ["PLANNING","IMPLEMENTING","TESTING","REVIEWING","FIXING","COMPLETE","BLOCKED","FAILED"]},
                    "force": {"type": "boolean", "default": false}
                }
            }),
            dispatch: McpDispatch::Direct("workflow.transition"),
        },
        McpTool {
            name: "workflow_spawn_worker",
            description: "Spawn a Hephaestus worker chat session tracked by the workflow. agent_name is the harness id \
                          (claude, codex, opencode, gemini, …). system_prompt and initial_prompt are optional.",
            input_schema: json!({
                "type": "object",
                "required": ["workflow_id", "agent_name"],
                "properties": {
                    "workflow_id": {"type": "string"},
                    "agent_name": {"type": "string"},
                    "system_prompt": {"type": "string"},
                    "initial_prompt": {"type": "string"},
                    "display_name": {"type": "string"},
                    "parent_session_id": {"type": "string"},
                    "preferred_model": {"type": "string"}
                }
            }),
            dispatch: McpDispatch::Direct("workflow.spawn_worker"),
        },
        McpTool {
            name: "workflow_record_handoff",
            description: "Record a worker's handoff. stop_reason is one of DONE / CONTEXT_EXHAUSTED / BLOCKED_NEED_INFO / \
                          BLOCKED_TECHNICAL / QUALITY_CONCERN / SCOPE_CREEP. context is required (multi-line summary).",
            input_schema: json!({
                "type": "object",
                "required": ["workflow_id", "worker_id", "stop_reason", "context"],
                "properties": {
                    "workflow_id": {"type": "string"},
                    "worker_id": {"type": "string"},
                    "stop_reason": {"type": "string", "enum": ["DONE","CONTEXT_EXHAUSTED","BLOCKED_NEED_INFO","BLOCKED_TECHNICAL","QUALITY_CONCERN","SCOPE_CREEP"]},
                    "context": {"type": "string"},
                    "progress": {"type": "array", "items": {"type": "string"}},
                    "next_steps": {"type": "array", "items": {"type": "string"}},
                    "blockers": {"type": "string"}
                }
            }),
            dispatch: McpDispatch::Direct("workflow.record_handoff"),
        },
        McpTool {
            name: "workflow_start_review",
            description: "Transition the workflow to REVIEWING and kick off the review swarm. With an empty reviewers[] \
                          (or omitted), Spindle auto-selects reviewers from git diff main...HEAD. Pass force=true to \
                          allow transitioning from phases that would otherwise be invalid.",
            input_schema: json!({
                "type": "object",
                "required": ["workflow_id"],
                "properties": {
                    "workflow_id": {"type": "string"},
                    "force": {"type": "boolean", "default": false},
                    "reviewers": {
                        "type": "array",
                        "description": "Optional explicit reviewer specs. Omit for auto-selection from git diff.",
                        "items": {
                            "type": "object",
                            "required": ["agent_name"],
                            "properties": {
                                "agent_name": {"type": "string", "description": "Harness id (claude, codex, opencode, gemini, …)."},
                                "system_prompt": {"type": "string"},
                                "initial_prompt": {"type": "string"},
                                "display_name": {"type": "string"},
                                "parent_session_id": {"type": "string"},
                                "preferred_model": {"type": "string"}
                            }
                        }
                    }
                }
            }),
            dispatch: McpDispatch::Direct("workflow.start_review"),
        },
        McpTool {
            name: "workflow_spawn_reviewer",
            description: "Spawn an individual reviewer session during a review round. Use for ad-hoc specialist reviewers \
                          beyond the auto-selected swarm.",
            input_schema: json!({
                "type": "object",
                "required": ["workflow_id", "agent_name"],
                "properties": {
                    "workflow_id": {"type": "string"},
                    "agent_name": {"type": "string"},
                    "system_prompt": {"type": "string"},
                    "initial_prompt": {"type": "string"},
                    "display_name": {"type": "string"},
                    "parent_session_id": {"type": "string"},
                    "preferred_model": {"type": "string"}
                }
            }),
            dispatch: McpDispatch::Direct("workflow.spawn_reviewer"),
        },
        McpTool {
            name: "workflow_list_reviewers",
            description: "List reviewer sessions attached to a workflow. Includes status and session_id for each.",
            input_schema: json!({
                "type": "object",
                "required": ["workflow_id"],
                "properties": {"workflow_id": {"type": "string"}}
            }),
            dispatch: McpDispatch::Direct("workflow.list_reviewers"),
        },
        McpTool {
            name: "workflow_record_findings",
            description: "Record synthesized findings for the current review round. Each finding needs severity \
                          (LOW/MEDIUM/HIGH), summary, details, and optionally source_reviewers / file_path / line.",
            input_schema: json!({
                "type": "object",
                "required": ["workflow_id", "findings"],
                "properties": {
                    "workflow_id": {"type": "string"},
                    "findings": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "required": ["severity", "summary", "details"],
                            "properties": {
                                "severity": {"type": "string", "enum": ["LOW", "MEDIUM", "HIGH"]},
                                "summary": {"type": "string"},
                                "details": {"type": "string"},
                                "source_reviewers": {"type": "array", "items": {"type": "string"}},
                                "file_path": {"type": "string"},
                                "line": {"type": "integer"}
                            }
                        }
                    }
                }
            }),
            dispatch: McpDispatch::Direct("workflow.record_findings"),
        },
        McpTool {
            name: "workflow_resolve_finding",
            description: "Mark a finding resolved (or unresolved). Default resolved=true.",
            input_schema: json!({
                "type": "object",
                "required": ["workflow_id", "finding_id"],
                "properties": {
                    "workflow_id": {"type": "string"},
                    "finding_id": {"type": "string"},
                    "resolved": {"type": "boolean", "default": true}
                }
            }),
            dispatch: McpDispatch::Direct("workflow.resolve_finding"),
        },
        McpTool {
            name: "workflow_complete",
            description: "Mark the workflow COMPLETE. force=true skips validation.",
            input_schema: json!({
                "type": "object",
                "required": ["workflow_id"],
                "properties": {
                    "workflow_id": {"type": "string"},
                    "force": {"type": "boolean", "default": false}
                }
            }),
            dispatch: McpDispatch::Direct("workflow.complete"),
        },
        // --- issue.* surface ---
        McpTool {
            name: "issue_list",
            description: "List issues on a project. Supports filtering by label, state (open/closed/all), \
                          and scope (all issues, PRD-labeled only, or issues linked to a specific workflow).",
            input_schema: json!({
                "type": "object",
                "required": ["project_id"],
                "properties": {
                    "project_id": {"type": "string"},
                    "label": {"type": "string"},
                    "state": {"type": "string", "enum": ["open", "closed", "all"]},
                    "scope": {
                        "type": "object",
                        "required": ["kind"],
                        "properties": {
                            "kind": {"type": "string", "enum": ["all", "prds", "linked_to"]},
                            "workflow_id": {"type": "string", "description": "Required when kind=linked_to."}
                        }
                    },
                    "limit": {"type": "integer"},
                    "bypass_cache": {"type": "boolean"}
                }
            }),
            dispatch: McpDispatch::Direct("issue.list"),
        },
        McpTool {
            name: "issue_resolve",
            description: "Fetch the live state of a single issue by URL — state, labels, assignees, and full body.",
            input_schema: json!({
                "type": "object",
                "required": ["project_id", "url"],
                "properties": {
                    "project_id": {"type": "string"},
                    "url": {"type": "string"}
                }
            }),
            dispatch: McpDispatch::Direct("issue.resolve"),
        },
        McpTool {
            name: "issue_close",
            description: "Close an issue. Optionally posts a reason as a comment before closing. \
                          Emits a workflow.issue_closed event if the issue is linked to any active workflow.",
            input_schema: json!({
                "type": "object",
                "required": ["project_id", "url"],
                "properties": {
                    "project_id": {"type": "string"},
                    "url": {"type": "string"},
                    "reason": {"type": "string"},
                    "by_worker_id": {"type": "string"}
                }
            }),
            dispatch: McpDispatch::Direct("issue.close"),
        },
        McpTool {
            name: "issue_comment",
            description: "Post a comment on an issue. Emits workflow.issue_commented.",
            input_schema: json!({
                "type": "object",
                "required": ["project_id", "url", "body"],
                "properties": {
                    "project_id": {"type": "string"},
                    "url": {"type": "string"},
                    "body": {"type": "string"},
                    "by_worker_id": {"type": "string"}
                }
            }),
            dispatch: McpDispatch::Direct("issue.comment"),
        },
        McpTool {
            name: "issue_create",
            description: "Create a new issue. Optionally links it to an existing workflow, which appends \
                          the new URL to the workflow's implementation_issue_urls and updates the inspector.",
            input_schema: json!({
                "type": "object",
                "required": ["project_id", "draft"],
                "properties": {
                    "project_id": {"type": "string"},
                    "draft": {
                        "type": "object",
                        "required": ["title", "body"],
                        "properties": {
                            "title": {"type": "string"},
                            "body": {"type": "string"},
                            "labels": {"type": "array", "items": {"type": "string"}},
                            "assignees": {"type": "array", "items": {"type": "string"}}
                        }
                    },
                    "link_to_workflow_id": {"type": "string"}
                }
            }),
            dispatch: McpDispatch::Direct("issue.create"),
        },
        McpTool {
            name: "workflow_add_linked_issue",
            description: "Append an implementation issue URL to a workflow's linked issues list. \
                          Dedupes existing URLs. Emits a workflow upsert state delta.",
            input_schema: json!({
                "type": "object",
                "required": ["workflow_id", "url"],
                "properties": {
                    "workflow_id": {"type": "string"},
                    "url": {"type": "string"}
                }
            }),
            dispatch: McpDispatch::Direct("workflow.add_linked_issue"),
        },
        McpTool {
            name: "workflow_resolve_linked_issues",
            description: "Resolve live state for the workflow's PRD and all implementation issues in one call. \
                          Returns EnrichedIssueWire for each, with null entries for URLs the transport cannot find.",
            input_schema: json!({
                "type": "object",
                "required": ["workflow_id"],
                "properties": {"workflow_id": {"type": "string"}}
            }),
            dispatch: McpDispatch::Direct("workflow.resolve_linked_issues"),
        },
        // --- read-only helpers ---
        McpTool {
            name: "thread_list",
            description: "List active threads. Useful for agents that need to reference other threads by name or id.",
            input_schema: json!({"type": "object"}),
            dispatch: McpDispatch::Direct("thread.list"),
        },
        McpTool {
            name: "chat_list",
            description: "List chat sessions in the current thread (workers, reviewers, orchestrator).",
            input_schema: json!({"type": "object"}),
            dispatch: McpDispatch::WithCurrentThread {
                method: "chat.list",
                inject_key: "thread_id",
            },
        },
        McpTool {
            name: "chat_status",
            description: "Fetch a single chat session's current status (agent type, worker count, display name, …).",
            input_schema: json!({
                "type": "object",
                "required": ["session_id"],
                "properties": {"session_id": {"type": "string"}}
            }),
            dispatch: McpDispatch::Direct("chat.status"),
        },
    ]
}

async fn run_mcp_server(ws_url: &str, auth_token: Option<String>) -> Result<(), CliError> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let tools = mcp_tool_catalog();
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    let mut initialized = false;

    eprintln!(
        "threadmill-cli mcp — {} tools, talking to {}",
        tools.len(),
        ws_url
    );

    loop {
        let line = match stdin.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => {
                eprintln!("mcp: stdin closed, exiting");
                return Ok(());
            }
            Err(err) => {
                eprintln!("mcp: stdin read error: {err}");
                return Err(CliError::connection(format!("stdin: {err}")));
            }
        };
        if line.trim().is_empty() {
            continue;
        }

        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(err) => {
                // Per JSON-RPC 2.0 the server MUST respond with a parse error
                // when a request is unparsable. Silent drop would hang a client
                // that's waiting on a specific id.
                eprintln!("mcp: parse error on stdin: {err} (raw: {line})");
                let resp = err_response(&Some(Value::Null), -32700, "parse error");
                write_mcp_frame(&mut stdout, &resp).await?;
                continue;
            }
        };

        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        // JSON-RPC 2.0: notifications have no `id` and MUST NOT receive a
        // response. Handle them before the request dispatcher so no path can
        // accidentally emit a reply with `id: null`.
        if id.is_none() {
            match method {
                "notifications/initialized" => {
                    // Per MCP 2024-11-05 the server transitions to "ready" on
                    // this notification, NOT on the initialize request — the
                    // client must complete the handshake before tool calls.
                    initialized = true;
                }
                "notifications/cancelled" | "notifications/progress" => {
                    // Client-side lifecycle notifications we don't act on yet.
                }
                other => {
                    eprintln!("mcp: ignoring unknown notification: {other}");
                }
            }
            continue;
        }

        let response: Value = match method {
            "initialize" => {
                // Protocol version negotiation: if the client's requested
                // version is one we support, echo it; otherwise fall back to
                // our default and let the client decide whether to continue.
                let client_version = params.get("protocolVersion").and_then(Value::as_str);
                let response_version = match client_version {
                    Some(v) if SUPPORTED_MCP_VERSIONS.contains(&v) => v,
                    _ => MCP_PROTOCOL_VERSION,
                };
                ok_response(
                    &id,
                    json!({
                        "protocolVersion": response_version,
                        "capabilities": {"tools": {"listChanged": false}},
                        "serverInfo": {
                            "name": "threadmill-cli",
                            "version": env!("CARGO_PKG_VERSION"),
                        }
                    }),
                )
            }
            "tools/list" => {
                if !initialized {
                    err_response(&id, -32002, "server not initialized")
                } else {
                    let tool_list: Vec<Value> = tools
                        .iter()
                        .map(|t| {
                            json!({
                                "name": t.name,
                                "description": t.description,
                                "inputSchema": t.input_schema,
                            })
                        })
                        .collect();
                    ok_response(&id, json!({"tools": tool_list}))
                }
            }
            "tools/call" => {
                if !initialized {
                    err_response(&id, -32002, "server not initialized")
                } else {
                    handle_tool_call(ws_url, auth_token.as_deref(), &tools, params, &id).await
                }
            }
            "ping" => ok_response(&id, json!({})),
            other => err_response(&id, -32601, &format!("unknown method: {other}")),
        };

        write_mcp_frame(&mut stdout, &response).await?;
    }
}

async fn handle_tool_call(
    ws_url: &str,
    auth_token: Option<&str>,
    tools: &[McpTool],
    params: Value,
    id: &Option<Value>,
) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let tool = match tools.iter().find(|t| t.name == name) {
        Some(t) => t,
        None => {
            return tool_error(id, &format!("unknown tool: {name}"));
        }
    };

    let rpc_params = match &tool.dispatch {
        McpDispatch::Direct(_) => args,
        McpDispatch::WithCurrentThread { inject_key, .. } => {
            // Resolve thread context: explicit arg > env var > error. A wrong-type
            // argument (e.g. `thread_id: 42` or `thread_id: {}`) is a caller bug —
            // surface it loudly instead of silently overwriting with the env var.
            let mut args = args;
            match args.get(*inject_key) {
                Some(Value::String(s)) if !s.is_empty() => { /* caller-supplied, keep as-is */ }
                Some(Value::Null) | None => {
                    let thread_id = match env::var("THREADMILL_THREAD") {
                        Ok(value) => value,
                        Err(_) => {
                            return tool_error(
                                id,
                                &format!(
                                    "this tool needs `{inject_key}` set in arguments, or THREADMILL_THREAD env var present"
                                ),
                            );
                        }
                    };
                    match args.as_object_mut() {
                        Some(obj) => {
                            obj.insert(inject_key.to_string(), Value::String(thread_id));
                        }
                        None => {
                            return tool_error(id, "tool arguments must be an object");
                        }
                    }
                }
                Some(Value::String(_)) => {
                    // Empty-string case — treat like missing so env fallback applies.
                    let thread_id = match env::var("THREADMILL_THREAD") {
                        Ok(value) => value,
                        Err(_) => {
                            return tool_error(
                                id,
                                &format!(
                                    "this tool needs `{inject_key}` set in arguments, or THREADMILL_THREAD env var present"
                                ),
                            );
                        }
                    };
                    if let Some(obj) = args.as_object_mut() {
                        obj.insert(inject_key.to_string(), Value::String(thread_id));
                    }
                }
                Some(other) => {
                    return tool_error(
                        id,
                        &format!("`{inject_key}` must be a string, got {}", type_label(other)),
                    );
                }
            }
            args
        }
    };

    let method = match &tool.dispatch {
        McpDispatch::Direct(method) | McpDispatch::WithCurrentThread { method, .. } => *method,
    };

    match rpc_request(ws_url, auth_token, method, rpc_params).await {
        Ok(result) => {
            let text = serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string());
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{"type": "text", "text": text}],
                    "isError": false,
                }
            })
        }
        Err(err) => tool_error(id, &err.message),
    }
}

fn type_label(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn ok_response(id: &Option<Value>, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn err_response(id: &Option<Value>, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message},
    })
}

/// Convention in MCP 2024-11-05: tool-execution failures return a `result` with
/// `isError: true` and the message in `content`, NOT a JSON-RPC `error` — that's
/// reserved for protocol-level failures (unknown method, bad params, etc.).
fn tool_error(id: &Option<Value>, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{"type": "text", "text": message}],
            "isError": true,
        }
    })
}

async fn write_mcp_frame(stdout: &mut tokio::io::Stdout, frame: &Value) -> Result<(), CliError> {
    use tokio::io::AsyncWriteExt;
    let mut payload = serde_json::to_vec(frame)
        .map_err(|err| CliError::error(format!("failed to encode MCP frame: {err}")))?;
    payload.push(b'\n');
    stdout
        .write_all(&payload)
        .await
        .map_err(|err| CliError::connection(format!("mcp stdout write: {err}")))?;
    stdout
        .flush()
        .await
        .map_err(|err| CliError::connection(format!("mcp stdout flush: {err}")))?;
    Ok(())
}
