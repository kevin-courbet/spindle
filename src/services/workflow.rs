use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use tracing::{info, warn};
use uuid::Uuid;

/// Per-issue resolve timeout. Transports shell out to `gh`/`glab` which can
/// hang on auth issues or network blackholes — capping per call keeps the
/// caller responsive and lets us fall back gracefully.
const ISSUE_RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
/// Hard ceiling on per-call transport fan-out when post-filtering is required
/// (e.g. `issue.list` scope=LinkedTo with a state filter). Prevents pathological
/// O(N) shell-outs on workflows with absurdly long linked-issue lists.
const ISSUE_RESOLVE_MAX_FANOUT: usize = 100;

use crate::{protocol, services::chat::ChatService, AppState};

pub struct WorkflowService;

// ---------- Auto-selection of reviewer personas ----------

/// Parsed `.threadmill/agents/<name>.md` — the deployed persona specs the review
/// swarm references by name. Mirrors `threadmill-cli`'s parse_agent_def.
struct AgentPersonaFile {
    agent: String,
    model: Option<String>,
    display_name: Option<String>,
    system_prompt: String,
}

fn parse_agent_persona_file(path: &Path) -> Result<AgentPersonaFile, String> {
    let content = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;

    let (frontmatter, body) = if let Some(after_open) = content.strip_prefix("---\n") {
        if let Some(end) = after_open.find("\n---\n") {
            (&after_open[..end], after_open[end + 5..].trim())
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
        if let Some(v) = line.strip_prefix("agent:") {
            agent = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("display_name:") {
            display_name = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("model:") {
            let t = v.trim();
            if !t.is_empty() {
                model = Some(t.to_string());
            }
        }
    }

    if agent.is_empty() {
        return Err(format!(
            "agent persona {} missing 'agent' field in frontmatter",
            path.display()
        ));
    }

    Ok(AgentPersonaFile {
        agent,
        model,
        display_name,
        system_prompt: body.to_string(),
    })
}

/// Resolve a persona name (e.g. "reviewer-codex") to its file, preferring the
/// project-local `.threadmill/agents/` over the global `~/.config/threadmill/agents/`.
fn resolve_persona_path(worktree: &Path, name: &str) -> Option<PathBuf> {
    let project = worktree
        .join(".threadmill/agents")
        .join(format!("{name}.md"));
    if project.exists() {
        return Some(project);
    }
    let global = dirs::config_dir()?
        .join("threadmill/agents")
        .join(format!("{name}.md"));
    if global.exists() {
        return Some(global);
    }
    None
}

fn spec_from_persona(
    persona: &AgentPersonaFile,
    display_suffix: &str,
    initial_prompt: String,
) -> protocol::WorkflowReviewerSpec {
    let display_name = persona
        .display_name
        .as_ref()
        .map(|base| format!("{base} — {display_suffix}"))
        .or_else(|| Some(display_suffix.to_string()));
    protocol::WorkflowReviewerSpec {
        agent_name: persona.agent.clone(),
        parent_session_id: None,
        system_prompt: Some(persona.system_prompt.clone()),
        initial_prompt: Some(initial_prompt),
        display_name,
        preferred_model: persona.model.clone(),
    }
}

/// Read the branch's diff against the nearest default branch. Tries `main`, then
/// `master`. Empty string if neither exists or git is unavailable.
async fn branch_diff(worktree: &Path) -> String {
    for base in ["main", "master"] {
        let output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(["diff", &format!("{base}...HEAD")])
            .output()
            .await;
        if let Ok(output) = output {
            if output.status.success() {
                return String::from_utf8_lossy(&output.stdout).to_string();
            }
        }
    }
    String::new()
}

/// True if the given changed-file path looks like a test file. Matches filename
/// extensions (`*.test.ts`, `*.test.tsx`, `*.spec.ts`, `*.spec.tsx`, `*_test.rs`,
/// `*_test.py`, `*Test.swift`, `*Tests.swift`, `*Spec.swift`) and common test-directory
/// segments (`tests/`, `__tests__/`). Avoids the substring trap — "test" on its own
/// hits `contested.ts`, `attestation.rs`, etc.
fn is_test_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    if lower.contains("/tests/") || lower.contains("/__tests__/") || lower.contains("/test/") {
        return true;
    }
    for suffix in [
        ".test.ts",
        ".test.tsx",
        ".test.js",
        ".test.jsx",
        ".spec.ts",
        ".spec.tsx",
        ".spec.js",
        ".spec.jsx",
        "_test.rs",
        "_test.py",
        "_test.go",
        "test.swift",
        "tests.swift",
        "spec.swift",
    ] {
        if lower.ends_with(suffix) {
            return true;
        }
    }
    false
}

async fn branch_changed_files(worktree: &Path) -> Vec<String> {
    for base in ["main", "master"] {
        let output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(["diff", "--name-only", &format!("{base}...HEAD")])
            .output()
            .await;
        if let Ok(output) = output {
            if output.status.success() {
                return String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .map(str::to_string)
                    .filter(|line| !line.is_empty())
                    .collect();
            }
        }
    }
    Vec::new()
}

/// Build the default review-swarm spec list from `git diff main...HEAD`.
///
/// Baseline (always when the persona file is present): 3 correctness-focus +
/// 3 architecture-focus reviewers across `reviewer-codex` / `reviewer-opus` /
/// `reviewer-gemini`. Plus conditional specialists — effect-reviewer,
/// logging-review, test-quality-review — added when the diff pattern-matches.
///
/// Personas that are not deployed (missing agent file) are silently skipped,
/// so a repo that only has, say, `reviewer-opus.md` still gets a 2-reviewer
/// round instead of erroring. The caller can always override by passing
/// `params.reviewers` explicitly.
async fn auto_select_reviewer_specs(
    worktree: &Path,
) -> Result<Vec<protocol::WorkflowReviewerSpec>, String> {
    const CORRECTNESS_PROMPT: &str =
        "Review ALL changes on this branch vs main: `git diff main...HEAD`. \
Focus on: correctness, edge cases, security, performance. \
For each issue: file:line, severity (Critical/High/Medium/Low), concrete fix. \
Conclude with an actionable items list ordered by severity. Only report violations.";

    const ARCHITECTURE_PROMPT: &str =
        "Review ALL changes on this branch vs main: `git diff main...HEAD`. \
Focus on: architecture, patterns, error handling, missing tests, silent fallbacks. \
CRITICAL — silent fallback scan: errors are a feature. Flag every instance of error suppression \
that masks failures instead of propagating them (|| true, 2>/dev/null, catch blocks returning \
defaults, _, _ = on operations that should fail loudly, ?? [] hiding malformed responses). \
For each flagged pattern, state whether it is legitimate cleanup/teardown or an erroneous mask. \
For each issue: file:line, severity (Critical/High/Medium/Low), concrete fix. \
Conclude with an actionable items list ordered by severity. Only report violations.";

    const EFFECT_PROMPT: &str = "Review ALL Effect.ts code changes on this branch vs main. \
Check for: reinvented library functions, non-idiomatic patterns, `_tag` checks that should use \
library type guards (Cause.isCause, Exit.isExit, Option.isSome), reimplemented transformers \
(Cause.pretty, Exit.match). Reference llms.txt / vendored Effect docs. \
For each issue: file:line, severity, concrete fix. Only report violations.";

    const LOGGING_PROMPT: &str = "Review logging changes on this branch vs main. \
Effect.logError must pass cause as 2nd arg. Services should prefer spans over ad-hoc logInfo/logDebug. \
Effect.logDebug must not be committed. \
Report only violations, as a table: | # | Severity | Issue | File:Line |";

    const TEST_QUALITY_PROMPT: &str = "Review test changes on this branch vs main. \
Check for: source-reading tests, tautological assertions, mock-as-subject, mock-server echo, \
over-mocked wiring, no-op assertions. \
Report only violations, as a table: | # | Severity | Pattern | Issue | File:Line |";

    let mut specs = Vec::new();

    for persona_name in ["reviewer-codex", "reviewer-opus", "reviewer-gemini"] {
        let Some(path) = resolve_persona_path(worktree, persona_name) else {
            continue;
        };
        let persona = parse_agent_persona_file(&path)?;
        specs.push(spec_from_persona(
            &persona,
            "Correctness",
            CORRECTNESS_PROMPT.to_string(),
        ));
        specs.push(spec_from_persona(
            &persona,
            "Architecture",
            ARCHITECTURE_PROMPT.to_string(),
        ));
    }

    let diff = branch_diff(worktree).await;
    let changed = branch_changed_files(worktree).await;

    let has_ts_effect = changed
        .iter()
        .any(|f| f.ends_with(".ts") || f.ends_with(".tsx"))
        && (diff.contains("from \"effect\"")
            || diff.contains("from 'effect'")
            || diff.contains("from '@effect/")
            || diff.contains("from \"@effect/"));

    // `Effect.log` already subsumes `Effect.logError` / `.logInfo` / `.logDebug`; keep it
    // as the single signal to avoid misleading redundancy in the condition list.
    let has_logging = diff.contains("Effect.log");

    // Match test-file paths by filename extension / directory segment, not bare substring —
    // `contains("test")` false-positives on `contested.ts`, `attestation.rs`, etc.;
    // `contains("spec")` trips on `inspector.ts`, `respect.ts`, `specify.ts`.
    let has_test_changes = changed.iter().any(|f| is_test_path(f));

    if has_ts_effect {
        if let Some(path) = resolve_persona_path(worktree, "effect-reviewer") {
            let persona = parse_agent_persona_file(&path)?;
            specs.push(spec_from_persona(
                &persona,
                "Effect.ts",
                EFFECT_PROMPT.to_string(),
            ));
        }
    }
    if has_logging {
        if let Some(path) = resolve_persona_path(worktree, "logging-review") {
            let persona = parse_agent_persona_file(&path)?;
            specs.push(spec_from_persona(
                &persona,
                "Logging",
                LOGGING_PROMPT.to_string(),
            ));
        }
    }
    if has_test_changes {
        if let Some(path) = resolve_persona_path(worktree, "test-quality-review") {
            let persona = parse_agent_persona_file(&path)?;
            specs.push(spec_from_persona(
                &persona,
                "Tests",
                TEST_QUALITY_PROMPT.to_string(),
            ));
        }
    }

    Ok(specs)
}

/// Inject a `<system-context>` message into the orchestrator session, if one is set.
///
/// Fire-and-forget. Used to keep the orchestrator informed when non-orchestrator actors
/// (UI override, CLI, another agent) mutate workflow state. No-op when the workflow has
/// no `orchestrator_session_id` or the session is unreachable.
async fn notify_orchestrator(
    state: &Arc<AppState>,
    workflow: &protocol::WorkflowState,
    body: &str,
) {
    let Some(session_id) = workflow.orchestrator_session_id.as_deref() else {
        return;
    };
    let context = format!(
        "[Workflow {} — phase: {:?}]\n{body}",
        workflow.workflow_id, workflow.phase
    );
    if let Err(err) = ChatService::inject_system_context(state.clone(), session_id, &context).await
    {
        warn!(
            workflow_id = %workflow.workflow_id,
            session_id = %session_id,
            error = %err,
            "failed to inject orchestrator context"
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkflowStoreData {
    #[serde(default)]
    workflows: Vec<PersistedWorkflow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedWorkflow {
    state: protocol::WorkflowState,
    #[serde(default = "default_next_index")]
    next_worker_index: u32,
    #[serde(default = "default_next_index")]
    next_reviewer_index: u32,
    #[serde(default = "default_next_index")]
    next_finding_index: u32,
    /// How many reviewer rows `start_review` intends to spawn for the current round.
    /// Used to gate `workflow.review_completed` — we must not emit until every spec
    /// has actually been inserted into `reviewers`, otherwise a fast first reviewer
    /// finishing before the second is spawned would trip `all_terminal` with `count=1`.
    /// Reset to 0 on review_completed.
    #[serde(default)]
    expected_reviewer_count: u32,
}

fn default_next_index() -> u32 {
    1
}

#[derive(Clone)]
struct ReviewStartCheckpoint {
    thread_id: String,
    previous_phase: protocol::WorkflowPhase,
    previous_completed_at: Option<String>,
    previous_review_started_at: Option<String>,
    previous_reviewer_count: usize,
    previous_next_reviewer_index: u32,
}

pub struct WorkflowStore {
    pub path: PathBuf,
    pub data: WorkflowStoreData,
}

impl WorkflowStore {
    pub fn load_from_state_store(state_path: &Path) -> Result<Self, String> {
        let parent = state_path
            .parent()
            .ok_or_else(|| format!("invalid state path {}", state_path.display()))?;
        Self::load_from_dir(parent)
    }

    pub fn load() -> Result<Self, String> {
        let config_dir =
            dirs::config_dir().ok_or_else(|| "unable to locate config dir".to_string())?;
        let dir = config_dir.join("threadmill");
        Self::load_from_dir(&dir)
    }

    pub fn load_from_dir(dir: &Path) -> Result<Self, String> {
        fs::create_dir_all(dir)
            .map_err(|err| format!("failed to create {}: {err}", dir.display()))?;

        let path = dir.join("workflows.json");
        if !path.exists() {
            let store = Self {
                path,
                data: WorkflowStoreData::default(),
            };
            store.save()?;
            return Ok(store);
        }

        let raw = fs::read_to_string(&path)
            .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        let data = if raw.trim().is_empty() {
            WorkflowStoreData::default()
        } else {
            serde_json::from_str(&raw)
                .map_err(|err| format!("failed to parse {}: {err}", path.display()))?
        };

        Ok(Self { path, data })
    }

    pub fn save(&self) -> Result<(), String> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| format!("invalid workflow state path {}", self.path.display()))?;
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;

        let serialized = serde_json::to_vec_pretty(&self.data)
            .map_err(|err| format!("failed to serialize workflow state: {err}"))?;

        let tmp_path = self.path.with_extension("json.tmp");
        fs::write(&tmp_path, serialized)
            .map_err(|err| format!("failed to write {}: {err}", tmp_path.display()))?;
        fs::rename(&tmp_path, &self.path).map_err(|err| {
            format!(
                "failed to move {} to {}: {err}",
                tmp_path.display(),
                self.path.display()
            )
        })?;

        Ok(())
    }
}

impl WorkflowService {
    pub fn start_event_listener(state: Arc<AppState>) {
        let mut events_rx = state.subscribe_events();
        tokio::spawn(async move {
            loop {
                match events_rx.recv().await {
                    Ok(event) => {
                        if let Err(err) = handle_server_event(Arc::clone(&state), event).await {
                            warn!(error = %err, "workflow event binding failed");
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!(skipped, "workflow event listener lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    pub async fn reconcile_startup(state: Arc<AppState>) -> Result<(), String> {
        let reconciled = {
            let mut store = state.workflows.lock().await;
            let mut reconciled = Vec::new();
            let now = Utc::now().to_rfc3339();

            for workflow in &mut store.data.workflows {
                let mut workflow_changed = false;
                let mut changed_workers = Vec::new();
                let mut changed_reviewers = Vec::new();

                for worker in &mut workflow.state.workers {
                    if worker_is_active(&worker.status) {
                        worker.status = protocol::WorkflowWorkerStatus::Failed;
                        worker.completed_at = Some(now.clone());
                        worker.failure_message =
                            Some("Spindle restarted before worker completed".to_string());
                        changed_workers.push(worker.clone());
                        workflow_changed = true;
                    }
                }

                for reviewer in &mut workflow.state.reviewers {
                    if worker_is_active(&reviewer.status) {
                        reviewer.status = protocol::WorkflowWorkerStatus::Failed;
                        reviewer.completed_at = Some(now.clone());
                        reviewer.failure_message =
                            Some("Spindle restarted before reviewer completed".to_string());
                        changed_reviewers.push(reviewer.clone());
                        workflow_changed = true;
                    }
                }

                let had_active_sessions =
                    !changed_workers.is_empty() || !changed_reviewers.is_empty();

                let mut phase_change = None;
                if had_active_sessions
                    && workflow_is_active_phase(&workflow.state.phase)
                    && workflow.state.phase != protocol::WorkflowPhase::Blocked
                {
                    let old_phase = workflow.state.phase.clone();
                    workflow.state.phase = protocol::WorkflowPhase::Blocked;
                    phase_change = Some((old_phase, protocol::WorkflowPhase::Blocked));
                    workflow_changed = true;
                }

                if workflow_changed {
                    touch_workflow(&mut workflow.state);
                    reconciled.push((
                        workflow.state.clone(),
                        phase_change,
                        changed_workers,
                        changed_reviewers,
                    ));
                }
            }

            if !reconciled.is_empty() {
                store.save()?;
            }
            reconciled
        };

        for (workflow, phase_change, workers, reviewers) in reconciled {
            if let Some((old_phase, new_phase)) = phase_change {
                state.emit_event(
                    "workflow.phase_changed",
                    protocol::WorkflowPhaseChangedEvent {
                        workflow_id: workflow.workflow_id.clone(),
                        thread_id: workflow.thread_id.clone(),
                        old_phase,
                        new_phase,
                        forced: true,
                    },
                );
            }
            for worker in workers {
                state.emit_event(
                    "workflow.worker_completed",
                    protocol::WorkflowWorkerCompletedEvent {
                        workflow_id: workflow.workflow_id.clone(),
                        thread_id: workflow.thread_id.clone(),
                        worker,
                    },
                );
            }
            for reviewer in reviewers {
                state.emit_event(
                    "workflow.reviewer_completed",
                    protocol::WorkflowReviewerCompletedEvent {
                        workflow_id: workflow.workflow_id.clone(),
                        thread_id: workflow.thread_id.clone(),
                        reviewer,
                    },
                );
            }
            emit_workflow_upsert(&state, workflow);
        }

        Ok(())
    }

    pub async fn snapshot_workflows(state: &Arc<AppState>) -> Vec<protocol::WorkflowState> {
        let store = state.workflows.lock().await;
        list_workflows_locked(&store, None)
    }

    pub async fn create(
        state: Arc<AppState>,
        params: protocol::WorkflowCreateParams,
    ) -> Result<protocol::WorkflowCreateResult, String> {
        let workflow = {
            let mut workflows = state.workflows.lock().await;
            ensure_thread_can_host_workflow(&state, &params.thread_id).await?;
            if workflows.data.workflows.iter().any(|workflow| {
                workflow.state.thread_id == params.thread_id
                    && workflow_is_active_phase(&workflow.state.phase)
            }) {
                return Err(format!(
                    "active workflow already exists for thread {}",
                    params.thread_id
                ));
            }

            let now = Utc::now().to_rfc3339();
            let workflow = protocol::WorkflowState {
                workflow_id: Uuid::new_v4().to_string(),
                thread_id: params.thread_id,
                phase: protocol::WorkflowPhase::Planning,
                created_at: now.clone(),
                updated_at: now,
                completed_at: None,
                prd_issue_url: params.prd_issue_url,
                implementation_issue_urls: params.implementation_issue_urls,
                orchestrator_session_id: params.orchestrator_session_id,
                workers: Vec::new(),
                reviewers: Vec::new(),
                findings: Vec::new(),
                review_started_at: None,
                current_review_round: 0,
            };
            workflows.data.workflows.push(PersistedWorkflow {
                state: workflow.clone(),
                next_worker_index: 1,
                next_reviewer_index: 1,
                next_finding_index: 1,
                expected_reviewer_count: 0,
            });
            workflows.save()?;
            workflow
        };

        state.emit_event(
            "workflow.created",
            protocol::WorkflowCreatedEvent {
                workflow: workflow.clone(),
            },
        );
        emit_workflow_upsert(&state, workflow.clone());

        Ok(protocol::WorkflowCreateResult {
            workflow_id: workflow.workflow_id,
        })
    }

    pub async fn status(
        state: Arc<AppState>,
        params: protocol::WorkflowStatusParams,
    ) -> Result<protocol::WorkflowStatusResult, String> {
        let store = state.workflows.lock().await;
        find_workflow_locked(&store, &params.workflow_id)
            .cloned()
            .ok_or_else(|| format!("workflow not found: {}", params.workflow_id))
    }

    pub async fn list(
        state: Arc<AppState>,
        params: protocol::WorkflowListParams,
    ) -> Result<protocol::WorkflowListResult, String> {
        let store = state.workflows.lock().await;
        Ok(list_workflows_locked(&store, params.thread_id.as_deref()))
    }

    pub async fn transition(
        state: Arc<AppState>,
        params: protocol::WorkflowTransitionParams,
    ) -> Result<protocol::WorkflowTransitionResult, String> {
        let (workflow, old_phase, new_phase, forced) = {
            let mut store = state.workflows.lock().await;
            let workflow = find_workflow_locked_mut(&mut store, &params.workflow_id)
                .ok_or_else(|| format!("workflow not found: {}", params.workflow_id))?;
            let old_phase = workflow.state.phase.clone();
            if old_phase == params.phase {
                return Ok(workflow.state.clone());
            }

            validate_phase_transition(&old_phase, &params.phase, params.force)?;
            if params.force {
                info!(
                    workflow_id = %workflow.state.workflow_id,
                    from = ?old_phase,
                    to = ?params.phase,
                    "workflow phase transition force-skipped"
                );
            }

            workflow.state.phase = params.phase.clone();
            if workflow.state.phase == protocol::WorkflowPhase::Complete {
                workflow.state.completed_at = Some(Utc::now().to_rfc3339());
            }
            touch_workflow(&mut workflow.state);
            let workflow = workflow.state.clone();
            store.save()?;
            (workflow, old_phase, params.phase, params.force)
        };

        state.emit_event(
            "workflow.phase_changed",
            protocol::WorkflowPhaseChangedEvent {
                workflow_id: workflow.workflow_id.clone(),
                thread_id: workflow.thread_id.clone(),
                old_phase: old_phase.clone(),
                new_phase: new_phase.clone(),
                forced,
            },
        );
        if workflow.phase == protocol::WorkflowPhase::Complete {
            state.emit_event(
                "workflow.completed",
                protocol::WorkflowCompletedEvent {
                    workflow: workflow.clone(),
                },
            );
        }
        emit_workflow_upsert(&state, workflow.clone());

        if forced {
            let body =
                format!("User force-transitioned workflow from {old_phase:?} to {new_phase:?}.");
            notify_orchestrator(&state, &workflow, &body).await;
        }
        Ok(workflow)
    }

    pub async fn spawn_worker(
        state: Arc<AppState>,
        params: protocol::WorkflowSpawnWorkerParams,
    ) -> Result<protocol::WorkflowSpawnWorkerResult, String> {
        let (workflow_id, thread_id, worker_id) = {
            let mut store = state.workflows.lock().await;
            let tuple = {
                let workflow = find_workflow_locked_mut(&mut store, &params.workflow_id)
                    .ok_or_else(|| format!("workflow not found: {}", params.workflow_id))?;
                ensure_spawn_allowed(&workflow.state.phase)?;
                let worker_id = format!("W{:03}", workflow.next_worker_index);
                workflow.next_worker_index += 1;
                workflow.state.workers.push(protocol::WorkflowWorker {
                    worker_id: worker_id.clone(),
                    agent_name: params.agent_name.clone(),
                    status: protocol::WorkflowWorkerStatus::Planned,
                    session_id: None,
                    parent_session_id: params.parent_session_id.clone(),
                    display_name: params.display_name.clone(),
                    created_at: Utc::now().to_rfc3339(),
                    started_at: None,
                    completed_at: None,
                    stop_reason: None,
                    handoff: None,
                    failure_message: None,
                    agent_status: None,
                    issue_url: params.issue_url.clone(),
                });
                touch_workflow(&mut workflow.state);
                (
                    workflow.state.workflow_id.clone(),
                    workflow.state.thread_id.clone(),
                    worker_id,
                )
            };
            store.save()?;
            tuple
        };

        // Best-effort issue context injection: if the caller bound this worker
        // to an issue, resolve its body via IssueTransport and prepend to
        // initial_prompt so the agent boots with full context. Resolve failures
        // log a warning and fall through with the caller's original prompt.
        let initial_prompt = match params.issue_url.as_ref() {
            Some(url) => {
                let worktree = {
                    let store = state.store.lock().await;
                    store
                        .thread_by_id(&thread_id)
                        .map(|thread| thread.worktree_path.clone())
                };
                match worktree {
                    Some(worktree) => {
                        let transport = crate::services::issues::for_project(&worktree, None).await;
                        match timeout(ISSUE_RESOLVE_TIMEOUT, transport.resolve(url)).await {
                            Ok(Ok(Some(issue))) => {
                                prepend_issue_context(params.initial_prompt.clone(), url, &issue)
                            }
                            Ok(Ok(None)) => {
                                warn!("spawn_worker: issue not found for injection: {url}");
                                params.initial_prompt.clone()
                            }
                            Ok(Err(err)) => {
                                warn!("spawn_worker: issue resolve failed for {url}: {err}");
                                params.initial_prompt.clone()
                            }
                            Err(_elapsed) => {
                                warn!("spawn_worker: issue resolve timed out for {url}");
                                params.initial_prompt.clone()
                            }
                        }
                    }
                    None => {
                        warn!("spawn_worker: thread {thread_id} not found for issue injection");
                        params.initial_prompt.clone()
                    }
                }
            }
            None => params.initial_prompt.clone(),
        };

        let chat_result = ChatService::start(
            Arc::clone(&state),
            protocol::ChatStartParams {
                thread_id: thread_id.clone(),
                agent_name: params.agent_name,
                system_prompt: params.system_prompt,
                initial_prompt,
                display_name: params.display_name,
                parent_session_id: params.parent_session_id,
                preferred_model: params.preferred_model,
            },
        )
        .await;

        match chat_result {
            Ok(chat_result) => {
                {
                    let mut store = state.workflows.lock().await;
                    let workflow = find_workflow_locked_mut(&mut store, &workflow_id)
                        .ok_or_else(|| format!("workflow not found: {workflow_id}"))?;
                    let worker_index = workflow
                        .state
                        .workers
                        .iter()
                        .position(|worker| worker.worker_id == worker_id)
                        .ok_or_else(|| format!("worker not found: {worker_id}"))?;
                    {
                        let worker = &mut workflow.state.workers[worker_index];
                        worker.status = protocol::WorkflowWorkerStatus::Spawning;
                        worker.session_id = Some(chat_result.session_id.clone());
                        worker.started_at = Some(Utc::now().to_rfc3339());
                    }
                    touch_workflow(&mut workflow.state);
                    store.save()?;
                }

                let chat_status = ChatService::status(
                    Arc::clone(&state),
                    protocol::ChatStatusParams {
                        session_id: chat_result.session_id.clone(),
                    },
                )
                .await?;
                reconcile_bound_session_status(
                    Arc::clone(&state),
                    &chat_result.session_id,
                    &chat_status,
                )
                .await?;

                let worker = {
                    let store = state.workflows.lock().await;
                    let workflow = find_workflow_locked(&store, &workflow_id)
                        .ok_or_else(|| format!("workflow not found: {workflow_id}"))?;
                    workflow
                        .workers
                        .iter()
                        .find(|worker| worker.worker_id == worker_id)
                        .cloned()
                        .ok_or_else(|| format!("worker not found: {worker_id}"))?
                };

                state.emit_event(
                    "workflow.worker_spawned",
                    protocol::WorkflowWorkerSpawnedEvent {
                        workflow_id: workflow_id.clone(),
                        thread_id: thread_id.clone(),
                        worker: worker.clone(),
                    },
                );
                emit_workflow_state_delta(&state, &workflow_id).await;

                let snapshot = {
                    let store = state.workflows.lock().await;
                    find_workflow_locked(&store, &workflow_id).cloned()
                };
                if let Some(snapshot) = snapshot {
                    let body = format!(
                        "Worker {} ({}) spawned with session {}.",
                        worker.worker_id,
                        worker.agent_name,
                        worker.session_id.clone().unwrap_or_else(|| "?".to_string())
                    );
                    notify_orchestrator(&state, &snapshot, &body).await;
                }

                Ok(protocol::WorkflowSpawnWorkerResult {
                    workflow_id,
                    worker,
                })
            }
            Err(error) => {
                let failed_worker =
                    mark_worker_failed(Arc::clone(&state), &workflow_id, &worker_id, &error)
                        .await?;
                state.emit_event(
                    "workflow.worker_completed",
                    protocol::WorkflowWorkerCompletedEvent {
                        workflow_id: workflow_id.clone(),
                        thread_id,
                        worker: failed_worker,
                    },
                );
                emit_workflow_state_delta(&state, &workflow_id).await;
                Err(error)
            }
        }
    }

    pub async fn record_handoff(
        state: Arc<AppState>,
        params: protocol::WorkflowRecordHandoffParams,
    ) -> Result<protocol::WorkflowRecordHandoffResult, String> {
        let (thread_id, worker, session_id) = {
            let mut store = state.workflows.lock().await;
            let workflow = find_workflow_locked_mut(&mut store, &params.workflow_id)
                .ok_or_else(|| format!("workflow not found: {}", params.workflow_id))?;
            let worker_index = workflow
                .state
                .workers
                .iter()
                .position(|worker| worker.worker_id == params.worker_id)
                .ok_or_else(|| format!("worker not found: {}", params.worker_id))?;
            if worker_is_terminal(&workflow.state.workers[worker_index].status) {
                return Err(format!("worker already completed: {}", params.worker_id));
            }

            let now = Utc::now().to_rfc3339();
            {
                let worker = &mut workflow.state.workers[worker_index];
                worker.completed_at = Some(now.clone());
                worker.stop_reason = Some(params.stop_reason.clone());
                worker.handoff = Some(protocol::WorkflowHandoff {
                    stop_reason: params.stop_reason.clone(),
                    progress: params.progress,
                    next_steps: params.next_steps,
                    context: params.context,
                    blockers: params.blockers,
                    recorded_at: now,
                });
                worker.status = if params.stop_reason == protocol::StopReason::Done {
                    protocol::WorkflowWorkerStatus::Completed
                } else {
                    protocol::WorkflowWorkerStatus::Failed
                };
            }
            touch_workflow(&mut workflow.state);
            let worker = workflow.state.workers[worker_index].clone();
            let session_id = worker.session_id.clone();
            let thread_id = workflow.state.thread_id.clone();
            store.save()?;
            (thread_id, worker, session_id)
        };

        state.emit_event(
            "workflow.worker_completed",
            protocol::WorkflowWorkerCompletedEvent {
                workflow_id: params.workflow_id.clone(),
                thread_id: thread_id.clone(),
                worker: worker.clone(),
            },
        );
        emit_workflow_state_delta(&state, &params.workflow_id).await;

        if let Some(session_id) = session_id {
            let _ = ChatService::stop(
                Arc::clone(&state),
                protocol::ChatStopParams {
                    thread_id,
                    session_id,
                },
            )
            .await;
        }

        Ok(worker)
    }

    pub async fn start_review(
        state: Arc<AppState>,
        params: protocol::WorkflowStartReviewParams,
    ) -> Result<protocol::WorkflowStartReviewResult, String> {
        let checkpoint = snapshot_review_start_checkpoint(&state, &params.workflow_id).await?;
        Self::transition(
            Arc::clone(&state),
            protocol::WorkflowTransitionParams {
                workflow_id: params.workflow_id.clone(),
                phase: protocol::WorkflowPhase::Reviewing,
                force: params.force,
            },
        )
        .await?;

        // Bump the round counter so findings recorded during this round can be
        // distinguished from future post-fix re-review findings.
        {
            let mut store = state.workflows.lock().await;
            if let Some(workflow) = find_workflow_locked_mut(&mut store, &params.workflow_id) {
                workflow.state.current_review_round =
                    workflow.state.current_review_round.saturating_add(1);
                touch_workflow(&mut workflow.state);
                store.save()?;
            }
        }

        // thread's worktree. Empty specs = "Spindle, pick the review swarm".
        let reviewer_specs = if params.reviewers.is_empty() {
            let thread_id = {
                let store = state.workflows.lock().await;
                find_workflow_locked(&store, &params.workflow_id)
                    .ok_or_else(|| format!("workflow not found: {}", params.workflow_id))?
                    .thread_id
                    .clone()
            };
            let worktree = {
                let store = state.store.lock().await;
                store
                    .thread_by_id(&thread_id)
                    .ok_or_else(|| format!("thread not found: {thread_id}"))?
                    .worktree_path
                    .clone()
            };
            auto_select_reviewer_specs(std::path::Path::new(&worktree)).await?
        } else {
            params.reviewers
        };

        // Record how many reviewer rows this round will produce. `set_session_failed`
        // gates `workflow.review_completed` on `reviewers.len() >= expected_reviewer_count`
        // so a fast-finishing first reviewer can't fire the end-of-round event before
        // the remaining specs are even inserted.
        {
            let mut store = state.workflows.lock().await;
            if let Some(workflow) = find_workflow_locked_mut(&mut store, &params.workflow_id) {
                workflow.expected_reviewer_count =
                    u32::try_from(reviewer_specs.len()).unwrap_or(u32::MAX);
                store.save()?;
            }
        }

        let mut reviewers = Vec::with_capacity(reviewer_specs.len());
        for reviewer in reviewer_specs {
            let spawned = Self::spawn_reviewer(
                Arc::clone(&state),
                protocol::WorkflowSpawnReviewerParams {
                    workflow_id: params.workflow_id.clone(),
                    agent_name: reviewer.agent_name,
                    parent_session_id: reviewer.parent_session_id,
                    system_prompt: reviewer.system_prompt,
                    initial_prompt: reviewer.initial_prompt,
                    display_name: reviewer.display_name,
                    preferred_model: reviewer.preferred_model,
                },
            )
            .await;
            let spawned = match spawned {
                Ok(spawned) => spawned,
                Err(error) => {
                    rollback_review_start(Arc::clone(&state), &params.workflow_id, &checkpoint)
                        .await
                        .map_err(|rollback_error| {
                            format!("{error}; rollback failed: {rollback_error}")
                        })?;
                    return Err(error);
                }
            };
            reviewers.push(spawned.reviewer);
        }

        let workflow = Self::status(
            Arc::clone(&state),
            protocol::WorkflowStatusParams {
                workflow_id: params.workflow_id,
            },
        )
        .await?;
        state.emit_event(
            "workflow.review_started",
            protocol::WorkflowReviewStartedEvent {
                workflow_id: workflow.workflow_id.clone(),
                thread_id: workflow.thread_id.clone(),
                reviewers: reviewers.clone(),
            },
        );
        emit_workflow_upsert(&state, workflow.clone());
        Ok(protocol::WorkflowStartReviewResult {
            workflow,
            reviewers,
        })
    }

    pub async fn spawn_reviewer(
        state: Arc<AppState>,
        params: protocol::WorkflowSpawnReviewerParams,
    ) -> Result<protocol::WorkflowSpawnReviewerResult, String> {
        let (workflow_id, thread_id, reviewer_id) = {
            let mut store = state.workflows.lock().await;
            let tuple = {
                let workflow = find_workflow_locked_mut(&mut store, &params.workflow_id)
                    .ok_or_else(|| format!("workflow not found: {}", params.workflow_id))?;
                if workflow.state.phase != protocol::WorkflowPhase::Reviewing {
                    return Err(format!(
                        "invalid workflow phase transition: reviewers require REVIEWING, got {:?}",
                        workflow.state.phase
                    ));
                }
                let reviewer_id = format!("R{:03}", workflow.next_reviewer_index);
                workflow.next_reviewer_index += 1;
                workflow
                    .state
                    .review_started_at
                    .get_or_insert_with(|| Utc::now().to_rfc3339());
                workflow.state.reviewers.push(protocol::WorkflowReviewer {
                    reviewer_id: reviewer_id.clone(),
                    agent_name: params.agent_name.clone(),
                    status: protocol::WorkflowWorkerStatus::Planned,
                    session_id: None,
                    parent_session_id: params.parent_session_id.clone(),
                    display_name: params.display_name.clone(),
                    created_at: Utc::now().to_rfc3339(),
                    started_at: None,
                    completed_at: None,
                    failure_message: None,
                    agent_status: None,
                });
                touch_workflow(&mut workflow.state);
                (
                    workflow.state.workflow_id.clone(),
                    workflow.state.thread_id.clone(),
                    reviewer_id,
                )
            };
            store.save()?;
            tuple
        };

        let chat_result = ChatService::start(
            Arc::clone(&state),
            protocol::ChatStartParams {
                thread_id: thread_id.clone(),
                agent_name: params.agent_name,
                system_prompt: params.system_prompt,
                initial_prompt: params.initial_prompt,
                display_name: params.display_name,
                parent_session_id: params.parent_session_id,
                preferred_model: params.preferred_model,
            },
        )
        .await;

        match chat_result {
            Ok(chat_result) => {
                {
                    let mut store = state.workflows.lock().await;
                    let workflow = find_workflow_locked_mut(&mut store, &workflow_id)
                        .ok_or_else(|| format!("workflow not found: {workflow_id}"))?;
                    let reviewer_index = workflow
                        .state
                        .reviewers
                        .iter()
                        .position(|reviewer| reviewer.reviewer_id == reviewer_id)
                        .ok_or_else(|| format!("reviewer not found: {reviewer_id}"))?;
                    {
                        let reviewer = &mut workflow.state.reviewers[reviewer_index];
                        reviewer.status = protocol::WorkflowWorkerStatus::Spawning;
                        reviewer.session_id = Some(chat_result.session_id.clone());
                        reviewer.started_at = Some(Utc::now().to_rfc3339());
                    }
                    touch_workflow(&mut workflow.state);
                    store.save()?;
                }
                let chat_status = ChatService::status(
                    Arc::clone(&state),
                    protocol::ChatStatusParams {
                        session_id: chat_result.session_id.clone(),
                    },
                )
                .await?;
                reconcile_bound_session_status(
                    Arc::clone(&state),
                    &chat_result.session_id,
                    &chat_status,
                )
                .await?;

                let (reviewer, workflow_snapshot) = {
                    let store = state.workflows.lock().await;
                    let workflow = find_workflow_locked(&store, &workflow_id)
                        .ok_or_else(|| format!("workflow not found: {workflow_id}"))?;
                    let reviewer = workflow
                        .reviewers
                        .iter()
                        .find(|reviewer| reviewer.reviewer_id == reviewer_id)
                        .cloned()
                        .ok_or_else(|| format!("reviewer not found: {reviewer_id}"))?;
                    (reviewer, workflow.clone())
                };
                emit_workflow_state_delta(&state, &workflow_id).await;

                let body = format!(
                    "Reviewer {} ({}) spawned with session {}.",
                    reviewer.reviewer_id,
                    reviewer.agent_name,
                    reviewer
                        .session_id
                        .clone()
                        .unwrap_or_else(|| "?".to_string())
                );
                notify_orchestrator(&state, &workflow_snapshot, &body).await;

                Ok(protocol::WorkflowSpawnReviewerResult {
                    workflow_id,
                    reviewer,
                })
            }
            Err(error) => {
                let failed_reviewer =
                    mark_reviewer_failed(Arc::clone(&state), &workflow_id, &reviewer_id, &error)
                        .await?;
                state.emit_event(
                    "workflow.reviewer_completed",
                    protocol::WorkflowReviewerCompletedEvent {
                        workflow_id: workflow_id.clone(),
                        thread_id,
                        reviewer: failed_reviewer,
                    },
                );
                emit_workflow_state_delta(&state, &workflow_id).await;
                Err(error)
            }
        }
    }

    pub async fn list_reviewers(
        state: Arc<AppState>,
        params: protocol::WorkflowListReviewersParams,
    ) -> Result<protocol::WorkflowListReviewersResult, String> {
        let store = state.workflows.lock().await;
        let workflow = find_workflow_locked(&store, &params.workflow_id)
            .ok_or_else(|| format!("workflow not found: {}", params.workflow_id))?;
        Ok(workflow.reviewers.clone())
    }

    pub async fn record_findings(
        state: Arc<AppState>,
        params: protocol::WorkflowRecordFindingsParams,
    ) -> Result<protocol::WorkflowRecordFindingsResult, String> {
        let (workflow, findings) = {
            let mut store = state.workflows.lock().await;
            let workflow = find_workflow_locked_mut(&mut store, &params.workflow_id)
                .ok_or_else(|| format!("workflow not found: {}", params.workflow_id))?;

            for finding in &params.findings {
                for reviewer_id in &finding.source_reviewers {
                    if !workflow
                        .state
                        .reviewers
                        .iter()
                        .any(|reviewer| reviewer.reviewer_id == *reviewer_id)
                    {
                        return Err(format!("reviewer not found: {reviewer_id}"));
                    }
                }
            }

            let now = Utc::now().to_rfc3339();
            let round = workflow.state.current_review_round.max(1);
            let mut findings = Vec::with_capacity(params.findings.len());
            for finding in params.findings {
                let finding = protocol::WorkflowFinding {
                    finding_id: format!("H{}", workflow.next_finding_index),
                    severity: finding.severity,
                    summary: finding.summary,
                    details: finding.details,
                    source_reviewers: finding.source_reviewers,
                    file_path: finding.file_path,
                    line: finding.line,
                    created_at: now.clone(),
                    resolved: false,
                    review_round: round,
                };
                workflow.next_finding_index += 1;
                workflow.state.findings.push(finding.clone());
                findings.push(finding);
            }
            touch_workflow(&mut workflow.state);
            let workflow_state = workflow.state.clone();
            store.save()?;
            (workflow_state, findings)
        };

        state.emit_event(
            "workflow.findings_recorded",
            protocol::WorkflowFindingsRecordedEvent {
                workflow_id: workflow.workflow_id.clone(),
                thread_id: workflow.thread_id.clone(),
                findings: findings.clone(),
            },
        );
        emit_workflow_upsert(&state, workflow);
        Ok(findings)
    }

    pub async fn resolve_finding(
        state: Arc<AppState>,
        params: protocol::WorkflowResolveFindingParams,
    ) -> Result<protocol::WorkflowResolveFindingResult, String> {
        let (workflow, finding) = {
            let mut store = state.workflows.lock().await;
            let workflow = find_workflow_locked_mut(&mut store, &params.workflow_id)
                .ok_or_else(|| format!("workflow not found: {}", params.workflow_id))?;
            let index = workflow
                .state
                .findings
                .iter()
                .position(|finding| finding.finding_id == params.finding_id)
                .ok_or_else(|| format!("finding not found: {}", params.finding_id))?;
            workflow.state.findings[index].resolved = params.resolved;
            touch_workflow(&mut workflow.state);
            let finding = workflow.state.findings[index].clone();
            let workflow_state = workflow.state.clone();
            store.save()?;
            (workflow_state, finding)
        };

        emit_workflow_upsert(&state, workflow.clone());
        Ok(protocol::WorkflowResolveFindingResult {
            workflow_id: workflow.workflow_id,
            finding,
        })
    }

    pub async fn complete(
        state: Arc<AppState>,
        params: protocol::WorkflowCompleteParams,
    ) -> Result<protocol::WorkflowCompleteResult, String> {
        Self::transition(
            state,
            protocol::WorkflowTransitionParams {
                workflow_id: params.workflow_id,
                phase: protocol::WorkflowPhase::Complete,
                force: params.force,
            },
        )
        .await
    }

    /// `issue.list` — unified issue listing across scopes (All / Prds / LinkedTo)
    /// and an optional label filter. For `scope=All` / `scope=Prds`, cached for
    /// 30s keyed on `project_id|label|limit`; `scope=LinkedTo` is never cached
    /// because the linked-URL set can change at any time. The `state` filter is
    /// only honored for `scope=LinkedTo` today (transport `list` returns open
    /// issues only; post-filtering unscoped lists would require a resolve per
    /// row which is too expensive).
    pub async fn issue_list(
        state: Arc<AppState>,
        params: protocol::IssueListParams,
    ) -> Result<protocol::IssueListResult, String> {
        let scope = params.scope.unwrap_or_default();
        let limit = params.limit.unwrap_or(50).min(100);

        // Resolve project path.
        let project_path = {
            let store = state.store.lock().await;
            let project = store
                .project_by_id(&params.project_id)
                .ok_or_else(|| format!("project not found: {}", params.project_id))?;
            project.path.clone()
        };

        match scope {
            protocol::IssueListScope::LinkedTo { workflow_id } => {
                let (prd_url, impl_urls) = {
                    let store = state.workflows.lock().await;
                    let workflow = find_workflow_locked(&store, &workflow_id)
                        .ok_or_else(|| format!("workflow not found: {workflow_id}"))?;
                    (
                        workflow.prd_issue_url.clone(),
                        workflow.implementation_issue_urls.clone(),
                    )
                };

                let mut seen = std::collections::HashSet::<String>::new();
                let mut ordered: Vec<String> = Vec::new();
                if let Some(url) = prd_url {
                    if seen.insert(url.clone()) {
                        ordered.push(url);
                    }
                }
                for url in impl_urls {
                    if seen.insert(url.clone()) {
                        ordered.push(url);
                    }
                }

                let transport = crate::services::issues::for_project(&project_path, None).await;
                let platform = transport.kind();
                let state_filter = params.state.unwrap_or_default();
                let mut issues = Vec::new();
                for url in ordered {
                    match transport.resolve(&url).await {
                        Ok(Some(enriched)) => {
                            let keep = match state_filter {
                                protocol::IssueListState::All => true,
                                protocol::IssueListState::Open => {
                                    enriched.state == protocol::IssueState::Open
                                }
                                protocol::IssueListState::Closed => {
                                    enriched.state == protocol::IssueState::Closed
                                }
                            };
                            if keep {
                                issues.push(enriched.r#ref);
                            }
                        }
                        Ok(None) | Err(_) => {
                            // Transport failed or issue vanished. Skip — linked-to
                            // lists favor showing what we have over failing wholesale.
                        }
                    }
                    if issues.len() as u32 >= limit {
                        break;
                    }
                }
                Ok(protocol::IssueListResult {
                    platform,
                    issues,
                    cached: false,
                })
            }
            protocol::IssueListScope::All | protocol::IssueListScope::Prds => {
                let label = match scope {
                    protocol::IssueListScope::Prds => "prd".to_string(),
                    _ => params.label.unwrap_or_default(),
                };
                // argv is passed via execve so there's no shell-injection path,
                // but a label beginning with `-` can still be interpreted as a
                // flag by gh/glab. Reject up-front with a clear error.
                if label.starts_with('-') {
                    return Err(format!("invalid label `{label}` — must not start with `-`"));
                }

                let cache_key = format!("{}|{}|{}", params.project_id, label, limit);
                if !params.bypass_cache {
                    if let Some(cached) = issue_cache_get(&cache_key).await {
                        return Ok(protocol::IssueListResult {
                            platform: cached.platform,
                            issues: cached.issues,
                            cached: true,
                        });
                    }
                }

                let transport = crate::services::issues::for_project(&project_path, None).await;
                let platform = transport.kind();
                let issues = transport.list(&label, limit).await?;
                issue_cache_put(cache_key, platform.clone(), issues.clone()).await;
                Ok(protocol::IssueListResult {
                    platform,
                    issues,
                    cached: false,
                })
            }
        }
    }

    /// `issue.resolve` — fetch the live enriched view of a single issue. Returns
    /// `None` when the transport can't locate it (404, invalid URL, …).
    pub async fn issue_resolve(
        state: Arc<AppState>,
        params: protocol::IssueResolveParams,
    ) -> Result<protocol::IssueResolveResult, String> {
        let project_path = {
            let store = state.store.lock().await;
            let project = store
                .project_by_id(&params.project_id)
                .ok_or_else(|| format!("project not found: {}", params.project_id))?;
            project.path.clone()
        };
        let transport = crate::services::issues::for_project(&project_path, None).await;
        let enriched = transport.resolve(&params.url).await?;
        Ok(protocol::IssueResolveResult {
            issue: enriched.map(enriched_to_wire),
        })
    }

    /// `issue.close` — close an issue via the transport. Emits a
    /// `workflow.issue_closed` direct event on success; attributes the action to
    /// any active workflow whose PRD or implementation list contains the URL.
    pub async fn issue_close(
        state: Arc<AppState>,
        params: protocol::IssueCloseParams,
    ) -> Result<protocol::IssueCloseResult, String> {
        let project_path = {
            let store = state.store.lock().await;
            let project = store
                .project_by_id(&params.project_id)
                .ok_or_else(|| format!("project not found: {}", params.project_id))?;
            project.path.clone()
        };
        let transport = crate::services::issues::for_project(&project_path, None).await;
        transport
            .close(&params.url, params.reason.as_deref())
            .await?;

        let workflow_id = find_workflow_id_for_issue_url(&state, &params.url).await;
        state.emit_event(
            "workflow.issue_closed",
            protocol::WorkflowIssueClosedEvent {
                project_id: params.project_id.clone(),
                url: params.url.clone(),
                reason: params.reason.clone(),
                by_worker_id: params.by_worker_id.clone(),
                workflow_id,
            },
        );
        Ok(protocol::IssueCloseResult { success: true })
    }

    /// `issue.comment` — post a comment on an issue. Emits a
    /// `workflow.issue_commented` direct event on success. The event payload
    /// intentionally omits `body` to keep the broadcast small — consumers who
    /// need the comment text should resolve the issue.
    pub async fn issue_comment(
        state: Arc<AppState>,
        params: protocol::IssueCommentParams,
    ) -> Result<protocol::IssueCommentResult, String> {
        let project_path = {
            let store = state.store.lock().await;
            let project = store
                .project_by_id(&params.project_id)
                .ok_or_else(|| format!("project not found: {}", params.project_id))?;
            project.path.clone()
        };
        let transport = crate::services::issues::for_project(&project_path, None).await;
        transport.comment(&params.url, &params.body).await?;

        let workflow_id = find_workflow_id_for_issue_url(&state, &params.url).await;
        state.emit_event(
            "workflow.issue_commented",
            protocol::WorkflowIssueCommentedEvent {
                project_id: params.project_id.clone(),
                url: params.url.clone(),
                by_worker_id: params.by_worker_id.clone(),
                workflow_id,
            },
        );
        Ok(protocol::IssueCommentResult { success: true })
    }

    /// `issue.create` — create a new issue via the transport. If
    /// `link_to_workflow_id` is set and the workflow exists, the new URL is
    /// appended to its `implementation_issue_urls` (with persistence + a
    /// `WorkflowUpsert` state delta). Also emits a `workflow.issue_created`
    /// direct event with the new ref.
    pub async fn issue_create(
        state: Arc<AppState>,
        params: protocol::IssueCreateParams,
    ) -> Result<protocol::IssueCreateResult, String> {
        let project_path = {
            let store = state.store.lock().await;
            let project = store
                .project_by_id(&params.project_id)
                .ok_or_else(|| format!("project not found: {}", params.project_id))?;
            project.path.clone()
        };
        let transport = crate::services::issues::for_project(&project_path, None).await;
        let draft = crate::services::issues::IssueDraft {
            title: params.draft.title,
            body: params.draft.body,
            labels: params.draft.labels,
            assignees: params.draft.assignees,
        };
        let issue_ref = transport.create(draft).await?;

        // Best-effort link. If the workflow has vanished we still return the
        // created ref — the issue exists, silently dropping the link would be
        // worse than returning the ref with `linked_workflow_id=None`.
        let mut linked_workflow_id = None;
        if let Some(workflow_id) = params.link_to_workflow_id.clone() {
            let updated = {
                let mut store = state.workflows.lock().await;
                if let Some(workflow) = find_workflow_locked_mut(&mut store, &workflow_id) {
                    if !workflow
                        .state
                        .implementation_issue_urls
                        .contains(&issue_ref.url)
                    {
                        workflow
                            .state
                            .implementation_issue_urls
                            .push(issue_ref.url.clone());
                        touch_workflow(&mut workflow.state);
                    }
                    let cloned = workflow.state.clone();
                    store.save()?;
                    Some(cloned)
                } else {
                    None
                }
            };
            if let Some(workflow_state) = updated {
                linked_workflow_id = Some(workflow_id);
                emit_workflow_upsert(&state, workflow_state);
            }
        }

        state.emit_event(
            "workflow.issue_created",
            protocol::WorkflowIssueCreatedEvent {
                project_id: params.project_id.clone(),
                issue_ref: issue_ref.clone(),
                linked_workflow_id: linked_workflow_id.clone(),
            },
        );
        Ok(protocol::IssueCreateResult {
            issue_ref,
            linked_workflow_id,
        })
    }

    /// `workflow.add_linked_issue` — append an implementation-issue URL to a
    /// workflow. Idempotent (no-op if already linked). State change is
    /// broadcast via the standard `WorkflowUpsert` delta; no side-event.
    pub async fn workflow_add_linked_issue(
        state: Arc<AppState>,
        params: protocol::WorkflowAddLinkedIssueParams,
    ) -> Result<protocol::WorkflowAddLinkedIssueResult, String> {
        if params.url.trim().is_empty() {
            return Err("invalid issue url: must be non-empty".to_string());
        }
        let workflow_state = {
            let mut store = state.workflows.lock().await;
            let workflow = find_workflow_locked_mut(&mut store, &params.workflow_id)
                .ok_or_else(|| format!("workflow not found: {}", params.workflow_id))?;
            if !workflow
                .state
                .implementation_issue_urls
                .contains(&params.url)
            {
                workflow
                    .state
                    .implementation_issue_urls
                    .push(params.url.clone());
                touch_workflow(&mut workflow.state);
            }
            let cloned = workflow.state.clone();
            store.save()?;
            cloned
        };

        emit_workflow_upsert(&state, workflow_state.clone());
        Ok(protocol::WorkflowAddLinkedIssueResult {
            workflow: workflow_state,
        })
    }

    /// `workflow.resolve_linked_issues` — batch-resolve a workflow's PRD + all
    /// implementation issues. Deduplicates transport calls when the same URL
    /// appears in multiple slots. Individual resolve failures return `None`
    /// for that entry rather than failing the whole RPC.
    pub async fn workflow_resolve_linked_issues(
        state: Arc<AppState>,
        params: protocol::WorkflowResolveLinkedIssuesParams,
    ) -> Result<protocol::WorkflowResolveLinkedIssuesResult, String> {
        let (thread_id, prd_url, impl_urls) = {
            let store = state.workflows.lock().await;
            let workflow = find_workflow_locked(&store, &params.workflow_id)
                .ok_or_else(|| format!("workflow not found: {}", params.workflow_id))?;
            (
                workflow.thread_id.clone(),
                workflow.prd_issue_url.clone(),
                workflow.implementation_issue_urls.clone(),
            )
        };

        let project_path = {
            let store = state.store.lock().await;
            let thread = store
                .thread_by_id(&thread_id)
                .ok_or_else(|| format!("thread not found: {thread_id}"))?;
            thread.worktree_path.clone()
        };
        let transport = crate::services::issues::for_project(&project_path, None).await;

        // Intra-call dedupe — build a HashMap<url, resolved> so a URL that
        // appears as both PRD and implementation only hits the transport once.
        let mut unique_urls: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::<String>::new();
        if let Some(ref url) = prd_url {
            if seen.insert(url.clone()) {
                unique_urls.push(url.clone());
            }
        }
        for url in &impl_urls {
            if seen.insert(url.clone()) {
                unique_urls.push(url.clone());
            }
        }

        let mut resolved =
            std::collections::HashMap::<String, Option<protocol::EnrichedIssueWire>>::new();
        for url in &unique_urls {
            let wire = transport
                .resolve(url)
                .await
                .ok()
                .flatten()
                .map(enriched_to_wire);
            resolved.insert(url.clone(), wire);
        }

        let prd = prd_url
            .as_ref()
            .and_then(|url| resolved.get(url).cloned().flatten());
        let implementations = impl_urls
            .iter()
            .filter_map(|url| resolved.get(url).cloned().flatten())
            .collect::<Vec<_>>();

        Ok(protocol::WorkflowResolveLinkedIssuesResult {
            prd,
            implementations,
        })
    }

    pub async fn start_from_issue(
        state: Arc<AppState>,
        params: protocol::WorkflowStartFromIssueParams,
    ) -> Result<protocol::WorkflowStartFromIssueResult, String> {
        // Resolve worktree so we can look up the persona file.
        let worktree = {
            let store = state.store.lock().await;
            store
                .thread_by_id(&params.thread_id)
                .ok_or_else(|| format!("thread not found: {}", params.thread_id))?
                .worktree_path
                .clone()
        };

        let persona_name = params.persona.unwrap_or_else(|| "sisyphus".to_string());
        let persona_path =
            resolve_persona_path(Path::new(&worktree), &persona_name).ok_or_else(|| {
                format!(
                    "persona not found: {persona_name} (looked in .threadmill/agents/ \
                     and ~/.config/threadmill/agents/)"
                )
            })?;
        let persona = parse_agent_persona_file(&persona_path)?;

        // The orchestrator prompt says "read the issue thoroughly" — a 200-char
        // preview is not enough. If the caller didn't send a full body, try to
        // fetch it via the transport. Failures fall back gracefully to whatever
        // the caller did send.
        let issue_body = match params.issue_body.clone() {
            Some(body) if body.len() > 300 => Some(body),
            supplied => {
                let transport = crate::services::issues::for_project(&worktree, None).await;
                match transport.resolve(&params.issue_url).await.ok().flatten() {
                    Some(enriched) => enriched.body.or(supplied),
                    None => supplied,
                }
            }
        };

        let initial_prompt = format!(
            "Start a Sisyphus workflow for this PRD issue.\n\n\
             Issue: {}\nTitle: {}\n\n{}\n\n\
             Read the issue thoroughly, define the goal, then transition to IMPLEMENTING \
             and spawn Hephaestus workers for each chunk of work.",
            params.issue_url,
            params.issue_title,
            issue_body.as_deref().unwrap_or("(no body provided)"),
        );

        let chat_result = ChatService::start(
            Arc::clone(&state),
            protocol::ChatStartParams {
                thread_id: params.thread_id.clone(),
                agent_name: persona.agent.clone(),
                system_prompt: Some(persona.system_prompt.clone()),
                initial_prompt: Some(initial_prompt),
                display_name: persona.display_name.clone(),
                parent_session_id: None,
                preferred_model: persona.model.clone(),
            },
        )
        .await?;

        // If workflow.create fails after chat.start succeeded, stop the orphaned
        // session so the user doesn't end up with an ACP process nobody owns.
        let workflow = match Self::create(
            Arc::clone(&state),
            protocol::WorkflowCreateParams {
                thread_id: params.thread_id.clone(),
                prd_issue_url: Some(params.issue_url),
                implementation_issue_urls: Vec::new(),
                orchestrator_session_id: Some(chat_result.session_id.clone()),
            },
        )
        .await
        {
            Ok(w) => w,
            Err(err) => {
                let _ = ChatService::stop(
                    Arc::clone(&state),
                    protocol::ChatStopParams {
                        thread_id: params.thread_id,
                        session_id: chat_result.session_id,
                    },
                )
                .await;
                return Err(err);
            }
        };

        Ok(protocol::WorkflowStartFromIssueResult {
            workflow_id: workflow.workflow_id,
            orchestrator_session_id: chat_result.session_id,
        })
    }
}

// ---------- Issue cache (30s TTL, in-memory) ----------

struct CachedIssues {
    at: std::time::Instant,
    platform: protocol::WorkflowIssuePlatform,
    issues: Vec<protocol::WorkflowIssueRef>,
}

const ISSUE_CACHE_TTL_SECS: u64 = 30;

static ISSUE_CACHE: std::sync::OnceLock<
    tokio::sync::Mutex<std::collections::HashMap<String, CachedIssues>>,
> = std::sync::OnceLock::new();

fn issue_cache() -> &'static tokio::sync::Mutex<std::collections::HashMap<String, CachedIssues>> {
    ISSUE_CACHE.get_or_init(|| tokio::sync::Mutex::new(std::collections::HashMap::new()))
}

async fn issue_cache_get(key: &str) -> Option<CachedIssues> {
    let cache = issue_cache().lock().await;
    cache.get(key).and_then(|entry| {
        if entry.at.elapsed().as_secs() < ISSUE_CACHE_TTL_SECS {
            Some(CachedIssues {
                at: entry.at,
                platform: entry.platform.clone(),
                issues: entry.issues.clone(),
            })
        } else {
            None
        }
    })
}

async fn issue_cache_put(
    key: String,
    platform: protocol::WorkflowIssuePlatform,
    issues: Vec<protocol::WorkflowIssueRef>,
) {
    let mut cache = issue_cache().lock().await;
    cache.insert(
        key,
        CachedIssues {
            at: std::time::Instant::now(),
            platform,
            issues,
        },
    );
}

async fn handle_server_event(
    state: Arc<AppState>,
    event: crate::ServerEvent,
) -> Result<(), String> {
    match event.method.as_str() {
        "chat.session_ready" => {
            let event: protocol::ChatSessionReadyEvent = serde_json::from_value(event.params)
                .map_err(|err| format!("invalid chat.session_ready payload: {err}"))?;
            set_session_running(state, &event.session_id).await?;
        }
        "chat.session_failed" => {
            let event: protocol::ChatSessionFailedEvent = serde_json::from_value(event.params)
                .map_err(|err| format!("invalid chat.session_failed payload: {err}"))?;
            set_session_failed(state, &event.session_id, &event.error).await?;
        }
        "chat.session_ended" => {
            let event: protocol::ChatSessionEndedEvent = serde_json::from_value(event.params)
                .map_err(|err| format!("invalid chat.session_ended payload: {err}"))?;
            set_session_failed(state, &event.session_id, &event.reason).await?;
        }
        "chat.status_changed" => {
            let event: protocol::ChatStatusChangedEvent = serde_json::from_value(event.params)
                .map_err(|err| format!("invalid chat.status_changed payload: {err}"))?;
            update_session_agent_status(state, &event.session_id, event.new_status).await?;
        }
        "state.delta" => {
            let event: protocol::StateDeltaEvent = serde_json::from_value(event.params)
                .map_err(|err| format!("invalid state.delta payload: {err}"))?;
            for operation in event.operations {
                if let protocol::StateDeltaOperationPayload::ChatSessionRemoved {
                    session_id, ..
                } = operation.payload
                {
                    set_session_failed(Arc::clone(&state), &session_id, "chat session removed")
                        .await?;
                }
            }
        }
        _ => {}
    }

    Ok(())
}

async fn ensure_thread_can_host_workflow(
    state: &Arc<AppState>,
    thread_id: &str,
) -> Result<(), String> {
    let store = state.store.lock().await;
    let thread = store
        .thread_by_id(thread_id)
        .ok_or_else(|| format!("thread not found: {thread_id}"))?;
    if !matches!(
        thread.status,
        protocol::ThreadStatus::Active | protocol::ThreadStatus::Hidden
    ) {
        return Err(format!("thread must be active or hidden: {thread_id}"));
    }
    Ok(())
}

fn ensure_spawn_allowed(phase: &protocol::WorkflowPhase) -> Result<(), String> {
    match phase {
        protocol::WorkflowPhase::Planning
        | protocol::WorkflowPhase::Implementing
        | protocol::WorkflowPhase::Testing
        | protocol::WorkflowPhase::Fixing
        | protocol::WorkflowPhase::Blocked => Ok(()),
        protocol::WorkflowPhase::Reviewing => Err(
            "invalid workflow phase transition: workers cannot spawn during REVIEWING".to_string(),
        ),
        protocol::WorkflowPhase::Complete | protocol::WorkflowPhase::Failed => Err(format!(
            "invalid workflow phase transition: cannot spawn in {phase:?}"
        )),
    }
}

fn workflow_is_active_phase(phase: &protocol::WorkflowPhase) -> bool {
    !matches!(
        phase,
        protocol::WorkflowPhase::Complete | protocol::WorkflowPhase::Failed
    )
}

fn worker_is_active(status: &protocol::WorkflowWorkerStatus) -> bool {
    matches!(
        status,
        protocol::WorkflowWorkerStatus::Planned
            | protocol::WorkflowWorkerStatus::Spawning
            | protocol::WorkflowWorkerStatus::Running
    )
}

fn worker_is_terminal(status: &protocol::WorkflowWorkerStatus) -> bool {
    matches!(
        status,
        protocol::WorkflowWorkerStatus::Completed | protocol::WorkflowWorkerStatus::Failed
    )
}

fn touch_workflow(workflow: &mut protocol::WorkflowState) {
    workflow.updated_at = Utc::now().to_rfc3339();
}

fn prepend_issue_context(
    base: Option<String>,
    url: &str,
    issue: &crate::services::issues::EnrichedIssue,
) -> Option<String> {
    let header = format!(
        "# Linked issue\n\nURL: {}\nTitle: {}\nState: {}\n\n{}\n\n---\n\n",
        url,
        issue.r#ref.title,
        crate::services::issues::issue_state_as_str(&issue.state),
        issue.body.as_deref().unwrap_or("(no body)"),
    );
    Some(match base {
        Some(prompt) if !prompt.trim().is_empty() => format!("{header}{prompt}"),
        _ => header.trim_end_matches("\n\n---\n\n").to_string(),
    })
}

fn validate_phase_transition(
    current: &protocol::WorkflowPhase,
    next: &protocol::WorkflowPhase,
    force: bool,
) -> Result<(), String> {
    if force {
        return Ok(());
    }

    let valid = match current {
        protocol::WorkflowPhase::Planning => {
            matches!(
                next,
                protocol::WorkflowPhase::Implementing | protocol::WorkflowPhase::Blocked
            )
        }
        protocol::WorkflowPhase::Implementing => matches!(
            next,
            protocol::WorkflowPhase::Testing
                | protocol::WorkflowPhase::Blocked
                | protocol::WorkflowPhase::Failed
        ),
        protocol::WorkflowPhase::Testing => matches!(
            next,
            protocol::WorkflowPhase::Reviewing
                | protocol::WorkflowPhase::Blocked
                | protocol::WorkflowPhase::Failed
        ),
        protocol::WorkflowPhase::Reviewing => matches!(
            next,
            protocol::WorkflowPhase::Fixing
                | protocol::WorkflowPhase::Complete
                | protocol::WorkflowPhase::Blocked
                | protocol::WorkflowPhase::Failed
        ),
        protocol::WorkflowPhase::Fixing => matches!(
            next,
            protocol::WorkflowPhase::Testing
                | protocol::WorkflowPhase::Complete
                | protocol::WorkflowPhase::Blocked
                | protocol::WorkflowPhase::Failed
        ),
        protocol::WorkflowPhase::Blocked => matches!(
            next,
            protocol::WorkflowPhase::Planning
                | protocol::WorkflowPhase::Implementing
                | protocol::WorkflowPhase::Testing
                | protocol::WorkflowPhase::Reviewing
                | protocol::WorkflowPhase::Fixing
                | protocol::WorkflowPhase::Failed
        ),
        protocol::WorkflowPhase::Complete | protocol::WorkflowPhase::Failed => false,
    };

    if valid {
        Ok(())
    } else {
        Err(format!(
            "invalid workflow phase transition: {:?} -> {:?}",
            current, next
        ))
    }
}

fn list_workflows_locked(
    store: &WorkflowStore,
    thread_id: Option<&str>,
) -> Vec<protocol::WorkflowState> {
    let mut workflows = store
        .data
        .workflows
        .iter()
        .filter(|workflow| match thread_id {
            Some(thread_id) => workflow.state.thread_id == thread_id,
            None => true,
        })
        .map(|workflow| workflow.state.clone())
        .collect::<Vec<_>>();
    workflows.sort_by(|left, right| left.created_at.cmp(&right.created_at));
    workflows
}

fn find_workflow_locked<'a>(
    store: &'a WorkflowStore,
    workflow_id: &str,
) -> Option<&'a protocol::WorkflowState> {
    store
        .data
        .workflows
        .iter()
        .find(|workflow| workflow.state.workflow_id == workflow_id)
        .map(|workflow| &workflow.state)
}

fn find_workflow_locked_mut<'a>(
    store: &'a mut WorkflowStore,
    workflow_id: &str,
) -> Option<&'a mut PersistedWorkflow> {
    store
        .data
        .workflows
        .iter_mut()
        .find(|workflow| workflow.state.workflow_id == workflow_id)
}

fn emit_workflow_upsert(state: &AppState, workflow: protocol::WorkflowState) {
    state.emit_state_delta(vec![protocol::StateDeltaOperationPayload::WorkflowUpsert {
        workflow,
    }]);
}

/// Convert internal `EnrichedIssue` to the wire-safe `EnrichedIssueWire` for
/// RPC responses. The split keeps `services::issues::EnrichedIssue` free of
/// serde requirements — only the wire type needs them.
fn enriched_to_wire(issue: crate::services::issues::EnrichedIssue) -> protocol::EnrichedIssueWire {
    protocol::EnrichedIssueWire {
        r#ref: issue.r#ref,
        state: issue.state,
        labels: issue.labels,
        assignees: issue.assignees,
        body: issue.body,
    }
}

/// Scan active workflows for the first one whose PRD or implementation list
/// contains `url`. Used to attribute ambient issue events (close, comment) to
/// a workflow when the caller didn't supply one explicitly.
async fn find_workflow_id_for_issue_url(state: &AppState, url: &str) -> Option<String> {
    let store = state.workflows.lock().await;
    workflow_id_for_issue_url_locked(&store, url)
}

fn workflow_id_for_issue_url_locked(store: &WorkflowStore, url: &str) -> Option<String> {
    store
        .data
        .workflows
        .iter()
        .find(|workflow| {
            workflow.state.prd_issue_url.as_deref() == Some(url)
                || workflow
                    .state
                    .implementation_issue_urls
                    .iter()
                    .any(|candidate| candidate == url)
        })
        .map(|workflow| workflow.state.workflow_id.clone())
}

async fn emit_workflow_state_delta(state: &AppState, workflow_id: &str) {
    let workflow = {
        let store = state.workflows.lock().await;
        find_workflow_locked(&store, workflow_id).cloned()
    };
    if let Some(workflow) = workflow {
        emit_workflow_upsert(state, workflow);
    }
}

async fn snapshot_review_start_checkpoint(
    state: &Arc<AppState>,
    workflow_id: &str,
) -> Result<ReviewStartCheckpoint, String> {
    let store = state.workflows.lock().await;
    let workflow = find_workflow_locked(&store, workflow_id)
        .ok_or_else(|| format!("workflow not found: {workflow_id}"))?;
    let persisted = store
        .data
        .workflows
        .iter()
        .find(|workflow| workflow.state.workflow_id == workflow_id)
        .ok_or_else(|| format!("workflow not found: {workflow_id}"))?;
    Ok(ReviewStartCheckpoint {
        thread_id: workflow.thread_id.clone(),
        previous_phase: workflow.phase.clone(),
        previous_completed_at: workflow.completed_at.clone(),
        previous_review_started_at: workflow.review_started_at.clone(),
        previous_reviewer_count: workflow.reviewers.len(),
        previous_next_reviewer_index: persisted.next_reviewer_index,
    })
}

async fn rollback_review_start(
    state: Arc<AppState>,
    workflow_id: &str,
    checkpoint: &ReviewStartCheckpoint,
) -> Result<(), String> {
    let (workflow, phase_change, session_ids) = {
        let mut store = state.workflows.lock().await;
        let workflow = find_workflow_locked_mut(&mut store, workflow_id)
            .ok_or_else(|| format!("workflow not found: {workflow_id}"))?;
        if workflow.state.reviewers.len() < checkpoint.previous_reviewer_count {
            return Err(format!(
                "workflow rollback invariant violated: reviewer count {} < checkpoint {}",
                workflow.state.reviewers.len(),
                checkpoint.previous_reviewer_count
            ));
        }

        let current_phase = workflow.state.phase.clone();
        let removed_reviewers = workflow
            .state
            .reviewers
            .split_off(checkpoint.previous_reviewer_count);
        let session_ids = removed_reviewers
            .into_iter()
            .filter_map(|reviewer| reviewer.session_id)
            .collect::<Vec<_>>();
        workflow.next_reviewer_index = checkpoint.previous_next_reviewer_index;
        workflow.state.phase = checkpoint.previous_phase.clone();
        workflow.state.completed_at = checkpoint.previous_completed_at.clone();
        workflow.state.review_started_at = checkpoint.previous_review_started_at.clone();
        touch_workflow(&mut workflow.state);
        let workflow_state = workflow.state.clone();
        let phase_change = (current_phase != checkpoint.previous_phase)
            .then_some((current_phase, checkpoint.previous_phase.clone()));
        store.save()?;
        (workflow_state, phase_change, session_ids)
    };

    if let Some((old_phase, new_phase)) = phase_change {
        state.emit_event(
            "workflow.phase_changed",
            protocol::WorkflowPhaseChangedEvent {
                workflow_id: workflow.workflow_id.clone(),
                thread_id: workflow.thread_id.clone(),
                old_phase,
                new_phase,
                forced: true,
            },
        );
    }
    emit_workflow_upsert(&state, workflow);

    let mut rollback_errors = Vec::new();
    for session_id in session_ids {
        if let Err(error) = ChatService::stop(
            Arc::clone(&state),
            protocol::ChatStopParams {
                thread_id: checkpoint.thread_id.clone(),
                session_id,
            },
        )
        .await
        {
            rollback_errors.push(error);
        }
    }

    if rollback_errors.is_empty() {
        Ok(())
    } else {
        Err(rollback_errors.join("; "))
    }
}

async fn mark_worker_failed(
    state: Arc<AppState>,
    workflow_id: &str,
    worker_id: &str,
    message: &str,
) -> Result<protocol::WorkflowWorker, String> {
    let mut store = state.workflows.lock().await;
    let workflow = find_workflow_locked_mut(&mut store, workflow_id)
        .ok_or_else(|| format!("workflow not found: {workflow_id}"))?;
    let worker_index = workflow
        .state
        .workers
        .iter()
        .position(|worker| worker.worker_id == worker_id)
        .ok_or_else(|| format!("worker not found: {worker_id}"))?;
    {
        let worker = &mut workflow.state.workers[worker_index];
        worker.status = protocol::WorkflowWorkerStatus::Failed;
        worker.completed_at = Some(Utc::now().to_rfc3339());
        worker.failure_message = Some(message.to_string());
    }
    touch_workflow(&mut workflow.state);
    let worker = workflow.state.workers[worker_index].clone();
    store.save()?;
    Ok(worker)
}

async fn mark_reviewer_failed(
    state: Arc<AppState>,
    workflow_id: &str,
    reviewer_id: &str,
    message: &str,
) -> Result<protocol::WorkflowReviewer, String> {
    let mut store = state.workflows.lock().await;
    let workflow = find_workflow_locked_mut(&mut store, workflow_id)
        .ok_or_else(|| format!("workflow not found: {workflow_id}"))?;
    let reviewer_index = workflow
        .state
        .reviewers
        .iter()
        .position(|reviewer| reviewer.reviewer_id == reviewer_id)
        .ok_or_else(|| format!("reviewer not found: {reviewer_id}"))?;
    {
        let reviewer = &mut workflow.state.reviewers[reviewer_index];
        reviewer.status = protocol::WorkflowWorkerStatus::Failed;
        reviewer.completed_at = Some(Utc::now().to_rfc3339());
        reviewer.failure_message = Some(message.to_string());
    }
    touch_workflow(&mut workflow.state);
    let reviewer = workflow.state.reviewers[reviewer_index].clone();
    store.save()?;
    Ok(reviewer)
}

async fn set_session_running(state: Arc<AppState>, session_id: &str) -> Result<(), String> {
    let workflow_id = {
        let mut store = state.workflows.lock().await;
        let mut changed_workflow = None;
        for workflow in &mut store.data.workflows {
            if let Some(worker_index) = workflow
                .state
                .workers
                .iter()
                .position(|worker| worker.session_id.as_deref() == Some(session_id))
            {
                if workflow.state.workers[worker_index].status
                    != protocol::WorkflowWorkerStatus::Running
                {
                    {
                        let worker = &mut workflow.state.workers[worker_index];
                        worker.status = protocol::WorkflowWorkerStatus::Running;
                        worker.failure_message = None;
                    }
                    touch_workflow(&mut workflow.state);
                    changed_workflow = Some(workflow.state.workflow_id.clone());
                }
                break;
            }
            if let Some(reviewer_index) = workflow
                .state
                .reviewers
                .iter()
                .position(|reviewer| reviewer.session_id.as_deref() == Some(session_id))
            {
                if workflow.state.reviewers[reviewer_index].status
                    != protocol::WorkflowWorkerStatus::Running
                {
                    {
                        let reviewer = &mut workflow.state.reviewers[reviewer_index];
                        reviewer.status = protocol::WorkflowWorkerStatus::Running;
                        reviewer.failure_message = None;
                    }
                    touch_workflow(&mut workflow.state);
                    changed_workflow = Some(workflow.state.workflow_id.clone());
                }
                break;
            }
        }
        if changed_workflow.is_some() {
            store.save()?;
        }
        changed_workflow
    };

    if let Some(workflow_id) = workflow_id {
        emit_workflow_state_delta(&state, &workflow_id).await;
    }

    Ok(())
}

async fn reconcile_bound_session_status(
    state: Arc<AppState>,
    session_id: &str,
    chat_status: &protocol::ChatStatusResult,
) -> Result<(), String> {
    update_session_agent_status(
        Arc::clone(&state),
        session_id,
        chat_status.agent_status.clone(),
    )
    .await?;

    if chat_status.status == protocol::ChatSessionStatus::Ready {
        set_session_running(state, session_id).await?;
    }

    Ok(())
}

async fn set_session_failed(
    state: Arc<AppState>,
    session_id: &str,
    message: &str,
) -> Result<(), String> {
    let updates = {
        let mut store = state.workflows.lock().await;
        let mut updates = Vec::new();
        for workflow in &mut store.data.workflows {
            if let Some(worker_index) = workflow
                .state
                .workers
                .iter()
                .position(|worker| worker.session_id.as_deref() == Some(session_id))
            {
                if !worker_is_terminal(&workflow.state.workers[worker_index].status) {
                    {
                        let worker = &mut workflow.state.workers[worker_index];
                        worker.status = protocol::WorkflowWorkerStatus::Failed;
                        worker.completed_at = Some(Utc::now().to_rfc3339());
                        worker.failure_message = Some(message.to_string());
                    }
                    touch_workflow(&mut workflow.state);
                    updates.push(SessionFailureUpdate::Worker {
                        workflow: workflow.state.clone(),
                        worker: workflow.state.workers[worker_index].clone(),
                    });
                }
                break;
            }

            if let Some(reviewer_index) = workflow
                .state
                .reviewers
                .iter()
                .position(|reviewer| reviewer.session_id.as_deref() == Some(session_id))
            {
                if !worker_is_terminal(&workflow.state.reviewers[reviewer_index].status) {
                    {
                        let reviewer = &mut workflow.state.reviewers[reviewer_index];
                        reviewer.status = protocol::WorkflowWorkerStatus::Failed;
                        reviewer.completed_at = Some(Utc::now().to_rfc3339());
                        reviewer.failure_message = Some(message.to_string());
                    }
                    touch_workflow(&mut workflow.state);
                    updates.push(SessionFailureUpdate::Reviewer {
                        workflow: workflow.state.clone(),
                        reviewer: workflow.state.reviewers[reviewer_index].clone(),
                    });
                }
                break;
            }
        }

        if !updates.is_empty() {
            store.save()?;
        }
        updates
    };

    for update in updates {
        match update {
            SessionFailureUpdate::Worker { workflow, worker } => {
                state.emit_event(
                    "workflow.worker_completed",
                    protocol::WorkflowWorkerCompletedEvent {
                        workflow_id: workflow.workflow_id.clone(),
                        thread_id: workflow.thread_id.clone(),
                        worker,
                    },
                );
                emit_workflow_upsert(&state, workflow);
            }
            SessionFailureUpdate::Reviewer { workflow, reviewer } => {
                state.emit_event(
                    "workflow.reviewer_completed",
                    protocol::WorkflowReviewerCompletedEvent {
                        workflow_id: workflow.workflow_id.clone(),
                        thread_id: workflow.thread_id.clone(),
                        reviewer,
                    },
                );
                emit_workflow_upsert(&state, workflow.clone());

                // If every reviewer for this round is now in a terminal state AND
                // `start_review` has finished appending the full spec list, the round
                // is over — emit `review_completed` so the UI can surface
                // "awaiting findings" and the orchestrator can call Momus.
                //
                // The `expected_reviewer_count` gate matters: reviewers are spawned
                // sequentially, so a fast-finishing first reviewer could otherwise trip
                // `all_terminal` with count=1 before the others are even inserted.
                let expected = {
                    let store = state.workflows.lock().await;
                    store
                        .data
                        .workflows
                        .iter()
                        .find(|w| w.state.workflow_id == workflow.workflow_id)
                        .map(|w| w.expected_reviewer_count)
                        .unwrap_or(0)
                };
                let all_terminal = !workflow.reviewers.is_empty()
                    && workflow
                        .reviewers
                        .iter()
                        .all(|r| worker_is_terminal(&r.status))
                    && u32::try_from(workflow.reviewers.len()).unwrap_or(u32::MAX) >= expected;
                if all_terminal {
                    state.emit_event(
                        "workflow.review_completed",
                        protocol::WorkflowReviewCompletedEvent {
                            workflow_id: workflow.workflow_id.clone(),
                            thread_id: workflow.thread_id.clone(),
                            review_round: workflow.current_review_round,
                            reviewer_count: workflow.reviewers.len(),
                        },
                    );
                    // Reset so a mid-round manual `spawn_reviewer` added later can't
                    // re-fire the end-of-round event on its own terminal transition.
                    let mut store = state.workflows.lock().await;
                    if let Some(wf) = find_workflow_locked_mut(&mut store, &workflow.workflow_id) {
                        wf.expected_reviewer_count = 0;
                        let _ = store.save();
                    }
                }
            }
        }
    }

    Ok(())
}

async fn update_session_agent_status(
    state: Arc<AppState>,
    session_id: &str,
    agent_status: protocol::AgentStatus,
) -> Result<(), String> {
    let workflow_id = {
        let mut store = state.workflows.lock().await;
        let mut changed_workflow = None;
        for workflow in &mut store.data.workflows {
            if let Some(worker_index) = workflow
                .state
                .workers
                .iter()
                .position(|worker| worker.session_id.as_deref() == Some(session_id))
            {
                workflow.state.workers[worker_index].agent_status = Some(agent_status.clone());
                touch_workflow(&mut workflow.state);
                changed_workflow = Some(workflow.state.workflow_id.clone());
                break;
            }
            if let Some(reviewer_index) = workflow
                .state
                .reviewers
                .iter()
                .position(|reviewer| reviewer.session_id.as_deref() == Some(session_id))
            {
                workflow.state.reviewers[reviewer_index].agent_status = Some(agent_status.clone());
                touch_workflow(&mut workflow.state);
                changed_workflow = Some(workflow.state.workflow_id.clone());
                break;
            }
        }
        if changed_workflow.is_some() {
            store.save()?;
        }
        changed_workflow
    };

    if let Some(workflow_id) = workflow_id {
        emit_workflow_state_delta(&state, &workflow_id).await;
    }

    Ok(())
}

enum SessionFailureUpdate {
    Worker {
        workflow: protocol::WorkflowState,
        worker: protocol::WorkflowWorker,
    },
    Reviewer {
        workflow: protocol::WorkflowState,
        reviewer: protocol::WorkflowReviewer,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_store::{AppData, StateStore};

    fn make_test_state() -> Arc<AppState> {
        let state_dir = std::env::temp_dir().join(format!(
            "threadmill-workflow-tests-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&state_dir).expect("create workflow test dir");
        let state_path = state_dir.join("threads.json");
        Arc::new(AppState::new(StateStore {
            path: state_path,
            data: AppData::default(),
        }))
    }

    async fn insert_workflow_with_worker(
        state: &Arc<AppState>,
        workflow_id: &str,
        worker_id: &str,
        session_id: &str,
    ) {
        let mut store = state.workflows.lock().await;
        store.data.workflows.push(PersistedWorkflow {
            state: protocol::WorkflowState {
                workflow_id: workflow_id.to_string(),
                thread_id: "thread-1".to_string(),
                phase: protocol::WorkflowPhase::Implementing,
                created_at: Utc::now().to_rfc3339(),
                updated_at: Utc::now().to_rfc3339(),
                completed_at: None,
                prd_issue_url: None,
                implementation_issue_urls: Vec::new(),
                orchestrator_session_id: None,
                workers: vec![protocol::WorkflowWorker {
                    worker_id: worker_id.to_string(),
                    agent_name: "mock".to_string(),
                    status: protocol::WorkflowWorkerStatus::Spawning,
                    session_id: Some(session_id.to_string()),
                    parent_session_id: None,
                    display_name: None,
                    created_at: Utc::now().to_rfc3339(),
                    started_at: Some(Utc::now().to_rfc3339()),
                    completed_at: None,
                    stop_reason: None,
                    handoff: None,
                    failure_message: Some("stale error".to_string()),
                    agent_status: None,
                    issue_url: None,
                }],
                reviewers: Vec::new(),
                findings: Vec::new(),
                review_started_at: None,
                current_review_round: 0,
            },
            next_worker_index: 2,
            next_reviewer_index: 1,
            next_finding_index: 1,
            expected_reviewer_count: 0,
        });
        store.save().expect("save workflow store");
    }

    async fn insert_workflow_with_reviewer(
        state: &Arc<AppState>,
        workflow_id: &str,
        reviewer_id: &str,
        session_id: &str,
    ) {
        let mut store = state.workflows.lock().await;
        store.data.workflows.push(PersistedWorkflow {
            state: protocol::WorkflowState {
                workflow_id: workflow_id.to_string(),
                thread_id: "thread-1".to_string(),
                phase: protocol::WorkflowPhase::Reviewing,
                created_at: Utc::now().to_rfc3339(),
                updated_at: Utc::now().to_rfc3339(),
                completed_at: None,
                prd_issue_url: None,
                implementation_issue_urls: Vec::new(),
                orchestrator_session_id: None,
                workers: Vec::new(),
                reviewers: vec![protocol::WorkflowReviewer {
                    reviewer_id: reviewer_id.to_string(),
                    agent_name: "mock".to_string(),
                    status: protocol::WorkflowWorkerStatus::Spawning,
                    session_id: Some(session_id.to_string()),
                    parent_session_id: None,
                    display_name: None,
                    created_at: Utc::now().to_rfc3339(),
                    started_at: Some(Utc::now().to_rfc3339()),
                    completed_at: None,
                    failure_message: Some("stale error".to_string()),
                    agent_status: None,
                }],
                findings: Vec::new(),
                review_started_at: Some(Utc::now().to_rfc3339()),
                current_review_round: 1,
            },
            next_worker_index: 1,
            next_reviewer_index: 2,
            next_finding_index: 1,
            expected_reviewer_count: 0,
        });
        store.save().expect("save workflow store");
    }

    #[test]
    fn invalid_transition_requires_force() {
        let error = validate_phase_transition(
            &protocol::WorkflowPhase::Planning,
            &protocol::WorkflowPhase::Reviewing,
            false,
        )
        .expect_err("planning -> reviewing should be rejected");
        assert!(error.contains("invalid workflow phase transition"));
    }

    #[test]
    fn fixing_can_return_to_testing() {
        validate_phase_transition(
            &protocol::WorkflowPhase::Fixing,
            &protocol::WorkflowPhase::Testing,
            false,
        )
        .expect("fixing -> testing should be allowed");
    }

    #[tokio::test]
    async fn reconcile_bound_session_status_marks_worker_running_when_chat_already_ready() {
        let state = make_test_state();
        insert_workflow_with_worker(&state, "wf-1", "W001", "session-1").await;

        reconcile_bound_session_status(
            Arc::clone(&state),
            "session-1",
            &protocol::ChatStatusResult {
                session_id: "session-1".to_string(),
                agent_type: "mock".to_string(),
                status: protocol::ChatSessionStatus::Ready,
                agent_status: protocol::AgentStatus::Busy,
                worker_count: 0,
                title: None,
                model_id: None,
                created_at: Utc::now().to_rfc3339(),
                display_name: None,
                parent_session_id: None,
            },
        )
        .await
        .expect("reconcile ready worker session");

        let store = state.workflows.lock().await;
        let workflow = find_workflow_locked(&store, "wf-1").expect("workflow");
        assert_eq!(
            workflow.workers[0].status,
            protocol::WorkflowWorkerStatus::Running
        );
        assert_eq!(workflow.workers[0].failure_message, None);
        assert_eq!(
            workflow.workers[0].agent_status,
            Some(protocol::AgentStatus::Busy)
        );
    }

    #[tokio::test]
    async fn reconcile_bound_session_status_marks_reviewer_running_when_chat_already_ready() {
        let state = make_test_state();
        insert_workflow_with_reviewer(&state, "wf-1", "R001", "session-1").await;

        reconcile_bound_session_status(
            Arc::clone(&state),
            "session-1",
            &protocol::ChatStatusResult {
                session_id: "session-1".to_string(),
                agent_type: "mock".to_string(),
                status: protocol::ChatSessionStatus::Ready,
                agent_status: protocol::AgentStatus::Idle,
                worker_count: 0,
                title: None,
                model_id: None,
                created_at: Utc::now().to_rfc3339(),
                display_name: None,
                parent_session_id: None,
            },
        )
        .await
        .expect("reconcile ready reviewer session");

        let store = state.workflows.lock().await;
        let workflow = find_workflow_locked(&store, "wf-1").expect("workflow");
        assert_eq!(
            workflow.reviewers[0].status,
            protocol::WorkflowWorkerStatus::Running
        );
        assert_eq!(workflow.reviewers[0].failure_message, None);
        assert_eq!(
            workflow.reviewers[0].agent_status,
            Some(protocol::AgentStatus::Idle)
        );
    }

    fn make_issue(title: &str, body: Option<&str>) -> crate::services::issues::EnrichedIssue {
        crate::services::issues::EnrichedIssue {
            r#ref: protocol::WorkflowIssueRef {
                number: 42,
                title: title.to_string(),
                url: "https://example.com/42".to_string(),
                author: None,
                created_at: None,
                body_preview: None,
            },
            state: crate::services::issues::IssueState::Open,
            labels: Vec::new(),
            assignees: Vec::new(),
            body: body.map(str::to_string),
        }
    }

    #[test]
    fn prepend_issue_context_wraps_existing_prompt() {
        let issue = make_issue("Add thing", Some("Long description of the thing"));
        let result =
            prepend_issue_context(Some("Do it now.".to_string()), "url-42", &issue).unwrap();
        assert!(result.contains("URL: url-42"));
        assert!(result.contains("Title: Add thing"));
        assert!(result.contains("State: open"));
        assert!(result.contains("Long description of the thing"));
        assert!(result.ends_with("Do it now."));
    }

    #[test]
    fn prepend_issue_context_handles_missing_base_prompt() {
        let issue = make_issue("Add thing", Some("Body here"));
        let result = prepend_issue_context(None, "url-42", &issue).unwrap();
        assert!(result.contains("Body here"));
        assert!(!result.ends_with("---\n\n"));
    }

    #[test]
    fn prepend_issue_context_handles_empty_body() {
        let issue = make_issue("Stub", None);
        let result = prepend_issue_context(Some("Prompt".into()), "url", &issue).unwrap();
        assert!(result.contains("(no body)"));
        assert!(result.ends_with("Prompt"));
    }
}
