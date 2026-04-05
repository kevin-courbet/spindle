use std::process::Command;

use serde::{Deserialize, Serialize};
use tracing::info;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AgentRegistryEntry {
    pub id: String,
    pub name: String,
    pub command: String,
    pub launch_args: Vec<String>,
    pub installed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_path: Option<String>,
    pub install_method: Option<InstallMethod>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum InstallMethod {
    #[serde(rename = "npm")]
    Npm { package: String },
    #[serde(rename = "uv")]
    Uv { package: String },
}

struct BuiltInAgent {
    id: &'static str,
    name: &'static str,
    binary: &'static str,
    launch_args: &'static [&'static str],
    install_package: &'static str,
    install_type: &'static str,
    /// Whether the agent auto-loads AGENTS.md from cwd (skip injection for these)
    self_injects_context: bool,
}

const BUILT_IN_AGENTS: &[BuiltInAgent] = &[
    BuiltInAgent {
        id: "opencode",
        name: "OpenCode",
        binary: "opencode",
        launch_args: &["acp"],
        install_package: "opencode-ai@latest",
        install_type: "npm",
        self_injects_context: true,
    },
    BuiltInAgent {
        id: "claude",
        name: "Claude Code",
        binary: "claude-agent-acp",
        launch_args: &[],
        install_package: "@agentclientprotocol/claude-agent-acp@latest",
        install_type: "npm",
        self_injects_context: false,
    },
    BuiltInAgent {
        id: "codex",
        name: "Codex",
        binary: "codex-acp",
        launch_args: &[],
        install_package: "@openai/codex@latest",
        install_type: "npm",
        self_injects_context: false,
    },
    BuiltInAgent {
        id: "gemini",
        name: "Gemini",
        binary: "gemini",
        launch_args: &["--experimental-acp"],
        install_package: "@google/gemini-cli@latest",
        install_type: "npm",
        self_injects_context: false,
    },
    BuiltInAgent {
        id: "copilot",
        name: "GitHub Copilot",
        binary: "copilot",
        launch_args: &["--acp", "--stdio"],
        install_package: "@github/copilot@latest",
        install_type: "npm",
        self_injects_context: false,
    },
    BuiltInAgent {
        id: "kimi",
        name: "Kimi",
        binary: "kimi",
        launch_args: &["acp"],
        install_package: "kimi-cli",
        install_type: "uv",
        self_injects_context: false,
    },
    BuiltInAgent {
        id: "vibe",
        name: "Vibe",
        binary: "vibe-acp",
        launch_args: &[],
        install_package: "mistral-vibe",
        install_type: "uv",
        self_injects_context: false,
    },
    BuiltInAgent {
        id: "qwen",
        name: "Qwen Code",
        binary: "qwen",
        launch_args: &["--experimental-acp"],
        install_package: "@qwen-code/qwen-code@latest",
        install_type: "npm",
        self_injects_context: false,
    },
];

fn install_method_from(agent: &BuiltInAgent) -> InstallMethod {
    match agent.install_type {
        "uv" => InstallMethod::Uv {
            package: agent.install_package.to_string(),
        },
        _ => InstallMethod::Npm {
            package: agent.install_package.to_string(),
        },
    }
}

fn which(binary: &str) -> Option<String> {
    let output = Command::new("which")
        .arg(binary)
        .output()
        .ok()?;
    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if path.is_empty() {
            None
        } else {
            Some(path)
        }
    } else {
        None
    }
}

fn build_command(binary: &str, launch_args: &[&str]) -> String {
    let mut parts = vec![binary.to_string()];
    for arg in launch_args {
        parts.push(arg.to_string());
    }
    parts.join(" ")
}

pub fn discover_agents() -> Vec<AgentRegistryEntry> {
    let mut entries = Vec::with_capacity(BUILT_IN_AGENTS.len());

    for agent in BUILT_IN_AGENTS {
        let resolved = which(agent.binary);
        let installed = resolved.is_some();
        if installed {
            info!(
                agent_id = agent.id,
                path = resolved.as_deref().unwrap_or(""),
                "agent discovered"
            );
        }
        entries.push(AgentRegistryEntry {
            id: agent.id.to_string(),
            name: agent.name.to_string(),
            command: build_command(agent.binary, agent.launch_args),
            launch_args: agent.launch_args.iter().map(|s| s.to_string()).collect(),
            installed,
            resolved_path: resolved,
            install_method: Some(install_method_from(agent)),
        });
    }

    entries
}

pub async fn install_agent(agent_id: &str) -> Result<String, String> {
    let agent = BUILT_IN_AGENTS
        .iter()
        .find(|a| a.id == agent_id)
        .ok_or_else(|| format!("unknown agent: {agent_id}"))?;

    let method = install_method_from(agent);

    match method {
        InstallMethod::Npm { package } => {
            info!(agent_id, %package, "installing agent via npm");
            let output = Command::new("bash")
                .args(["-lc", &format!("npm install -g {package}")])
                .output()
                .map_err(|err| format!("failed to run npm: {err}"))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!("npm install failed: {stderr}"));
            }

            which(agent.binary)
                .ok_or_else(|| format!("installed but binary '{}' not found in PATH", agent.binary))
        }
        InstallMethod::Uv { package } => {
            info!(agent_id, %package, "installing agent via uv");
            let output = Command::new("bash")
                .args(["-lc", &format!("uv tool install {package}")])
                .output()
                .map_err(|err| format!("failed to run uv: {err}"))?;

            if !output.status.success() {
                // uv sometimes returns non-zero even on success
                if which(agent.binary).is_some() {
                    info!(agent_id, "uv returned non-zero but binary found");
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(format!("uv install failed: {stderr}"));
                }
            }

            which(agent.binary)
                .ok_or_else(|| format!("installed but binary '{}' not found in PATH", agent.binary))
        }
    }
}

/// Returns whether a built-in agent auto-loads AGENTS.md from cwd.
pub fn agent_self_injects_context(agent_id: &str) -> bool {
    BUILT_IN_AGENTS
        .iter()
        .find(|a| a.id == agent_id)
        .map(|a| a.self_injects_context)
        .unwrap_or(false)
}

/// Returns the ACP command for a given agent_id (e.g. "opencode acp").
/// Falls back to the default if the agent isn't in the catalog.
pub fn agent_command(agent_id: &str) -> Option<String> {
    BUILT_IN_AGENTS
        .iter()
        .find(|a| a.id == agent_id)
        .map(|a| build_command(a.binary, a.launch_args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discover_agents_returns_all_built_ins() {
        let agents = discover_agents();
        assert_eq!(agents.len(), BUILT_IN_AGENTS.len());
        assert!(agents.iter().any(|a| a.id == "opencode"));
        assert!(agents.iter().any(|a| a.id == "claude"));
    }

    #[test]
    fn test_build_command_with_args() {
        assert_eq!(build_command("opencode", &["acp"]), "opencode acp");
        assert_eq!(build_command("gemini", &["--experimental-acp"]), "gemini --experimental-acp");
        assert_eq!(build_command("claude-agent-acp", &[]), "claude-agent-acp");
    }

    #[test]
    fn test_agent_command_lookup() {
        assert_eq!(agent_command("opencode"), Some("opencode acp".to_string()));
        assert_eq!(agent_command("claude"), Some("claude-agent-acp".to_string()));
        assert_eq!(agent_command("nonexistent"), None);
    }
}
