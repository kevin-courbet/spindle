use std::{collections::HashMap, fs, path::Path, sync::Arc};

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
        let (thread, protocol_thread) = {
            let mut store = state.store.lock().await;
            let project = store
                .project_by_id(&params.project_id)
                .ok_or_else(|| format!("project not found: {}", params.project_id))?
                .clone();

            let thread_name = sanitize_name(&params.name);
            let branch = resolve_branch(&params, &thread_name)?;
            let worktree_path = match &params.source_type {
                protocol::SourceType::MainCheckout => project.path.clone(),
                _ => crate::config::workspace_root()
                    .join(".threadmill")
                    .join(sanitize_name(&project.name))
                    .join(&thread_name)
                    .to_string_lossy()
                    .into_owned(),
            };
            if store.data.threads.iter().any(|existing| {
                existing.project_id == project.id
                    && existing.name == thread_name
                    && existing.status != protocol::ThreadStatus::Closed
                    && existing.status != protocol::ThreadStatus::Failed
            }) {
                return Err(format!(
                    "thread name already exists for project {}: {}",
                    project.id, thread_name
                ));
            }
            let tmux_session = format!(
                "tm_{}_{}",
                short_id(&project.id),
                sanitize_name(&params.name)
            );
            let config = load_threadmill_config(&worktree_path, &project.path)?;
            let port_offset = store.allocate_port_offset(&project.id, config.ports.offset)?;

            let thread = Thread::new(
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

        // Worktree-dependent cleanup: checkpoint refs, config hooks, and
        // worktree removal all need the directory to exist. If it's already
        // gone (e.g. manually deleted or stale from a prior failed run), skip
        // gracefully instead of failing the close.
        let worktree_exists = Path::new(&thread.worktree_path).is_dir();
        if !worktree_exists {
            tracing::info!(
                thread_id = %thread.id,
                worktree = %thread.worktree_path,
                "worktree missing, skipping git-dependent cleanup"
            );
            return Ok(());
        }

        if let Err(err) = CheckpointService::cleanup_thread(Arc::clone(&state), &thread.id).await {
            warn!(
                thread_id = %thread.id,
                error = %err,
                "checkpoint cleanup failed during close, continuing"
            );
        }

        let config = load_threadmill_config(&thread.worktree_path, project_path)?;
        let port_base = port_base_with_offset(config.ports.base, thread.port_offset)?;
        run_hooks(
            &config.teardown,
            &thread.worktree_path,
            project_path,
            thread,
            port_base,
        )
        .await?;
        remove_worktree(project_path, &thread.worktree_path, &thread.source_type).await?;

        Ok(())
    }

    pub async fn hide(
        state: Arc<AppState>,
        params: protocol::ThreadHideParams,
    ) -> Result<protocol::ThreadHideResult, String> {
        let thread = {
            let store = state.store.lock().await;
            store
                .thread_by_id(&params.thread_id)
                .ok_or_else(|| format!("thread not found: {}", params.thread_id))?
                .clone()
        };
        if thread.status == protocol::ThreadStatus::Closed {
            return Err(format!("thread {} is closed", thread.id));
        }
        if Path::new(&thread.worktree_path).is_dir() {
            CheckpointService::cleanup_thread(Arc::clone(&state), &thread.id).await?;
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
        if !Path::new(&thread.worktree_path).exists() {
            return Err(format!(
                "worktree no longer exists: {}",
                thread.worktree_path
            ));
        }

        let project = {
            let store = state.store.lock().await;
            store
                .project_by_id(&thread.project_id)
                .ok_or_else(|| format!("project not found: {}", thread.project_id))?
                .clone()
        };

        let config = load_threadmill_config(&thread.worktree_path, &project_path)?;
        let port_base = port_base_with_offset(config.ports.base, thread.port_offset)?;

        if tmux::session_exists(&thread.tmux_session).await? {
            let _ = tmux::kill_session(&thread.tmux_session).await;
        }

        let env = thread_env(&project, &thread, port_base);
        tmux::create_session(&thread.tmux_session, &thread.worktree_path, &env).await?;

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

        if thread.source_type != protocol::SourceType::MainCheckout {
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

        let config = load_threadmill_config(&thread.worktree_path, &project.path)?;

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
            copy_from_main(&project.path, &thread.worktree_path, relative)?;
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
            &thread.worktree_path,
            &project.path,
            &thread,
            port_base,
        )
        .await?;

        if tmux::session_exists(&thread.tmux_session).await? {
            let _ = tmux::kill_session(&thread.tmux_session).await;
        }

        let env = thread_env(&project, &thread, port_base);
        tmux::create_session(&thread.tmux_session, &thread.worktree_path, &env).await?;

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
        let _ = remove_worktree(&project_path, &thread.worktree_path, &thread.source_type).await;
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
    if thread.source_type == protocol::SourceType::MainCheckout {
        if !Path::new(&thread.worktree_path).exists() {
            return Err(format!(
                "main checkout path does not exist: {}",
                thread.worktree_path
            ));
        }
        return Ok(());
    }

    let worktree_parent = Path::new(&thread.worktree_path)
        .parent()
        .ok_or_else(|| format!("invalid worktree path: {}", thread.worktree_path))?;
    fs::create_dir_all(worktree_parent)
        .map_err(|err| format!("failed to create {}: {err}", worktree_parent.display()))?;

    if Path::new(&thread.worktree_path).exists() {
        return Err(format!(
            "worktree path already exists: {}",
            thread.worktree_path
        ));
    }

    match thread.source_type {
        protocol::SourceType::NewFeature => {
            let base = format!("origin/{default_branch}");
            let args = [
                "worktree",
                "add",
                &thread.worktree_path,
                "-b",
                &thread.branch,
                &base,
            ];
            if git(project_path, &args).await.is_err() {
                let fallback = [
                    "worktree",
                    "add",
                    &thread.worktree_path,
                    "-b",
                    &thread.branch,
                    default_branch,
                ];
                git(project_path, &fallback).await?;
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
                    &["worktree", "add", &thread.worktree_path, &thread.branch],
                )
                .await?;
            } else {
                let remote_branch = format!("origin/{}", thread.branch);
                git(
                    project_path,
                    &[
                        "worktree",
                        "add",
                        &thread.worktree_path,
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
    worktree_path: &str,
    source_type: &protocol::SourceType,
) -> Result<(), String> {
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

async fn git(project_path: &str, args: &[&str]) -> Result<(), String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_path)
        .args(args)
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

    Ok(())
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
