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
pub struct ProjectAgentConfig {
    pub name: String,
    pub command: String,
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
    pub agents: Vec<ProjectAgentConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_chat_model: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatSessionCreatedEvent {
    pub thread_id: String,
    pub session_id: String,
    pub agent_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
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

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatWorkerUpdateEvent {
    pub parent_session_id: String,
    pub worker_session_id: String,
    pub agent_status: AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_tool: Option<WorkerToolSummary>,
    pub tool_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkerToolSummary {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatStatusParams {
    pub session_id: String,
}

pub type ChatStatusResult = ChatSessionSummary;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum WorkflowPhase {
    #[serde(rename = "PLANNING")]
    Planning,
    #[serde(rename = "IMPLEMENTING")]
    Implementing,
    #[serde(rename = "TESTING")]
    Testing,
    #[serde(rename = "REVIEWING")]
    Reviewing,
    #[serde(rename = "FIXING")]
    Fixing,
    #[serde(rename = "COMPLETE")]
    Complete,
    #[serde(rename = "BLOCKED")]
    Blocked,
    #[serde(rename = "FAILED")]
    Failed,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum WorkflowWorkerStatus {
    #[serde(rename = "PLANNED")]
    Planned,
    #[serde(rename = "SPAWNING")]
    Spawning,
    #[serde(rename = "RUNNING")]
    Running,
    #[serde(rename = "COMPLETED")]
    Completed,
    #[serde(rename = "FAILED")]
    Failed,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum WorkflowFindingSeverity {
    #[serde(rename = "LOW")]
    Low,
    #[serde(rename = "MEDIUM")]
    Medium,
    #[serde(rename = "HIGH")]
    High,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    #[serde(rename = "DONE")]
    Done,
    #[serde(rename = "CONTEXT_EXHAUSTED")]
    ContextExhausted,
    #[serde(rename = "BLOCKED_NEED_INFO")]
    BlockedNeedInfo,
    #[serde(rename = "BLOCKED_TECHNICAL")]
    BlockedTechnical,
    #[serde(rename = "QUALITY_CONCERN")]
    QualityConcern,
    #[serde(rename = "SCOPE_CREEP")]
    ScopeCreep,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowHandoff {
    pub stop_reason: StopReason,
    #[serde(default)]
    pub progress: Vec<String>,
    #[serde(default)]
    pub next_steps: Vec<String>,
    pub context: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blockers: Option<String>,
    pub recorded_at: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowWorker {
    pub worker_id: String,
    pub agent_name: String,
    pub status: WorkflowWorkerStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<WorkflowHandoff>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_status: Option<AgentStatus>,
    /// URL of the implementation issue this worker is assigned to. When set at
    /// spawn time, Spindle resolves the issue body via `IssueTransport` and
    /// prepends it to the worker's initial prompt so the agent boots with full
    /// context. Used by the Mac inspector to render "Working on #42" per worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_url: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowReviewer {
    pub reviewer_id: String,
    pub agent_name: String,
    pub status: WorkflowWorkerStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_status: Option<AgentStatus>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowFinding {
    pub finding_id: String,
    pub severity: WorkflowFindingSeverity,
    pub summary: String,
    pub details: String,
    #[serde(default)]
    pub source_reviewers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub created_at: String,
    /// Resolution flag toggled by `workflow.resolve_finding`. UI uses this to
    /// drive the "all findings resolved → back to TESTING" transition.
    #[serde(default)]
    pub resolved: bool,
    /// Review round in which this finding was surfaced — 1 for the initial
    /// review, 2 for post-fix re-review, etc. Populated from the workflow's
    /// `current_review_round` at the time `workflow.record_findings` ran.
    #[serde(default = "default_review_round")]
    pub review_round: u32,
}

fn default_review_round() -> u32 {
    1
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowState {
    pub workflow_id: String,
    pub thread_id: String,
    pub phase: WorkflowPhase,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prd_issue_url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub implementation_issue_urls: Vec<String>,
    /// Chat session driving the workflow (e.g. the session that ran /sisyphus).
    /// Target for `inject_system_context` when non-orchestrator actors (UI, CLI, other
    /// agents) mutate workflow state and the orchestrator must be notified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestrator_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workers: Vec<WorkflowWorker>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviewers: Vec<WorkflowReviewer>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<WorkflowFinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_started_at: Option<String>,
    /// Monotonic counter incremented on each `workflow.start_review`. Used to
    /// distinguish initial-round findings from post-fix re-review findings.
    #[serde(default)]
    pub current_review_round: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowCreatedEvent {
    pub workflow: WorkflowState,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowPhaseChangedEvent {
    pub workflow_id: String,
    pub thread_id: String,
    pub old_phase: WorkflowPhase,
    pub new_phase: WorkflowPhase,
    #[serde(default)]
    pub forced: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowWorkerSpawnedEvent {
    pub workflow_id: String,
    pub thread_id: String,
    pub worker: WorkflowWorker,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowWorkerCompletedEvent {
    pub workflow_id: String,
    pub thread_id: String,
    pub worker: WorkflowWorker,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowReviewStartedEvent {
    pub workflow_id: String,
    pub thread_id: String,
    #[serde(default)]
    pub reviewers: Vec<WorkflowReviewer>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowReviewerCompletedEvent {
    pub workflow_id: String,
    pub thread_id: String,
    pub reviewer: WorkflowReviewer,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowFindingsRecordedEvent {
    pub workflow_id: String,
    pub thread_id: String,
    #[serde(default)]
    pub findings: Vec<WorkflowFinding>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowCompletedEvent {
    pub workflow: WorkflowState,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowReviewCompletedEvent {
    pub workflow_id: String,
    pub thread_id: String,
    /// Review round that just finished (1-indexed).
    pub review_round: u32,
    /// How many reviewers participated in this round (all terminal by emit time).
    /// Reviewer sessions always route through `set_session_failed` on session end —
    /// whether the agent errored or finished cleanly — so a separate
    /// "completed vs failed" split would be structurally meaningless at this layer.
    /// Callers that want to distinguish can inspect individual
    /// `WorkflowReviewer.failure_message` fields.
    pub reviewer_count: usize,
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
    #[serde(default)]
    pub workflows: Vec<WorkflowState>,
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
    #[serde(rename = "workflow.upsert")]
    WorkflowUpsert { workflow: WorkflowState },
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

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum FileDiffScope {
    #[serde(rename = "working")]
    Working,
    #[serde(rename = "staged")]
    Staged,
    #[serde(rename = "head")]
    Head,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileDiffSummaryParams {
    pub thread_id: String,
    pub scope: FileDiffScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileDiffParams {
    pub thread_id: String,
    pub scope: FileDiffScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
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
pub struct ChatStartParams {
    pub thread_id: String,
    pub agent_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Explicit ACP model ID override (e.g. `claude-opus-4-7`). Takes precedence over
    /// the project's `default_chat_model`. Sourced from agent-def `model:` frontmatter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_model: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatLoadParams {
    pub thread_id: String,
    pub session_id: String,
    #[serde(default)]
    pub agent_name: Option<String>,
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
pub struct WorkflowCreateParams {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prd_issue_url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub implementation_issue_urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestrator_session_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowStatusParams {
    pub workflow_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct WorkflowListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowTransitionParams {
    pub workflow_id: String,
    pub phase: WorkflowPhase,
    #[serde(default)]
    pub force: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowSpawnWorkerParams {
    pub workflow_id: String,
    pub agent_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_model: Option<String>,
    /// Implementation issue URL this worker owns. If set, Spindle resolves the
    /// issue via `IssueTransport` at spawn time and prepends the body to
    /// `initial_prompt`. Best-effort: resolve failures skip injection but do
    /// not fail the spawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_url: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowRecordHandoffParams {
    pub workflow_id: String,
    pub worker_id: String,
    pub stop_reason: StopReason,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub progress: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_steps: Vec<String>,
    pub context: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blockers: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowReviewerSpec {
    pub agent_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_model: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowStartReviewParams {
    pub workflow_id: String,
    #[serde(default)]
    pub force: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviewers: Vec<WorkflowReviewerSpec>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowSpawnReviewerParams {
    pub workflow_id: String,
    pub agent_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_model: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowListReviewersParams {
    pub workflow_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowFindingInput {
    pub severity: WorkflowFindingSeverity,
    pub summary: String,
    pub details: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_reviewers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowRecordFindingsParams {
    pub workflow_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<WorkflowFindingInput>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowCompleteParams {
    pub workflow_id: String,
    #[serde(default)]
    pub force: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct IssueListParams {
    pub project_id: String,
    /// Optional label filter. `None` means no label filter (all issues).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Desired state filter. Defaults to `Open`. `Closed` / `All` are only
    /// honored when `scope=LinkedTo` (post-filtered via `resolve`); requesting
    /// `Closed` or `All` together with `scope=All` / `scope=Prds` returns an
    /// explicit error rather than silently surfacing open-only results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<IssueListState>,
    /// Scope the listing: all repo issues, PRDs only, or the issues linked to a
    /// specific workflow. Defaults to `All`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<IssueListScope>,
    /// Maximum number of issues to return. Capped server-side at 100; defaults to 50.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// When true, skip the in-memory 30s cache and always re-shell out. The fresh
    /// result is still written back to the cache. UI refresh buttons should set this.
    #[serde(default)]
    pub bypass_cache: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum IssueListState {
    #[default]
    Open,
    Closed,
    All,
}

/// Scope selector for `issue.list`. Serialized with an internal `kind` tag so
/// the wire form stays `{ "kind": "linked_to", "workflow_id": "..." }` etc.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IssueListScope {
    #[default]
    All,
    LinkedTo {
        workflow_id: String,
    },
    Prds,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum WorkflowIssuePlatform {
    #[serde(rename = "github")]
    Github,
    #[serde(rename = "gitlab")]
    Gitlab,
    #[serde(rename = "unknown")]
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowIssueRef {
    pub number: u64,
    pub title: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// Short preview of the issue body — first ~200 chars — so the Mac can show
    /// it in the list without a second round-trip. Full body is fetched on demand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_preview: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum IssueState {
    Open,
    Closed,
}

/// Wire-format mirror of the internal `EnrichedIssue`. Kept in `protocol` so
/// the Mac-side decoder doesn't have to pull in `services::issues`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EnrichedIssueWire {
    pub r#ref: WorkflowIssueRef,
    pub state: IssueState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assignees: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// Wire-format mirror of the internal `IssueDraft`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct IssueDraftWire {
    pub title: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assignees: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct IssueListResult {
    pub platform: WorkflowIssuePlatform,
    pub issues: Vec<WorkflowIssueRef>,
    /// True when this response came from the 30s TTL cache rather than a fresh
    /// gh/glab shell-out. UI can use this to skip flashing spinners.
    #[serde(default)]
    pub cached: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct IssueResolveParams {
    pub project_id: String,
    pub url: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct IssueResolveResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<EnrichedIssueWire>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct IssueCloseParams {
    pub project_id: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_worker_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct IssueCloseResult {
    pub success: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct IssueCommentParams {
    pub project_id: String,
    pub url: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_worker_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct IssueCommentResult {
    pub success: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct IssueCreateParams {
    pub project_id: String,
    pub draft: IssueDraftWire,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_to_workflow_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct IssueCreateResult {
    pub issue_ref: WorkflowIssueRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked_workflow_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowAddLinkedIssueParams {
    pub workflow_id: String,
    pub url: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowAddLinkedIssueResult {
    pub workflow: WorkflowState,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowResolveLinkedIssuesParams {
    pub workflow_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowResolveLinkedIssuesResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prd: Option<EnrichedIssueWire>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub implementations: Vec<EnrichedIssueWire>,
}

// ---------- Issue event payloads (direct events, not state deltas) ----------

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowIssueClosedEvent {
    pub project_id: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_worker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowIssueCommentedEvent {
    pub project_id: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_worker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<String>,
}

/// Project-level lifecycle signal that an issue was just created. Linkage to
/// a workflow (when `link_to_workflow_id` is supplied to `issue.create`) is
/// observed via the concurrent `WorkflowUpsert` state delta — that delta is
/// the sole source of truth for workflow ownership of an issue. This event
/// intentionally does not carry a workflow id so consumers don't race the
/// delta or treat it as authoritative.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowIssueCreatedEvent {
    pub project_id: String,
    pub issue_ref: WorkflowIssueRef,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowStartFromIssueParams {
    pub thread_id: String,
    pub issue_url: String,
    pub issue_title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_body: Option<String>,
    /// Persona name under `.threadmill/agents/` — defaults to "sisyphus".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowStartFromIssueResult {
    pub workflow_id: String,
    pub orchestrator_session_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowResolveFindingParams {
    pub workflow_id: String,
    pub finding_id: String,
    /// Target resolution state. Default is `true` (mark resolved); pass `false` to
    /// unresolve.
    #[serde(default = "default_true")]
    pub resolved: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowResolveFindingResult {
    pub workflow_id: String,
    pub finding: WorkflowFinding,
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

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileDiffSummaryEntry {
    pub path: String,
    pub status: String,
    pub added: u32,
    pub removed: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileDiffSummaryResult {
    pub files: Vec<FileDiffSummaryEntry>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileDiffStats {
    pub added: u32,
    pub removed: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileDiffResult {
    pub path: String,
    pub status: String,
    pub diff_text: String,
    pub stats: FileDiffStats,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CheckpointSummary {
    pub seq: u64,
    pub git_ref: String,
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub head_oid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CheckpointSaveParams {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_preview: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CheckpointSaveResult {
    pub skipped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<CheckpointSummary>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CheckpointRestoreParams {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CheckpointRestoreResult {
    pub restored: bool,
    pub checkpoint: CheckpointSummary,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CheckpointListParams {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

pub type CheckpointListResult = Vec<CheckpointSummary>;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CheckpointDiffParams {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_ref: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CheckpointDiffResult {
    pub base_ref: String,
    pub target_ref: String,
    pub diff_text: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GitStatusSummaryFile {
    pub path: String,
    pub staged: bool,
    pub unstaged: bool,
    pub untracked: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GitStatusSummaryParams {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GitStatusSummaryResult {
    pub branch: String,
    pub ahead_count: u32,
    pub behind_count: u32,
    pub files: Vec<GitStatusSummaryFile>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GitCommitParams {
    pub thread_id: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    #[serde(default)]
    pub amend: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GitCommitResult {
    pub commit_hash: String,
    pub summary: String,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GitPushParams {
    pub thread_id: String,
    #[serde(default)]
    pub force: bool,
    #[serde(default = "default_true")]
    pub set_upstream: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GitPushResult {
    pub success: bool,
    pub remote: String,
    pub branch: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GitCreatePrParams {
    pub thread_id: String,
    pub title: String,
    pub body: String,
    #[serde(default = "default_true")]
    pub draft: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GitCreatePrResult {
    pub pr_url: String,
    pub pr_number: u64,
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
pub struct ChatStartResult {
    pub session_id: String,
    pub status: ChatSessionStatus,
}

pub type ChatLoadResult = ChatStartResult;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatForkParams {
    pub thread_id: String,
    pub source_session_id: String,
    pub message_cursor: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatForkResult {
    pub session_id: String,
    pub agent_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

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

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowCreateResult {
    pub workflow_id: String,
}

pub type WorkflowStatusResult = WorkflowState;
pub type WorkflowListResult = Vec<WorkflowState>;
pub type WorkflowTransitionResult = WorkflowState;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowSpawnWorkerResult {
    pub workflow_id: String,
    pub worker: WorkflowWorker,
}

pub type WorkflowRecordHandoffResult = WorkflowWorker;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowStartReviewResult {
    pub workflow: WorkflowState,
    #[serde(default)]
    pub reviewers: Vec<WorkflowReviewer>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkflowSpawnReviewerResult {
    pub workflow_id: String,
    pub reviewer: WorkflowReviewer,
}

pub type WorkflowListReviewersResult = Vec<WorkflowReviewer>;
pub type WorkflowRecordFindingsResult = Vec<WorkflowFinding>;
pub type WorkflowCompleteResult = WorkflowState;

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
pub const METHOD_FILE_DIFF_SUMMARY: &str = "file.diff_summary";
pub const METHOD_FILE_DIFF: &str = "file.diff";
pub const METHOD_CHECKPOINT_SAVE: &str = "checkpoint.save";
pub const METHOD_CHECKPOINT_RESTORE: &str = "checkpoint.restore";
pub const METHOD_CHECKPOINT_LIST: &str = "checkpoint.list";
pub const METHOD_CHECKPOINT_DIFF: &str = "checkpoint.diff";
pub const METHOD_GIT_STATUS_SUMMARY: &str = "git.status_summary";
pub const METHOD_GIT_COMMIT: &str = "git.commit";
pub const METHOD_GIT_PUSH: &str = "git.push";
pub const METHOD_GIT_CREATE_PR: &str = "git.create_pr";
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
pub const METHOD_CHAT_START: &str = "chat.start";
pub const METHOD_CHAT_LOAD: &str = "chat.load";
pub const METHOD_CHAT_STOP: &str = "chat.stop";
pub const METHOD_CHAT_LIST: &str = "chat.list";
pub const METHOD_CHAT_ATTACH: &str = "chat.attach";
pub const METHOD_CHAT_DETACH: &str = "chat.detach";
pub const METHOD_CHAT_HISTORY: &str = "chat.history";
pub const METHOD_CHAT_STATUS: &str = "chat.status";
pub const METHOD_CHAT_FORK: &str = "chat.fork";
pub const METHOD_WORKFLOW_CREATE: &str = "workflow.create";
pub const METHOD_WORKFLOW_STATUS: &str = "workflow.status";
pub const METHOD_WORKFLOW_LIST: &str = "workflow.list";
pub const METHOD_WORKFLOW_TRANSITION: &str = "workflow.transition";
pub const METHOD_WORKFLOW_SPAWN_WORKER: &str = "workflow.spawn_worker";
pub const METHOD_WORKFLOW_RECORD_HANDOFF: &str = "workflow.record_handoff";
pub const METHOD_WORKFLOW_START_REVIEW: &str = "workflow.start_review";
pub const METHOD_WORKFLOW_SPAWN_REVIEWER: &str = "workflow.spawn_reviewer";
pub const METHOD_WORKFLOW_LIST_REVIEWERS: &str = "workflow.list_reviewers";
pub const METHOD_WORKFLOW_RECORD_FINDINGS: &str = "workflow.record_findings";
pub const METHOD_WORKFLOW_COMPLETE: &str = "workflow.complete";
pub const METHOD_WORKFLOW_RESOLVE_FINDING: &str = "workflow.resolve_finding";
pub const METHOD_WORKFLOW_START_FROM_ISSUE: &str = "workflow.start_from_issue";
pub const METHOD_WORKFLOW_ADD_LINKED_ISSUE: &str = "workflow.add_linked_issue";
pub const METHOD_WORKFLOW_RESOLVE_LINKED_ISSUES: &str = "workflow.resolve_linked_issues";
pub const METHOD_ISSUE_LIST: &str = "issue.list";
pub const METHOD_ISSUE_RESOLVE: &str = "issue.resolve";
pub const METHOD_ISSUE_CLOSE: &str = "issue.close";
pub const METHOD_ISSUE_COMMENT: &str = "issue.comment";
pub const METHOD_ISSUE_CREATE: &str = "issue.create";

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
    #[serde(rename = "file.diff_summary")]
    FileDiffSummary(FileDiffSummaryParams),
    #[serde(rename = "file.diff")]
    FileDiff(FileDiffParams),
    #[serde(rename = "checkpoint.save")]
    CheckpointSave(CheckpointSaveParams),
    #[serde(rename = "checkpoint.restore")]
    CheckpointRestore(CheckpointRestoreParams),
    #[serde(rename = "checkpoint.list")]
    CheckpointList(CheckpointListParams),
    #[serde(rename = "checkpoint.diff")]
    CheckpointDiff(CheckpointDiffParams),
    #[serde(rename = "git.status_summary")]
    GitStatusSummary(GitStatusSummaryParams),
    #[serde(rename = "git.commit")]
    GitCommit(GitCommitParams),
    #[serde(rename = "git.push")]
    GitPush(GitPushParams),
    #[serde(rename = "git.create_pr")]
    GitCreatePr(GitCreatePrParams),
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
    #[serde(rename = "chat.status")]
    ChatStatus(ChatStatusParams),
    ChatFork(ChatForkParams),
    #[serde(rename = "workflow.create")]
    WorkflowCreate(WorkflowCreateParams),
    #[serde(rename = "workflow.status")]
    WorkflowStatus(WorkflowStatusParams),
    #[serde(rename = "workflow.list")]
    WorkflowList(WorkflowListParams),
    #[serde(rename = "workflow.transition")]
    WorkflowTransition(WorkflowTransitionParams),
    #[serde(rename = "workflow.spawn_worker")]
    WorkflowSpawnWorker(WorkflowSpawnWorkerParams),
    #[serde(rename = "workflow.record_handoff")]
    WorkflowRecordHandoff(WorkflowRecordHandoffParams),
    #[serde(rename = "workflow.start_review")]
    WorkflowStartReview(WorkflowStartReviewParams),
    #[serde(rename = "workflow.spawn_reviewer")]
    WorkflowSpawnReviewer(WorkflowSpawnReviewerParams),
    #[serde(rename = "workflow.list_reviewers")]
    WorkflowListReviewers(WorkflowListReviewersParams),
    #[serde(rename = "workflow.record_findings")]
    WorkflowRecordFindings(WorkflowRecordFindingsParams),
    #[serde(rename = "workflow.complete")]
    WorkflowComplete(WorkflowCompleteParams),
    #[serde(rename = "workflow.resolve_finding")]
    WorkflowResolveFinding(WorkflowResolveFindingParams),
    #[serde(rename = "workflow.start_from_issue")]
    WorkflowStartFromIssue(WorkflowStartFromIssueParams),
    #[serde(rename = "workflow.add_linked_issue")]
    WorkflowAddLinkedIssue(WorkflowAddLinkedIssueParams),
    #[serde(rename = "workflow.resolve_linked_issues")]
    WorkflowResolveLinkedIssues(WorkflowResolveLinkedIssuesParams),
    #[serde(rename = "issue.list")]
    IssueList(IssueListParams),
    #[serde(rename = "issue.resolve")]
    IssueResolve(IssueResolveParams),
    #[serde(rename = "issue.close")]
    IssueClose(IssueCloseParams),
    #[serde(rename = "issue.comment")]
    IssueComment(IssueCommentParams),
    #[serde(rename = "issue.create")]
    IssueCreate(IssueCreateParams),
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
        METHOD_FILE_DIFF_SUMMARY => serde_json::from_value::<FileDiffSummaryParams>(params)
            .map(RequestDispatch::FileDiffSummary)
            .map_err(|err| format!("invalid file.diff_summary params: {err}")),
        METHOD_FILE_DIFF => serde_json::from_value::<FileDiffParams>(params)
            .map(RequestDispatch::FileDiff)
            .map_err(|err| format!("invalid file.diff params: {err}")),
        METHOD_CHECKPOINT_SAVE => serde_json::from_value::<CheckpointSaveParams>(params)
            .map(RequestDispatch::CheckpointSave)
            .map_err(|err| format!("invalid checkpoint.save params: {err}")),
        METHOD_CHECKPOINT_RESTORE => serde_json::from_value::<CheckpointRestoreParams>(params)
            .map(RequestDispatch::CheckpointRestore)
            .map_err(|err| format!("invalid checkpoint.restore params: {err}")),
        METHOD_CHECKPOINT_LIST => serde_json::from_value::<CheckpointListParams>(params)
            .map(RequestDispatch::CheckpointList)
            .map_err(|err| format!("invalid checkpoint.list params: {err}")),
        METHOD_CHECKPOINT_DIFF => serde_json::from_value::<CheckpointDiffParams>(params)
            .map(RequestDispatch::CheckpointDiff)
            .map_err(|err| format!("invalid checkpoint.diff params: {err}")),
        METHOD_GIT_STATUS_SUMMARY => serde_json::from_value::<GitStatusSummaryParams>(params)
            .map(RequestDispatch::GitStatusSummary)
            .map_err(|err| format!("invalid git.status_summary params: {err}")),
        METHOD_GIT_COMMIT => serde_json::from_value::<GitCommitParams>(params)
            .map(RequestDispatch::GitCommit)
            .map_err(|err| format!("invalid git.commit params: {err}")),
        METHOD_GIT_PUSH => serde_json::from_value::<GitPushParams>(params)
            .map(RequestDispatch::GitPush)
            .map_err(|err| format!("invalid git.push params: {err}")),
        METHOD_GIT_CREATE_PR => serde_json::from_value::<GitCreatePrParams>(params)
            .map(RequestDispatch::GitCreatePr)
            .map_err(|err| format!("invalid git.create_pr params: {err}")),
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
        METHOD_CHAT_STATUS => serde_json::from_value::<ChatStatusParams>(params)
            .map(RequestDispatch::ChatStatus)
            .map_err(|err| format!("invalid chat.status params: {err}")),
        METHOD_CHAT_FORK => serde_json::from_value::<ChatForkParams>(params)
            .map(RequestDispatch::ChatFork)
            .map_err(|err| format!("invalid chat.fork params: {err}")),
        METHOD_WORKFLOW_CREATE => serde_json::from_value::<WorkflowCreateParams>(params)
            .map(RequestDispatch::WorkflowCreate)
            .map_err(|err| format!("invalid workflow.create params: {err}")),
        METHOD_WORKFLOW_STATUS => serde_json::from_value::<WorkflowStatusParams>(params)
            .map(RequestDispatch::WorkflowStatus)
            .map_err(|err| format!("invalid workflow.status params: {err}")),
        METHOD_WORKFLOW_LIST => serde_json::from_value::<WorkflowListParams>(params)
            .map(RequestDispatch::WorkflowList)
            .map_err(|err| format!("invalid workflow.list params: {err}")),
        METHOD_WORKFLOW_TRANSITION => serde_json::from_value::<WorkflowTransitionParams>(params)
            .map(RequestDispatch::WorkflowTransition)
            .map_err(|err| format!("invalid workflow.transition params: {err}")),
        METHOD_WORKFLOW_SPAWN_WORKER => serde_json::from_value::<WorkflowSpawnWorkerParams>(params)
            .map(RequestDispatch::WorkflowSpawnWorker)
            .map_err(|err| format!("invalid workflow.spawn_worker params: {err}")),
        METHOD_WORKFLOW_RECORD_HANDOFF => {
            serde_json::from_value::<WorkflowRecordHandoffParams>(params)
                .map(RequestDispatch::WorkflowRecordHandoff)
                .map_err(|err| format!("invalid workflow.record_handoff params: {err}"))
        }
        METHOD_WORKFLOW_START_REVIEW => serde_json::from_value::<WorkflowStartReviewParams>(params)
            .map(RequestDispatch::WorkflowStartReview)
            .map_err(|err| format!("invalid workflow.start_review params: {err}")),
        METHOD_WORKFLOW_SPAWN_REVIEWER => {
            serde_json::from_value::<WorkflowSpawnReviewerParams>(params)
                .map(RequestDispatch::WorkflowSpawnReviewer)
                .map_err(|err| format!("invalid workflow.spawn_reviewer params: {err}"))
        }
        METHOD_WORKFLOW_LIST_REVIEWERS => {
            serde_json::from_value::<WorkflowListReviewersParams>(params)
                .map(RequestDispatch::WorkflowListReviewers)
                .map_err(|err| format!("invalid workflow.list_reviewers params: {err}"))
        }
        METHOD_WORKFLOW_RECORD_FINDINGS => {
            serde_json::from_value::<WorkflowRecordFindingsParams>(params)
                .map(RequestDispatch::WorkflowRecordFindings)
                .map_err(|err| format!("invalid workflow.record_findings params: {err}"))
        }
        METHOD_WORKFLOW_COMPLETE => serde_json::from_value::<WorkflowCompleteParams>(params)
            .map(RequestDispatch::WorkflowComplete)
            .map_err(|err| format!("invalid workflow.complete params: {err}")),
        METHOD_WORKFLOW_RESOLVE_FINDING => {
            serde_json::from_value::<WorkflowResolveFindingParams>(params)
                .map(RequestDispatch::WorkflowResolveFinding)
                .map_err(|err| format!("invalid workflow.resolve_finding params: {err}"))
        }
        METHOD_WORKFLOW_START_FROM_ISSUE => {
            serde_json::from_value::<WorkflowStartFromIssueParams>(params)
                .map(RequestDispatch::WorkflowStartFromIssue)
                .map_err(|err| format!("invalid workflow.start_from_issue params: {err}"))
        }
        METHOD_WORKFLOW_ADD_LINKED_ISSUE => {
            serde_json::from_value::<WorkflowAddLinkedIssueParams>(params)
                .map(RequestDispatch::WorkflowAddLinkedIssue)
                .map_err(|err| format!("invalid workflow.add_linked_issue params: {err}"))
        }
        METHOD_WORKFLOW_RESOLVE_LINKED_ISSUES => {
            serde_json::from_value::<WorkflowResolveLinkedIssuesParams>(params)
                .map(RequestDispatch::WorkflowResolveLinkedIssues)
                .map_err(|err| format!("invalid workflow.resolve_linked_issues params: {err}"))
        }
        METHOD_ISSUE_LIST => serde_json::from_value::<IssueListParams>(params)
            .map(RequestDispatch::IssueList)
            .map_err(|err| format!("invalid issue.list params: {err}")),
        METHOD_ISSUE_RESOLVE => serde_json::from_value::<IssueResolveParams>(params)
            .map(RequestDispatch::IssueResolve)
            .map_err(|err| format!("invalid issue.resolve params: {err}")),
        METHOD_ISSUE_CLOSE => serde_json::from_value::<IssueCloseParams>(params)
            .map(RequestDispatch::IssueClose)
            .map_err(|err| format!("invalid issue.close params: {err}")),
        METHOD_ISSUE_COMMENT => serde_json::from_value::<IssueCommentParams>(params)
            .map(RequestDispatch::IssueComment)
            .map_err(|err| format!("invalid issue.comment params: {err}")),
        METHOD_ISSUE_CREATE => serde_json::from_value::<IssueCreateParams>(params)
            .map(RequestDispatch::IssueCreate)
            .map_err(|err| format!("invalid issue.create params: {err}")),
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_workflow() -> WorkflowState {
        WorkflowState {
            workflow_id: "workflow-1".into(),
            thread_id: "thread-1".into(),
            current_review_round: 0,
            phase: WorkflowPhase::Reviewing,
            created_at: "2026-04-12T00:00:00Z".into(),
            updated_at: "2026-04-12T00:00:00Z".into(),
            completed_at: None,
            prd_issue_url: None,
            implementation_issue_urls: Vec::new(),
            orchestrator_session_id: None,
            workers: Vec::new(),
            reviewers: Vec::new(),
            findings: Vec::new(),
            review_started_at: None,
        }
    }

    #[test]
    fn state_snapshot_serializes_empty_workflows_array() {
        let snapshot = StateSnapshot {
            state_version: 1,
            projects: Vec::new(),
            threads: Vec::new(),
            workflows: Vec::new(),
            agent_registry: Vec::new(),
        };

        let value = serde_json::to_value(&snapshot).expect("serialize state snapshot");

        assert_eq!(
            value,
            json!({
                "state_version": 1,
                "projects": [],
                "threads": [],
                "workflows": []
            })
        );
    }

    #[test]
    fn workflow_review_payloads_serialize_empty_required_arrays() {
        let workflow = sample_workflow();

        let start_review = WorkflowStartReviewResult {
            workflow: workflow.clone(),
            reviewers: Vec::new(),
        };
        let review_started = WorkflowReviewStartedEvent {
            workflow_id: workflow.workflow_id.clone(),
            thread_id: workflow.thread_id.clone(),
            reviewers: Vec::new(),
        };
        let findings_recorded = WorkflowFindingsRecordedEvent {
            workflow_id: workflow.workflow_id.clone(),
            thread_id: workflow.thread_id.clone(),
            findings: Vec::new(),
        };

        let start_review_value =
            serde_json::to_value(&start_review).expect("serialize workflow.start_review result");
        let review_started_value =
            serde_json::to_value(&review_started).expect("serialize workflow.review_started event");
        let findings_recorded_value = serde_json::to_value(&findings_recorded)
            .expect("serialize workflow.findings_recorded event");

        assert_eq!(start_review_value["reviewers"], json!([]));
        assert_eq!(review_started_value["reviewers"], json!([]));
        assert_eq!(findings_recorded_value["findings"], json!([]));
    }
}
