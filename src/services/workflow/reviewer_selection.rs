use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{config, protocol};

/// Parsed `.threadmill/agents/<name>.md` — deployed persona specs referenced by name.
pub(super) struct AgentPersonaFile {
    pub(super) agent: String,
    pub(super) model: Option<String>,
    pub(super) display_name: Option<String>,
    pub(super) system_prompt: String,
}

pub(super) fn parse_agent_persona_file(path: &Path) -> Result<AgentPersonaFile, String> {
    let content = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;

    let (frontmatter, body) = if let Some(after_open) = content.strip_prefix("---\n") {
        if let Some(end) = after_open.find("\n---\n") {
            (&after_open[..end], after_open[end + 5..].trim())
        } else {
            ("", content.as_str())
        }
    } else {
        ("", content.as_str())
    };

    let mut agent = String::new();
    let mut display_name = None;
    let mut model: Option<String> = None;
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("agent:") {
            agent = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("display_name:") {
            display_name = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("model:") {
            let t = v.trim();
            if !t.is_empty() {
                model = Some(t.to_string());
            }
        }
    }

    if agent.is_empty() {
        return Err(format!(
            "agent persona {} missing 'agent' field in frontmatter",
            path.display()
        ));
    }

    Ok(AgentPersonaFile {
        agent,
        model,
        display_name,
        system_prompt: body.to_string(),
    })
}

/// Resolve a persona name (e.g. "reviewer-codex") to its file, preferring the
/// project-local `.threadmill/agents/` over the global `~/.config/threadmill/agents/`.
pub(super) fn resolve_persona_path(worktree: &Path, name: &str) -> Option<PathBuf> {
    let project = worktree
        .join(".threadmill/agents")
        .join(format!("{name}.md"));
    if project.exists() {
        return Some(project);
    }
    let global = config::config_dir()?
        .join("threadmill/agents")
        .join(format!("{name}.md"));
    if global.exists() {
        return Some(global);
    }
    None
}

fn spec_from_persona(
    persona: &AgentPersonaFile,
    display_suffix: &str,
    initial_prompt: String,
) -> protocol::WorkflowReviewerSpec {
    let display_name = persona
        .display_name
        .as_ref()
        .map(|base| format!("{base} — {display_suffix}"))
        .or_else(|| Some(display_suffix.to_string()));
    protocol::WorkflowReviewerSpec {
        agent_name: persona.agent.clone(),
        parent_session_id: None,
        system_prompt: Some(persona.system_prompt.clone()),
        initial_prompt: Some(initial_prompt),
        display_name,
        preferred_model: persona.model.clone(),
    }
}

async fn branch_diff(worktree: &Path) -> String {
    for base in ["main", "master"] {
        let output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(["diff", &format!("{base}...HEAD")])
            .output()
            .await;
        if let Ok(output) = output {
            if output.status.success() {
                return String::from_utf8_lossy(&output.stdout).to_string();
            }
        }
    }
    String::new()
}

fn is_test_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    if lower.contains("/tests/") || lower.contains("/__tests__/") || lower.contains("/test/") {
        return true;
    }
    for suffix in [
        ".test.ts",
        ".test.tsx",
        ".test.js",
        ".test.jsx",
        ".spec.ts",
        ".spec.tsx",
        ".spec.js",
        ".spec.jsx",
        "_test.rs",
        "_test.py",
        "_test.go",
        "test.swift",
        "tests.swift",
        "spec.swift",
    ] {
        if lower.ends_with(suffix) {
            return true;
        }
    }
    false
}

async fn branch_changed_files(worktree: &Path) -> Vec<String> {
    for base in ["main", "master"] {
        let output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(["diff", "--name-only", &format!("{base}...HEAD")])
            .output()
            .await;
        if let Ok(output) = output {
            if output.status.success() {
                return String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .map(str::to_string)
                    .filter(|line| !line.is_empty())
                    .collect();
            }
        }
    }
    Vec::new()
}

/// Build the default review-swarm spec list from `git diff main...HEAD`.
pub(super) async fn auto_select_reviewer_specs(
    worktree: &Path,
) -> Result<Vec<protocol::WorkflowReviewerSpec>, String> {
    const CORRECTNESS_PROMPT: &str =
        "Review ALL changes on this branch vs main: `git diff main...HEAD`. \
Focus on: correctness, edge cases, security, performance. \
For each issue: file:line, severity (Critical/High/Medium/Low), concrete fix. \
Conclude with an actionable items list ordered by severity. Only report violations.";

    const ARCHITECTURE_PROMPT: &str =
        "Review ALL changes on this branch vs main: `git diff main...HEAD`. \
Focus on: architecture, patterns, error handling, missing tests, silent fallbacks. \
CRITICAL — silent fallback scan: errors are a feature. Flag every instance of error suppression \
that masks failures instead of propagating them (|| true, 2>/dev/null, catch blocks returning \
defaults, _, _ = on operations that should fail loudly, ?? [] hiding malformed responses). \
For each flagged pattern, state whether it is legitimate cleanup/teardown or an erroneous mask. \
For each issue: file:line, severity (Critical/High/Medium/Low), concrete fix. \
Conclude with an actionable items list ordered by severity. Only report violations.";

    const EFFECT_PROMPT: &str = "Review ALL Effect.ts code changes on this branch vs main. \
Check for: reinvented library functions, non-idiomatic patterns, `_tag` checks that should use \
library type guards (Cause.isCause, Exit.isExit, Option.isSome), reimplemented transformers \
(Cause.pretty, Exit.match). Reference llms.txt / vendored Effect docs. \
For each issue: file:line, severity, concrete fix. Only report violations.";

    const LOGGING_PROMPT: &str = "Review logging changes on this branch vs main. \
Effect.logError must pass cause as 2nd arg. Services should prefer spans over ad-hoc logInfo/logDebug. \
Effect.logDebug must not be committed. \
Report only violations, as a table: | # | Severity | Issue | File:Line |";

    const TEST_QUALITY_PROMPT: &str = "Review test changes on this branch vs main. \
Check for: source-reading tests, tautological assertions, mock-as-subject, mock-server echo, \
over-mocked wiring, no-op assertions. \
Report only violations, as a table: | # | Severity | Pattern | Issue | File:Line |";

    let mut specs = Vec::new();

    for persona_name in ["reviewer-codex", "reviewer-opus", "reviewer-gemini"] {
        let Some(path) = resolve_persona_path(worktree, persona_name) else {
            continue;
        };
        let persona = parse_agent_persona_file(&path)?;
        specs.push(spec_from_persona(
            &persona,
            "Correctness",
            CORRECTNESS_PROMPT.to_string(),
        ));
        specs.push(spec_from_persona(
            &persona,
            "Architecture",
            ARCHITECTURE_PROMPT.to_string(),
        ));
    }

    let diff = branch_diff(worktree).await;
    let changed = branch_changed_files(worktree).await;

    let has_ts_effect = changed
        .iter()
        .any(|f| f.ends_with(".ts") || f.ends_with(".tsx"))
        && (diff.contains("from \"effect\"")
            || diff.contains("from 'effect'")
            || diff.contains("from '@effect/")
            || diff.contains("from \"@effect/"));

    let has_logging = diff.contains("Effect.log");
    let has_test_changes = changed.iter().any(|f| is_test_path(f));

    if has_ts_effect {
        if let Some(path) = resolve_persona_path(worktree, "effect-reviewer") {
            let persona = parse_agent_persona_file(&path)?;
            specs.push(spec_from_persona(
                &persona,
                "Effect.ts",
                EFFECT_PROMPT.to_string(),
            ));
        }
    }
    if has_logging {
        if let Some(path) = resolve_persona_path(worktree, "logging-review") {
            let persona = parse_agent_persona_file(&path)?;
            specs.push(spec_from_persona(
                &persona,
                "Logging",
                LOGGING_PROMPT.to_string(),
            ));
        }
    }
    if has_test_changes {
        if let Some(path) = resolve_persona_path(worktree, "test-quality-review") {
            let persona = parse_agent_persona_file(&path)?;
            specs.push(spec_from_persona(
                &persona,
                "Tests",
                TEST_QUALITY_PROMPT.to_string(),
            ));
        }
    }

    Ok(specs)
}
