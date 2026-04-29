use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::Utc;
use serde::Deserialize;
use tokio::process::Command;
use tracing::{error, warn};
use uuid::Uuid;

use crate::{
    protocol,
    services::{
        chat::ChatService, checkpoint::CheckpointService, preset::PresetService, sanitize_name,
        short_id,
    },
    state_store::{port_base_with_offset, thread_env, Thread},
    tmux, AppState,
};

pub struct ThreadService;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ThreadmillConfig {
    #[serde(default)]
    pub setup: Vec<String>,
    #[serde(default)]
    pub teardown: Vec<String>,
    #[serde(default)]
    pub copy_from_main: Vec<String>,
    #[serde(default)]
    pub presets: HashMap<String, PresetDefinition>,
    #[serde(default)]
    pub agents: HashMap<String, AgentDefinition>,
    #[serde(default)]
    pub ports: PortsConfig,
    #[serde(default)]
    pub checkpoints: CheckpointConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PortsConfig {
    #[serde(default = "default_base_port")]
    pub base: u16,
    #[serde(default = "default_port_offset")]
    pub offset: u16,
}

impl Default for PortsConfig {
    fn default() -> Self {
        Self {
            base: default_base_port(),
            offset: default_port_offset(),
        }
    }
}

fn default_base_port() -> u16 {
    3000
}

fn default_port_offset() -> u16 {
    20
}

fn default_checkpoint_max_count() -> usize {
    50
}

#[derive(Debug, Clone, Deserialize)]
pub struct CheckpointConfig {
    #[serde(default = "default_checkpoint_max_count")]
    pub max_count: usize,
    #[serde(default)]
    pub max_age_days: Option<u64>,
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            max_count: default_checkpoint_max_count(),
            max_age_days: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PresetDefinition {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub parallel: bool,
    #[serde(default)]
    pub autostart: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AgentDefinition {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

impl ThreadService {
    pub async fn create(
        state: Arc<AppState>,
        params: protocol::ThreadCreateParams,
    ) -> Result<protocol::Thread, String> {
        let (project, mut thread_name, mut branch, mut worktree_path) = {
            let store = state.store.lock().await;
            let project = store
                .project_by_id(&params.project_id)
                .ok_or_else(|| format!("project not found: {}", params.project_id))?
                .clone();

            let requested_name = sanitize_name(&params.name);
            let thread_name = allocate_thread_name(&store, &project.id, &requested_name)?;

            let mut branch = resolve_branch(&params, &thread_name)?;
            let worktree_path =
                planned_worktree_path(&project, &thread_name, &params.source_type, params.sandbox);
            if worktree_path.is_none() {
                if params.branch.is_some() && branch != project.default_branch {
                    return Err(format!(
                        "sandbox=false cannot create branch {} from shared checkout {}; use sandbox/worktree mode",
                        branch, project.default_branch
                    ));
                }
                branch = project.default_branch.clone();
            }
            (project, thread_name, branch, worktree_path)
        };

        if worktree_path.is_none() {
            ensure_shared_checkout_branch_for_create(&project.path, &project.default_branch)
                .await?;
        }

        let (thread, protocol_thread) = {
            let mut store = state.store.lock().await;
            if active_thread_name_exists(&store, &project.id, &thread_name) {
                thread_name = allocate_thread_name(&store, &project.id, &thread_name)?;
                branch = resolve_branch(&params, &thread_name)?;
                worktree_path = planned_worktree_path(
                    &project,
                    &thread_name,
                    &params.source_type,
                    params.sandbox,
                );
                if worktree_path.is_none() {
                    if params.branch.is_some() && branch != project.default_branch {
                        return Err(format!(
                            "sandbox=false cannot create branch {} from shared checkout {}; use sandbox/worktree mode",
                            branch, project.default_branch
                        ));
                    }
                    branch = project.default_branch.clone();
                }
            }

            let tmux_session = format!("tm_{}_{}", short_id(&project.id), thread_name);
            let checkout_path = thread_checkout_path(worktree_path.as_deref(), &project.path);
            let config = load_threadmill_config(checkout_path, &project.path)?;
            let port_offset = store.allocate_port_offset(&project.id, config.ports.offset)?;

            let display_name = requested_display_name(&params.name, &thread_name);
            let mut thread = Thread::new(
                Uuid::new_v4().to_string(),
                project.id.clone(),
                thread_name,
                branch,
                worktree_path,
                protocol::ThreadStatus::Creating,
                params.source_type.clone(),
                Utc::now(),
                tmux_session,
                port_offset,
            );
            thread.display_name = display_name;

            let protocol_thread = thread.to_protocol();
            store.data.threads.push(thread.clone());
            store.save()?;
            (thread, protocol_thread)
        };

        state.emit_state_delta(vec![protocol::StateDeltaOperationPayload::ThreadCreated {
            thread: protocol_thread.clone(),
        }]);

        let thread_id = thread.id.clone();
        let state_for_task = Arc::clone(&state);
        let thread_id_for_task = thread_id.clone();
        let handle = tokio::spawn(async move {
            if let Err(err) =
                Self::run_create_workflow(state_for_task.clone(), &thread_id_for_task).await
            {
                error!(thread_id = %thread_id_for_task, error = %err, "thread.create workflow failed");
                let _ =
                    Self::mark_failed(Arc::clone(&state_for_task), &thread_id_for_task, &err).await;
            }

            let mut create_tasks = state_for_task.create_tasks.lock().await;
            create_tasks.remove(&thread_id_for_task);
        });

        {
            let mut create_tasks = state.create_tasks.lock().await;
            create_tasks.insert(thread_id, handle);
        }

        Ok(protocol_thread)
    }

    pub async fn list(
        state: Arc<AppState>,
        params: protocol::ThreadListParams,
    ) -> Result<Vec<protocol::Thread>, String> {
        let store = state.store.lock().await;
        let project_filter = params.project_id.as_deref();
        Ok(store
            .data
            .threads
            .iter()
            .filter(|thread| match project_filter {
                Some(project_id) => thread.project_id == project_id,
                None => true,
            })
            .map(Thread::to_protocol)
            .collect())
    }

    pub async fn cancel(
        state: Arc<AppState>,
        params: protocol::ThreadCancelParams,
    ) -> Result<protocol::ThreadCancelResult, String> {
        {
            let store = state.store.lock().await;
            store
                .thread_by_id(&params.thread_id)
                .ok_or_else(|| format!("thread not found: {}", params.thread_id))?;
        }

        let handle = {
            let mut create_tasks = state.create_tasks.lock().await;
            create_tasks.remove(&params.thread_id)
        };

        if let Some(handle) = handle {
            handle.abort();
        }

        Self::mark_failed(
            Arc::clone(&state),
            &params.thread_id,
            "thread creation cancelled",
        )
        .await?;

        Ok(protocol::ThreadCancelResult {
            status: protocol::ThreadStatus::Failed,
        })
    }

    pub async fn close(
        state: Arc<AppState>,
        params: protocol::ThreadCloseParams,
    ) -> Result<protocol::ThreadCloseResult, String> {
        let mode = params.mode.as_str();
        if mode != "close" && mode != "hide" {
            return Err(format!("unsupported close mode: {}", params.mode));
        }

        if mode == "hide" {
            let result = Self::hide(
                state,
                protocol::ThreadHideParams {
                    thread_id: params.thread_id,
                },
            )
            .await?;
            return Ok(protocol::ThreadCloseResult {
                status: Some(result.status),
            });
        }

        let (thread, project_path) = {
            let mut store = state.store.lock().await;
            let thread = store
                .thread_by_id(&params.thread_id)
                .ok_or_else(|| format!("thread not found: {}", params.thread_id))?
                .clone();
            let project_path = store
                .project_by_id(&thread.project_id)
                .ok_or_else(|| format!("project not found: {}", thread.project_id))?
                .path
                .clone();

            Self::set_status_locked(
                &state,
                &mut store,
                &params.thread_id,
                protocol::ThreadStatus::Closing,
            )?;
            (thread, project_path)
        };

        let cleanup_result =
            Self::perform_close_cleanup(Arc::clone(&state), &thread, &project_path).await;

        match cleanup_result {
            Ok(()) => {
                let mut store = state.store.lock().await;
                Self::set_status_locked(
                    &state,
                    &mut store,
                    &params.thread_id,
                    protocol::ThreadStatus::Closed,
                )?;
                store.save()?;
                Ok(protocol::ThreadCloseResult {
                    status: Some(protocol::ThreadStatus::Closed),
                })
            }
            Err(err) => {
                warn!(
                    thread_id = %params.thread_id,
                    error = %err,
                    "close cleanup failed, marking thread as failed"
                );
                let mut store = state.store.lock().await;
                let _ = Self::set_status_locked(
                    &state,
                    &mut store,
                    &params.thread_id,
                    protocol::ThreadStatus::Failed,
                );
                let _ = store.save();
                Err(err)
            }
        }
    }

    /// Best-effort cleanup during thread close. All steps that can fail are
    /// collected here so the caller can transition to Failed on error instead
    /// of leaving the thread stranded in Closing.
    async fn perform_close_cleanup(
        state: Arc<AppState>,
        thread: &Thread,
        project_path: &str,
    ) -> Result<(), String> {
        ChatService::stop_all_for_thread(Arc::clone(&state), &thread.id, "thread_closed", true)
            .await?;

        if tmux::session_exists(&thread.tmux_session).await? {
            let _ = tmux::kill_session(&thread.tmux_session).await;
        }

        let checkout_path = thread.checkout_path(project_path);
        if Path::new(checkout_path).is_dir() {
            let config = load_threadmill_config(checkout_path, project_path)?;
            let port_base = port_base_with_offset(config.ports.base, thread.port_offset)?;
            run_hooks(
                &config.teardown,
                checkout_path,
                project_path,
                thread,
                port_base,
            )
            .await?;
        } else {
            tracing::info!(
                thread_id = %thread.id,
                checkout = %checkout_path,
                "thread checkout missing, skipping teardown hooks"
            );
        }

        if let Some(worktree_path) = dedicated_worktree_cleanup_path(thread, project_path) {
            if Path::new(worktree_path).is_dir() {
                if let Err(err) =
                    CheckpointService::cleanup_thread(Arc::clone(&state), &thread.id).await
                {
                    warn!(
                        thread_id = %thread.id,
                        error = %err,
                        "checkpoint cleanup failed during close, continuing"
                    );
                }
                remove_worktree(project_path, Some(worktree_path), &thread.source_type).await?;
            } else {
                tracing::info!(
                    thread_id = %thread.id,
                    worktree = %worktree_path,
                    "worktree missing, skipping git-dependent cleanup"
                );
            }
        }

        Ok(())
    }

    pub async fn hide(
        state: Arc<AppState>,
        params: protocol::ThreadHideParams,
    ) -> Result<protocol::ThreadHideResult, String> {
        let thread = {
            let store = state.store.lock().await;
            let thread = store
                .thread_by_id(&params.thread_id)
                .ok_or_else(|| format!("thread not found: {}", params.thread_id))?
                .clone();
            let project_path = store
                .project_by_id(&thread.project_id)
                .ok_or_else(|| format!("project not found: {}", thread.project_id))?
                .path
                .clone();
            (thread, project_path)
        };
        let (thread, project_path) = thread;
        if thread.status == protocol::ThreadStatus::Closed {
            return Err(format!("thread {} is closed", thread.id));
        }
        if let Some(worktree_path) = dedicated_worktree_cleanup_path(&thread, &project_path) {
            if Path::new(worktree_path).is_dir() {
                CheckpointService::cleanup_thread(Arc::clone(&state), &thread.id).await?;
            }
        }

        {
            let mut store = state.store.lock().await;
            Self::set_status_locked(
                &state,
                &mut store,
                &params.thread_id,
                protocol::ThreadStatus::Hidden,
            )?;
            store.save()?;
        }

        Ok(protocol::ThreadHideResult {
            status: protocol::ThreadStatus::Hidden,
        })
    }

    pub async fn reopen(
        state: Arc<AppState>,
        params: protocol::ThreadReopenParams,
    ) -> Result<protocol::Thread, String> {
        let (thread, project_path) = {
            let store = state.store.lock().await;
            let thread = store
                .thread_by_id(&params.thread_id)
                .ok_or_else(|| format!("thread not found: {}", params.thread_id))?
                .clone();
            let project = store
                .project_by_id(&thread.project_id)
                .ok_or_else(|| format!("project not found: {}", thread.project_id))?
                .clone();
            (thread, project.path)
        };

        if thread.status != protocol::ThreadStatus::Hidden {
            return Err(format!("thread {} is not hidden", thread.id));
        }
        let checkout_path = thread.checkout_path(&project_path);
        if !Path::new(checkout_path).exists() {
            return Err(format!(
                "thread checkout no longer exists: {}",
                checkout_path
            ));
        }

        let project = {
            let store = state.store.lock().await;
            store
                .project_by_id(&thread.project_id)
                .ok_or_else(|| format!("project not found: {}", thread.project_id))?
                .clone()
        };

        let config = load_threadmill_config(checkout_path, &project_path)?;
        let port_base = port_base_with_offset(config.ports.base, thread.port_offset)?;

        if tmux::session_exists(&thread.tmux_session).await? {
            let _ = tmux::kill_session(&thread.tmux_session).await;
        }

        let env = thread_env(&project, &thread, port_base);
        tmux::create_session(&thread.tmux_session, checkout_path, &env).await?;

        for (preset_name, preset) in &config.presets {
            if preset.autostart {
                let _ = PresetService::start(
                    Arc::clone(&state),
                    protocol::PresetStartParams {
                        thread_id: thread.id.clone(),
                        preset: preset_name.clone(),
                        session_id: None,
                    },
                )
                .await;
            }
        }

        let updated_thread = {
            let mut store = state.store.lock().await;
            Self::set_status_locked(
                &state,
                &mut store,
                &thread.id,
                protocol::ThreadStatus::Active,
            )?;
            store.save()?;
            store
                .thread_by_id(&thread.id)
                .ok_or_else(|| format!("thread not found after reopen: {}", thread.id))?
                .clone()
        };

        Ok(updated_thread.to_protocol())
    }

    pub async fn switch_to_worktree(
        state: Arc<AppState>,
        params: protocol::ThreadWorktreeMutationParams,
    ) -> Result<protocol::Thread, String> {
        let (thread, project) = {
            let store = state.store.lock().await;
            let thread = store
                .thread_by_id(&params.thread_id)
                .ok_or_else(|| format!("thread not found: {}", params.thread_id))?
                .clone();
            let project = store
                .project_by_id(&thread.project_id)
                .ok_or_else(|| format!("project not found: {}", thread.project_id))?
                .clone();
            (thread, project)
        };

        if matches!(
            thread.status,
            protocol::ThreadStatus::Closed | protocol::ThreadStatus::Failed
        ) {
            return Err(format!(
                "thread {} cannot switch to worktree from status {:?}",
                thread.id, thread.status
            ));
        }
        if thread.worktree_path.is_some() {
            return Err(format!(
                "thread {} already has a dedicated worktree",
                thread.id
            ));
        }

        let worktree_name = params
            .worktree_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(sanitize_name)
            .unwrap_or_else(|| thread.name.clone());
        let mut switched_thread = thread.clone();
        if switched_thread.branch == project.default_branch {
            switched_thread.branch = worktree_name.clone();
            switched_thread.source_type = protocol::SourceType::NewFeature;
        }
        let worktree_path =
            planned_worktree_path(&project, &worktree_name, &thread.source_type, true).ok_or_else(
                || format!("thread {} cannot resolve target worktree path", thread.id),
            )?;
        switched_thread.worktree_path = Some(worktree_path.clone());

        create_worktree(&project.path, &project.default_branch, &switched_thread).await?;

        let config = load_threadmill_config(&worktree_path, &project.path)?;
        for relative in &config.copy_from_main {
            copy_from_main(&project.path, &worktree_path, relative)?;
        }
        let port_base = port_base_with_offset(config.ports.base, switched_thread.port_offset)?;
        run_hooks(
            &config.setup,
            &worktree_path,
            &project.path,
            &switched_thread,
            port_base,
        )
        .await?;

        let (switched_thread, save_error) = {
            let mut store = state.store.lock().await;
            let thread = store
                .thread_by_id_mut(&params.thread_id)
                .ok_or_else(|| format!("thread not found: {}", params.thread_id))?;
            thread.worktree_path = Some(worktree_path.clone());
            thread.branch = switched_thread.branch.clone();
            thread.source_type = switched_thread.source_type.clone();
            let switched_thread = thread.clone();
            let save_error = store.save().err();
            (switched_thread, save_error)
        };
        emit_thread_upsert(&state, &switched_thread);

        if let Err(err) = Self::restart_tmux_session_if_active(
            Arc::clone(&state),
            &project,
            &switched_thread,
            &config,
        )
        .await
        {
            Self::mark_thread_failed_after_worktree_mutation(
                Arc::clone(&state),
                &params.thread_id,
                &err,
            )
            .await;
            return Err(format!(
                "thread switched to dedicated worktree but failed to restart session: {err}"
            ));
        }
        if let Some(err) = save_error {
            return Err(format!(
                "thread switched to dedicated worktree but failed to persist state: {err}"
            ));
        }

        Ok(switched_thread.to_protocol())
    }

    pub async fn switch_to_local_checkout(
        state: Arc<AppState>,
        params: protocol::ThreadWorktreeMutationParams,
    ) -> Result<protocol::Thread, String> {
        let (thread, project) = {
            let store = state.store.lock().await;
            let thread = store
                .thread_by_id(&params.thread_id)
                .ok_or_else(|| format!("thread not found: {}", params.thread_id))?
                .clone();
            let project = store
                .project_by_id(&thread.project_id)
                .ok_or_else(|| format!("project not found: {}", thread.project_id))?
                .clone();
            (thread, project)
        };

        if matches!(
            thread.status,
            protocol::ThreadStatus::Closed | protocol::ThreadStatus::Failed
        ) {
            return Err(format!(
                "thread {} cannot switch to local checkout from status {:?}",
                thread.id, thread.status
            ));
        }

        let worktree_path = thread
            .worktree_path
            .clone()
            .ok_or_else(|| format!("thread {} already uses base checkout", thread.id))?;
        ensure_local_checkout_switchable_worktree(&project, &thread, &worktree_path).await?;
        let local_branch = current_branch_name(&project.path).await?;
        if local_branch.is_empty() {
            return Err(format!(
                "cannot switch thread {} to local checkout: project root is not on a branch",
                thread.id
            ));
        }
        let base_config = load_threadmill_config(&project.path, &project.path)?;

        if Path::new(&worktree_path).is_dir() {
            let _ = CheckpointService::cleanup_thread(Arc::clone(&state), &thread.id).await;
        }
        if thread.status == protocol::ThreadStatus::Active
            && tmux::session_exists(&thread.tmux_session).await?
        {
            let _ = tmux::kill_session(&thread.tmux_session).await;
        }

        remove_worktree(&project.path, Some(&worktree_path), &thread.source_type).await?;

        let (switched_thread, save_error) = {
            let mut store = state.store.lock().await;
            let thread = store
                .thread_by_id_mut(&params.thread_id)
                .ok_or_else(|| format!("thread not found: {}", params.thread_id))?;
            thread.worktree_path = None;
            thread.branch = local_branch;
            let switched_thread = thread.clone();
            let save_error = store.save().err();
            (switched_thread, save_error)
        };
        emit_thread_upsert(&state, &switched_thread);

        if let Err(err) = Self::restart_tmux_session_if_active(
            Arc::clone(&state),
            &project,
            &switched_thread,
            &base_config,
        )
        .await
        {
            Self::mark_thread_failed_after_worktree_mutation(
                Arc::clone(&state),
                &params.thread_id,
                &err,
            )
            .await;
            return Err(format!(
                "thread switched to local checkout but failed to restart session: {err}"
            ));
        }
        if let Some(err) = save_error {
            return Err(format!(
                "thread switched to local checkout but failed to persist state: {err}"
            ));
        }

        Ok(switched_thread.to_protocol())
    }

    pub async fn switch_branch(
        state: Arc<AppState>,
        params: protocol::ThreadSwitchBranchParams,
    ) -> Result<protocol::Thread, String> {
        let branch = params.branch.trim();
        if branch.is_empty() {
            return Err("branch must not be empty".to_string());
        }

        let (thread, project) = {
            let store = state.store.lock().await;
            let thread = store
                .thread_by_id(&params.thread_id)
                .ok_or_else(|| format!("thread not found: {}", params.thread_id))?
                .clone();
            let project = store
                .project_by_id(&thread.project_id)
                .ok_or_else(|| format!("project not found: {}", thread.project_id))?
                .clone();
            (thread, project)
        };

        if matches!(
            thread.status,
            protocol::ThreadStatus::Closed | protocol::ThreadStatus::Failed
        ) {
            return Err(format!(
                "thread {} cannot switch branch from status {:?}",
                thread.id, thread.status
            ));
        }

        let checkout_path = thread.checkout_path(&project.path).to_string();
        checkout_branch(&checkout_path, branch).await?;
        let config = load_threadmill_config(&checkout_path, &project.path)?;

        let (updated_thread, save_error) = {
            let mut store = state.store.lock().await;
            let thread = store
                .thread_by_id_mut(&params.thread_id)
                .ok_or_else(|| format!("thread not found: {}", params.thread_id))?;
            thread.branch = branch.to_string();
            let updated_thread = thread.clone();
            let save_error = store.save().err();
            (updated_thread, save_error)
        };
        emit_thread_upsert(&state, &updated_thread);

        if let Err(err) = Self::restart_tmux_session_if_active(
            Arc::clone(&state),
            &project,
            &updated_thread,
            &config,
        )
        .await
        {
            Self::mark_thread_failed_after_worktree_mutation(
                Arc::clone(&state),
                &params.thread_id,
                &err,
            )
            .await;
            return Err(format!(
                "thread switched to branch {branch} but failed to restart session: {err}"
            ));
        }
        if let Some(err) = save_error {
            return Err(format!(
                "thread switched to branch {branch} but failed to persist state: {err}"
            ));
        }

        Ok(updated_thread.to_protocol())
    }

    async fn mark_thread_failed_after_worktree_mutation(
        state: Arc<AppState>,
        thread_id: &str,
        reason: &str,
    ) {
        warn!(thread_id = %thread_id, error = %reason, "worktree mutation left thread failed");
        let mut store = state.store.lock().await;
        if Self::set_status_locked(
            &state,
            &mut store,
            thread_id,
            protocol::ThreadStatus::Failed,
        )
        .is_ok()
        {
            let _ = store.save();
        }
    }

    async fn restart_tmux_session_if_active(
        state: Arc<AppState>,
        project: &crate::state_store::Project,
        thread: &Thread,
        config: &ThreadmillConfig,
    ) -> Result<(), String> {
        if thread.status != protocol::ThreadStatus::Active {
            return Ok(());
        }

        ChatService::stop_all_for_thread(
            Arc::clone(&state),
            &thread.id,
            "workspace_changed",
            false,
        )
        .await?;

        let checkout_path = thread.checkout_path(&project.path);
        let port_base = port_base_with_offset(config.ports.base, thread.port_offset)?;
        if tmux::session_exists(&thread.tmux_session).await? {
            let _ = tmux::kill_session(&thread.tmux_session).await;
        }

        let env = thread_env(project, thread, port_base);
        tmux::create_session(&thread.tmux_session, checkout_path, &env).await?;

        for (preset_name, preset) in &config.presets {
            if preset.autostart {
                let _ = PresetService::start(
                    Arc::clone(&state),
                    protocol::PresetStartParams {
                        thread_id: thread.id.clone(),
                        preset: preset_name.clone(),
                        session_id: None,
                    },
                )
                .await;
            }
        }

        Ok(())
    }

    async fn run_create_workflow(state: Arc<AppState>, thread_id: &str) -> Result<(), String> {
        let (thread, project) = {
            let store = state.store.lock().await;
            let thread = store
                .thread_by_id(thread_id)
                .ok_or_else(|| format!("thread not found: {thread_id}"))?
                .clone();
            let project = store
                .project_by_id(&thread.project_id)
                .ok_or_else(|| format!("project not found: {}", thread.project_id))?
                .clone();
            (thread, project)
        };

        let checkout_path = thread.checkout_path(&project.path).to_string();

        if thread.worktree_path.is_some()
            && thread.source_type != protocol::SourceType::MainCheckout
        {
            if has_origin_remote(&project.path).await? {
                Self::emit_progress(
                    &state,
                    protocol::ThreadProgress {
                        thread_id: thread.id.clone(),
                        step: protocol::ThreadProgressStep::Fetching,
                        message: Some("Fetching origin".to_string()),
                        error: None,
                    },
                );
                git(&project.path, &["fetch", "origin"]).await?;
            }

            Self::emit_progress(
                &state,
                protocol::ThreadProgress {
                    thread_id: thread.id.clone(),
                    step: protocol::ThreadProgressStep::CreatingWorktree,
                    message: Some("Creating git worktree".to_string()),
                    error: None,
                },
            );
            create_worktree(&project.path, &project.default_branch, &thread).await?;
        }

        let config = load_threadmill_config(&checkout_path, &project.path)?;

        Self::emit_progress(
            &state,
            protocol::ThreadProgress {
                thread_id: thread.id.clone(),
                step: protocol::ThreadProgressStep::CopyingFiles,
                message: Some("Copying configured files".to_string()),
                error: None,
            },
        );
        for relative in &config.copy_from_main {
            copy_from_main(&project.path, &checkout_path, relative)?;
        }

        Self::emit_progress(
            &state,
            protocol::ThreadProgress {
                thread_id: thread.id.clone(),
                step: protocol::ThreadProgressStep::RunningHooks,
                message: Some("Running setup hooks".to_string()),
                error: None,
            },
        );
        let port_base = port_base_with_offset(config.ports.base, thread.port_offset)?;
        run_hooks(
            &config.setup,
            &checkout_path,
            &project.path,
            &thread,
            port_base,
        )
        .await?;

        if tmux::session_exists(&thread.tmux_session).await? {
            let _ = tmux::kill_session(&thread.tmux_session).await;
        }

        let env = thread_env(&project, &thread, port_base);
        tmux::create_session(&thread.tmux_session, &checkout_path, &env).await?;

        Self::emit_progress(
            &state,
            protocol::ThreadProgress {
                thread_id: thread.id.clone(),
                step: protocol::ThreadProgressStep::StartingPresets,
                message: Some("Starting autostart presets".to_string()),
                error: None,
            },
        );
        for (preset_name, preset) in &config.presets {
            if preset.autostart {
                PresetService::start(
                    Arc::clone(&state),
                    protocol::PresetStartParams {
                        thread_id: thread.id.clone(),
                        preset: preset_name.clone(),
                        session_id: None,
                    },
                )
                .await?;
            }
        }

        {
            let mut store = state.store.lock().await;
            let status = store
                .thread_by_id(&thread.id)
                .ok_or_else(|| format!("thread not found: {}", thread.id))?
                .status
                .clone();
            if status != protocol::ThreadStatus::Creating {
                return Ok(());
            }

            Self::set_status_locked(
                &state,
                &mut store,
                &thread.id,
                protocol::ThreadStatus::Active,
            )?;
            store.save()?;
        }

        Self::emit_progress(
            &state,
            protocol::ThreadProgress {
                thread_id: thread.id,
                step: protocol::ThreadProgressStep::Ready,
                message: Some("Thread is ready".to_string()),
                error: None,
            },
        );
        Ok(())
    }

    async fn mark_failed(
        state: Arc<AppState>,
        thread_id: &str,
        reason: &str,
    ) -> Result<(), String> {
        let (thread, project_path) = {
            let mut store = state.store.lock().await;
            let thread = store
                .thread_by_id(thread_id)
                .ok_or_else(|| format!("thread not found: {thread_id}"))?
                .clone();
            let project_path = store
                .project_by_id(&thread.project_id)
                .ok_or_else(|| format!("project not found: {}", thread.project_id))?
                .path
                .clone();
            Self::set_status_locked(
                &state,
                &mut store,
                thread_id,
                protocol::ThreadStatus::Failed,
            )?;
            store.save()?;
            (thread, project_path)
        };

        Self::emit_progress(
            &state,
            protocol::ThreadProgress {
                thread_id: thread_id.to_string(),
                step: protocol::ThreadProgressStep::RunningHooks,
                message: Some("Thread creation failed".to_string()),
                error: Some(reason.to_string()),
            },
        );

        let _ = tmux::kill_session(&thread.tmux_session).await;
        let _ = remove_worktree(
            &project_path,
            thread.worktree_path.as_deref(),
            &thread.source_type,
        )
        .await;
        Ok(())
    }

    fn set_status_locked(
        state: &Arc<AppState>,
        store: &mut crate::state_store::StateStore,
        thread_id: &str,
        next: protocol::ThreadStatus,
    ) -> Result<(), String> {
        let thread = store
            .thread_by_id_mut(thread_id)
            .ok_or_else(|| format!("thread not found: {thread_id}"))?;
        let previous = thread.status.clone();
        thread.status = next.clone();

        state.emit_state_delta(vec![
            protocol::StateDeltaOperationPayload::ThreadStatusChanged {
                thread_id: thread_id.to_string(),
                old: previous,
                new: next,
            },
        ]);
        Ok(())
    }

    fn emit_progress(state: &Arc<AppState>, event: protocol::ThreadProgress) {
        state.emit_thread_progress(event);
    }
}

fn active_thread_name_exists(
    store: &crate::state_store::StateStore,
    project_id: &str,
    thread_name: &str,
) -> bool {
    store.data.threads.iter().any(|existing| {
        existing.project_id == project_id
            && existing.name == thread_name
            && existing.status != protocol::ThreadStatus::Closed
            && existing.status != protocol::ThreadStatus::Failed
    })
}

fn allocate_thread_name(
    store: &crate::state_store::StateStore,
    project_id: &str,
    requested_name: &str,
) -> Result<String, String> {
    if !active_thread_name_exists(store, project_id, requested_name) {
        return Ok(requested_name.to_string());
    }

    let Some(mut suffix) = new_thread_placeholder_suffix(requested_name) else {
        return Err(format!(
            "thread name already exists for project {}: {}",
            project_id, requested_name
        ));
    };

    loop {
        suffix += 1;
        let candidate = format!("new-thread-{suffix}");
        if !active_thread_name_exists(store, project_id, &candidate) {
            return Ok(candidate);
        }
    }
}

fn new_thread_placeholder_suffix(value: &str) -> Option<u64> {
    if value == "new-thread" {
        return Some(1);
    }

    let suffix = value.strip_prefix("new-thread-")?;
    if suffix.is_empty() || !suffix.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    suffix.parse().ok()
}

async fn ensure_shared_checkout_branch_for_create(
    project_path: &str,
    expected_branch: &str,
) -> Result<(), String> {
    let current_branch = current_branch_name(project_path).await?;
    if current_branch == expected_branch {
        return Ok(());
    }

    Err(format!(
        "sandbox=false requires shared checkout branch {} but project root is on {}; use sandbox/worktree mode",
        expected_branch, current_branch
    ))
}

fn emit_thread_upsert(state: &Arc<AppState>, thread: &Thread) {
    state.emit_state_delta(vec![protocol::StateDeltaOperationPayload::ThreadCreated {
        thread: thread.to_protocol(),
    }]);
}

fn planned_worktree_path(
    project: &crate::state_store::Project,
    thread_name: &str,
    source_type: &protocol::SourceType,
    sandbox: bool,
) -> Option<String> {
    if *source_type == protocol::SourceType::MainCheckout {
        return Some(project.path.clone());
    }

    sandbox.then(|| {
        crate::config::workspace_root()
            .join(".threadmill")
            .join(sanitize_name(&project.name))
            .join(thread_name)
            .to_string_lossy()
            .into_owned()
    })
}

fn thread_checkout_path<'a>(worktree_path: Option<&'a str>, project_path: &'a str) -> &'a str {
    worktree_path.unwrap_or(project_path)
}

fn dedicated_worktree_cleanup_path<'a>(
    thread: &'a Thread,
    project_path: &'a str,
) -> Option<&'a str> {
    let worktree_path = thread.worktree_path.as_deref()?;
    if thread.source_type == protocol::SourceType::MainCheckout || worktree_path == project_path {
        return None;
    }
    Some(worktree_path)
}

pub fn load_threadmill_config(
    worktree_path: &str,
    project_path: &str,
) -> Result<ThreadmillConfig, String> {
    let worktree_config = Path::new(worktree_path).join(".threadmill.yml");
    if worktree_config.exists() {
        let raw = fs::read_to_string(&worktree_config)
            .map_err(|err| format!("failed to read {}: {err}", worktree_config.display()))?;
        let config: ThreadmillConfig = serde_yaml::from_str(&raw)
            .map_err(|err| format!("failed to parse {}: {err}", worktree_config.display()))?;
        return finalize_threadmill_config(config);
    }

    let project_config = Path::new(project_path).join(".threadmill.yml");
    if !project_config.exists() {
        return Ok(default_threadmill_config());
    }

    let raw = fs::read_to_string(&project_config)
        .map_err(|err| format!("failed to read {}: {err}", project_config.display()))?;
    let config: ThreadmillConfig = serde_yaml::from_str(&raw)
        .map_err(|err| format!("failed to parse {}: {err}", project_config.display()))?;
    finalize_threadmill_config(config)
}

fn finalize_threadmill_config(config: ThreadmillConfig) -> Result<ThreadmillConfig, String> {
    let config = with_default_terminal_preset(config);
    if config.ports.offset == 0 {
        return Err("ports.offset must be greater than zero".to_string());
    }
    if config.checkpoints.max_count == 0 {
        return Err("checkpoints.max_count must be greater than zero".to_string());
    }

    Ok(config)
}

fn default_threadmill_config() -> ThreadmillConfig {
    let mut presets = HashMap::new();
    presets.insert(
        "terminal".to_string(),
        PresetDefinition {
            label: Some("Terminal".to_string()),
            commands: vec!["$SHELL".to_string()],
            parallel: false,
            autostart: true,
        },
    );

    ThreadmillConfig {
        setup: Vec::new(),
        teardown: Vec::new(),
        copy_from_main: Vec::new(),
        presets,
        agents: HashMap::new(),
        ports: PortsConfig::default(),
        checkpoints: CheckpointConfig::default(),
    }
}

fn with_default_terminal_preset(mut config: ThreadmillConfig) -> ThreadmillConfig {
    if !config.presets.contains_key("terminal") {
        config.presets.insert(
            "terminal".to_string(),
            PresetDefinition {
                label: Some("Terminal".to_string()),
                commands: vec!["$SHELL".to_string()],
                parallel: false,
                autostart: true,
            },
        );
    }

    config
}

async fn create_worktree(
    project_path: &str,
    default_branch: &str,
    thread: &Thread,
) -> Result<(), String> {
    let worktree_path = thread
        .worktree_path
        .as_deref()
        .ok_or_else(|| format!("thread {} has no worktree path", thread.id))?;

    if thread.source_type == protocol::SourceType::MainCheckout {
        if !Path::new(worktree_path).exists() {
            return Err(format!(
                "main checkout path does not exist: {}",
                worktree_path
            ));
        }
        return Ok(());
    }

    let worktree_parent = Path::new(worktree_path)
        .parent()
        .ok_or_else(|| format!("invalid worktree path: {}", worktree_path))?;
    fs::create_dir_all(worktree_parent)
        .map_err(|err| format!("failed to create {}: {err}", worktree_parent.display()))?;

    if Path::new(worktree_path).exists() {
        return Err(format!("worktree path already exists: {}", worktree_path));
    }

    match thread.source_type {
        protocol::SourceType::NewFeature => {
            if has_head_commit(project_path).await? {
                let local_ref = format!("refs/heads/{}", thread.branch);
                let has_local = git(project_path, &["show-ref", "--verify", &local_ref])
                    .await
                    .is_ok();
                if has_local {
                    git(
                        project_path,
                        &["worktree", "add", worktree_path, &thread.branch],
                    )
                    .await?;
                } else {
                    let base = format!("origin/{default_branch}");
                    let args = [
                        "worktree",
                        "add",
                        worktree_path,
                        "-b",
                        &thread.branch,
                        &base,
                    ];
                    if git(project_path, &args).await.is_err() {
                        let fallback = [
                            "worktree",
                            "add",
                            worktree_path,
                            "-b",
                            &thread.branch,
                            default_branch,
                        ];
                        git(project_path, &fallback).await?;
                    }
                }
            } else {
                git(
                    project_path,
                    &["worktree", "add", "-b", &thread.branch, worktree_path],
                )
                .await?;
            }
        }
        protocol::SourceType::ExistingBranch | protocol::SourceType::PullRequest => {
            let local_ref = format!("refs/heads/{}", thread.branch);
            let has_local = git(project_path, &["show-ref", "--verify", &local_ref])
                .await
                .is_ok();

            if has_local {
                git(
                    project_path,
                    &["worktree", "add", "--force", worktree_path, &thread.branch],
                )
                .await?;
            } else {
                let remote_branch = format!("origin/{}", thread.branch);
                git(
                    project_path,
                    &[
                        "worktree",
                        "add",
                        worktree_path,
                        "-b",
                        &thread.branch,
                        &remote_branch,
                    ],
                )
                .await?;
            }
        }
        protocol::SourceType::MainCheckout => {}
    }

    Ok(())
}

pub async fn remove_worktree(
    project_path: &str,
    worktree_path: Option<&str>,
    source_type: &protocol::SourceType,
) -> Result<(), String> {
    let Some(worktree_path) = worktree_path else {
        return Ok(());
    };

    if *source_type == protocol::SourceType::MainCheckout {
        return Ok(());
    }

    if project_path == worktree_path {
        return Ok(());
    }

    if !Path::new(worktree_path).exists() {
        return Ok(());
    }

    git(
        project_path,
        &["worktree", "remove", "--force", worktree_path],
    )
    .await
}

async fn ensure_local_checkout_switchable_worktree(
    project: &crate::state_store::Project,
    thread: &Thread,
    worktree_path: &str,
) -> Result<(), String> {
    if thread.source_type == protocol::SourceType::MainCheckout {
        return Err("cannot switch main checkout thread to local checkout".to_string());
    }
    let project_path = project.path.as_str();
    if project_path == worktree_path {
        return Err(format!(
            "cannot switch shared project path to local checkout: {}",
            worktree_path
        ));
    }
    let path = Path::new(worktree_path);
    if !path.exists() {
        return Err(format!("worktree does not exist: {}", worktree_path));
    }
    if !path.is_dir() {
        return Err(format!("worktree is not a directory: {}", worktree_path));
    }

    // WHY: `git worktree remove --force` deletes directory contents even if
    // persisted state points at wrong path. Require Threadmill-managed path,
    // git registration, and branch identity before removing so one thread can
    // never delete another thread's clean checkout.
    let expected_worktree_path =
        planned_worktree_path(project, &thread.name, &thread.source_type, true).ok_or_else(
            || {
                format!(
                    "thread {} cannot derive expected managed worktree path",
                    thread.id
                )
            },
        )?;
    if normalize_absolute_path(path) != normalize_absolute_path(Path::new(&expected_worktree_path))
    {
        return Err(format!(
            "worktree does not belong to thread {}; expected managed worktree path {} but found {}",
            thread.id, expected_worktree_path, worktree_path
        ));
    }
    if !worktree_registered(project_path, worktree_path).await? {
        return Err(format!(
            "worktree is not registered with git: {}",
            worktree_path
        ));
    }
    let worktree_branch = current_branch_name(worktree_path).await?;
    if worktree_branch != thread.branch {
        return Err(format!(
            "worktree branch does not match thread {}: expected {} but found {}",
            thread.id, thread.branch, worktree_branch
        ));
    }
    let dirtiness = worktree_dirtiness(worktree_path).await?;
    if dirtiness.has_ignored || dirtiness.has_changes {
        let reason = match (dirtiness.has_changes, dirtiness.has_ignored) {
            (true, true) => "tracked/untracked changes and ignored files",
            (true, false) => "tracked or untracked changes",
            (false, true) => "ignored files",
            (false, false) => unreachable!(),
        };
        return Err(format!(
            "worktree has {reason}; cannot switch to local checkout because git worktree remove --force would delete them: {}",
            worktree_path,
        ));
    }
    Ok(())
}

#[derive(Debug, Default, Clone, Copy)]
struct WorktreeDirtiness {
    has_changes: bool,
    has_ignored: bool,
}

fn normalize_absolute_path(path: &Path) -> PathBuf {
    path.components().collect()
}

async fn worktree_registered(project_path: &str, worktree_path: &str) -> Result<bool, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_path)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .await
        .map_err(|err| format!("failed to run git worktree list: {err}"))?;

    if !output.status.success() {
        return Err(format!(
            "git [\"worktree\", \"list\", \"--porcelain\"] failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let expected = fs::canonicalize(worktree_path)
        .map_err(|err| format!("failed to resolve {}: {err}", worktree_path))?;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some(candidate) = line.strip_prefix("worktree ") else {
            continue;
        };
        if let Ok(candidate_path) = fs::canonicalize(candidate) {
            if candidate_path == expected {
                return Ok(true);
            }
        }
    }

    Ok(false)
}

async fn current_branch_name(worktree_path: &str) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .await
        .map_err(|err| format!("failed to read git branch for {}: {err}", worktree_path))?;

    if !output.status.success() {
        return Err(format!(
            "git [\"rev-parse\", \"--abbrev-ref\", \"HEAD\"] failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn checkout_branch(worktree_path: &str, branch: &str) -> Result<(), String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["checkout", branch])
        .output()
        .await
        .map_err(|err| {
            format!(
                "failed to switch {} to branch {}: {err}",
                worktree_path, branch
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if let Some(message) = branch_already_checked_out_message(branch, &stderr) {
            return Err(message);
        }
        return Err(format!(
            "git [\"checkout\", \"{}\"] failed: {}",
            branch, stderr
        ));
    }

    Ok(())
}

async fn worktree_dirtiness(worktree_path: &str) -> Result<WorktreeDirtiness, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args([
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignored=matching",
        ])
        .output()
        .await
        .map_err(|err| format!("failed to run git status for {}: {err}", worktree_path))?;

    if !output.status.success() {
        return Err(format!(
            "git [\"status\", \"--porcelain=v1\", \"--untracked-files=all\", \"--ignored=matching\"] failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let mut dirtiness = WorktreeDirtiness::default();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("!!") {
            dirtiness.has_ignored = true;
        } else {
            dirtiness.has_changes = true;
        }
    }

    Ok(dirtiness)
}

fn copy_from_main(main_path: &str, worktree_path: &str, relative: &str) -> Result<(), String> {
    let source = Path::new(main_path).join(relative);
    if !source.exists() {
        return Err(format!(
            "copy_from_main path does not exist: {}",
            source.display()
        ));
    }

    let destination = Path::new(worktree_path).join(relative);
    if source == destination {
        return Ok(());
    }

    copy_path(&source, &destination)
}

fn copy_path(source: &Path, destination: &Path) -> Result<(), String> {
    if source.is_dir() {
        fs::create_dir_all(destination)
            .map_err(|err| format!("failed to create {}: {err}", destination.display()))?;

        for entry in fs::read_dir(source)
            .map_err(|err| format!("failed to read {}: {err}", source.display()))?
        {
            let entry = entry.map_err(|err| format!("failed to read directory entry: {err}"))?;
            copy_path(&entry.path(), &destination.join(entry.file_name()))?;
        }
        return Ok(());
    }

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }

    fs::copy(source, destination).map_err(|err| {
        format!(
            "failed to copy {} to {}: {err}",
            source.display(),
            destination.display()
        )
    })?;

    Ok(())
}

async fn run_hooks(
    commands: &[String],
    cwd: &str,
    project_path: &str,
    thread: &Thread,
    port_base: u16,
) -> Result<(), String> {
    if commands.is_empty() {
        return Ok(());
    }

    let project = crate::state_store::Project {
        id: thread.project_id.clone(),
        name: Path::new(project_path)
            .file_name()
            .and_then(|entry| entry.to_str())
            .unwrap_or("project")
            .to_string(),
        path: project_path.to_string(),
        default_branch: "main".to_string(),
    };

    let env = thread_env(&project, thread, port_base);

    for command in commands {
        let mut process = Command::new("bash");
        process.args(["-lc", command]).current_dir(cwd);
        for (key, value) in &env {
            process.env(key, value);
        }

        let output = process
            .output()
            .await
            .map_err(|err| format!("failed to execute hook {command}: {err}"))?;

        if !output.status.success() {
            return Err(format!(
                "hook failed {command}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }

    Ok(())
}

async fn has_origin_remote(project_path: &str) -> Result<bool, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_path)
        .args(["remote", "get-url", "origin"])
        .output()
        .await
        .map_err(|err| format!("failed to run git remote get-url origin: {err}"))?;

    if output.status.success() {
        return Ok(true);
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.contains("No such remote") {
        return Ok(false);
    }

    Err(format!(
        "git [\"remote\", \"get-url\", \"origin\"] failed: {}",
        stderr
    ))
}

async fn has_head_commit(project_path: &str) -> Result<bool, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_path)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .await
        .map_err(|err| format!("failed to run git rev-parse --verify HEAD: {err}"))?;

    Ok(output.status.success())
}

async fn git(project_path: &str, args: &[&str]) -> Result<(), String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_path)
        .args(args)
        .output()
        .await
        .map_err(|err| format!("failed to run git {:?}: {err}", args))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if let Some(branch) = git_target_branch(args) {
            if let Some(message) = branch_already_checked_out_message(branch, &stderr) {
                return Err(message);
            }
        }
        return Err(format!("git {:?} failed: {}", args, stderr));
    }

    Ok(())
}

fn git_target_branch<'a>(args: &'a [&str]) -> Option<&'a str> {
    if args.len() >= 2 && args[0] == "checkout" {
        return args.last().copied();
    }
    if args.len() >= 4 && args[0] == "worktree" && args[1] == "add" {
        if let Some(index) = args.iter().position(|arg| *arg == "-b") {
            return args.get(index + 1).copied();
        }
        return args.last().copied();
    }
    None
}

fn branch_already_checked_out_message(branch: &str, stderr: &str) -> Option<String> {
    if !stderr.contains("is already checked out at")
        && !stderr.contains("is already used by worktree at")
    {
        return None;
    }

    let location = stderr
        .rsplit_once(" at ")
        .map(|(_, path)| path.trim().trim_matches('\''))
        .filter(|path| !path.is_empty())
        .unwrap_or("another worktree");
    Some(format!(
        "Branch '{branch}' is already checked out in another worktree: {location}. Choose that existing thread/worktree, or use a different worktree name/branch."
    ))
}

fn resolve_branch(
    params: &protocol::ThreadCreateParams,
    thread_name: &str,
) -> Result<String, String> {
    if let Some(ref branch) = params.branch {
        return Ok(branch.clone());
    }

    if params.source_type == protocol::SourceType::PullRequest {
        if let Some(ref pr_url) = params.pr_url {
            return extract_branch_from_pr_url(pr_url);
        }
        return Err("pull_request source_type requires pr_url or branch".to_string());
    }

    Ok(thread_name.to_string())
}

fn requested_display_name(requested_name: &str, thread_name: &str) -> Option<String> {
    let trimmed = requested_name.trim();
    if trimmed.is_empty() || trimmed == thread_name {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn extract_branch_from_pr_url(pr_url: &str) -> Result<String, String> {
    // URL format: https://example.com/owner/repo/pull/123/head:<branch>
    if let Some(pos) = pr_url.rfind("head:") {
        let branch = &pr_url[pos + 5..];
        if branch.is_empty() {
            return Err(format!(
                "pr_url has empty branch after head: prefix: {pr_url}"
            ));
        }
        return Ok(branch.to_string());
    }
    Err(format!("could not resolve branch from pr_url: {pr_url}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{
        env,
        path::{Path, PathBuf},
        sync::OnceLock,
    };

    use chrono::Utc;
    use tokio::{process::Command, sync::Mutex};
    use uuid::Uuid;

    use crate::{state_store::StateStore, AppState, ServerEvent};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    struct TestProjectRepo {
        root_dir: PathBuf,
        repo_path: PathBuf,
    }

    #[tokio::test]
    async fn sandbox_disabled_create_returns_thread_without_worktree_path() {
        let _guard = env_lock().lock().await;
        let workspace_root = unique_temp_path("spindle-workspace-root");
        fs::create_dir_all(&workspace_root).expect("create workspace root");
        let previous_workspace_root = env::var_os("SPINDLE_WORKSPACE_ROOT");
        unsafe { env::set_var("SPINDLE_WORKSPACE_ROOT", &workspace_root) };

        let project = create_git_project().await;
        let project_id = "project-1".to_string();
        let state = Arc::new(app_state_with_project(&project_id, &project).await);

        let created = ThreadService::create(
            Arc::clone(&state),
            protocol::ThreadCreateParams {
                project_id: project_id.clone(),
                name: "Plain Thread".to_string(),
                source_type: protocol::SourceType::NewFeature,
                branch: None,
                pr_url: None,
                sandbox: false,
            },
        )
        .await
        .expect("create non-sandboxed thread");

        abort_create_task(&state, &created.id).await;

        assert_eq!(created.name, "plain-thread");
        assert_eq!(created.display_name.as_deref(), Some("Plain Thread"));
        assert_eq!(created.worktree_path, None);
        assert!(!expected_thread_worktree_path(
            &workspace_root,
            &project.repo_path,
            "plain-thread"
        )
        .exists());

        let persisted = fs::read_to_string(state_path(&state).await).expect("read persisted state");
        assert!(persisted.contains("\"worktree_path\": null"), "{persisted}");

        restore_workspace_root(previous_workspace_root);
        let _ = fs::remove_dir_all(project.root_dir);
        let _ = fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn create_auto_allocates_next_new_thread_placeholder_name() {
        let _guard = env_lock().lock().await;
        let project = create_git_project().await;
        let project_id = "project-1".to_string();
        let state = Arc::new(
            app_state_with_thread(
                &project_id,
                &project,
                crate::state_store::Thread::new(
                    "existing-thread".to_string(),
                    project_id.clone(),
                    "new-thread-3".to_string(),
                    "new-thread-3".to_string(),
                    Some(
                        project
                            .root_dir
                            .join("new-thread-3")
                            .to_string_lossy()
                            .to_string(),
                    ),
                    protocol::ThreadStatus::Active,
                    protocol::SourceType::NewFeature,
                    Utc::now(),
                    "tm_existing_thread".to_string(),
                    0,
                ),
            )
            .await,
        );

        let created = ThreadService::create(
            Arc::clone(&state),
            protocol::ThreadCreateParams {
                project_id: project_id.clone(),
                name: "new-thread-3".to_string(),
                source_type: protocol::SourceType::NewFeature,
                branch: None,
                pr_url: None,
                sandbox: true,
            },
        )
        .await
        .expect("create auto-named thread with colliding placeholder");

        abort_create_task(&state, &created.id).await;

        assert_eq!(created.name, "new-thread-4");
        assert_eq!(created.branch, "new-thread-4");
        assert!(created
            .worktree_path
            .as_deref()
            .is_some_and(|path| path.ends_with("new-thread-4")));

        let _ = fs::remove_dir_all(project.root_dir);
    }

    #[tokio::test]
    async fn switch_to_worktree_persists_and_emits_thread_delta() {
        let _guard = env_lock().lock().await;
        let workspace_root = unique_temp_path("spindle-workspace-root");
        fs::create_dir_all(&workspace_root).expect("create workspace root");
        let previous_workspace_root = env::var_os("SPINDLE_WORKSPACE_ROOT");
        unsafe { env::set_var("SPINDLE_WORKSPACE_ROOT", &workspace_root) };

        let project = create_git_project().await;
        let project_id = "project-1".to_string();
        let thread_id = "thread-1".to_string();
        let state = Arc::new(
            app_state_with_thread(
                &project_id,
                &project,
                crate::state_store::Thread::new(
                    thread_id.clone(),
                    project_id.clone(),
                    "switch-worktree".to_string(),
                    "switch-worktree".to_string(),
                    None,
                    protocol::ThreadStatus::Hidden,
                    protocol::SourceType::NewFeature,
                    Utc::now(),
                    "tm_switch_worktree".to_string(),
                    0,
                ),
            )
            .await,
        );
        let mut events = state.subscribe_events();

        let switched = ThreadService::switch_to_worktree(
            Arc::clone(&state),
            protocol::ThreadWorktreeMutationParams {
                thread_id: thread_id.clone(),
                worktree_name: None,
            },
        )
        .await
        .expect("switch thread to worktree");

        let switched_path = switched
            .worktree_path
            .clone()
            .expect("switched thread worktree path");
        assert!(Path::new(&switched_path).is_dir());

        let persisted = fs::read_to_string(state_path(&state).await).expect("read persisted state");
        assert!(persisted.contains(&format!("\"worktree_path\": \"{}\"", switched_path)));

        let event = next_state_delta_event(&mut events).await;
        assert_eq!(event.method, "state.delta");
        let operation = &event.params["operations"][0];
        assert_eq!(operation["type"], "thread.created");
        assert_eq!(operation["thread"]["id"], thread_id);
        assert_eq!(operation["thread"]["worktree_path"], switched_path);

        restore_workspace_root(previous_workspace_root);
        let _ = fs::remove_dir_all(project.root_dir);
        let _ = fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn switch_to_worktree_uses_custom_name_for_default_branch_thread() {
        let _guard = env_lock().lock().await;
        let workspace_root = unique_temp_path("spindle-workspace-root");
        fs::create_dir_all(&workspace_root).expect("create workspace root");
        let previous_workspace_root = env::var_os("SPINDLE_WORKSPACE_ROOT");
        unsafe { env::set_var("SPINDLE_WORKSPACE_ROOT", &workspace_root) };

        let project = create_git_project().await;
        let project_id = "project-1".to_string();
        let thread_id = "thread-1".to_string();
        let state = Arc::new(
            app_state_with_thread(
                &project_id,
                &project,
                crate::state_store::Thread::new(
                    thread_id.clone(),
                    project_id.clone(),
                    "new-thread".to_string(),
                    "main".to_string(),
                    None,
                    protocol::ThreadStatus::Hidden,
                    protocol::SourceType::NewFeature,
                    Utc::now(),
                    "tm_new_thread".to_string(),
                    0,
                ),
            )
            .await,
        );

        let switched = ThreadService::switch_to_worktree(
            Arc::clone(&state),
            protocol::ThreadWorktreeMutationParams {
                thread_id: thread_id.clone(),
                worktree_name: Some("smooth-acorn-4429".to_string()),
            },
        )
        .await
        .expect("switch default branch thread to named worktree");

        assert_eq!(switched.branch, "smooth-acorn-4429");
        assert!(switched
            .worktree_path
            .as_deref()
            .is_some_and(|path| path.ends_with("smooth-acorn-4429")));

        restore_workspace_root(previous_workspace_root);
        let _ = fs::remove_dir_all(project.root_dir);
        let _ = fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn create_worktree_from_unborn_repository_creates_orphan_branch() {
        let _guard = env_lock().lock().await;
        let project = create_unborn_git_project().await;
        let worktree_path = project.root_dir.join("feature-worktree");
        let thread = crate::state_store::Thread::new(
            "thread-1".to_string(),
            "project-1".to_string(),
            "feature-worktree".to_string(),
            "feature-worktree".to_string(),
            Some(worktree_path.to_string_lossy().to_string()),
            protocol::ThreadStatus::Creating,
            protocol::SourceType::NewFeature,
            Utc::now(),
            "tm_feature_worktree".to_string(),
            0,
        );

        create_worktree(&project.repo_path.to_string_lossy(), "main", &thread)
            .await
            .expect("create worktree from unborn repository");

        assert!(worktree_path.is_dir());
        let branch = git_raw(&["symbolic-ref", "--short", "HEAD"], Some(&worktree_path))
            .await
            .expect("read worktree branch");
        assert_eq!(branch.trim(), "feature-worktree");

        let _ = fs::remove_dir_all(project.root_dir);
    }

    #[tokio::test]
    async fn switch_to_local_checkout_persists_null_path_and_removes_worktree() {
        let _guard = env_lock().lock().await;
        let workspace_root = unique_temp_path("spindle-workspace-root");
        fs::create_dir_all(&workspace_root).expect("create workspace root");
        let previous_workspace_root = env::var_os("SPINDLE_WORKSPACE_ROOT");
        unsafe { env::set_var("SPINDLE_WORKSPACE_ROOT", &workspace_root) };

        let project = create_git_project().await;
        let project_id = "project-1".to_string();
        let thread_id = "thread-1".to_string();
        let worktree_path = expected_thread_worktree_path(
            &workspace_root,
            &project.repo_path,
            "local-checkout-switch",
        );
        git(
            &project.repo_path,
            &[
                "worktree",
                "add",
                "--force",
                worktree_path.to_str().unwrap(),
                "main",
            ],
        )
        .await
        .expect("create worktree for local checkout switch test");

        let state = Arc::new(
            app_state_with_thread(
                &project_id,
                &project,
                crate::state_store::Thread::new(
                    thread_id.clone(),
                    project_id.clone(),
                    "local-checkout-switch".to_string(),
                    "main".to_string(),
                    Some(worktree_path.to_string_lossy().to_string()),
                    protocol::ThreadStatus::Hidden,
                    protocol::SourceType::NewFeature,
                    Utc::now(),
                    "tm_local_checkout_switch".to_string(),
                    0,
                ),
            )
            .await,
        );
        let mut events = state.subscribe_events();

        let switched = ThreadService::switch_to_local_checkout(
            Arc::clone(&state),
            protocol::ThreadWorktreeMutationParams {
                thread_id: thread_id.clone(),
                worktree_name: None,
            },
        )
        .await
        .expect("switch thread to local checkout");

        assert_eq!(switched.worktree_path, None);
        assert!(!worktree_path.exists());

        let persisted = fs::read_to_string(state_path(&state).await).expect("read persisted state");
        assert!(persisted.contains("\"worktree_path\": null"), "{persisted}");

        let event = next_state_delta_event(&mut events).await;
        let operation = &event.params["operations"][0];
        assert_eq!(operation["type"], "thread.created");
        assert_eq!(operation["thread"]["id"], thread_id);
        assert!(operation["thread"]["worktree_path"].is_null());

        restore_workspace_root(previous_workspace_root);
        let _ = fs::remove_dir_all(project.root_dir);
        let _ = fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn switch_to_local_checkout_adopts_project_root_branch() {
        let _guard = env_lock().lock().await;
        let workspace_root = unique_temp_path("spindle-workspace-root");
        fs::create_dir_all(&workspace_root).expect("create workspace root");
        let previous_workspace_root = env::var_os("SPINDLE_WORKSPACE_ROOT");
        unsafe { env::set_var("SPINDLE_WORKSPACE_ROOT", &workspace_root) };

        let project = create_git_project().await;
        let project_id = "project-1".to_string();
        let thread_id = "thread-1".to_string();
        let worktree_path =
            expected_thread_worktree_path(&workspace_root, &project.repo_path, "feature-thread");
        git(
            &project.repo_path,
            &[
                "worktree",
                "add",
                worktree_path.to_str().unwrap(),
                "-b",
                "feature-thread",
                "main",
            ],
        )
        .await
        .expect("create feature worktree");

        let state = Arc::new(
            app_state_with_thread(
                &project_id,
                &project,
                crate::state_store::Thread::new(
                    thread_id.clone(),
                    project_id.clone(),
                    "feature-thread".to_string(),
                    "feature-thread".to_string(),
                    Some(worktree_path.to_string_lossy().to_string()),
                    protocol::ThreadStatus::Hidden,
                    protocol::SourceType::NewFeature,
                    Utc::now(),
                    "tm_feature_thread".to_string(),
                    0,
                ),
            )
            .await,
        );

        let switched = ThreadService::switch_to_local_checkout(
            Arc::clone(&state),
            protocol::ThreadWorktreeMutationParams {
                thread_id: thread_id.clone(),
                worktree_name: None,
            },
        )
        .await
        .expect("switch thread to local checkout");

        assert_eq!(switched.worktree_path, None);
        assert_eq!(switched.branch, "main");

        restore_workspace_root(previous_workspace_root);
        let _ = fs::remove_dir_all(project.root_dir);
        let _ = fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn switch_branch_updates_local_checkout_thread() {
        let project = create_git_project().await;
        git(&project.repo_path, &["branch", "feature-local"])
            .await
            .expect("create feature branch");
        let project_id = "project-1".to_string();
        let thread_id = "thread-1".to_string();
        let state = Arc::new(
            app_state_with_thread(
                &project_id,
                &project,
                crate::state_store::Thread::new(
                    thread_id.clone(),
                    project_id.clone(),
                    "local-thread".to_string(),
                    "main".to_string(),
                    None,
                    protocol::ThreadStatus::Hidden,
                    protocol::SourceType::NewFeature,
                    Utc::now(),
                    "tm_local_thread".to_string(),
                    0,
                ),
            )
            .await,
        );

        let switched = ThreadService::switch_branch(
            Arc::clone(&state),
            protocol::ThreadSwitchBranchParams {
                thread_id: thread_id.clone(),
                branch: "feature-local".to_string(),
            },
        )
        .await
        .expect("switch local checkout branch");

        let current_branch = git_raw(&["branch", "--show-current"], Some(&project.repo_path))
            .await
            .expect("read current branch");
        assert_eq!(current_branch.trim(), "feature-local");
        assert_eq!(switched.worktree_path, None);
        assert_eq!(switched.branch, "feature-local");

        let _ = fs::remove_dir_all(project.root_dir);
    }

    #[tokio::test]
    async fn switch_branch_updates_dedicated_worktree_thread() {
        let _guard = env_lock().lock().await;
        let workspace_root = unique_temp_path("spindle-workspace-root");
        fs::create_dir_all(&workspace_root).expect("create workspace root");
        let previous_workspace_root = env::var_os("SPINDLE_WORKSPACE_ROOT");
        unsafe { env::set_var("SPINDLE_WORKSPACE_ROOT", &workspace_root) };

        let project = create_git_project().await;
        git(&project.repo_path, &["branch", "feature-target"])
            .await
            .expect("create target branch");
        let project_id = "project-1".to_string();
        let thread_id = "thread-1".to_string();
        let worktree_path =
            expected_thread_worktree_path(&workspace_root, &project.repo_path, "feature-thread");
        git(
            &project.repo_path,
            &[
                "worktree",
                "add",
                worktree_path.to_str().unwrap(),
                "-b",
                "feature-thread",
                "main",
            ],
        )
        .await
        .expect("create feature worktree");
        let state = Arc::new(
            app_state_with_thread(
                &project_id,
                &project,
                crate::state_store::Thread::new(
                    thread_id.clone(),
                    project_id.clone(),
                    "feature-thread".to_string(),
                    "feature-thread".to_string(),
                    Some(worktree_path.to_string_lossy().to_string()),
                    protocol::ThreadStatus::Hidden,
                    protocol::SourceType::NewFeature,
                    Utc::now(),
                    "tm_feature_thread".to_string(),
                    0,
                ),
            )
            .await,
        );

        let switched = ThreadService::switch_branch(
            Arc::clone(&state),
            protocol::ThreadSwitchBranchParams {
                thread_id: thread_id.clone(),
                branch: "feature-target".to_string(),
            },
        )
        .await
        .expect("switch worktree branch");

        let worktree_branch = git_raw(&["branch", "--show-current"], Some(&worktree_path))
            .await
            .expect("read worktree branch");
        let project_branch = git_raw(&["branch", "--show-current"], Some(&project.repo_path))
            .await
            .expect("read project branch");
        assert_eq!(worktree_branch.trim(), "feature-target");
        assert_eq!(project_branch.trim(), "main");
        assert_eq!(switched.branch, "feature-target");
        assert_eq!(switched.worktree_path.as_deref(), worktree_path.to_str());

        restore_workspace_root(previous_workspace_root);
        let _ = fs::remove_dir_all(project.root_dir);
        let _ = fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn switch_branch_reports_branch_checked_out_elsewhere_without_raw_git_command() {
        let project = create_git_project().await;
        let other_worktree = project.root_dir.join("busy-worktree");
        git(
            &project.repo_path,
            &[
                "worktree",
                "add",
                other_worktree.to_str().unwrap(),
                "-b",
                "busy-branch",
                "main",
            ],
        )
        .await
        .expect("create other worktree");
        let project_id = "project-1".to_string();
        let state = Arc::new(
            app_state_with_thread(
                &project_id,
                &project,
                crate::state_store::Thread::new(
                    "thread-1".to_string(),
                    project_id.clone(),
                    "local-thread".to_string(),
                    "main".to_string(),
                    None,
                    protocol::ThreadStatus::Hidden,
                    protocol::SourceType::NewFeature,
                    Utc::now(),
                    "tm_local_thread".to_string(),
                    0,
                ),
            )
            .await,
        );

        let error = ThreadService::switch_branch(
            Arc::clone(&state),
            protocol::ThreadSwitchBranchParams {
                thread_id: "thread-1".to_string(),
                branch: "busy-branch".to_string(),
            },
        )
        .await
        .expect_err("branch checked out elsewhere should be explained");

        assert!(
            error.contains("already checked out in another worktree"),
            "{error}"
        );
        assert!(!error.contains("git ["), "{error}");

        let _ = fs::remove_dir_all(project.root_dir);
    }

    #[tokio::test]
    async fn switch_to_local_checkout_rejects_shared_project_path() {
        let project = create_git_project().await;
        let project_id = "project-1".to_string();
        let state = Arc::new(
            app_state_with_thread(
                &project_id,
                &project,
                crate::state_store::Thread::new(
                    "thread-1".to_string(),
                    project_id.clone(),
                    "base-thread".to_string(),
                    "base-thread".to_string(),
                    Some(project.repo_path.to_string_lossy().to_string()),
                    protocol::ThreadStatus::Hidden,
                    protocol::SourceType::NewFeature,
                    Utc::now(),
                    "tm_base_thread".to_string(),
                    0,
                ),
            )
            .await,
        );

        let error = ThreadService::switch_to_local_checkout(
            Arc::clone(&state),
            protocol::ThreadWorktreeMutationParams {
                thread_id: "thread-1".to_string(),
                worktree_name: None,
            },
        )
        .await
        .expect_err("shared project checkout should be rejected");

        assert!(
            error.contains("shared") || error.contains("project path"),
            "{error}"
        );

        let _ = fs::remove_dir_all(project.root_dir);
    }

    #[tokio::test]
    async fn switch_to_local_checkout_rejects_registered_worktree_owned_by_another_thread() {
        let _guard = env_lock().lock().await;
        let workspace_root = unique_temp_path("spindle-workspace-root");
        fs::create_dir_all(&workspace_root).expect("create workspace root");
        let previous_workspace_root = env::var_os("SPINDLE_WORKSPACE_ROOT");
        unsafe { env::set_var("SPINDLE_WORKSPACE_ROOT", &workspace_root) };

        let project = create_git_project().await;
        let project_id = "project-1".to_string();
        let owner_worktree_path =
            expected_thread_worktree_path(&workspace_root, &project.repo_path, "owner-thread");
        git(
            &project.repo_path,
            &[
                "worktree",
                "add",
                owner_worktree_path.to_str().unwrap(),
                "-b",
                "owner-thread",
                "main",
            ],
        )
        .await
        .expect("create owner worktree");

        let state = Arc::new(
            app_state_with_thread(
                &project_id,
                &project,
                crate::state_store::Thread::new(
                    "thread-1".to_string(),
                    project_id.clone(),
                    "intruder-thread".to_string(),
                    "intruder-thread".to_string(),
                    Some(owner_worktree_path.to_string_lossy().to_string()),
                    protocol::ThreadStatus::Hidden,
                    protocol::SourceType::NewFeature,
                    Utc::now(),
                    "tm_intruder_thread".to_string(),
                    0,
                ),
            )
            .await,
        );

        let error = ThreadService::switch_to_local_checkout(
            Arc::clone(&state),
            protocol::ThreadWorktreeMutationParams {
                thread_id: "thread-1".to_string(),
                worktree_name: None,
            },
        )
        .await
        .expect_err("worktree ownership mismatch should be rejected");

        assert!(
            error.contains("expected managed worktree path") || error.contains("does not belong"),
            "{error}"
        );
        assert!(owner_worktree_path.exists());

        restore_workspace_root(previous_workspace_root);
        let _ = fs::remove_dir_all(project.root_dir);
        let _ = fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn switch_to_local_checkout_rejects_ignored_files_in_worktree() {
        let _guard = env_lock().lock().await;
        let workspace_root = unique_temp_path("spindle-workspace-root");
        fs::create_dir_all(&workspace_root).expect("create workspace root");
        let previous_workspace_root = env::var_os("SPINDLE_WORKSPACE_ROOT");
        unsafe { env::set_var("SPINDLE_WORKSPACE_ROOT", &workspace_root) };

        let project = create_git_project().await;
        fs::write(project.repo_path.join(".gitignore"), "ignored.tmp\n").expect("write .gitignore");
        git(&project.repo_path, &["add", ".gitignore"])
            .await
            .expect("git add .gitignore");
        git(&project.repo_path, &["commit", "-m", "add ignore rule"])
            .await
            .expect("commit .gitignore");

        let project_id = "project-1".to_string();
        let worktree_path =
            expected_thread_worktree_path(&workspace_root, &project.repo_path, "ignored-thread");
        git(
            &project.repo_path,
            &[
                "worktree",
                "add",
                worktree_path.to_str().unwrap(),
                "-b",
                "ignored-thread",
                "main",
            ],
        )
        .await
        .expect("create ignored-file worktree");
        fs::write(worktree_path.join("ignored.tmp"), "do not delete me\n")
            .expect("write ignored file");

        let state = Arc::new(
            app_state_with_thread(
                &project_id,
                &project,
                crate::state_store::Thread::new(
                    "thread-1".to_string(),
                    project_id.clone(),
                    "ignored-thread".to_string(),
                    "ignored-thread".to_string(),
                    Some(worktree_path.to_string_lossy().to_string()),
                    protocol::ThreadStatus::Hidden,
                    protocol::SourceType::NewFeature,
                    Utc::now(),
                    "tm_ignored_thread".to_string(),
                    0,
                ),
            )
            .await,
        );

        let error = ThreadService::switch_to_local_checkout(
            Arc::clone(&state),
            protocol::ThreadWorktreeMutationParams {
                thread_id: "thread-1".to_string(),
                worktree_name: None,
            },
        )
        .await
        .expect_err("ignored files should block local checkout switch");

        assert!(error.contains("ignored"), "{error}");
        assert!(worktree_path.exists());

        restore_workspace_root(previous_workspace_root);
        let _ = fs::remove_dir_all(project.root_dir);
        let _ = fs::remove_dir_all(workspace_root);
    }

    #[test]
    fn dedicated_worktree_cleanup_path_skips_threads_without_worktrees() {
        let thread = crate::state_store::Thread::new(
            "thread-1".to_string(),
            "project-1".to_string(),
            "plain-thread".to_string(),
            "plain-thread".to_string(),
            None,
            protocol::ThreadStatus::Active,
            protocol::SourceType::NewFeature,
            Utc::now(),
            "tm_plain_thread".to_string(),
            0,
        );

        assert_eq!(
            dedicated_worktree_cleanup_path(&thread, "/tmp/project"),
            None
        );
    }

    fn env_lock() -> &'static Mutex<()> {
        ENV_LOCK.get_or_init(|| Mutex::new(()))
    }

    fn unique_temp_path(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            Uuid::new_v4().simple()
        ))
    }

    async fn create_git_project() -> TestProjectRepo {
        let root_dir = unique_temp_path("spindle-thread-service");
        let origin_path = root_dir.join("origin.git");
        let repo_path = root_dir.join("repo");
        fs::create_dir_all(&root_dir).expect("create root dir");

        git_raw(&["init", "--bare", origin_path.to_str().unwrap()], None)
            .await
            .expect("init bare origin");
        git_raw(
            &[
                "clone",
                origin_path.to_str().unwrap(),
                repo_path.to_str().unwrap(),
            ],
            None,
        )
        .await
        .expect("clone origin");

        git(&repo_path, &["config", "user.name", "Spindle Test"])
            .await
            .expect("set git user name");
        git(
            &repo_path,
            &["config", "user.email", "spindle-test@example.com"],
        )
        .await
        .expect("set git user email");
        git(&repo_path, &["config", "commit.gpgsign", "false"])
            .await
            .expect("disable gpgsign");
        git(&repo_path, &["checkout", "-b", "main"])
            .await
            .expect("create main branch");

        fs::write(repo_path.join("README.md"), "thread service test\n").expect("write README");
        git(&repo_path, &["add", "README.md"])
            .await
            .expect("git add README");
        git(&repo_path, &["commit", "-m", "initial commit"])
            .await
            .expect("initial commit");
        git(&repo_path, &["push", "-u", "origin", "main"])
            .await
            .expect("push main");

        TestProjectRepo {
            root_dir,
            repo_path,
        }
    }

    async fn create_unborn_git_project() -> TestProjectRepo {
        let root_dir = unique_temp_path("spindle-thread-service");
        let repo_path = root_dir.join("repo");
        fs::create_dir_all(&repo_path).expect("create repo dir");

        git_raw(&["init", repo_path.to_str().unwrap()], None)
            .await
            .expect("init repo");
        git(&repo_path, &["checkout", "-b", "main"])
            .await
            .expect("create main branch");

        TestProjectRepo {
            root_dir,
            repo_path,
        }
    }

    async fn app_state_with_project(project_id: &str, project: &TestProjectRepo) -> AppState {
        let state_path = unique_temp_path("spindle-state")
            .join("threadmill")
            .join("threads.json");
        let store = StateStore {
            path: state_path,
            data: crate::state_store::AppData {
                projects: vec![crate::state_store::Project {
                    id: project_id.to_string(),
                    name: project
                        .repo_path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .to_string(),
                    path: project.repo_path.to_string_lossy().to_string(),
                    default_branch: "main".to_string(),
                }],
                threads: vec![],
            },
        };
        store.save().expect("save initial state");
        AppState::new(store)
    }

    async fn app_state_with_thread(
        project_id: &str,
        project: &TestProjectRepo,
        thread: crate::state_store::Thread,
    ) -> AppState {
        let state_path = unique_temp_path("spindle-state")
            .join("threadmill")
            .join("threads.json");
        let store = StateStore {
            path: state_path,
            data: crate::state_store::AppData {
                projects: vec![crate::state_store::Project {
                    id: project_id.to_string(),
                    name: project
                        .repo_path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .to_string(),
                    path: project.repo_path.to_string_lossy().to_string(),
                    default_branch: "main".to_string(),
                }],
                threads: vec![thread],
            },
        };
        store.save().expect("save initial state");
        AppState::new(store)
    }

    async fn abort_create_task(state: &Arc<AppState>, thread_id: &str) {
        if let Some(handle) = state.create_tasks.lock().await.remove(thread_id) {
            handle.abort();
        }
    }

    fn expected_thread_worktree_path(
        workspace_root: &Path,
        project_path: &Path,
        thread_name: &str,
    ) -> PathBuf {
        workspace_root
            .join(".threadmill")
            .join(project_path.file_name().unwrap())
            .join(thread_name)
    }

    async fn state_path(state: &Arc<AppState>) -> PathBuf {
        state.store.lock().await.path.clone()
    }

    async fn next_state_delta_event(
        events: &mut tokio::sync::broadcast::Receiver<ServerEvent>,
    ) -> ServerEvent {
        loop {
            let event = events.recv().await.expect("receive server event");
            if event.method == "state.delta" {
                return event;
            }
        }
    }

    async fn git(repo_path: &Path, args: &[&str]) -> Result<(), String> {
        git_raw(args, Some(repo_path)).await.map(|_| ())
    }

    async fn git_raw(args: &[&str], cwd: Option<&Path>) -> Result<String, String> {
        let mut command = Command::new("git");
        command.args(args);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let output = command
            .output()
            .await
            .map_err(|err| format!("failed to run git {:?}: {err}", args))?;
        if !output.status.success() {
            return Err(format!(
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn restore_workspace_root(previous: Option<std::ffi::OsString>) {
        unsafe {
            match previous {
                Some(value) => env::set_var("SPINDLE_WORKSPACE_ROOT", value),
                None => env::remove_var("SPINDLE_WORKSPACE_ROOT"),
            }
        }
    }
}
