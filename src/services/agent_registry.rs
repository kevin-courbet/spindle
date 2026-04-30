use std::{fs, process::Command};

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
}

const BUILT_IN_AGENTS: &[BuiltInAgent] = &[
    BuiltInAgent {
        id: "opencode",
        name: "OpenCode",
        binary: "opencode",
        launch_args: &["acp"],
        install_package: "opencode-ai@latest",
        install_type: "npm",
    },
    BuiltInAgent {
        id: "claude",
        name: "Claude Code",
        binary: "claude-agent-acp",
        launch_args: &[],
        install_package: "@agentclientprotocol/claude-agent-acp@latest",
        install_type: "npm",
    },
    BuiltInAgent {
        id: "codex",
        name: "Codex",
        binary: "codex-acp",
        launch_args: &[],
        install_package: "@openai/codex@latest",
        install_type: "npm",
    },
    BuiltInAgent {
        id: "gemini",
        name: "Gemini",
        binary: "gemini",
        launch_args: &["--experimental-acp"],
        install_package: "@google/gemini-cli@latest",
        install_type: "npm",
    },
    BuiltInAgent {
        id: "copilot",
        name: "GitHub Copilot",
        binary: "copilot",
        launch_args: &["--acp", "--stdio"],
        install_package: "@github/copilot@latest",
        install_type: "npm",
    },
    BuiltInAgent {
        id: "kimi",
        name: "Kimi",
        binary: "kimi",
        launch_args: &["acp"],
        install_package: "kimi-cli",
        install_type: "uv",
    },
    BuiltInAgent {
        id: "vibe",
        name: "Vibe",
        binary: "vibe-acp",
        launch_args: &[],
        install_package: "mistral-vibe",
        install_type: "uv",
    },
    BuiltInAgent {
        id: "qwen",
        name: "Qwen Code",
        binary: "qwen",
        launch_args: &["--experimental-acp"],
        install_package: "@qwen-code/qwen-code@latest",
        install_type: "npm",
    },
];

const THREADMILL_CLAUDE_AGENT_ACP_PATH: &str = "/tmp/threadmill-claude-agent-acp.mjs";
const THREADMILL_CLAUDE_AGENT_ACP_COMMAND: &str = "node /tmp/threadmill-claude-agent-acp.mjs";
const THREADMILL_CLAUDE_AGENT_ACP_SOURCE: &str =
    include_str!("../assets/claude-agent-acp-threadmill.mjs");

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
    let output = Command::new("bash")
        .args(["-lc", &format!("command -v {binary}")])
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

fn command_for_agent(agent: &BuiltInAgent) -> String {
    if agent.id == "claude" {
        if let Err(err) = fs::write(
            THREADMILL_CLAUDE_AGENT_ACP_PATH,
            THREADMILL_CLAUDE_AGENT_ACP_SOURCE,
        ) {
            tracing::warn!(error = %err, "failed to write Threadmill Claude ACP bridge");
        }
        return THREADMILL_CLAUDE_AGENT_ACP_COMMAND.to_string();
    }

    build_command(agent.binary, agent.launch_args)
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
            command: command_for_agent(agent),
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

/// Returns the ACP command for a given agent_id (e.g. "opencode acp").
/// Falls back to the default if the agent isn't in the catalog.
pub fn agent_command(agent_id: &str) -> Option<String> {
    BUILT_IN_AGENTS
        .iter()
        .find(|a| a.id == agent_id)
        .map(command_for_agent)
}

pub fn resolve_agent_command(agent_id: &str, project_command: Option<String>) -> Option<String> {
    if agent_id == "claude" {
        return agent_command(agent_id);
    }

    project_command.or_else(|| agent_command(agent_id))
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
        assert_eq!(
            build_command("gemini", &["--experimental-acp"]),
            "gemini --experimental-acp"
        );
        assert_eq!(build_command("claude-agent-acp", &[]), "claude-agent-acp");
    }

    #[test]
    fn test_agent_command_lookup() {
        assert_eq!(agent_command("opencode"), Some("opencode acp".to_string()));
        assert_eq!(
            agent_command("claude"),
            Some("node /tmp/threadmill-claude-agent-acp.mjs".to_string())
        );
        assert_eq!(agent_command("nonexistent"), None);
    }

    #[test]
    fn test_claude_project_command_is_normalized_to_threadmill_bridge() {
        assert_eq!(
            resolve_agent_command("claude", Some("claude-agent-acp".to_string())),
            Some("node /tmp/threadmill-claude-agent-acp.mjs".to_string())
        );
    }
}
