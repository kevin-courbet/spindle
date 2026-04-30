use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{config, protocol};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkflowStoreData {
    #[serde(default)]
    pub(super) workflows: Vec<PersistedWorkflow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct PersistedWorkflow {
    pub(super) state: protocol::WorkflowState,
    #[serde(default = "default_next_index")]
    pub(super) next_worker_index: u32,
    #[serde(default = "default_next_index")]
    pub(super) next_reviewer_index: u32,
    #[serde(default = "default_next_index")]
    pub(super) next_finding_index: u32,
    /// How many reviewer rows `start_review` intends to spawn for the current round.
    /// Used to gate `workflow.review_completed` — we must not emit until every spec
    /// has actually been inserted into `reviewers`, otherwise a fast first reviewer
    /// finishing before the second is spawned would trip `all_terminal` with `count=1`.
    /// Reset to 0 on review_completed.
    #[serde(default)]
    pub(super) expected_reviewer_count: u32,
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
        let config_dir =
            config::config_dir().ok_or_else(|| "unable to locate config dir".to_string())?;
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
