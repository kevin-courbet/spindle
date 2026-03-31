// Generated from threadmill/protocol/threadmill-rpc.schema.json
// Manual extensions for Spindle runtime events and sync deltas.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type StateVersion = u64;

pub const PROTOCOL_VERSION: &str = "2026-03-17";
pub const SUPPORTED_CAPABILITIES: &[&str] = &[
    "state.delta.operations.v1",
    "preset.output.v1",
    "rpc.errors.structured.v1",
];

pub const REQUIRED_CLIENT_CAPABILITIES: &[&str] = &[
    "state.delta.operations.v1",
    "preset.output.v1",
    "rpc.errors.structured.v1",
];

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct RpcErrorData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PresetConfig {
    pub name: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AgentConfig {
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
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
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
    #[serde(default)]
    pub chat_sessions: Vec<ChatSessionSummary>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ChatSessionStatus {
    #[serde(rename = "starting")]
    Starting,
    #[serde(rename = "ready")]
    Ready,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "ended")]
    Ended,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub enum AgentStatus {
    #[default]
    #[serde(rename = "idle")]
    Idle,
    #[serde(rename = "busy")]
    Busy,
    #[serde(rename = "stalled")]
    Stalled,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatSessionSummary {
    pub session_id: String,
    pub agent_type: String,
    pub status: ChatSessionStatus,
    #[serde(default)]
    pub agent_status: AgentStatus,
    #[serde(default)]
    pub worker_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    pub created_at: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatSessionCreatedEvent {
    pub thread_id: String,
    pub session_id: String,
    pub agent_type: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatSessionReadyEvent {
    pub acp_session_id: String,
    pub thread_id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modes: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatSessionFailedEvent {
    pub thread_id: String,
    pub session_id: String,
    pub error: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatSessionEndedEvent {
    pub thread_id: String,
    pub session_id: String,
    pub reason: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatStatusChangedEvent {
    pub thread_id: String,
    pub session_id: String,
    pub old_status: AgentStatus,
    pub new_status: AgentStatus,
    pub worker_count: usize,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crash_context: Option<PresetCrashContext>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PresetCrashContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub last_output: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum PresetOutputStream {
    #[serde(rename = "stdout")]
    Stdout,
    #[serde(rename = "stderr")]
    Stderr,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PresetOutputEvent {
    pub thread_id: String,
    pub preset: String,
    pub stream: PresetOutputStream,
    pub chunk: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum AgentProcessKind {
    #[serde(rename = "started")]
    Started,
    #[serde(rename = "exited")]
    Exited,
    #[serde(rename = "crashed")]
    Crashed,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AgentStatusChanged {
    pub channel_id: u16,
    pub project_id: String,
    pub agent_name: String,
    pub event: AgentProcessKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionHelloClient {
    pub name: String,
    pub version: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionHelloParams {
    pub client: SessionHelloClient,
    pub protocol_version: String,
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub required_capabilities: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionHelloResult {
    pub session_id: String,
    pub protocol_version: String,
    pub capabilities: Vec<String>,
    pub required_capabilities: Vec<String>,
    pub state_version: StateVersion,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StateSnapshot {
    pub state_version: StateVersion,
    pub projects: Vec<Project>,
    pub threads: Vec<Thread>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_registry: Vec<crate::services::agent_registry::AgentRegistryEntry>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum StateDeltaOperationPayload {
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        crash_context: Option<PresetCrashContext>,
    },
    #[serde(rename = "preset.output")]
    PresetOutput {
        thread_id: String,
        preset: String,
        stream: PresetOutputStream,
        chunk: String,
    },
    #[serde(rename = "chat.session_added")]
    ChatSessionAdded {
        thread_id: String,
        chat_session: ChatSessionSummary,
    },
    #[serde(rename = "chat.session_updated")]
    ChatSessionUpdated {
        thread_id: String,
        chat_session: ChatSessionSummary,
    },
    #[serde(rename = "chat.session_removed")]
    ChatSessionRemoved {
        thread_id: String,
        session_id: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StateDeltaOperation {
    pub op_id: String,
    #[serde(flatten)]
    pub payload: StateDeltaOperationPayload,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StateDeltaEvent {
    pub state_version: StateVersion,
    pub operations: Vec<StateDeltaOperation>,
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
    pub session_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TerminalDetachParams {
    pub thread_id: String,
    pub preset: String,
    pub session_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TerminalResizeParams {
    pub thread_id: String,
    pub preset: String,
    pub session_id: Option<String>,
    pub cols: u32,
    pub rows: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PresetStartParams {
    pub thread_id: String,
    pub preset: String,
    pub session_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PresetStopParams {
    pub thread_id: String,
    pub preset: String,
    pub session_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PresetRestartParams {
    pub thread_id: String,
    pub preset: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AgentStartParams {
    pub project_id: String,
    pub agent_name: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AgentStopParams {
    pub channel_id: u16,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatStartParams {
    pub thread_id: String,
    pub agent_name: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatLoadParams {
    pub thread_id: String,
    pub session_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatStopParams {
    pub thread_id: String,
    pub session_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatListParams {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatAttachParams {
    pub thread_id: String,
    pub session_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatDetachParams {
    pub channel_id: u16,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatHistoryParams {
    pub thread_id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<u64>,
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

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AgentStartResult {
    pub channel_id: u16,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct AgentStopResult {}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatStartResult {
    pub session_id: String,
    pub status: ChatSessionStatus,
}

pub type ChatLoadResult = ChatStartResult;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatStopResult {
    pub archived: bool,
}

pub type ChatListResult = Vec<ChatSessionSummary>;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatAttachResult {
    pub channel_id: u16,
    pub acp_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modes: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatDetachResult {
    pub detached: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatHistoryResult {
    pub updates: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<u64>,
}

pub const METHOD_SESSION_HELLO: &str = "session.hello";
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

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProjectLookupParams {
    pub path: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProjectLookupResult {
    pub exists: bool,
    pub is_git_repo: bool,
    pub project_id: Option<String>,
}

pub const METHOD_PROJECT_LOOKUP: &str = "project.lookup";

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
pub const METHOD_AGENT_START: &str = "agent.start";
pub const METHOD_AGENT_STOP: &str = "agent.stop";
pub const METHOD_CHAT_START: &str = "chat.start";
pub const METHOD_CHAT_LOAD: &str = "chat.load";
pub const METHOD_CHAT_STOP: &str = "chat.stop";
pub const METHOD_CHAT_LIST: &str = "chat.list";
pub const METHOD_CHAT_ATTACH: &str = "chat.attach";
pub const METHOD_CHAT_DETACH: &str = "chat.detach";
pub const METHOD_CHAT_HISTORY: &str = "chat.history";

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SystemStatsParams {}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SystemStatsResult {
    pub load_avg_1m: f64,
    pub memory_total_mb: u32,
    pub memory_used_mb: u32,
    pub opencode_instances: u32,
}

pub const METHOD_AGENT_REGISTRY_LIST: &str = "agent.registry.list";
pub const METHOD_AGENT_REGISTRY_INSTALL: &str = "agent.registry.install";

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct AgentRegistryListParams {}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AgentRegistryInstallParams {
    pub agent_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AgentRegistryInstallResult {
    pub success: bool,
    pub resolved_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub type AgentRegistryListResult = Vec<crate::services::agent_registry::AgentRegistryEntry>;

pub const METHOD_SYSTEM_STATS: &str = "system.stats";

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "method", content = "params")]
pub enum RequestDispatch {
    #[serde(rename = "session.hello")]
    SessionHello(SessionHelloParams),
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
    #[serde(rename = "project.lookup")]
    ProjectLookup(ProjectLookupParams),
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
    #[serde(rename = "agent.start")]
    AgentStart(AgentStartParams),
    #[serde(rename = "agent.stop")]
    AgentStop(AgentStopParams),
    #[serde(rename = "chat.start")]
    ChatStart(ChatStartParams),
    #[serde(rename = "chat.load")]
    ChatLoad(ChatLoadParams),
    #[serde(rename = "chat.stop")]
    ChatStop(ChatStopParams),
    #[serde(rename = "chat.list")]
    ChatList(ChatListParams),
    #[serde(rename = "chat.attach")]
    ChatAttach(ChatAttachParams),
    #[serde(rename = "chat.detach")]
    ChatDetach(ChatDetachParams),
    #[serde(rename = "chat.history")]
    ChatHistory(ChatHistoryParams),
    #[serde(rename = "system.stats")]
    SystemStats(SystemStatsParams),
    #[serde(rename = "agent.registry.list")]
    AgentRegistryList(AgentRegistryListParams),
    #[serde(rename = "agent.registry.install")]
    AgentRegistryInstall(AgentRegistryInstallParams),
}

pub fn parse_request_dispatch(
    method: &str,
    params: serde_json::Value,
) -> Result<RequestDispatch, String> {
    match method {
        METHOD_SESSION_HELLO => serde_json::from_value::<SessionHelloParams>(params)
            .map(RequestDispatch::SessionHello)
            .map_err(|err| format!("invalid session.hello params: {err}")),
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
        METHOD_PROJECT_LOOKUP => serde_json::from_value::<ProjectLookupParams>(params)
            .map(RequestDispatch::ProjectLookup)
            .map_err(|err| format!("invalid project.lookup params: {err}")),
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
        METHOD_AGENT_START => serde_json::from_value::<AgentStartParams>(params)
            .map(RequestDispatch::AgentStart)
            .map_err(|err| format!("invalid agent.start params: {err}")),
        METHOD_AGENT_STOP => serde_json::from_value::<AgentStopParams>(params)
            .map(RequestDispatch::AgentStop)
            .map_err(|err| format!("invalid agent.stop params: {err}")),
        METHOD_CHAT_START => serde_json::from_value::<ChatStartParams>(params)
            .map(RequestDispatch::ChatStart)
            .map_err(|err| format!("invalid chat.start params: {err}")),
        METHOD_CHAT_LOAD => serde_json::from_value::<ChatLoadParams>(params)
            .map(RequestDispatch::ChatLoad)
            .map_err(|err| format!("invalid chat.load params: {err}")),
        METHOD_CHAT_STOP => serde_json::from_value::<ChatStopParams>(params)
            .map(RequestDispatch::ChatStop)
            .map_err(|err| format!("invalid chat.stop params: {err}")),
        METHOD_CHAT_LIST => serde_json::from_value::<ChatListParams>(params)
            .map(RequestDispatch::ChatList)
            .map_err(|err| format!("invalid chat.list params: {err}")),
        METHOD_CHAT_ATTACH => serde_json::from_value::<ChatAttachParams>(params)
            .map(RequestDispatch::ChatAttach)
            .map_err(|err| format!("invalid chat.attach params: {err}")),
        METHOD_CHAT_DETACH => serde_json::from_value::<ChatDetachParams>(params)
            .map(RequestDispatch::ChatDetach)
            .map_err(|err| format!("invalid chat.detach params: {err}")),
        METHOD_CHAT_HISTORY => serde_json::from_value::<ChatHistoryParams>(params)
            .map(RequestDispatch::ChatHistory)
            .map_err(|err| format!("invalid chat.history params: {err}")),
        METHOD_SYSTEM_STATS => serde_json::from_value::<SystemStatsParams>(params)
            .map(RequestDispatch::SystemStats)
            .map_err(|err| format!("invalid system.stats params: {err}")),
        METHOD_AGENT_REGISTRY_LIST => serde_json::from_value::<AgentRegistryListParams>(params)
            .map(RequestDispatch::AgentRegistryList)
            .map_err(|err| format!("invalid agent.registry.list params: {err}")),
        METHOD_AGENT_REGISTRY_INSTALL => {
            serde_json::from_value::<AgentRegistryInstallParams>(params)
                .map(RequestDispatch::AgentRegistryInstall)
                .map_err(|err| format!("invalid agent.registry.install params: {err}"))
        }
        _ => Err(format!("unknown method '{method}'")),
    }
}
