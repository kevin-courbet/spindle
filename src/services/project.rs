use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use serde::Deserialize;
use tokio::{io::AsyncReadExt, process::Command};
use tracing::warn;
use uuid::Uuid;

use crate::{protocol, state_store::Project, AppState};

pub struct ProjectService;

impl ProjectService {
    pub async fn add(
        state: Arc<AppState>,
        params: protocol::ProjectAddParams,
    ) -> Result<protocol::Project, String> {
        let path = PathBuf::from(&params.path);
        if !path.is_absolute() {
            return Err("project path must be absolute".to_string());
        }
        if !path.exists() {
            return Err("path does not exist".to_string());
        }

        let is_git_repo = ensure_git_repo(&path).await.is_ok();

        let canonical = std::fs::canonicalize(&path)
            .map_err(|err| format!("failed to canonicalize {}: {err}", path.display()))?;
        let canonical_str = canonical
            .to_str()
            .ok_or_else(|| format!("invalid utf-8 path: {}", canonical.display()))?
            .to_string();
        let name = canonical
            .file_name()
            .and_then(|entry| entry.to_str())
            .ok_or_else(|| {
                format!(
                    "unable to resolve project name from {}",
                    canonical.display()
                )
            })?
            .to_string();
        let default_branch = if is_git_repo {
            detect_default_branch(&canonical_str).await?
        } else {
            "main".to_string()
        };

        let (project, is_new) = {
            let mut store = state.store.lock().await;
            if let Some(existing) = store
                .data
                .projects
                .iter()
                .find(|project| project.path == canonical_str)
            {
                (existing.clone(), false)
            } else {
                let project = Project {
                    id: Uuid::new_v4().to_string(),
                    name,
                    path: canonical_str,
                    default_branch,
                };
                store.data.projects.push(project.clone());
                store.save()?;
                (project, true)
            }
        };

        let protocol_project = project.to_protocol()?;

        if is_new {
            state.emit_project_added(protocol::ProjectAddedEvent {
                project: protocol_project.clone(),
            });
            state.emit_state_delta(vec![protocol::StateDeltaOperationPayload::ProjectAdded {
                project: protocol_project.clone(),
            }]);
        }

        Ok(protocol_project)
    }

    pub async fn clone(
        state: Arc<AppState>,
        params: protocol::ProjectCloneParams,
    ) -> Result<protocol::Project, String> {
        let url = params.url.trim().to_string();
        if url.starts_with("-") {
            return Err("clone url must not start with '-'".to_string());
        }

        let destination = resolve_clone_destination(&url, params.path.as_deref())?;
        if destination.exists() {
            return Err(format!(
                "destination already exists: {}",
                destination.display()
            ));
        }

        let parent = destination
            .parent()
            .ok_or_else(|| format!("invalid clone destination: {}", destination.display()))?;
        if !parent.exists() {
            return Err(format!(
                "destination parent does not exist: {}",
                parent.display()
            ));
        }

        let destination_str = destination
            .to_str()
            .ok_or_else(|| format!("invalid utf-8 path: {}", destination.display()))?
            .to_string();

        let clone_id = Uuid::new_v4().to_string();
        emit_clone_progress(
            &state,
            &clone_id,
            protocol::ThreadProgressStep::Fetching,
            Some(format!("Cloning {}", url)),
            None,
        );

        if let Err(err) =
            run_git_clone_with_progress(&state, &clone_id, &url, &destination_str).await
        {
            emit_clone_progress(
                &state,
                &clone_id,
                protocol::ThreadProgressStep::Fetching,
                Some("Clone failed".to_string()),
                Some(err.clone()),
            );
            return Err(err);
        }

        emit_clone_progress(
            &state,
            &clone_id,
            protocol::ThreadProgressStep::Ready,
            Some(format!("Clone complete: {}", destination_str)),
            None,
        );

        Self::add(
            state,
            protocol::ProjectAddParams {
                path: destination_str,
            },
        )
        .await
    }

    pub async fn list(state: Arc<AppState>) -> Result<Vec<protocol::Project>, String> {
        let projects = {
            let store = state.store.lock().await;
            store.data.projects.clone()
        };

        projects
            .into_iter()
            .map(|project| project.to_protocol())
            .collect()
    }

    pub async fn remove(
        state: Arc<AppState>,
        params: protocol::ProjectRemoveParams,
    ) -> Result<protocol::ProjectRemoveResult, String> {
        let removed = {
            let mut store = state.store.lock().await;
            let before = store.data.projects.len();
            store
                .data
                .projects
                .retain(|project| project.id != params.project_id);
            let changed = store.data.projects.len() != before;
            if changed {
                store.save()?;
            }
            changed
        };

        if removed {
            state.emit_project_removed(protocol::ProjectRemovedEvent {
                project_id: params.project_id.clone(),
            });
            state.emit_state_delta(vec![protocol::StateDeltaOperationPayload::ProjectRemoved {
                project_id: params.project_id,
            }]);
        }

        Ok(protocol::ProjectRemoveResult {
            removed: Some(removed),
        })
    }

    pub async fn branches(
        state: Arc<AppState>,
        params: protocol::ProjectBranchesParams,
    ) -> Result<Vec<String>, String> {
        let project_path = {
            let store = state.store.lock().await;
            store
                .project_by_id(&params.project_id)
                .ok_or_else(|| format!("project not found: {}", params.project_id))?
                .path
                .clone()
        };

        let output = git_command(&project_path, &["branch", "-r", "--list", "origin/*"]).await?;

        let branches = output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .filter_map(|line| {
                if line.contains("->") {
                    None
                } else {
                    Some(line.trim_start_matches("origin/").to_string())
                }
            })
            .collect();

        Ok(branches)
    }

    pub async fn browse(
        params: protocol::ProjectBrowseParams,
    ) -> Result<Vec<protocol::DirectoryEntry>, String> {
        let path = PathBuf::from(&params.path);
        if !path.is_absolute() {
            return Err("browse path must be absolute".to_string());
        }

        let mut entries = Vec::new();
        let read_dir = std::fs::read_dir(&path)
            .map_err(|err| format!("failed to read {}: {err}", path.display()))?;

        for entry in read_dir {
            let entry = entry.map_err(|err| format!("failed to read directory entry: {err}"))?;
            let file_type = entry
                .file_type()
                .map_err(|err| format!("failed to inspect {}: {err}", entry.path().display()))?;
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = file_type.is_dir();
            let is_git_repo = if is_dir {
                Some(is_git_repo_dir(&entry.path()))
            } else {
                None
            };

            entries.push(protocol::DirectoryEntry {
                name,
                is_dir,
                is_git_repo,
            });
        }

        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(entries)
    }

    pub async fn lookup(
        state: Arc<AppState>,
        params: protocol::ProjectLookupParams,
    ) -> Result<protocol::ProjectLookupResult, String> {
        let path = PathBuf::from(&params.path);
        if !path.is_absolute() {
            return Err("lookup path must be absolute".to_string());
        }

        let exists = path.exists();
        let is_git_repo = if exists && path.is_dir() {
            is_git_repo_dir(&path)
        } else {
            false
        };

        let project_id = if exists {
            let canonical = std::fs::canonicalize(&path).ok();
            let canonical_str = canonical.as_ref().and_then(|candidate| candidate.to_str());
            if let Some(canonical_str) = canonical_str {
                let store = state.store.lock().await;
                store
                    .data
                    .projects
                    .iter()
                    .find(|project| project.path == canonical_str)
                    .map(|project| project.id.clone())
            } else {
                None
            }
        } else {
            None
        };

        Ok(protocol::ProjectLookupResult {
            exists,
            is_git_repo,
            project_id,
        })
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
struct ProjectConfigFile {
    #[serde(default)]
    presets: BTreeMap<String, ProjectPresetFile>,
    #[serde(default)]
    agents: BTreeMap<String, ProjectAgentFile>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct ProjectPresetFile {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    commands: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct ProjectAgentFile {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

pub fn load_project_presets(project_path: &str) -> Result<Vec<protocol::PresetConfig>, String> {
    let config_path = Path::new(project_path).join(".threadmill.yml");
    if !config_path.exists() {
        return Ok(default_presets());
    }

    let raw = fs::read_to_string(&config_path)
        .map_err(|err| format!("failed to read {}: {err}", config_path.display()))?;
    let parsed: ProjectConfigFile = match serde_yaml::from_str(&raw) {
        Ok(parsed) => parsed,
        Err(err) => {
            warn!(
                project_path = %project_path,
                config_path = %config_path.display(),
                error = %err,
                "failed to parse project presets; using defaults"
            );
            return Ok(default_presets());
        }
    };

    let mut presets = Vec::with_capacity(parsed.presets.len());
    for (name, preset) in parsed.presets {
        let command = preset
            .command
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| {
                if preset.commands.is_empty() {
                    None
                } else {
                    Some(preset.commands.join(" && "))
                }
            });

        let Some(command) = command else {
            warn!(
                project_path = %project_path,
                config_path = %config_path.display(),
                preset_name = %name,
                "invalid preset config missing command; using defaults"
            );
            return Ok(default_presets());
        };

        let cwd = preset
            .cwd
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);

        presets.push(protocol::PresetConfig { name, command, cwd });
    }

    Ok(presets)
}

pub fn resolve_preset_cwd(worktree_path: &str, cwd: Option<&str>) -> Result<String, String> {
    let Some(cwd) = cwd else {
        return Ok(worktree_path.to_string());
    };

    let cwd_path = Path::new(cwd);
    if cwd_path.is_absolute() {
        return Err(format!("preset cwd must be relative to worktree: {cwd}"));
    }

    let worktree_root = fs::canonicalize(worktree_path)
        .map_err(|err| format!("failed to canonicalize worktree {}: {err}", worktree_path))?;
    let joined = worktree_root.join(cwd_path);
    let resolved = fs::canonicalize(&joined)
        .map_err(|err| format!("failed to resolve preset cwd {}: {err}", joined.display()))?;

    if !resolved.starts_with(&worktree_root) {
        return Err(format!("preset cwd escapes worktree: {cwd}"));
    }

    resolved
        .to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("invalid utf-8 preset cwd: {}", resolved.display()))
}

pub fn default_agents() -> Vec<protocol::AgentConfig> {
    vec![protocol::AgentConfig {
        name: "opencode".to_string(),
        command: "opencode acp".to_string(),
        cwd: None,
    }]
}

pub fn default_presets() -> Vec<protocol::PresetConfig> {
    vec![
        protocol::PresetConfig {
            name: "terminal".to_string(),
            command: env_var_or_default("SHELL", "bash"),
            cwd: None,
        },
        protocol::PresetConfig {
            name: "opencode".to_string(),
            command: "opencode attach http://127.0.0.1:4101 --dir $THREADMILL_WORKTREE".to_string(),
            cwd: None,
        },
    ]
}

fn env_var_or_default(name: &str, fallback: &str) -> String {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => value,
        _ => fallback.to_string(),
    }
}

fn resolve_clone_destination(url: &str, requested_path: Option<&str>) -> Result<PathBuf, String> {
    let repo_name = repo_name_from_url(url)?;

    match requested_path {
        Some(path) => {
            let trimmed = path.trim();
            if trimmed.is_empty() {
                return Err("clone path must not be empty".to_string());
            }

            let destination = PathBuf::from(trimmed);
            if !destination.is_absolute() {
                return Err("clone path must be absolute".to_string());
            }

            if trimmed.ends_with('/') || destination.is_dir() {
                return Ok(destination.join(repo_name));
            }

            Ok(destination)
        }
        None => Ok(PathBuf::from("/home/wsl/dev").join(repo_name)),
    }
}

fn repo_name_from_url(url: &str) -> Result<String, String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err("invalid clone url".to_string());
    }
    if !trimmed.contains("://") && !trimmed.contains('/') && !trimmed.contains(':') {
        return Err("invalid clone url".to_string());
    }

    let normalized = trimmed
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .trim_end_matches('/');

    if normalized.is_empty() {
        return Err("invalid clone url".to_string());
    }

    let tail = normalized.rsplit('/').next().unwrap_or(normalized);
    let tail = tail.rsplit(':').next().unwrap_or(tail);
    let repo_name = tail.trim_end_matches(".git").trim();

    if repo_name.is_empty() || repo_name == "." || repo_name == ".." {
        return Err("invalid clone url".to_string());
    }

    Ok(repo_name.to_string())
}

fn emit_clone_progress(
    state: &Arc<AppState>,
    clone_id: &str,
    step: protocol::ThreadProgressStep,
    message: Option<String>,
    error: Option<String>,
) {
    state.emit_event(
        "project.clone_progress",
        protocol::ThreadProgress {
            thread_id: clone_id.to_string(),
            step,
            message,
            error,
        },
    );
}

async fn run_git_clone_with_progress(
    state: &Arc<AppState>,
    clone_id: &str,
    url: &str,
    destination: &str,
) -> Result<(), String> {
    let mut child = Command::new("git")
        .arg("clone")
        .arg("--progress")
        .arg("--")
        .arg(url)
        .arg(destination)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to run git clone: {err}"))?;

    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "failed to capture git clone stderr".to_string())?;

    let mut last_message: Option<String> = None;
    let mut buffer = [0_u8; 4096];
    let mut pending = String::new();

    loop {
        let read = stderr
            .read(&mut buffer)
            .await
            .map_err(|err| format!("failed to read git clone progress: {err}"))?;
        if read == 0 {
            break;
        }

        let chunk = String::from_utf8_lossy(&buffer[..read]);
        for ch in chunk.chars() {
            if ch == '\r' || ch == '\n' {
                let message = pending.trim();
                if !message.is_empty() {
                    let message = message.to_string();
                    last_message = Some(message.clone());
                    emit_clone_progress(
                        state,
                        clone_id,
                        protocol::ThreadProgressStep::Fetching,
                        Some(message),
                        None,
                    );
                }
                pending.clear();
            } else {
                pending.push(ch);
            }
        }
    }

    let trailing = pending.trim();
    if !trailing.is_empty() {
        let message = trailing.to_string();
        last_message = Some(message.clone());
        emit_clone_progress(
            state,
            clone_id,
            protocol::ThreadProgressStep::Fetching,
            Some(message),
            None,
        );
    }

    let status = child
        .wait()
        .await
        .map_err(|err| format!("failed to wait for git clone: {err}"))?;

    if !status.success() {
        return Err(format!(
            "git clone failed: {}",
            last_message.unwrap_or_else(|| "unknown git clone error".to_string())
        ));
    }

    Ok(())
}

async fn ensure_git_repo(path: &Path) -> Result<(), String> {
    let path_str = path
        .to_str()
        .ok_or_else(|| format!("invalid utf-8 path: {}", path.display()))?;

    let output = Command::new("git")
        .args([
            "-C",
            path_str,
            "rev-parse",
            "--is-inside-work-tree",
            "--is-bare-repository",
        ])
        .output()
        .await
        .map_err(|err| format!("failed to run git rev-parse: {err}"))?;

    if !output.status.success() {
        return Err("not a git repository".to_string());
    }

    let output_text = String::from_utf8_lossy(&output.stdout);
    let mut lines = output_text.lines();
    let inside_work_tree = lines.next() == Some("true");
    let is_bare = lines.next() == Some("true");

    if inside_work_tree || is_bare {
        return Ok(());
    }

    Err("not a git repository".to_string())
}

async fn detect_default_branch(path: &str) -> Result<String, String> {
    let remote_head = Command::new("git")
        .args([
            "-C",
            path,
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "--short",
        ])
        .output()
        .await
        .map_err(|err| format!("failed to run git symbolic-ref: {err}"))?;

    if remote_head.status.success() {
        let value = String::from_utf8_lossy(&remote_head.stdout)
            .trim()
            .trim_start_matches("origin/")
            .to_string();
        if !value.is_empty() {
            return Ok(value);
        }
    }

    let head = Command::new("git")
        .args(["-C", path, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .await
        .map_err(|err| format!("failed to run git rev-parse --abbrev-ref HEAD: {err}"))?;

    if !head.status.success() {
        return Err(format!(
            "failed to detect default branch: {}",
            String::from_utf8_lossy(&head.stderr).trim()
        ));
    }

    let value = String::from_utf8_lossy(&head.stdout).trim().to_string();
    if value.is_empty() || value == "HEAD" {
        return Err("failed to detect default branch".to_string());
    }

    Ok(value)
}

async fn git_command(path: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
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

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn is_git_repo_dir(path: &Path) -> bool {
    if path.join(".git").is_dir() {
        return true;
    }
    if path.join(".git").is_file() {
        return true;
    }

    std::fs::metadata(path.join("HEAD")).is_ok() && std::fs::metadata(path.join("objects")).is_ok()
}

pub fn load_project_agents(project_path: &str) -> Result<Vec<protocol::AgentConfig>, String> {
    let config_path = Path::new(project_path).join(".threadmill.yml");
    if !config_path.exists() {
        return Ok(default_agents());
    }

    let raw = fs::read_to_string(&config_path)
        .map_err(|err| format!("failed to read {}: {err}", config_path.display()))?;
    let parsed: ProjectConfigFile = match serde_yaml::from_str(&raw) {
        Ok(parsed) => parsed,
        Err(err) => {
            warn!(
                project_path = %project_path,
                config_path = %config_path.display(),
                error = %err,
                "failed to parse project agents; using none"
            );
            return Ok(Vec::new());
        }
    };

    let mut agents = Vec::with_capacity(parsed.agents.len());
    for (name, agent) in parsed.agents {
        let command = agent
            .command
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);

        let Some(command) = command else {
            warn!(
                project_path = %project_path,
                config_path = %config_path.display(),
                agent_name = %name,
                "invalid agent config missing command; skipping"
            );
            continue;
        };

        let cwd = agent
            .cwd
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);

        agents.push(protocol::AgentConfig { name, command, cwd });
    }

    if agents.is_empty() {
        return Ok(default_agents());
    }

    Ok(agents)
}
