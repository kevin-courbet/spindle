use std::{collections::HashMap, fs, path::Path};

use serde::Deserialize;

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
