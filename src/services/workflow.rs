use std::{fs, path::{Path, PathBuf}, sync::Arc};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use uuid::Uuid;

use crate::{protocol, services::chat::ChatService, AppState};

pub struct WorkflowService;

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
}

fn default_next_index() -> u32 {
    1
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
        let config_dir = dirs::config_dir().ok_or_else(|| "unable to locate config dir".to_string())?;
        let dir = config_dir.join("threadmill");
        Self::load_from_dir(&dir)
    }

    pub fn load_from_dir(dir: &Path) -> Result<Self, String> {
        fs::create_dir_all(dir).map_err(|err| format!("failed to create {}: {err}", dir.display()))?;

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
                        worker.failure_message = Some(
                            "Spindle restarted before worker completed".to_string(),
                        );
                        changed_workers.push(worker.clone());
                        workflow_changed = true;
                    }
                }

                for reviewer in &mut workflow.state.reviewers {
                    if worker_is_active(&reviewer.status) {
                        reviewer.status = protocol::WorkflowWorkerStatus::Failed;
                        reviewer.completed_at = Some(now.clone());
                        reviewer.failure_message = Some(
                            "Spindle restarted before reviewer completed".to_string(),
                        );
                        changed_reviewers.push(reviewer.clone());
                        workflow_changed = true;
                    }
                }

                let mut phase_change = None;
                if workflow_is_active_phase(&workflow.state.phase)
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
                workers: Vec::new(),
                reviewers: Vec::new(),
                findings: Vec::new(),
                review_started_at: None,
            };
            workflows.data.workflows.push(PersistedWorkflow {
                state: workflow.clone(),
                next_worker_index: 1,
                next_reviewer_index: 1,
                next_finding_index: 1,
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
                old_phase,
                new_phase,
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

        let chat_result = ChatService::start(
            Arc::clone(&state),
            protocol::ChatStartParams {
                thread_id: thread_id.clone(),
                agent_name: params.agent_name,
                system_prompt: params.system_prompt,
                initial_prompt: params.initial_prompt,
                display_name: params.display_name,
                parent_session_id: params.parent_session_id,
            },
        )
        .await;

        match chat_result {
            Ok(chat_result) => {
                let worker = {
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
                    let worker = workflow.state.workers[worker_index].clone();
                    store.save()?;
                    worker
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
                Ok(protocol::WorkflowSpawnWorkerResult { workflow_id, worker })
            }
            Err(error) => {
                let failed_worker = mark_worker_failed(
                    Arc::clone(&state),
                    &workflow_id,
                    &worker_id,
                    &error,
                )
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
        Self::transition(
            Arc::clone(&state),
            protocol::WorkflowTransitionParams {
                workflow_id: params.workflow_id.clone(),
                phase: protocol::WorkflowPhase::Reviewing,
                force: params.force,
            },
        )
        .await?;

        let mut reviewers = Vec::with_capacity(params.reviewers.len());
        for reviewer in params.reviewers {
            let spawned = Self::spawn_reviewer(
                Arc::clone(&state),
                protocol::WorkflowSpawnReviewerParams {
                    workflow_id: params.workflow_id.clone(),
                    agent_name: reviewer.agent_name,
                    parent_session_id: reviewer.parent_session_id,
                    system_prompt: reviewer.system_prompt,
                    initial_prompt: reviewer.initial_prompt,
                    display_name: reviewer.display_name,
                },
            )
            .await?;
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
        Ok(protocol::WorkflowStartReviewResult { workflow, reviewers })
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
            },
        )
        .await;

        match chat_result {
            Ok(chat_result) => {
                let reviewer = {
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
                    let reviewer = workflow.state.reviewers[reviewer_index].clone();
                    store.save()?;
                    reviewer
                };
                emit_workflow_state_delta(&state, &workflow_id).await;
                Ok(protocol::WorkflowSpawnReviewerResult {
                    workflow_id,
                    reviewer,
                })
            }
            Err(error) => {
                let failed_reviewer = mark_reviewer_failed(
                    Arc::clone(&state),
                    &workflow_id,
                    &reviewer_id,
                    &error,
                )
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
                if let protocol::StateDeltaOperationPayload::ChatSessionRemoved { session_id, .. } =
                    operation.payload
                {
                    set_session_failed(Arc::clone(&state), &session_id, "chat session removed").await?;
                }
            }
        }
        _ => {}
    }

    Ok(())
}

async fn ensure_thread_can_host_workflow(state: &Arc<AppState>, thread_id: &str) -> Result<(), String> {
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
        protocol::WorkflowPhase::Reviewing => {
            Err("invalid workflow phase transition: workers cannot spawn during REVIEWING".to_string())
        }
        protocol::WorkflowPhase::Complete | protocol::WorkflowPhase::Failed => {
            Err(format!("invalid workflow phase transition: cannot spawn in {phase:?}"))
        }
    }
}

fn workflow_is_active_phase(phase: &protocol::WorkflowPhase) -> bool {
    !matches!(phase, protocol::WorkflowPhase::Complete | protocol::WorkflowPhase::Failed)
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
            matches!(next, protocol::WorkflowPhase::Implementing | protocol::WorkflowPhase::Blocked)
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

async fn emit_workflow_state_delta(state: &AppState, workflow_id: &str) {
    let workflow = {
        let store = state.workflows.lock().await;
        find_workflow_locked(&store, workflow_id).cloned()
    };
    if let Some(workflow) = workflow {
        emit_workflow_upsert(state, workflow);
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
                if workflow.state.workers[worker_index].status != protocol::WorkflowWorkerStatus::Running {
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
                emit_workflow_upsert(&state, workflow);
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
}
