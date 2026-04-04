use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::{
    protocol::{self, RequestDispatch},
    services::{
        chat::ChatService, file::FileService, opencode::OpencodeService,
        preset::PresetService, project::ProjectService, system::SystemService, terminal,
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
    let (handshake_started, initialized) = {
        let guard = session_state.lock().await;
        (guard.is_handshake_started(), guard.is_initialized())
    };
    if method == protocol::METHOD_SESSION_HELLO && handshake_started {
        return Err(RpcError::session_already_initialized());
    }

    if method != protocol::METHOD_SESSION_HELLO && method != protocol::METHOD_PING && !initialized {
        return Err(RpcError::session_not_initialized(method));
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
            if params.protocol_version != protocol::PROTOCOL_VERSION {
                return Err(RpcError::session_protocol_mismatch(
                    &params.protocol_version,
                    protocol::PROTOCOL_VERSION,
                ));
            }

            let required_server_capabilities = if params.required_capabilities.is_empty() {
                params.capabilities.clone()
            } else {
                params.required_capabilities.clone()
            };
            let missing_capabilities = required_server_capabilities
                .iter()
                .filter(|required| !protocol::SUPPORTED_CAPABILITIES.contains(&required.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            if !missing_capabilities.is_empty() {
                return Err(RpcError::session_missing_capabilities(
                    &missing_capabilities,
                ));
            }

            let missing_client_capabilities = protocol::REQUIRED_CLIENT_CAPABILITIES
                .iter()
                .filter(|required| !params.capabilities.iter().any(|cap| cap == **required))
                .map(|required| (*required).to_string())
                .collect::<Vec<_>>();
            if !missing_client_capabilities.is_empty() {
                return Err(RpcError::session_missing_capabilities(
                    &missing_client_capabilities,
                ));
            }

            let negotiated = params
                .capabilities
                .into_iter()
                .filter(|cap| protocol::SUPPORTED_CAPABILITIES.contains(&cap.as_str()))
                .collect::<Vec<_>>();
            let required_client_capabilities = protocol::REQUIRED_CLIENT_CAPABILITIES
                .iter()
                .map(|cap| (*cap).to_string())
                .collect::<Vec<_>>();

            let session_id = {
                let mut guard = session_state.lock().await;
                if guard.is_handshake_started() {
                    return Err(RpcError::session_already_initialized());
                }

                let session_id = format!("session-{}", Uuid::new_v4().simple());
                guard.session_id = Some(session_id.clone());
                guard.protocol_version = Some(protocol::PROTOCOL_VERSION.to_string());
                guard.capabilities = negotiated.clone();
                guard.hello_acknowledged = false;
                session_id
            };

            to_value(
                "session.hello",
                protocol::SessionHelloResult {
                    session_id,
                    protocol_version: protocol::PROTOCOL_VERSION.to_string(),
                    capabilities: negotiated,
                    required_capabilities: required_client_capabilities,
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
        RequestDispatch::StateSnapshot(_) => {
            let (projects, mut threads) = {
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

            for thread in &mut threads {
                thread.chat_sessions =
                    ChatService::thread_chat_sessions(Arc::clone(&state), &thread.id).await;
            }

            let agent_registry = crate::services::agent_registry::discover_agents();
            let snapshot = protocol::StateSnapshot {
                state_version: state.state_version(),
                projects: projects
                    .into_iter()
                    .map(|project| project.to_protocol())
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|message| map_service_error("state.snapshot", message))?,
                threads,
                agent_registry,
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
        RequestDispatch::ChatStart(params) => {
            let result = ChatService::start(state, params)
                .await
                .map_err(|message| map_service_error("chat.start", message))?;
            to_value("chat.start", result)
        }
        RequestDispatch::ChatLoad(params) => {
            let result = ChatService::load(state, params)
                .await
                .map_err(|message| map_service_error("chat.load", message))?;
            to_value("chat.load", result)
        }
        RequestDispatch::ChatStop(params) => {
            let result = ChatService::stop(state, params)
                .await
                .map_err(|message| map_service_error("chat.stop", message))?;
            to_value("chat.stop", result)
        }
        RequestDispatch::ChatList(params) => {
            let result = ChatService::list(state, params)
                .await
                .map_err(|message| map_service_error("chat.list", message))?;
            to_value("chat.list", result)
        }
        RequestDispatch::ChatAttach(params) => {
            let result = ChatService::attach(params, state, connection_state, outbound_tx)
                .await
                .map_err(|message| map_service_error("chat.attach", message))?;
            to_value("chat.attach", result)
        }
        RequestDispatch::ChatDetach(params) => {
            let result = ChatService::detach(params, state, connection_state)
                .await
                .map_err(|message| map_service_error("chat.detach", message))?;
            to_value("chat.detach", result)
        }
        RequestDispatch::ChatHistory(params) => {
            let result = ChatService::history(state, params)
                .await
                .map_err(|message| map_service_error("chat.history", message))?;
            to_value("chat.history", result)
        }
        RequestDispatch::AgentRegistryList(_) => {
            let entries = crate::services::agent_registry::discover_agents();
            to_value("agent.registry.list", entries)
        }
        RequestDispatch::AgentRegistryInstall(params) => {
            match crate::services::agent_registry::install_agent(&params.agent_id).await {
                Ok(resolved_path) => to_value(
                    "agent.registry.install",
                    protocol::AgentRegistryInstallResult {
                        success: true,
                        resolved_path: Some(resolved_path),
                        error: None,
                    },
                ),
                Err(err) => to_value(
                    "agent.registry.install",
                    protocol::AgentRegistryInstallResult {
                        success: false,
                        resolved_path: None,
                        error: Some(err),
                    },
                ),
            }
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
        | protocol::METHOD_AGENT_REGISTRY_LIST => json!({}),
        _ => Value::Null,
    }
}

fn to_value<T: serde::Serialize>(method: &str, value: T) -> Result<Value, RpcError> {
    serde_json::to_value(value)
        .map_err(|err| RpcError::internal(format!("serialize {method}: {err}")))
}

fn map_service_error(method: &str, message: String) -> RpcError {
    if method == "terminal.attach" && terminal_attach_session_missing(&message) {
        return RpcError::terminal_session_missing(message, Some(json!({ "method": method })));
    }

    if message.contains(" not found") || message.starts_with("not found") {
        let kind = if method.starts_with("thread.") {
            "thread.not_found"
        } else if method.starts_with("project.") {
            "project.not_found"
        } else if method.starts_with("preset.") {
            "preset.not_found"
        } else if method.starts_with("chat.") {
            "chat.not_found"
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

fn terminal_attach_session_missing(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("tmux session not running")
        || message.contains("can't find session")
        || message.contains("no such session")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use tokio::sync::{mpsc, Mutex};

    use super::{dispatch_request, map_service_error};
    use crate::{
        protocol,
        services::terminal::TerminalConnectionState,
        state_store::{AppData, StateStore},
        AppState, ConnectionSessionState,
    };

    #[tokio::test]
    async fn session_hello_stays_pending_until_acknowledged() {
        let state = Arc::new(AppState::new(StateStore {
            path: std::env::temp_dir().join("threadmill-session-hello-state.json"),
            data: AppData::default(),
        }));
        let connection_state = Arc::new(Mutex::new(TerminalConnectionState::default()));
        let session_state = Arc::new(Mutex::new(ConnectionSessionState::default()));
        let (outbound_tx, _outbound_rx) = mpsc::unbounded_channel();

        let result = dispatch_request(
            protocol::METHOD_SESSION_HELLO,
            json!({
                "client": { "name": "spindle-tests", "version": "dev" },
                "protocol_version": protocol::PROTOCOL_VERSION,
                "capabilities": protocol::SUPPORTED_CAPABILITIES,
            }),
            state,
            connection_state,
            Arc::clone(&session_state),
            outbound_tx,
        )
        .await
        .expect("session.hello should negotiate successfully");

        let guard = session_state.lock().await;
        assert_eq!(guard.session_id.as_deref(), result["session_id"].as_str());
        assert!(guard.is_handshake_started());
        assert!(
            !guard.is_initialized(),
            "session.hello should stay pending until its success response is queued"
        );
    }

    #[test]
    fn terminal_attach_missing_session_maps_to_terminal_session_missing() {
        let error = map_service_error(
            "terminal.attach",
            "tmux list-panes failed for tm_thread:terminal: can't find session: tm_thread"
                .to_string(),
        );

        assert_eq!(error.code, -32041);
        let data = error
            .data
            .expect("terminal attach missing-session error should include data");
        assert_eq!(data.kind.as_deref(), Some("terminal.session_missing"));
    }

    #[test]
    fn terminal_attach_infra_failure_stays_internal() {
        let error = map_service_error(
            "terminal.attach",
            "failed to run mkfifo for /tmp/threadmill-in: Resource temporarily unavailable"
                .to_string(),
        );

        assert_eq!(error.code, -32001);
        let data = error.data.expect("internal errors should include data");
        assert_eq!(data.kind.as_deref(), Some("rpc.internal"));
    }
}
