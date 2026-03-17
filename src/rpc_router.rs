use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::{
    protocol::{self, RequestDispatch},
    services::{
        file::FileService, opencode::OpencodeService, preset::PresetService,
        project::ProjectService, system::SystemService, terminal,
        terminal::TerminalConnectionState, thread::ThreadService,
    },
    AppState, ConnectionSessionState, RpcError,
};

pub async fn dispatch_request(
    method: &str,
    params: Value,
    state: Arc<AppState>,
    connection_state: Arc<Mutex<TerminalConnectionState>>,
    session_state: Arc<Mutex<ConnectionSessionState>>,
    outbound_tx: mpsc::UnboundedSender<Message>,
) -> Result<Value, RpcError> {
    if method != protocol::METHOD_SESSION_HELLO && method != protocol::METHOD_PING {
        let initialized = session_state.lock().await.is_initialized();
        if !initialized {
            return Err(RpcError::session_not_initialized(method));
        }
    }

    let params = normalize_params(method, params);
    let request = protocol::parse_request_dispatch(method, params).map_err(|err| {
        if err.starts_with("unknown method") {
            RpcError::method_not_found(method)
        } else {
            RpcError::invalid_params(err)
        }
    })?;

    match request {
        RequestDispatch::SessionHello(params) => {
            let negotiated = params
                .capabilities
                .into_iter()
                .filter(|cap| protocol::SUPPORTED_CAPABILITIES.contains(&cap.as_str()))
                .collect::<Vec<_>>();

            let protocol_version = if params.protocol_version == protocol::PROTOCOL_VERSION {
                params.protocol_version
            } else {
                protocol::PROTOCOL_VERSION.to_string()
            };

            let session_id = format!("session-{}", Uuid::new_v4().simple());
            {
                let mut guard = session_state.lock().await;
                guard.session_id = Some(session_id.clone());
                guard.protocol_version = Some(protocol_version.clone());
                guard.capabilities = negotiated.clone();
            }

            to_value(
                "session.hello",
                protocol::SessionHelloResult {
                    session_id,
                    protocol_version,
                    capabilities: negotiated,
                    state_version: state.state_version(),
                },
            )
        }
        RequestDispatch::Ping(_) => Ok(json!("pong")),
        RequestDispatch::SystemStats(params) => {
            let stats = SystemService::stats(params)
                .await
                .map_err(|message| map_service_error("system.stats", message))?;
            to_value("system.stats", stats)
        }
        RequestDispatch::SystemCleanup(params) => {
            let result = SystemService::cleanup(params)
                .await
                .map_err(|message| map_service_error("system.cleanup", message))?;
            to_value("system.cleanup", result)
        }
        RequestDispatch::StateSnapshot(_) => {
            let (projects, threads) = {
                let store = state.store.lock().await;
                (
                    store.data.projects.clone(),
                    store
                        .data
                        .threads
                        .iter()
                        .map(crate::state_store::Thread::to_protocol)
                        .collect::<Vec<_>>(),
                )
            };

            let snapshot = protocol::StateSnapshot {
                state_version: state.state_version(),
                projects: projects
                    .into_iter()
                    .map(|project| project.to_protocol())
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|message| map_service_error("state.snapshot", message))?,
                threads,
            };
            to_value("state.snapshot", snapshot)
        }
        RequestDispatch::OpencodeStatus(params) => {
            let status = OpencodeService::status(params)
                .await
                .map_err(|message| map_service_error("opencode.status", message))?;
            to_value("opencode.status", status)
        }
        RequestDispatch::OpencodeEnsure(params) => {
            let result = OpencodeService::ensure(params)
                .await
                .map_err(|message| map_service_error("opencode.ensure", message))?;
            to_value("opencode.ensure", result)
        }
        RequestDispatch::ProjectList(_) => {
            let projects = ProjectService::list(state)
                .await
                .map_err(|message| map_service_error("project.list", message))?;
            to_value("project.list", projects)
        }
        RequestDispatch::ProjectAdd(params) => {
            let project = ProjectService::add(state, params)
                .await
                .map_err(|message| map_service_error("project.add", message))?;
            to_value("project.add", project)
        }
        RequestDispatch::ProjectClone(params) => {
            let project = ProjectService::clone(state, params)
                .await
                .map_err(|message| map_service_error("project.clone", message))?;
            to_value("project.clone", project)
        }
        RequestDispatch::ProjectRemove(params) => {
            let result = ProjectService::remove(state, params)
                .await
                .map_err(|message| map_service_error("project.remove", message))?;
            to_value("project.remove", result)
        }
        RequestDispatch::ProjectBranches(params) => {
            let branches = ProjectService::branches(state, params)
                .await
                .map_err(|message| map_service_error("project.branches", message))?;
            to_value("project.branches", branches)
        }
        RequestDispatch::ProjectBrowse(params) => {
            let entries = ProjectService::browse(params)
                .await
                .map_err(|message| map_service_error("project.browse", message))?;
            to_value("project.browse", entries)
        }
        RequestDispatch::ProjectLookup(params) => {
            let result = ProjectService::lookup(state, params)
                .await
                .map_err(|message| map_service_error("project.lookup", message))?;
            to_value("project.lookup", result)
        }
        RequestDispatch::FileList(params) => {
            let result = FileService::list(state, params)
                .await
                .map_err(|message| map_service_error("file.list", message))?;
            to_value("file.list", result)
        }
        RequestDispatch::FileRead(params) => {
            let result = FileService::read(state, params)
                .await
                .map_err(|message| map_service_error("file.read", message))?;
            to_value("file.read", result)
        }
        RequestDispatch::FileGitStatus(params) => {
            let result = FileService::git_status(state, params)
                .await
                .map_err(|message| map_service_error("file.git_status", message))?;
            to_value("file.git_status", result)
        }
        RequestDispatch::ThreadCreate(params) => {
            let thread = ThreadService::create(state, params)
                .await
                .map_err(|message| map_service_error("thread.create", message))?;
            to_value("thread.create", thread)
        }
        RequestDispatch::ThreadList(params) => {
            let threads = ThreadService::list(state, params)
                .await
                .map_err(|message| map_service_error("thread.list", message))?;
            to_value("thread.list", threads)
        }
        RequestDispatch::ThreadClose(params) => {
            let result = ThreadService::close(state, params)
                .await
                .map_err(|message| map_service_error("thread.close", message))?;
            to_value("thread.close", result)
        }
        RequestDispatch::ThreadCancel(params) => {
            let result = ThreadService::cancel(state, params)
                .await
                .map_err(|message| map_service_error("thread.cancel", message))?;
            to_value("thread.cancel", result)
        }
        RequestDispatch::ThreadReopen(params) => {
            let thread = ThreadService::reopen(state, params)
                .await
                .map_err(|message| map_service_error("thread.reopen", message))?;
            to_value("thread.reopen", thread)
        }
        RequestDispatch::ThreadHide(params) => {
            let result = ThreadService::hide(state, params)
                .await
                .map_err(|message| map_service_error("thread.hide", message))?;
            to_value("thread.hide", result)
        }
        RequestDispatch::TerminalAttach(params) => {
            terminal::attach(params, state, connection_state, outbound_tx)
                .await
                .map_err(|message| map_service_error("terminal.attach", message))
        }
        RequestDispatch::TerminalDetach(params) => terminal::detach(params, connection_state)
            .await
            .map_err(|message| map_service_error("terminal.detach", message)),
        RequestDispatch::TerminalResize(params) => terminal::resize(params, connection_state)
            .await
            .map_err(|message| map_service_error("terminal.resize", message)),
        RequestDispatch::PresetStart(params) => {
            let result = PresetService::start(state, params)
                .await
                .map_err(|message| map_service_error("preset.start", message))?;
            to_value("preset.start", result)
        }
        RequestDispatch::PresetStop(params) => {
            let result = PresetService::stop(state, params)
                .await
                .map_err(|message| map_service_error("preset.stop", message))?;
            to_value("preset.stop", result)
        }
        RequestDispatch::PresetRestart(params) => {
            let result = PresetService::restart(state, params)
                .await
                .map_err(|message| map_service_error("preset.restart", message))?;
            to_value("preset.restart", result)
        }
    }
}

fn normalize_params(method: &str, params: Value) -> Value {
    if !params.is_null() {
        return params;
    }

    match method {
        protocol::METHOD_SESSION_HELLO
        | protocol::METHOD_PING
        | protocol::METHOD_STATE_SNAPSHOT
        | protocol::METHOD_OPENCODE_STATUS
        | protocol::METHOD_OPENCODE_ENSURE
        | protocol::METHOD_PROJECT_LIST
        | protocol::METHOD_THREAD_LIST
        | protocol::METHOD_SYSTEM_STATS
        | protocol::METHOD_SYSTEM_CLEANUP => json!({}),
        _ => Value::Null,
    }
}

fn to_value<T: serde::Serialize>(method: &str, value: T) -> Result<Value, RpcError> {
    serde_json::to_value(value)
        .map_err(|err| RpcError::internal(format!("serialize {method}: {err}")))
}

fn map_service_error(method: &str, message: String) -> RpcError {
    if method == "terminal.attach" {
        return RpcError::terminal_session_missing(message, Some(json!({ "method": method })));
    }

    if message.contains(" not found") || message.starts_with("not found") {
        let kind = if method.starts_with("thread.") {
            "thread.not_found"
        } else if method.starts_with("project.") {
            "project.not_found"
        } else if method.starts_with("preset.") {
            "preset.not_found"
        } else {
            "resource.not_found"
        };
        return RpcError::not_found(kind, message);
    }

    if message.contains("must be")
        || message.starts_with("invalid")
        || message.contains("unsupported")
    {
        return RpcError::invalid_params(message);
    }

    RpcError::internal(message)
}
