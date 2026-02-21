use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

use crate::{
    protocol::{
        PresetRestartParams, PresetStartParams, PresetStopParams, ProjectAddParams,
        ProjectBranchesParams, ProjectBrowseParams, ProjectRemoveParams, TerminalAttachParams,
        TerminalDetachParams, TerminalResizeParams, ThreadCloseParams, ThreadCreateParams,
        ThreadListParams, ThreadReopenParams, METHOD_PING, METHOD_PRESET_RESTART,
        METHOD_PRESET_START, METHOD_PRESET_STOP, METHOD_PROJECT_ADD, METHOD_PROJECT_BRANCHES,
        METHOD_PROJECT_BROWSE, METHOD_PROJECT_LIST, METHOD_PROJECT_REMOVE, METHOD_TERMINAL_ATTACH,
        METHOD_TERMINAL_DETACH, METHOD_TERMINAL_RESIZE, METHOD_THREAD_CLOSE,
        METHOD_THREAD_CREATE, METHOD_THREAD_LIST, METHOD_THREAD_REOPEN,
    },
    services::{
        preset::PresetService, project::ProjectService, terminal, terminal::TerminalConnectionState,
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
    match method {
        METHOD_PING => Ok(json!("pong")),
        METHOD_PROJECT_LIST => {
            let projects = ProjectService::list(state).await?;
            Ok(serde_json::to_value(projects).map_err(|err| format!("serialize project.list: {err}"))?)
        }
        METHOD_PROJECT_ADD => {
            let params: ProjectAddParams = parse_params(method, params)?;
            let project = ProjectService::add(state, params).await?;
            Ok(serde_json::to_value(project).map_err(|err| format!("serialize project.add: {err}"))?)
        }
        METHOD_PROJECT_REMOVE => {
            let params: ProjectRemoveParams = parse_params(method, params)?;
            let result = ProjectService::remove(state, params).await?;
            Ok(serde_json::to_value(result).map_err(|err| format!("serialize project.remove: {err}"))?)
        }
        METHOD_PROJECT_BRANCHES => {
            let params: ProjectBranchesParams = parse_params(method, params)?;
            let branches = ProjectService::branches(state, params).await?;
            Ok(serde_json::to_value(branches).map_err(|err| format!("serialize project.branches: {err}"))?)
        }
        METHOD_PROJECT_BROWSE => {
            let params: ProjectBrowseParams = parse_params(method, params)?;
            let entries = ProjectService::browse(params).await?;
            Ok(serde_json::to_value(entries).map_err(|err| format!("serialize project.browse: {err}"))?)
        }
        METHOD_THREAD_CREATE => {
            let params: ThreadCreateParams = parse_params(method, params)?;
            let thread = ThreadService::create(state, params).await?;
            Ok(serde_json::to_value(thread).map_err(|err| format!("serialize thread.create: {err}"))?)
        }
        METHOD_THREAD_LIST => {
            let params = if params.is_null() {
                ThreadListParams::default()
            } else {
                serde_json::from_value::<ThreadListParams>(params)
                    .map_err(|err| format!("invalid {method} params: {err}"))?
            };
            let threads = ThreadService::list(state, params).await?;
            Ok(serde_json::to_value(threads).map_err(|err| format!("serialize thread.list: {err}"))?)
        }
        METHOD_THREAD_CLOSE => {
            let params: ThreadCloseParams = parse_params(method, params)?;
            let result = ThreadService::close(state, params).await?;
            Ok(serde_json::to_value(result).map_err(|err| format!("serialize thread.close: {err}"))?)
        }
        METHOD_THREAD_REOPEN => {
            let params: ThreadReopenParams = parse_params(method, params)?;
            let thread = ThreadService::reopen(state, params).await?;
            Ok(serde_json::to_value(thread).map_err(|err| format!("serialize thread.reopen: {err}"))?)
        }
        METHOD_TERMINAL_ATTACH => {
            let params: TerminalAttachParams = parse_params(method, params)?;
            terminal::attach(params, state, connection_state, outbound_tx).await
        }
        METHOD_TERMINAL_DETACH => {
            let params: TerminalDetachParams = parse_params(method, params)?;
            terminal::detach(params, connection_state).await
        }
        METHOD_TERMINAL_RESIZE => {
            let params: TerminalResizeParams = parse_params(method, params)?;
            terminal::resize(params, connection_state).await
        }
        METHOD_PRESET_START => {
            let params: PresetStartParams = parse_params(method, params)?;
            let result = PresetService::start(state, params).await?;
            Ok(serde_json::to_value(result).map_err(|err| format!("serialize preset.start: {err}"))?)
        }
        METHOD_PRESET_STOP => {
            let params: PresetStopParams = parse_params(method, params)?;
            let result = PresetService::stop(state, params).await?;
            Ok(serde_json::to_value(result).map_err(|err| format!("serialize preset.stop: {err}"))?)
        }
        METHOD_PRESET_RESTART => {
            let params: PresetRestartParams = parse_params(method, params)?;
            let result = PresetService::restart(state, params).await?;
            Ok(serde_json::to_value(result).map_err(|err| format!("serialize preset.restart: {err}"))?)
        }
        _ => Err(format!("unknown method '{method}'")),
    }
}

fn parse_params<T>(method: &str, params: Value) -> Result<T, String>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(params).map_err(|err| format!("invalid {method} params: {err}"))
}
