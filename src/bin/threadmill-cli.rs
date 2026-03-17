use std::{env, fs, process::ExitCode};

use clap::{Parser, Subcommand};
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
