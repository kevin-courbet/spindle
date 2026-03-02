// Generated from threadmill/protocol/threadmill-rpc.schema.json
// Manual extensions for Spindle runtime events and sync deltas.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub type StateVersion = u64;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PresetConfig {
    pub name: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: String,
    pub default_branch: String,
    #[serde(default)]
    pub presets: Vec<PresetConfig>,
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
    #[serde(default)]
    pub port_offset: u16,
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
    #[serde(rename = "main_checkout")]
    MainCheckout,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum PresetStatus {
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "stopped")]
    Stopped,
    #[serde(rename = "crashed")]
    Crashed,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DirectoryEntry {
    pub name: String,
    pub is_dir: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_git_repo: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ThreadProgressStep {
    #[serde(rename = "fetching")]
    Fetching,
    #[serde(rename = "creating_worktree")]
    CreatingWorktree,
    #[serde(rename = "copying_files")]
    CopyingFiles,
    #[serde(rename = "running_hooks")]
    RunningHooks,
    #[serde(rename = "starting_presets")]
    StartingPresets,
    #[serde(rename = "ready")]
    Ready,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ThreadProgress {
    pub thread_id: String,
    pub step: ThreadProgressStep,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ThreadStatusChanged {
    pub thread_id: String,
    pub old: ThreadStatus,
    pub new: ThreadStatus,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum PresetProcessKind {
    #[serde(rename = "started")]
    Started,
    #[serde(rename = "exited")]
    Exited,
    #[serde(rename = "crashed")]
    Crashed,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PresetProcessEvent {
    pub thread_id: String,
    pub preset: String,
    pub event: PresetProcessKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StateSnapshot {
    pub state_version: StateVersion,
    pub projects: Vec<Project>,
    pub threads: Vec<Thread>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum StateDeltaChange {
    #[serde(rename = "project.added")]
    ProjectAdded { project: Project },
    #[serde(rename = "project.removed")]
    ProjectRemoved { project_id: String },
    #[serde(rename = "thread.created")]
    ThreadCreated { thread: Thread },
    #[serde(rename = "thread.removed")]
    ThreadRemoved { thread_id: String },
    #[serde(rename = "thread.status_changed")]
    ThreadStatusChanged {
        thread_id: String,
        old: ThreadStatus,
        new: ThreadStatus,
    },
    #[serde(rename = "preset.process_event")]
    PresetProcessEvent {
        thread_id: String,
        preset: String,
        event: PresetProcessKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i64>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StateDeltaEvent {
    pub state_version: StateVersion,
    pub changes: Vec<StateDeltaChange>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProjectAddedEvent {
    pub project: Project,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProjectRemovedEvent {
    pub project_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ThreadCreatedEvent {
    pub thread: Thread,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ThreadRemovedEvent {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BinaryFrame {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct PingParams {}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct StateSnapshotParams {}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct OpencodeStatusParams {}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct OpencodeEnsureParams {}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ProjectListParams {}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProjectAddParams {
    pub path: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProjectCloneParams {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
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
pub struct FileListParams {
    pub path: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileReadParams {
    pub path: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileGitStatusParams {
    pub path: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ThreadCreateParams {
    pub project_id: String,
    pub name: String,
    pub source_type: SourceType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ThreadCloseParams {
    pub thread_id: String,
    pub mode: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ThreadCancelParams {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ThreadReopenParams {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ThreadHideParams {
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

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    #[serde(rename = "isDirectory")]
    pub is_directory: bool,
    pub size: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileListResult {
    pub entries: Vec<FileEntry>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileReadResult {
    pub content: String,
    pub size: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileGitStatusResult {
    pub entries: HashMap<String, String>,
}

pub type PingResult = String;
pub type StateSnapshotResult = StateSnapshot;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OpencodeStatusResult {
    pub running: bool,
    pub port: u16,
    pub url: String,
}

pub type OpencodeEnsureResult = String;

pub type ProjectListResult = Vec<Project>;
pub type ProjectAddResult = Project;
pub type ProjectCloneResult = Project;
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
pub struct ThreadHideResult {
    pub status: ThreadStatus,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ThreadCancelResult {
    pub status: ThreadStatus,
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
    pub ok: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PresetStopResult {
    pub ok: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PresetRestartResult {
    pub ok: bool,
}

pub const METHOD_PING: &str = "ping";
pub const METHOD_STATE_SNAPSHOT: &str = "state.snapshot";
pub const METHOD_OPENCODE_STATUS: &str = "opencode.status";
pub const METHOD_OPENCODE_ENSURE: &str = "opencode.ensure";
pub const METHOD_PROJECT_LIST: &str = "project.list";
pub const METHOD_PROJECT_ADD: &str = "project.add";
pub const METHOD_PROJECT_CLONE: &str = "project.clone";
pub const METHOD_PROJECT_REMOVE: &str = "project.remove";
pub const METHOD_PROJECT_BRANCHES: &str = "project.branches";
pub const METHOD_PROJECT_BROWSE: &str = "project.browse";
pub const METHOD_FILE_LIST: &str = "file.list";
pub const METHOD_FILE_READ: &str = "file.read";
pub const FILE_GIT_STATUS: &str = "file.git_status";
pub const METHOD_THREAD_CREATE: &str = "thread.create";
pub const METHOD_THREAD_CLOSE: &str = "thread.close";
pub const METHOD_THREAD_CANCEL: &str = "thread.cancel";
pub const METHOD_THREAD_REOPEN: &str = "thread.reopen";
pub const METHOD_THREAD_HIDE: &str = "thread.hide";
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
    #[serde(rename = "state.snapshot")]
    StateSnapshot(StateSnapshotParams),
    #[serde(rename = "opencode.status")]
    OpencodeStatus(OpencodeStatusParams),
    #[serde(rename = "opencode.ensure")]
    OpencodeEnsure(OpencodeEnsureParams),
    #[serde(rename = "project.list")]
    ProjectList(ProjectListParams),
    #[serde(rename = "project.add")]
    ProjectAdd(ProjectAddParams),
    #[serde(rename = "project.clone")]
    ProjectClone(ProjectCloneParams),
    #[serde(rename = "project.remove")]
    ProjectRemove(ProjectRemoveParams),
    #[serde(rename = "project.branches")]
    ProjectBranches(ProjectBranchesParams),
    #[serde(rename = "project.browse")]
    ProjectBrowse(ProjectBrowseParams),
    #[serde(rename = "file.list")]
    FileList(FileListParams),
    #[serde(rename = "file.read")]
    FileRead(FileReadParams),
    #[serde(rename = "file.git_status")]
    FileGitStatus(FileGitStatusParams),
    #[serde(rename = "thread.create")]
    ThreadCreate(ThreadCreateParams),
    #[serde(rename = "thread.close")]
    ThreadClose(ThreadCloseParams),
    #[serde(rename = "thread.cancel")]
    ThreadCancel(ThreadCancelParams),
    #[serde(rename = "thread.reopen")]
    ThreadReopen(ThreadReopenParams),
    #[serde(rename = "thread.hide")]
    ThreadHide(ThreadHideParams),
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

pub fn parse_request_dispatch(
    method: &str,
    params: serde_json::Value,
) -> Result<RequestDispatch, String> {
    match method {
        METHOD_PING => serde_json::from_value::<PingParams>(params)
            .map(RequestDispatch::Ping)
            .map_err(|err| format!("invalid ping params: {err}")),
        METHOD_STATE_SNAPSHOT => serde_json::from_value::<StateSnapshotParams>(params)
            .map(RequestDispatch::StateSnapshot)
            .map_err(|err| format!("invalid state.snapshot params: {err}")),
        METHOD_OPENCODE_STATUS => serde_json::from_value::<OpencodeStatusParams>(params)
            .map(RequestDispatch::OpencodeStatus)
            .map_err(|err| format!("invalid opencode.status params: {err}")),
        METHOD_OPENCODE_ENSURE => serde_json::from_value::<OpencodeEnsureParams>(params)
            .map(RequestDispatch::OpencodeEnsure)
            .map_err(|err| format!("invalid opencode.ensure params: {err}")),
        METHOD_PROJECT_LIST => serde_json::from_value::<ProjectListParams>(params)
            .map(RequestDispatch::ProjectList)
            .map_err(|err| format!("invalid project.list params: {err}")),
        METHOD_PROJECT_ADD => serde_json::from_value::<ProjectAddParams>(params)
            .map(RequestDispatch::ProjectAdd)
            .map_err(|err| format!("invalid project.add params: {err}")),
        METHOD_PROJECT_CLONE => serde_json::from_value::<ProjectCloneParams>(params)
            .map(RequestDispatch::ProjectClone)
            .map_err(|err| format!("invalid project.clone params: {err}")),
        METHOD_PROJECT_REMOVE => serde_json::from_value::<ProjectRemoveParams>(params)
            .map(RequestDispatch::ProjectRemove)
            .map_err(|err| format!("invalid project.remove params: {err}")),
        METHOD_PROJECT_BRANCHES => serde_json::from_value::<ProjectBranchesParams>(params)
            .map(RequestDispatch::ProjectBranches)
            .map_err(|err| format!("invalid project.branches params: {err}")),
        METHOD_PROJECT_BROWSE => serde_json::from_value::<ProjectBrowseParams>(params)
            .map(RequestDispatch::ProjectBrowse)
            .map_err(|err| format!("invalid project.browse params: {err}")),
        METHOD_FILE_LIST => serde_json::from_value::<FileListParams>(params)
            .map(RequestDispatch::FileList)
            .map_err(|err| format!("invalid file.list params: {err}")),
        METHOD_FILE_READ => serde_json::from_value::<FileReadParams>(params)
            .map(RequestDispatch::FileRead)
            .map_err(|err| format!("invalid file.read params: {err}")),
        FILE_GIT_STATUS => serde_json::from_value::<FileGitStatusParams>(params)
            .map(RequestDispatch::FileGitStatus)
            .map_err(|err| format!("invalid file.git_status params: {err}")),
        METHOD_THREAD_CREATE => serde_json::from_value::<ThreadCreateParams>(params)
            .map(RequestDispatch::ThreadCreate)
            .map_err(|err| format!("invalid thread.create params: {err}")),
        METHOD_THREAD_CLOSE => serde_json::from_value::<ThreadCloseParams>(params)
            .map(RequestDispatch::ThreadClose)
            .map_err(|err| format!("invalid thread.close params: {err}")),
        METHOD_THREAD_CANCEL => serde_json::from_value::<ThreadCancelParams>(params)
            .map(RequestDispatch::ThreadCancel)
            .map_err(|err| format!("invalid thread.cancel params: {err}")),
        METHOD_THREAD_REOPEN => serde_json::from_value::<ThreadReopenParams>(params)
            .map(RequestDispatch::ThreadReopen)
            .map_err(|err| format!("invalid thread.reopen params: {err}")),
        METHOD_THREAD_HIDE => serde_json::from_value::<ThreadHideParams>(params)
            .map(RequestDispatch::ThreadHide)
            .map_err(|err| format!("invalid thread.hide params: {err}")),
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
