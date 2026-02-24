use std::{
    collections::HashMap,
    fs,
    path::Path,
    sync::Arc,
};

use chrono::Utc;
use serde::Deserialize;
use tokio::process::Command;
use tracing::error;
use uuid::Uuid;

use crate::{
    protocol,
    services::{preset::PresetService, sanitize_name, short_id},
    state_store::{thread_env, Thread},
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

impl ThreadService {
    pub async fn create(state: Arc<AppState>, params: protocol::ThreadCreateParams) -> Result<protocol::Thread, String> {
        let (thread, protocol_thread) = {
            let mut store = state.store.lock().await;
            let project = store
                .project_by_id(&params.project_id)
                .ok_or_else(|| format!("project not found: {}", params.project_id))?
                .clone();

            let thread_name = sanitize_name(&params.name);
            let branch = params.branch.clone().unwrap_or_else(|| thread_name.clone());
            let worktree_path = format!(
                "/home/wsl/dev/.threadmill/{}/{}",
                sanitize_name(&project.name),
                thread_name
            );
            let tmux_session = format!("tm_{}_{}", short_id(&project.id), sanitize_name(&params.name));

            let thread = Thread {
                id: Uuid::new_v4().to_string(),
                project_id: project.id.clone(),
                name: thread_name,
                branch,
                worktree_path,
                status: protocol::ThreadStatus::Creating,
                source_type: params.source_type.clone(),
                created_at: Utc::now(),
                tmux_session,
            };

            let protocol_thread = thread.to_protocol();
            store.data.threads.push(thread.clone());
            store.save()?;
            (thread, protocol_thread)
        };

        state.emit_thread_created(protocol::ThreadCreatedEvent {
            thread: protocol_thread.clone(),
        });
        state.emit_state_delta(vec![protocol::StateDeltaChange::ThreadCreated {
            thread: protocol_thread.clone(),
        }]);

        let thread_id = thread.id.clone();
        let state_for_task = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(err) = Self::run_create_workflow(state_for_task.clone(), &thread_id).await {
                error!(thread_id = %thread_id, error = %err, "thread.create workflow failed");
                let _ = Self::mark_failed(state_for_task, &thread_id, &err).await;
            }
        });

        Ok(protocol_thread)
    }

    pub async fn list(state: Arc<AppState>, params: protocol::ThreadListParams) -> Result<Vec<protocol::Thread>, String> {
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

            Self::set_status_locked(&state, &mut store, &params.thread_id, protocol::ThreadStatus::Closing)?;
            (thread, project_path)
        };

        if tmux::session_exists(&thread.tmux_session).await? {
            let _ = tmux::kill_session(&thread.tmux_session).await;
        }

        let config = load_threadmill_config(&thread.worktree_path, &project_path)?;
        run_hooks(&config.teardown, &thread.worktree_path, &project_path, &thread).await?;
        remove_worktree(&project_path, &thread.worktree_path).await?;

        {
            let mut store = state.store.lock().await;
            Self::set_status_locked(&state, &mut store, &params.thread_id, protocol::ThreadStatus::Closed)?;
            store.save()?;
        }

        Ok(protocol::ThreadCloseResult {
            status: Some(protocol::ThreadStatus::Closed),
        })
    }

    pub async fn hide(
        state: Arc<AppState>,
        params: protocol::ThreadHideParams,
    ) -> Result<protocol::ThreadHideResult, String> {
        {
            let mut store = state.store.lock().await;
            let thread = store
                .thread_by_id(&params.thread_id)
                .ok_or_else(|| format!("thread not found: {}", params.thread_id))?
                .clone();

            if thread.status == protocol::ThreadStatus::Closed {
                return Err(format!("thread {} is closed", thread.id));
            }

            Self::set_status_locked(&state, &mut store, &params.thread_id, protocol::ThreadStatus::Hidden)?;
            store.save()?;
        }

        Ok(protocol::ThreadHideResult {
            status: protocol::ThreadStatus::Hidden,
        })
    }

    pub async fn reopen(state: Arc<AppState>, params: protocol::ThreadReopenParams) -> Result<protocol::Thread, String> {
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
            return Err(format!("worktree no longer exists: {}", thread.worktree_path));
        }

        let project = {
            let store = state.store.lock().await;
            store
                .project_by_id(&thread.project_id)
                .ok_or_else(|| format!("project not found: {}", thread.project_id))?
                .clone()
        };

        if tmux::session_exists(&thread.tmux_session).await? {
            let _ = tmux::kill_session(&thread.tmux_session).await;
        }

        let env = thread_env(&project, &thread);
        tmux::create_session(&thread.tmux_session, &thread.worktree_path, &env).await?;

        let config = load_threadmill_config(&thread.worktree_path, &project_path)?;
        for (preset_name, preset) in &config.presets {
            if preset.autostart {
                let _ = PresetService::start(
                    Arc::clone(&state),
                    protocol::PresetStartParams {
                        thread_id: thread.id.clone(),
                        preset: preset_name.clone(),
                    },
                )
                .await;
            }
        }

        let updated_thread = {
            let mut store = state.store.lock().await;
            Self::set_status_locked(&state, &mut store, &thread.id, protocol::ThreadStatus::Active)?;
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
        run_hooks(&config.setup, &thread.worktree_path, &project.path, &thread).await?;

        if tmux::session_exists(&thread.tmux_session).await? {
            let _ = tmux::kill_session(&thread.tmux_session).await;
        }

        let env = thread_env(&project, &thread);
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
                    },
                )
                .await?;
            }
        }

        {
            let mut store = state.store.lock().await;
            Self::set_status_locked(&state, &mut store, &thread.id, protocol::ThreadStatus::Active)?;
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

    async fn mark_failed(state: Arc<AppState>, thread_id: &str, reason: &str) -> Result<(), String> {
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
            Self::set_status_locked(&state, &mut store, thread_id, protocol::ThreadStatus::Failed)?;
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
        let _ = remove_worktree(&project_path, &thread.worktree_path).await;
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

        state.emit_thread_status_changed(protocol::ThreadStatusChanged {
            thread_id: thread_id.to_string(),
            old: previous.clone(),
            new: next.clone(),
        });
        state.emit_state_delta(vec![protocol::StateDeltaChange::ThreadStatusChanged {
            thread_id: thread_id.to_string(),
            old: previous,
            new: next,
        }]);
        Ok(())
    }

    fn emit_progress(state: &Arc<AppState>, event: protocol::ThreadProgress) {
        state.emit_thread_progress(event);
    }
}

pub fn load_threadmill_config(worktree_path: &str, project_path: &str) -> Result<ThreadmillConfig, String> {
    let worktree_config = Path::new(worktree_path).join(".threadmill.yml");
    if worktree_config.exists() {
        let raw = fs::read_to_string(&worktree_config)
            .map_err(|err| format!("failed to read {}: {err}", worktree_config.display()))?;
        let config: ThreadmillConfig = serde_yaml::from_str(&raw)
            .map_err(|err| format!("failed to parse {}: {err}", worktree_config.display()))?;
        return Ok(with_default_terminal_preset(config));
    }

    let project_config = Path::new(project_path).join(".threadmill.yml");
    if !project_config.exists() {
        return Ok(default_threadmill_config());
    }

    let raw = fs::read_to_string(&project_config)
        .map_err(|err| format!("failed to read {}: {err}", project_config.display()))?;
    let config: ThreadmillConfig = serde_yaml::from_str(&raw)
        .map_err(|err| format!("failed to parse {}: {err}", project_config.display()))?;
    Ok(with_default_terminal_preset(config))
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
    }
}

fn with_default_terminal_preset(mut config: ThreadmillConfig) -> ThreadmillConfig {
    if config.presets.is_empty() {
        return default_threadmill_config();
    }

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

async fn create_worktree(project_path: &str, default_branch: &str, thread: &Thread) -> Result<(), String> {
    let worktree_parent = Path::new(&thread.worktree_path)
        .parent()
        .ok_or_else(|| format!("invalid worktree path: {}", thread.worktree_path))?;
    fs::create_dir_all(worktree_parent)
        .map_err(|err| format!("failed to create {}: {err}", worktree_parent.display()))?;

    if Path::new(&thread.worktree_path).exists() {
        return Ok(());
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
    }

    Ok(())
}

async fn remove_worktree(project_path: &str, worktree_path: &str) -> Result<(), String> {
    if !Path::new(worktree_path).exists() {
        return Ok(());
    }

    git(project_path, &["worktree", "remove", "--force", worktree_path]).await
}

fn copy_from_main(main_path: &str, worktree_path: &str, relative: &str) -> Result<(), String> {
    let source = Path::new(main_path).join(relative);
    if !source.exists() {
        return Err(format!("copy_from_main path does not exist: {}", source.display()));
    }

    let destination = Path::new(worktree_path).join(relative);
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

async fn run_hooks(commands: &[String], cwd: &str, project_path: &str, thread: &Thread) -> Result<(), String> {
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

    let env = thread_env(&project, thread);

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
