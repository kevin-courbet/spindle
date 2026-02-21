// Generated from threadmill/protocol/threadmill-rpc.schema.json
// Manual extensions for Spindle M1 services.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: String,
    pub default_branch: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Thread {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub branch: String,
    pub worktree_path: String,
    pub status: ThreadStatus,
    pub source_type: SourceType,
    pub created_at: String,
    pub tmux_session: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ThreadStatus {
    #[serde(rename = "creating")]
    Creating,
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "closing")]
    Closing,
    #[serde(rename = "closed")]
    Closed,
    #[serde(rename = "hidden")]
    Hidden,
    #[serde(rename = "failed")]
    Failed,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum SourceType {
    #[serde(rename = "new_feature")]
    NewFeature,
    #[serde(rename = "existing_branch")]
    ExistingBranch,
    #[serde(rename = "pull_request")]
    PullRequest,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DirectoryEntry {
    pub name: String,
    pub is_dir: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_git_repo: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct PingParams;

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ProjectListParams;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProjectAddParams {
    pub path: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProjectRemoveParams {
    pub project_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProjectBranchesParams {
    pub project_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProjectBrowseParams {
    pub path: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ThreadCreateParams {
    pub project_id: String,
    pub name: String,
    pub source_type: SourceType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ThreadCloseParams {
    pub thread_id: String,
    pub mode: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ThreadReopenParams {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ThreadListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TerminalAttachParams {
    pub thread_id: String,
    pub preset: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TerminalDetachParams {
    pub thread_id: String,
    pub preset: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TerminalResizeParams {
    pub thread_id: String,
    pub preset: String,
    pub cols: u32,
    pub rows: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PresetStartParams {
    pub thread_id: String,
    pub preset: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PresetStopParams {
    pub thread_id: String,
    pub preset: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PresetRestartParams {
    pub thread_id: String,
    pub preset: String,
}

pub type PingResult = String;
pub type ProjectListResult = Vec<Project>;
pub type ProjectAddResult = Project;
pub type ProjectBranchesResult = Vec<String>;
pub type ProjectBrowseResult = Vec<DirectoryEntry>;
pub type ThreadCreateResult = Thread;
pub type ThreadReopenResult = Thread;
pub type ThreadListResult = Vec<Thread>;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProjectRemoveResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ThreadCloseResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ThreadStatus>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TerminalAttachResult {
    pub channel_id: u16,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TerminalDetachResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detached: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TerminalResizeResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resized: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PresetStartResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PresetStopResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PresetRestartResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restarted: Option<bool>,
}

pub const METHOD_PING: &str = "ping";
pub const METHOD_PROJECT_LIST: &str = "project.list";
pub const METHOD_PROJECT_ADD: &str = "project.add";
pub const METHOD_PROJECT_REMOVE: &str = "project.remove";
pub const METHOD_PROJECT_BRANCHES: &str = "project.branches";
pub const METHOD_PROJECT_BROWSE: &str = "project.browse";
pub const METHOD_THREAD_CREATE: &str = "thread.create";
pub const METHOD_THREAD_CLOSE: &str = "thread.close";
pub const METHOD_THREAD_REOPEN: &str = "thread.reopen";
pub const METHOD_THREAD_LIST: &str = "thread.list";
pub const METHOD_TERMINAL_ATTACH: &str = "terminal.attach";
pub const METHOD_TERMINAL_DETACH: &str = "terminal.detach";
pub const METHOD_TERMINAL_RESIZE: &str = "terminal.resize";
pub const METHOD_PRESET_START: &str = "preset.start";
pub const METHOD_PRESET_STOP: &str = "preset.stop";
pub const METHOD_PRESET_RESTART: &str = "preset.restart";

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "method", content = "params")]
pub enum RequestDispatch {
    #[serde(rename = "ping")]
    Ping(PingParams),
    #[serde(rename = "project.list")]
    ProjectList(ProjectListParams),
    #[serde(rename = "project.add")]
    ProjectAdd(ProjectAddParams),
    #[serde(rename = "project.remove")]
    ProjectRemove(ProjectRemoveParams),
    #[serde(rename = "project.branches")]
    ProjectBranches(ProjectBranchesParams),
    #[serde(rename = "project.browse")]
    ProjectBrowse(ProjectBrowseParams),
    #[serde(rename = "thread.create")]
    ThreadCreate(ThreadCreateParams),
    #[serde(rename = "thread.close")]
    ThreadClose(ThreadCloseParams),
    #[serde(rename = "thread.reopen")]
    ThreadReopen(ThreadReopenParams),
    #[serde(rename = "thread.list")]
    ThreadList(ThreadListParams),
    #[serde(rename = "terminal.attach")]
    TerminalAttach(TerminalAttachParams),
    #[serde(rename = "terminal.detach")]
    TerminalDetach(TerminalDetachParams),
    #[serde(rename = "terminal.resize")]
    TerminalResize(TerminalResizeParams),
    #[serde(rename = "preset.start")]
    PresetStart(PresetStartParams),
    #[serde(rename = "preset.stop")]
    PresetStop(PresetStopParams),
    #[serde(rename = "preset.restart")]
    PresetRestart(PresetRestartParams),
}

pub fn parse_request_dispatch(method: &str, params: serde_json::Value) -> Result<RequestDispatch, String> {
    match method {
        METHOD_PING => serde_json::from_value::<PingParams>(params)
            .map(RequestDispatch::Ping)
            .map_err(|err| format!("invalid ping params: {err}")),
        METHOD_PROJECT_LIST => serde_json::from_value::<ProjectListParams>(params)
            .map(RequestDispatch::ProjectList)
            .map_err(|err| format!("invalid project.list params: {err}")),
        METHOD_PROJECT_ADD => serde_json::from_value::<ProjectAddParams>(params)
            .map(RequestDispatch::ProjectAdd)
            .map_err(|err| format!("invalid project.add params: {err}")),
        METHOD_PROJECT_REMOVE => serde_json::from_value::<ProjectRemoveParams>(params)
            .map(RequestDispatch::ProjectRemove)
            .map_err(|err| format!("invalid project.remove params: {err}")),
        METHOD_PROJECT_BRANCHES => serde_json::from_value::<ProjectBranchesParams>(params)
            .map(RequestDispatch::ProjectBranches)
            .map_err(|err| format!("invalid project.branches params: {err}")),
        METHOD_PROJECT_BROWSE => serde_json::from_value::<ProjectBrowseParams>(params)
            .map(RequestDispatch::ProjectBrowse)
            .map_err(|err| format!("invalid project.browse params: {err}")),
        METHOD_THREAD_CREATE => serde_json::from_value::<ThreadCreateParams>(params)
            .map(RequestDispatch::ThreadCreate)
            .map_err(|err| format!("invalid thread.create params: {err}")),
        METHOD_THREAD_CLOSE => serde_json::from_value::<ThreadCloseParams>(params)
            .map(RequestDispatch::ThreadClose)
            .map_err(|err| format!("invalid thread.close params: {err}")),
        METHOD_THREAD_REOPEN => serde_json::from_value::<ThreadReopenParams>(params)
            .map(RequestDispatch::ThreadReopen)
            .map_err(|err| format!("invalid thread.reopen params: {err}")),
        METHOD_THREAD_LIST => serde_json::from_value::<ThreadListParams>(params)
            .map(RequestDispatch::ThreadList)
            .map_err(|err| format!("invalid thread.list params: {err}")),
        METHOD_TERMINAL_ATTACH => serde_json::from_value::<TerminalAttachParams>(params)
            .map(RequestDispatch::TerminalAttach)
            .map_err(|err| format!("invalid terminal.attach params: {err}")),
        METHOD_TERMINAL_DETACH => serde_json::from_value::<TerminalDetachParams>(params)
            .map(RequestDispatch::TerminalDetach)
            .map_err(|err| format!("invalid terminal.detach params: {err}")),
        METHOD_TERMINAL_RESIZE => serde_json::from_value::<TerminalResizeParams>(params)
            .map(RequestDispatch::TerminalResize)
            .map_err(|err| format!("invalid terminal.resize params: {err}")),
        METHOD_PRESET_START => serde_json::from_value::<PresetStartParams>(params)
            .map(RequestDispatch::PresetStart)
            .map_err(|err| format!("invalid preset.start params: {err}")),
        METHOD_PRESET_STOP => serde_json::from_value::<PresetStopParams>(params)
            .map(RequestDispatch::PresetStop)
            .map_err(|err| format!("invalid preset.stop params: {err}")),
        METHOD_PRESET_RESTART => serde_json::from_value::<PresetRestartParams>(params)
            .map(RequestDispatch::PresetRestart)
            .map_err(|err| format!("invalid preset.restart params: {err}")),
        _ => Err(format!("unknown method '{method}'")),
    }
}
