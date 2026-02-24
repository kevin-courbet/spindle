use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{protocol, tmux};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppData {
    #[serde(default)]
    pub projects: Vec<Project>,
    #[serde(default)]
    pub threads: Vec<Thread>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: String,
    pub default_branch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thread {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub branch: String,
    pub worktree_path: String,
    pub status: protocol::ThreadStatus,
    pub source_type: protocol::SourceType,
    pub created_at: DateTime<Utc>,
    pub tmux_session: String,
}

impl Project {
    pub fn to_protocol(&self) -> protocol::Project {
        protocol::Project {
            id: self.id.clone(),
            name: self.name.clone(),
            path: self.path.clone(),
            default_branch: self.default_branch.clone(),
        }
    }
}

impl Thread {
    pub fn to_protocol(&self) -> protocol::Thread {
        protocol::Thread {
            id: self.id.clone(),
            project_id: self.project_id.clone(),
            name: self.name.clone(),
            branch: self.branch.clone(),
            worktree_path: self.worktree_path.clone(),
            status: self.status.clone(),
            source_type: self.source_type.clone(),
            created_at: self.created_at.to_rfc3339(),
            tmux_session: self.tmux_session.clone(),
        }
    }
}

pub struct StateStore {
    pub path: PathBuf,
    pub data: AppData,
}

impl StateStore {
    pub fn load() -> Result<Self, String> {
        let config_dir = dirs::config_dir().ok_or_else(|| "unable to locate config dir".to_string())?;
        let dir = config_dir.join("threadmill");
        fs::create_dir_all(&dir).map_err(|err| format!("failed to create {}: {err}", dir.display()))?;

        let path = dir.join("threads.json");
        if !path.exists() {
            let store = Self {
                path,
                data: AppData::default(),
            };
            store.save()?;
            return Ok(store);
        }

        let raw = fs::read_to_string(&path)
            .map_err(|err| format!("failed to read {}: {err}", path.display()))?;

        let data: AppData = if raw.trim().is_empty() {
            AppData::default()
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
            .ok_or_else(|| format!("invalid state path {}", self.path.display()))?;
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;

        let serialized = serde_json::to_vec_pretty(&self.data)
            .map_err(|err| format!("failed to serialize state: {err}"))?;

        let tmp_path = self.path.with_extension("json.tmp");
        fs::write(&tmp_path, serialized)
            .map_err(|err| format!("failed to write {}: {err}", tmp_path.display()))?;
        fs::rename(&tmp_path, &self.path)
            .map_err(|err| format!("failed to move {} to {}: {err}", tmp_path.display(), self.path.display()))?;

        Ok(())
    }

    pub async fn reconcile(&mut self) -> Result<(), String> {
        let sessions: HashSet<String> = tmux::list_sessions().await?.into_iter().collect();
        let projects: HashMap<String, Project> = self
            .data
            .projects
            .iter()
            .cloned()
            .map(|project| (project.id.clone(), project))
            .collect();

        for thread in &mut self.data.threads {
            let worktree_exists = Path::new(&thread.worktree_path).exists();
            let tmux_exists = sessions.contains(&thread.tmux_session);

            if tmux_exists && !worktree_exists {
                let _ = tmux::kill_session(&thread.tmux_session).await;
                thread.status = protocol::ThreadStatus::Failed;
                continue;
            }

            if !worktree_exists {
                thread.status = protocol::ThreadStatus::Closed;
                continue;
            }

            if thread.status == protocol::ThreadStatus::Active && !tmux_exists {
                let Some(project) = projects.get(&thread.project_id) else {
                    thread.status = protocol::ThreadStatus::Failed;
                    continue;
                };

                let env = thread_env(project, thread);
                if tmux::create_session(&thread.tmux_session, &thread.worktree_path, &env)
                    .await
                    .is_err()
                {
                    thread.status = protocol::ThreadStatus::Failed;
                }
            }
        }

        Ok(())
    }

    pub fn snapshot(&self, state_version: protocol::StateVersion) -> protocol::StateSnapshot {
        protocol::StateSnapshot {
            state_version,
            projects: self.data.projects.iter().map(Project::to_protocol).collect(),
            threads: self.data.threads.iter().map(Thread::to_protocol).collect(),
        }
    }

    pub fn project_by_id(&self, project_id: &str) -> Option<&Project> {
        self.data.projects.iter().find(|project| project.id == project_id)
    }

    pub fn thread_by_id(&self, thread_id: &str) -> Option<&Thread> {
        self.data.threads.iter().find(|thread| thread.id == thread_id)
    }

    pub fn thread_by_id_mut(&mut self, thread_id: &str) -> Option<&mut Thread> {
        self.data
            .threads
            .iter_mut()
            .find(|thread| thread.id == thread_id)
    }
}

pub fn thread_env(project: &Project, thread: &Thread) -> Vec<(String, String)> {
    vec![
        ("THREADMILL_PROJECT".to_string(), project.name.clone()),
        ("THREADMILL_THREAD".to_string(), thread.name.clone()),
        ("THREADMILL_BRANCH".to_string(), thread.branch.clone()),
        (
            "THREADMILL_WORKTREE".to_string(),
            thread.worktree_path.clone(),
        ),
        ("THREADMILL_MAIN".to_string(), project.path.clone()),
    ]
}
