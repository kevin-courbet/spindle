use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::process::Command;
use uuid::Uuid;

use crate::{
    protocol,
    state::Project,
    AppState,
};

pub struct ProjectService;

impl ProjectService {
    pub async fn add(state: Arc<AppState>, params: protocol::ProjectAddParams) -> Result<protocol::Project, String> {
        let path = PathBuf::from(&params.path);
        if !path.is_absolute() {
            return Err("project path must be absolute".to_string());
        }
        if !path.exists() {
            return Err(format!("project path does not exist: {}", path.display()));
        }

        ensure_git_repo(&params.path).await?;

        let canonical = std::fs::canonicalize(&path)
            .map_err(|err| format!("failed to canonicalize {}: {err}", path.display()))?;
        let canonical_str = canonical
            .to_str()
            .ok_or_else(|| format!("invalid utf-8 path: {}", canonical.display()))?
            .to_string();
        let name = canonical
            .file_name()
            .and_then(|entry| entry.to_str())
            .ok_or_else(|| format!("unable to resolve project name from {}", canonical.display()))?
            .to_string();
        let default_branch = detect_default_branch(&canonical_str).await?;

        let project = {
            let mut store = state.store.lock().await;
            if let Some(existing) = store
                .data
                .projects
                .iter()
                .find(|project| project.path == canonical_str)
            {
                return Ok(existing.to_protocol());
            }

            let project = Project {
                id: Uuid::new_v4().to_string(),
                name,
                path: canonical_str,
                default_branch,
            };
            store.data.projects.push(project.clone());
            store.save()?;
            project
        };

        Ok(project.to_protocol())
    }

    pub async fn list(state: Arc<AppState>) -> Result<Vec<protocol::Project>, String> {
        let store = state.store.lock().await;
        Ok(store
            .data
            .projects
            .iter()
            .map(Project::to_protocol)
            .collect())
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

        let output = git_command(
            &project_path,
            &["branch", "-r", "--list", "origin/*"],
        )
        .await?;

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

    pub async fn browse(params: protocol::ProjectBrowseParams) -> Result<Vec<protocol::DirectoryEntry>, String> {
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
}

async fn ensure_git_repo(path: &str) -> Result<(), String> {
    let output = Command::new("git")
        .args(["-C", path, "rev-parse", "--is-inside-work-tree"])
        .output()
        .await
        .map_err(|err| format!("failed to run git rev-parse: {err}"))?;

    if !output.status.success() {
        return Err(format!(
            "path is not a git repository: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value != "true" {
        return Err(format!("path is not a git repository: {path}"));
    }

    Ok(())
}

async fn detect_default_branch(path: &str) -> Result<String, String> {
    let remote_head = Command::new("git")
        .args(["-C", path, "symbolic-ref", "refs/remotes/origin/HEAD", "--short"])
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
