use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

use crate::{
    protocol::{self, RequestDispatch},
    services::{
        file::FileService, opencode::OpencodeService, preset::PresetService,
        project::ProjectService, terminal, terminal::TerminalConnectionState,
        thread::ThreadService,
    },
    AppState,
};

pub async fn dispatch_request(
    method: &str,
    params: Value,
    state: Arc<AppState>,
    connection_state: Arc<Mutex<TerminalConnectionState>>,
    outbound_tx: mpsc::UnboundedSender<Message>,
) -> Result<Value, String> {
    let params = normalize_params(method, params);
    let request = protocol::parse_request_dispatch(method, params)?;

    match request {
        RequestDispatch::Ping(_) => Ok(json!("pong")),
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
                    .collect::<Result<Vec<_>, _>>()?,
                threads,
            };
            to_value("state.snapshot", snapshot)
        }

        RequestDispatch::OpencodeStatus(params) => {
            let status = OpencodeService::status(params).await?;
            to_value("opencode.status", status)
        }
        RequestDispatch::OpencodeEnsure(params) => {
            let url = OpencodeService::ensure(params).await?;
            to_value("opencode.ensure", url)
        }
        RequestDispatch::ProjectList(_) => {
            let projects = ProjectService::list(state).await?;
            to_value("project.list", projects)
        }
        RequestDispatch::ProjectAdd(params) => {
            let project = ProjectService::add(state, params).await?;
            to_value("project.add", project)
        }
        RequestDispatch::ProjectClone(params) => {
            let project = ProjectService::clone(state, params).await?;
            to_value("project.clone", project)
        }
        RequestDispatch::ProjectRemove(params) => {
            let result = ProjectService::remove(state, params).await?;
            to_value("project.remove", result)
        }
        RequestDispatch::ProjectBranches(params) => {
            let branches = ProjectService::branches(state, params).await?;
            to_value("project.branches", branches)
        }
        RequestDispatch::ProjectBrowse(params) => {
            let entries = ProjectService::browse(params).await?;
            to_value("project.browse", entries)
        }
        RequestDispatch::FileList(params) => {
            let result = FileService::list(state, params).await?;
            to_value("file.list", result)
        }
        RequestDispatch::FileRead(params) => {
            let result = FileService::read(state, params).await?;
            to_value("file.read", result)
        }
        RequestDispatch::ThreadCreate(params) => {
            let thread = ThreadService::create(state, params).await?;
            to_value("thread.create", thread)
        }
        RequestDispatch::ThreadList(params) => {
            let threads = ThreadService::list(state, params).await?;
            to_value("thread.list", threads)
        }
        RequestDispatch::ThreadClose(params) => {
            let result = ThreadService::close(state, params).await?;
            to_value("thread.close", result)
        }
        RequestDispatch::ThreadCancel(params) => {
            let result = ThreadService::cancel(state, params).await?;
            to_value("thread.cancel", result)
        }
        RequestDispatch::ThreadReopen(params) => {
            let thread = ThreadService::reopen(state, params).await?;
            to_value("thread.reopen", thread)
        }
        RequestDispatch::ThreadHide(params) => {
            let result = ThreadService::hide(state, params).await?;
            to_value("thread.hide", result)
        }
        RequestDispatch::TerminalAttach(params) => {
            terminal::attach(params, state, connection_state, outbound_tx).await
        }
        RequestDispatch::TerminalDetach(params) => terminal::detach(params, connection_state).await,
        RequestDispatch::TerminalResize(params) => terminal::resize(params, connection_state).await,
        RequestDispatch::PresetStart(params) => {
            let result = PresetService::start(state, params).await?;
            to_value("preset.start", result)
        }
        RequestDispatch::PresetStop(params) => {
            let result = PresetService::stop(state, params).await?;
            to_value("preset.stop", result)
        }
        RequestDispatch::PresetRestart(params) => {
            let result = PresetService::restart(state, params).await?;
            to_value("preset.restart", result)
        }
    }
}

fn normalize_params(method: &str, params: Value) -> Value {
    if !params.is_null() {
        return params;
    }

    match method {
        protocol::METHOD_PING
        | protocol::METHOD_STATE_SNAPSHOT
        | protocol::METHOD_OPENCODE_STATUS
        | protocol::METHOD_OPENCODE_ENSURE
        | protocol::METHOD_PROJECT_LIST
        | protocol::METHOD_THREAD_LIST => json!({}),
        _ => Value::Null,
    }
}

fn to_value<T: serde::Serialize>(method: &str, value: T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|err| format!("serialize {method}: {err}"))
}
