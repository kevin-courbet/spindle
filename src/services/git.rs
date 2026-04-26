use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use serde::Deserialize;
use tokio::process::Command;

use crate::{protocol, AppState};

pub struct GitService;

#[derive(Debug, Deserialize)]
struct GhPrView {
    url: String,
    number: u64,
}

struct ParsedStatusSummary {
    ahead_count: u32,
    behind_count: u32,
    files: Vec<protocol::GitStatusSummaryFile>,
}

impl GitService {
    pub async fn status_summary(
        state: Arc<AppState>,
        params: protocol::GitStatusSummaryParams,
    ) -> Result<protocol::GitStatusSummaryResult, String> {
        let worktree_path = resolve_thread_worktree(state, &params.thread_id).await?;
        ensure_worktree_exists(&worktree_path)?;

        let branch = current_branch(&worktree_path).await?;
        let args = ["status", "--porcelain=v1", "-z", "--branch", "-uall"];
        let output = run_git_output(&worktree_path, &args).await?;
        if !output.status.success() {
            return Err(nonzero_git_message(&worktree_path, &args, &output));
        }

        let parsed = parse_status_summary(&output.stdout)?;
        Ok(protocol::GitStatusSummaryResult {
            branch,
            ahead_count: parsed.ahead_count,
            behind_count: parsed.behind_count,
            files: parsed.files,
        })
    }

    pub async fn commit(
        state: Arc<AppState>,
        params: protocol::GitCommitParams,
    ) -> Result<protocol::GitCommitResult, String> {
        let worktree_path = resolve_thread_worktree(state, &params.thread_id).await?;
        ensure_worktree_exists(&worktree_path)?;

        let message = params.message.trim();
        if message.is_empty() {
            return Err("commit message must not be empty".to_string());
        }

        stage_changes(&worktree_path, params.paths.as_deref()).await?;

        let mut args = vec!["commit".to_string()];
        if params.amend {
            args.push("--amend".to_string());
        }
        args.push("-m".to_string());
        args.push(message.to_string());

        let output = run_git_output_owned(&worktree_path, &args).await?;
        if !output.status.success() {
            let message = combined_output(&output);
            if message.to_ascii_lowercase().contains("nothing to commit") {
                return Err("nothing to commit".to_string());
            }
            return Err(nonzero_git_message_owned(&worktree_path, &args, &output));
        }

        let commit_hash = run_git(&worktree_path, &["rev-parse", "HEAD"]).await?;
        let summary = run_git(&worktree_path, &["log", "-1", "--format=%s"]).await?;

        Ok(protocol::GitCommitResult {
            commit_hash,
            summary,
        })
    }

    pub async fn push(
        state: Arc<AppState>,
        params: protocol::GitPushParams,
    ) -> Result<protocol::GitPushResult, String> {
        let worktree_path = resolve_thread_worktree(state, &params.thread_id).await?;
        ensure_worktree_exists(&worktree_path)?;

        let branch = current_branch(&worktree_path).await?;
        let upstream = branch_upstream(&worktree_path, &branch).await?;

        let mut args = vec!["push".to_string()];
        if params.force {
            args.push("--force".to_string());
        }

        let remote = if let Some(upstream) = upstream {
            if upstream.remote_branch == branch {
                args.push(upstream.remote.clone());
                args.push(format!("HEAD:{}", upstream.remote_branch));
                upstream.remote
            } else if params.set_upstream {
                args.push("--set-upstream".to_string());
                args.push(upstream.remote.clone());
                args.push(branch.clone());
                upstream.remote
            } else {
                args.push(upstream.remote.clone());
                args.push(format!("HEAD:{}", upstream.remote_branch));
                upstream.remote
            }
        } else {
            if !params.set_upstream {
                return Err(
                    "current branch has no upstream; set_upstream must be true to publish it"
                        .to_string(),
                );
            }
            let remote = "origin".to_string();
            args.push("--set-upstream".to_string());
            args.push(remote.clone());
            args.push(branch.clone());
            remote
        };

        let output = run_git_output_owned(&worktree_path, &args).await?;
        if !output.status.success() {
            return Err(nonzero_git_message_owned(&worktree_path, &args, &output));
        }

        Ok(protocol::GitPushResult {
            success: true,
            remote,
            branch,
        })
    }

    pub async fn create_pr(
        state: Arc<AppState>,
        params: protocol::GitCreatePrParams,
    ) -> Result<protocol::GitCreatePrResult, String> {
        let worktree_path = resolve_thread_worktree(state, &params.thread_id).await?;
        ensure_worktree_exists(&worktree_path)?;

        let title = params.title.trim();
        if title.is_empty() {
            return Err("pull request title must not be empty".to_string());
        }

        let branch = current_branch(&worktree_path).await?;
        let mut args = vec![
            "pr".to_string(),
            "create".to_string(),
            "--title".to_string(),
            title.to_string(),
            "--body".to_string(),
            params.body.clone(),
            "--head".to_string(),
            branch.clone(),
        ];
        if params.draft {
            args.push("--draft".to_string());
        }

        let output = run_gh_output_owned(&worktree_path, &args).await?;
        if !output.status.success() {
            return Err(nonzero_gh_message_owned(&worktree_path, &args, &output));
        }

        let created_url = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with("http://") || line.starts_with("https://"))
            .ok_or_else(|| "gh pr create returned no pull request URL".to_string())?
            .to_string();

        let view_args = vec![
            "pr".to_string(),
            "view".to_string(),
            branch,
            "--json".to_string(),
            "url,number".to_string(),
        ];
        let view_output = run_gh_output_owned(&worktree_path, &view_args).await?;
        let (pr_url, pr_number) = if view_output.status.success() {
            let view: GhPrView = serde_json::from_slice(&view_output.stdout)
                .map_err(|err| format!("failed to parse gh pr view JSON: {err}"))?;
            (view.url, view.number)
        } else {
            (created_url.clone(), parse_pr_number(&created_url)?)
        };

        Ok(protocol::GitCreatePrResult { pr_url, pr_number })
    }
}

async fn resolve_thread_worktree(state: Arc<AppState>, thread_id: &str) -> Result<String, String> {
    let store = state.store.lock().await;
    let thread = store
        .thread_by_id(thread_id)
        .ok_or_else(|| format!("thread not found: {thread_id}"))?;
    let project = store
        .project_by_id(&thread.project_id)
        .ok_or_else(|| format!("project not found: {}", thread.project_id))?;
    Ok(thread.checkout_path(&project.path).to_string())
}

fn ensure_worktree_exists(worktree_path: &str) -> Result<(), String> {
    let path = Path::new(worktree_path);
    if !path.exists() {
        return Err(format!("worktree not found: {worktree_path}"));
    }
    if !path.is_dir() {
        return Err(format!("worktree is not a directory: {worktree_path}"));
    }
    Ok(())
}

async fn stage_changes(worktree_path: &str, paths: Option<&[String]>) -> Result<(), String> {
    let args = build_git_add_args(Path::new(worktree_path), paths)?;
    let output = run_git_output_owned(worktree_path, &args).await?;
    if !output.status.success() {
        return Err(nonzero_git_message_owned(worktree_path, &args, &output));
    }
    Ok(())
}

fn build_git_add_args(
    worktree_root: &Path,
    paths: Option<&[String]>,
) -> Result<Vec<String>, String> {
    let mut args = vec!["add".to_string(), "-A".to_string(), "--".to_string()];
    match paths {
        Some(paths) => {
            if paths.is_empty() {
                return Err("paths must not be empty when provided".to_string());
            }
            for path in paths {
                args.push(normalize_git_path(worktree_root, path)?);
            }
        }
        None => args.push(".".to_string()),
    }
    Ok(args)
}

fn normalize_git_path(worktree_root: &Path, raw_path: &str) -> Result<String, String> {
    if raw_path.trim().is_empty() {
        return Err("paths must not contain empty values".to_string());
    }

    let input = Path::new(raw_path);
    let relative = if input.is_absolute() {
        input
            .strip_prefix(worktree_root)
            .map_err(|_| format!("path is outside thread worktree: {raw_path}"))?
            .to_path_buf()
    } else {
        input.to_path_buf()
    };

    let normalized = normalize_relative_path(&relative)?;
    if normalized.as_os_str().is_empty() {
        Ok(".".to_string())
    } else {
        normalized
            .to_str()
            .ok_or_else(|| format!("invalid utf-8 path: {}", normalized.display()))
            .map(ToString::to_string)
    }
}

fn normalize_relative_path(path: &Path) -> Result<PathBuf, String> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(format!("path escapes thread worktree: {}", path.display()));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "path must be relative to the thread worktree: {}",
                    path.display()
                ));
            }
        }
    }
    Ok(normalized)
}

async fn current_branch(worktree_path: &str) -> Result<String, String> {
    let branch = run_git(worktree_path, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
    if branch == "HEAD" {
        return Err("current branch is detached".to_string());
    }
    Ok(branch)
}

struct BranchUpstream {
    remote: String,
    remote_branch: String,
}

async fn branch_upstream(
    worktree_path: &str,
    branch: &str,
) -> Result<Option<BranchUpstream>, String> {
    let remote_key = format!("branch.{branch}.remote");
    let remote_output = run_git_output(worktree_path, &["config", "--get", &remote_key]).await?;
    if !remote_output.status.success() {
        return Ok(None);
    }
    let remote = String::from_utf8_lossy(&remote_output.stdout)
        .trim()
        .to_string();
    if remote.is_empty() {
        return Ok(None);
    }

    let merge_key = format!("branch.{branch}.merge");
    let merge_output = run_git_output(worktree_path, &["config", "--get", &merge_key]).await?;
    let remote_branch = if merge_output.status.success() {
        let merge = String::from_utf8_lossy(&merge_output.stdout)
            .trim()
            .to_string();
        let merge = merge.trim_start_matches("refs/heads/").trim();
        if merge.is_empty() {
            branch.to_string()
        } else {
            merge.to_string()
        }
    } else {
        branch.to_string()
    };

    Ok(Some(BranchUpstream {
        remote,
        remote_branch,
    }))
}

fn parse_status_summary(stdout: &[u8]) -> Result<ParsedStatusSummary, String> {
    let mut ahead_count = 0;
    let mut behind_count = 0;
    let mut files = BTreeMap::<String, protocol::GitStatusSummaryFile>::new();
    let mut records = stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty());

    while let Some(record) = records.next() {
        if record.starts_with(b"## ") {
            let header = std::str::from_utf8(record)
                .map_err(|_| "branch header is not valid UTF-8".to_string())?;
            let (ahead, behind) = parse_branch_header(header);
            ahead_count = ahead;
            behind_count = behind;
            continue;
        }

        if record.len() < 4 || record[2] != b' ' {
            continue;
        }

        let xy = std::str::from_utf8(&record[0..2])
            .map_err(|_| "status marker is not valid UTF-8".to_string())?;
        let path = String::from_utf8(record[3..].to_vec())
            .map_err(|_| "path is not valid UTF-8".to_string())?;

        let entry = files
            .entry(path.clone())
            .or_insert_with(|| protocol::GitStatusSummaryFile {
                path,
                staged: false,
                unstaged: false,
                untracked: false,
            });

        if xy == "??" {
            entry.untracked = true;
            continue;
        }

        let xy_bytes = xy.as_bytes();
        entry.staged |= xy_bytes[0] != b' ' && xy_bytes[0] != b'?';
        entry.unstaged |= xy_bytes[1] != b' ' && xy_bytes[1] != b'?';

        if xy_bytes.iter().any(|byte| *byte == b'R' || *byte == b'C') {
            let _ = records.next();
        }
    }

    Ok(ParsedStatusSummary {
        ahead_count,
        behind_count,
        files: files.into_values().collect(),
    })
}

fn parse_branch_header(header: &str) -> (u32, u32) {
    let Some(start) = header.find('[') else {
        return (0, 0);
    };
    let Some(end) = header[start + 1..].find(']') else {
        return (0, 0);
    };
    let detail = &header[start + 1..start + 1 + end];

    let mut ahead_count = 0;
    let mut behind_count = 0;
    for part in detail.split(',').map(str::trim) {
        if let Some(raw) = part.strip_prefix("ahead ") {
            ahead_count = raw.parse::<u32>().unwrap_or(0);
        } else if let Some(raw) = part.strip_prefix("behind ") {
            behind_count = raw.parse::<u32>().unwrap_or(0);
        }
    }

    (ahead_count, behind_count)
}

fn parse_pr_number(pr_url: &str) -> Result<u64, String> {
    let candidate = pr_url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .ok_or_else(|| format!("failed to parse pull request number from {pr_url}"))?;
    candidate
        .parse::<u64>()
        .map_err(|err| format!("failed to parse pull request number from {pr_url}: {err}"))
}

async fn run_git(worktree_path: &str, args: &[&str]) -> Result<String, String> {
    let output = run_git_output(worktree_path, args).await?;
    if !output.status.success() {
        return Err(nonzero_git_message(worktree_path, args, &output));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn run_git_output(
    worktree_path: &str,
    args: &[&str],
) -> Result<std::process::Output, String> {
    Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(args)
        .output()
        .await
        .map_err(|err| format!("failed to run git {:?} in {worktree_path}: {err}", args))
}

async fn run_git_output_owned(
    worktree_path: &str,
    args: &[String],
) -> Result<std::process::Output, String> {
    Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(args)
        .output()
        .await
        .map_err(|err| format!("failed to run git {:?} in {worktree_path}: {err}", args))
}

async fn run_gh_output_owned(
    worktree_path: &str,
    args: &[String],
) -> Result<std::process::Output, String> {
    Command::new("gh")
        .current_dir(worktree_path)
        .args(args)
        .output()
        .await
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                "gh is not installed".to_string()
            } else {
                format!("failed to run gh {:?} in {worktree_path}: {err}", args)
            }
        })
}

fn nonzero_git_message(
    worktree_path: &str,
    args: &[&str],
    output: &std::process::Output,
) -> String {
    let message = combined_output(output);
    if message.is_empty() {
        format!("git {:?} failed in {worktree_path}", args)
    } else {
        format!("git {:?} failed in {worktree_path}: {message}", args)
    }
}

fn nonzero_git_message_owned(
    worktree_path: &str,
    args: &[String],
    output: &std::process::Output,
) -> String {
    let message = combined_output(output);
    if message.is_empty() {
        format!("git {:?} failed in {worktree_path}", args)
    } else {
        format!("git {:?} failed in {worktree_path}: {message}", args)
    }
}

fn nonzero_gh_message_owned(
    worktree_path: &str,
    args: &[String],
    output: &std::process::Output,
) -> String {
    let message = combined_output(output);
    if message.is_empty() {
        format!("gh {:?} failed in {worktree_path}", args)
    } else {
        format!("gh {:?} failed in {worktree_path}: {message}", args)
    }
}

fn combined_output(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout,
        (true, false) => stderr,
        (false, false) => format!("{stdout}\n{stderr}"),
    }
}
