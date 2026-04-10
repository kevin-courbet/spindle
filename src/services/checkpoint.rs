use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncWriteExt, process::Command};
use uuid::Uuid;

use crate::{
    protocol,
    services::{
        chat::{history_path_for_session, ChatService},
        preset::PresetService,
        project::load_project_presets,
        thread::{load_threadmill_config, CheckpointConfig},
    },
    AppState,
};

const CHECKPOINT_REF_NAMESPACE: &str = "refs/threadmill-checkpoints";
const CHECKPOINT_AUTHOR_NAME: &str = "Threadmill";
const CHECKPOINT_AUTHOR_EMAIL: &str = "threadmill@localhost";

pub struct CheckpointService;

#[derive(Debug, Clone)]
struct ThreadContext {
    thread_id: String,
    project_path: String,
    worktree_path: String,
}

#[derive(Debug, Clone)]
struct StoredCheckpoint {
    summary: protocol::CheckpointSummary,
    _commit_oid: String,
    history_cursor: Option<u64>,
}

impl StoredCheckpoint {
    fn history_cursor(&self) -> Option<u64> {
        self.history_cursor
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredCheckpointMetadata {
    seq: u64,
    thread_id: String,
    timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    head_oid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prompt_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    history_cursor: Option<u64>,
}

impl CheckpointService {
    pub async fn save(
        state: Arc<AppState>,
        params: protocol::CheckpointSaveParams,
    ) -> Result<protocol::CheckpointSaveResult, String> {
        let context = thread_context(&state, &params.thread_id).await?;
        let config = load_threadmill_config(&context.worktree_path, &context.project_path)?;

        if git_is_busy(&context.worktree_path).await? {
            return Ok(protocol::CheckpointSaveResult {
                skipped: true,
                checkpoint: None,
            });
        }

        let existing =
            list_checkpoints_for_thread(&context.worktree_path, &context.thread_id).await?;
        let seq = existing
            .iter()
            .map(|checkpoint| checkpoint.summary.seq)
            .max()
            .unwrap_or(0)
            + 1;
        let timestamp = Utc::now().to_rfc3339();
        let head_oid = git_output(&context.worktree_path, &["rev-parse", "HEAD"]).await?;
        let history_cursor = if let Some(session_id) = params.session_id.as_deref() {
            Some(current_history_cursor(&state, &context.thread_id, session_id).await?)
        } else {
            None
        };
        let metadata = StoredCheckpointMetadata {
            seq,
            thread_id: context.thread_id.clone(),
            timestamp: timestamp.clone(),
            session_id: params.session_id.clone(),
            head_oid: head_oid.clone(),
            prompt_preview: sanitize_optional_text(params.prompt_preview),
            message: sanitize_optional_text(params.message),
            history_cursor,
        };
        let git_ref = checkpoint_ref(&context.thread_id, seq);
        let commit_oid =
            create_checkpoint_commit(&context.worktree_path, &head_oid, &metadata).await?;
        git_output(
            &context.worktree_path,
            &["update-ref", &git_ref, &commit_oid],
        )
        .await?;

        let checkpoint = protocol::CheckpointSummary {
            seq,
            git_ref,
            timestamp,
            session_id: metadata.session_id,
            head_oid,
            prompt_preview: metadata.prompt_preview,
            message: metadata.message,
        };

        apply_retention_policy(
            &context.worktree_path,
            &context.thread_id,
            &config.checkpoints,
            Utc::now(),
        )
        .await?;

        Ok(protocol::CheckpointSaveResult {
            skipped: false,
            checkpoint: Some(checkpoint),
        })
    }

    pub async fn restore(
        state: Arc<AppState>,
        params: protocol::CheckpointRestoreParams,
    ) -> Result<protocol::CheckpointRestoreResult, String> {
        let context = thread_context(&state, &params.thread_id).await?;
        let checkpoint = resolve_checkpoint(
            &context.worktree_path,
            &context.thread_id,
            params.seq,
            params.git_ref,
        )
        .await?;

        ChatService::stop_all_for_thread(
            Arc::clone(&state),
            &context.thread_id,
            "checkpoint_restore",
            false,
        )
        .await?;
        stop_all_presets_for_thread(Arc::clone(&state), &context).await?;

        git_output(
            &context.worktree_path,
            &["reset", "--hard", &checkpoint.summary.git_ref],
        )
        .await?;
        git_output(&context.worktree_path, &["clean", "-fd"]).await?;

        if let (Some(session_id), Some(history_cursor)) = (
            checkpoint.summary.session_id.as_deref(),
            checkpoint.history_cursor(),
        ) {
            truncate_history_to_cursor(&state, &context.thread_id, session_id, history_cursor).await?;
        }

        Ok(protocol::CheckpointRestoreResult {
            restored: true,
            checkpoint: checkpoint.summary,
        })
    }

    pub async fn list(
        state: Arc<AppState>,
        params: protocol::CheckpointListParams,
    ) -> Result<protocol::CheckpointListResult, String> {
        let context = thread_context(&state, &params.thread_id).await?;
        let mut checkpoints =
            list_checkpoints_for_thread(&context.worktree_path, &context.thread_id)
                .await?
                .into_iter()
                .map(|checkpoint| checkpoint.summary)
                .collect::<Vec<_>>();

        if let Some(session_id) = params.session_id.as_deref() {
            checkpoints.retain(|checkpoint| checkpoint.session_id.as_deref() == Some(session_id));
        }

        if let Some(limit) = params.limit {
            checkpoints.truncate(limit);
        }

        Ok(checkpoints)
    }

    pub async fn diff(
        state: Arc<AppState>,
        params: protocol::CheckpointDiffParams,
    ) -> Result<protocol::CheckpointDiffResult, String> {
        let context = thread_context(&state, &params.thread_id).await?;
        let base = resolve_checkpoint(
            &context.worktree_path,
            &context.thread_id,
            params.base_seq,
            params.base_ref,
        )
        .await?;
        let target = if params.target_ref.is_some() || params.target_seq.is_some() {
            resolve_checkpoint(
                &context.worktree_path,
                &context.thread_id,
                params.target_seq,
                params.target_ref,
            )
            .await?
            .summary
            .git_ref
        } else {
            "HEAD".to_string()
        };

        let diff_text = git_output(
            &context.worktree_path,
            &["diff", &base.summary.git_ref, &target],
        )
        .await?;

        Ok(protocol::CheckpointDiffResult {
            base_ref: base.summary.git_ref,
            target_ref: target,
            diff_text,
        })
    }

    pub async fn cleanup_thread(state: Arc<AppState>, thread_id: &str) -> Result<(), String> {
        let context = thread_context(&state, thread_id).await?;
        delete_checkpoint_refs(&context.worktree_path, thread_id).await
    }
}

async fn thread_context(state: &Arc<AppState>, thread_id: &str) -> Result<ThreadContext, String> {
    let store = state.store.lock().await;
    let thread = store
        .thread_by_id(thread_id)
        .ok_or_else(|| format!("thread not found: {thread_id}"))?
        .clone();
    let project = store
        .project_by_id(&thread.project_id)
        .ok_or_else(|| format!("project not found: {}", thread.project_id))?
        .clone();
    Ok(ThreadContext {
        thread_id: thread.id,
        project_path: project.path,
        worktree_path: thread.worktree_path,
    })
}

async fn stop_all_presets_for_thread(
    state: Arc<AppState>,
    context: &ThreadContext,
) -> Result<(), String> {
    let config = load_threadmill_config(&context.worktree_path, &context.project_path)?;
    let mut preset_names = config.presets.keys().cloned().collect::<HashSet<_>>();
    for preset in load_project_presets(&context.project_path)? {
        preset_names.insert(preset.name);
    }

    let mut preset_names = preset_names.into_iter().collect::<Vec<_>>();
    preset_names.sort();
    for preset in preset_names {
        let _ = PresetService::stop(
            Arc::clone(&state),
            protocol::PresetStopParams {
                thread_id: context.thread_id.clone(),
                preset,
                session_id: None,
            },
        )
        .await?;
    }

    Ok(())
}

async fn resolve_checkpoint(
    worktree_path: &str,
    thread_id: &str,
    seq: Option<u64>,
    git_ref: Option<String>,
) -> Result<StoredCheckpoint, String> {
    let git_ref = match (seq, git_ref) {
        (_, Some(git_ref)) => git_ref,
        (Some(seq), None) => checkpoint_ref(thread_id, seq),
        (None, None) => return Err("checkpoint reference requires seq or git_ref".to_string()),
    };

    let commit_oid = git_output(worktree_path, &["rev-parse", &git_ref]).await?;
    let metadata = read_checkpoint_metadata(worktree_path, &commit_oid).await?;
    Ok(StoredCheckpoint {
        _commit_oid: commit_oid,
        summary: summary_from_metadata(metadata.clone(), git_ref),
        history_cursor: metadata.history_cursor,
    })
}

async fn list_checkpoints_for_thread(
    worktree_path: &str,
    thread_id: &str,
) -> Result<Vec<StoredCheckpoint>, String> {
    let prefix = checkpoint_ref_prefix(thread_id);
    let output = git_output(
        worktree_path,
        &["for-each-ref", "--format=%(refname) %(objectname)", &prefix],
    )
    .await?;
    if output.is_empty() {
        return Ok(Vec::new());
    }

    let mut checkpoints = Vec::new();
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let mut parts = line.split_whitespace();
        let Some(git_ref) = parts.next() else {
            continue;
        };
        let Some(commit_oid) = parts.next() else {
            continue;
        };
        let metadata = read_checkpoint_metadata(worktree_path, commit_oid).await?;
        checkpoints.push(StoredCheckpoint {
            _commit_oid: commit_oid.to_string(),
            summary: summary_from_metadata(metadata.clone(), git_ref.to_string()),
            history_cursor: metadata.history_cursor,
        });
    }

    checkpoints.sort_by(|left, right| right.summary.seq.cmp(&left.summary.seq));
    Ok(checkpoints)
}

async fn read_checkpoint_metadata(
    worktree_path: &str,
    commit_oid: &str,
) -> Result<StoredCheckpointMetadata, String> {
    let raw = git_output(worktree_path, &["show", "-s", "--format=%B", commit_oid]).await?;
    serde_json::from_str(raw.trim()).map_err(|err| {
        format!(
            "failed to parse checkpoint metadata for {}: {err}",
            commit_oid
        )
    })
}

fn summary_from_metadata(
    metadata: StoredCheckpointMetadata,
    git_ref: String,
) -> protocol::CheckpointSummary {
    protocol::CheckpointSummary {
        seq: metadata.seq,
        git_ref,
        timestamp: metadata.timestamp,
        session_id: metadata.session_id,
        head_oid: metadata.head_oid,
        prompt_preview: metadata.prompt_preview,
        message: metadata.message,
    }
}

async fn create_checkpoint_commit(
    worktree_path: &str,
    head_oid: &str,
    metadata: &StoredCheckpointMetadata,
) -> Result<String, String> {
    let temp_index = std::env::temp_dir().join(format!(
        "threadmill-checkpoint-{}.index",
        Uuid::new_v4().simple()
    ));
    let index_path = temp_index
        .to_str()
        .ok_or_else(|| format!("invalid temp index path: {}", temp_index.display()))?
        .to_string();
    let metadata_json = serde_json::to_string(metadata)
        .map_err(|err| format!("failed to serialize checkpoint metadata: {err}"))?;
    let commit_timestamp = metadata.timestamp.clone();

    let result = async {
        git_output_env(
            worktree_path,
            &["read-tree", "HEAD"],
            &[("GIT_INDEX_FILE", index_path.as_str())],
        )
        .await?;
        git_output_env(
            worktree_path,
            &["add", "-A", "--", "."],
            &[("GIT_INDEX_FILE", index_path.as_str())],
        )
        .await?;
        let tree_oid = git_output_env(
            worktree_path,
            &["write-tree"],
            &[("GIT_INDEX_FILE", index_path.as_str())],
        )
        .await?;

        git_output_with_input(
            worktree_path,
            &["commit-tree", &tree_oid, "-p", head_oid],
            &[
                ("GIT_AUTHOR_NAME", CHECKPOINT_AUTHOR_NAME),
                ("GIT_AUTHOR_EMAIL", CHECKPOINT_AUTHOR_EMAIL),
                ("GIT_COMMITTER_NAME", CHECKPOINT_AUTHOR_NAME),
                ("GIT_COMMITTER_EMAIL", CHECKPOINT_AUTHOR_EMAIL),
                ("GIT_AUTHOR_DATE", commit_timestamp.as_str()),
                ("GIT_COMMITTER_DATE", commit_timestamp.as_str()),
            ],
            &metadata_json,
        )
        .await
    }
    .await;

    let _ = std::fs::remove_file(&temp_index);
    result
}

async fn apply_retention_policy(
    worktree_path: &str,
    thread_id: &str,
    config: &CheckpointConfig,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let checkpoints = list_checkpoints_for_thread(worktree_path, thread_id).await?;
    let refs_to_delete = refs_to_prune(&checkpoints, config, now);
    if refs_to_delete.is_empty() {
        return Ok(());
    }

    for git_ref in refs_to_delete {
        git_output(worktree_path, &["update-ref", "-d", &git_ref]).await?;
    }
    Ok(())
}

fn refs_to_prune(
    checkpoints: &[StoredCheckpoint],
    config: &CheckpointConfig,
    now: DateTime<Utc>,
) -> Vec<String> {
    let max_age_cutoff = config
        .max_age_days
        .map(|days| now - Duration::days(i64::try_from(days).unwrap_or(i64::MAX)));

    let mut kept = Vec::new();
    let mut pruned = Vec::new();
    for checkpoint in checkpoints {
        let too_old = max_age_cutoff.is_some_and(|cutoff| {
            DateTime::parse_from_rfc3339(&checkpoint.summary.timestamp)
                .map(|timestamp| timestamp.with_timezone(&Utc) < cutoff)
                .unwrap_or(false)
        });
        if too_old {
            pruned.push(checkpoint.summary.git_ref.clone());
        } else {
            kept.push(checkpoint.summary.git_ref.clone());
        }
    }

    if kept.len() > config.max_count {
        pruned.extend(kept.iter().skip(config.max_count).cloned());
    }

    pruned
}

async fn current_history_cursor(
    state: &Arc<AppState>,
    thread_id: &str,
    session_id: &str,
) -> Result<u64, String> {
    let history_root = {
        let chat = state.chat.lock().await;
        chat.history_root_path().to_path_buf()
    };
    let history_path = history_path_for_session(&history_root, thread_id, session_id);
    if !history_path.exists() {
        return Ok(0);
    }

    let file = fs::File::open(&history_path)
        .map_err(|err| format!("failed to open {}: {err}", history_path.display()))?;
    let reader = BufReader::new(file);
    let mut count = 0_u64;
    for line in reader.lines() {
        line.map_err(|err| format!("failed to read {}: {err}", history_path.display()))?;
        count += 1;
    }
    Ok(count)
}

async fn truncate_history_to_cursor(
    state: &Arc<AppState>,
    thread_id: &str,
    session_id: &str,
    cursor: u64,
) -> Result<(), String> {
    let history_root = {
        let chat = state.chat.lock().await;
        chat.history_root_path().to_path_buf()
    };
    let history_path = history_path_for_session(&history_root, thread_id, session_id);
    if !history_path.exists() {
        return Ok(());
    }

    let file = fs::File::open(&history_path)
        .map_err(|err| format!("failed to open {}: {err}", history_path.display()))?;
    let reader = BufReader::new(file);
    let kept_lines = reader
        .lines()
        .take(cursor as usize)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("failed to read {}: {err}", history_path.display()))?;

    let mut file = fs::File::create(&history_path)
        .map_err(|err| format!("failed to rewrite {}: {err}", history_path.display()))?;
    for line in kept_lines {
        file.write_all(line.as_bytes())
            .and_then(|_| file.write_all(b"\n"))
            .map_err(|err| format!("failed to truncate {}: {err}", history_path.display()))?;
    }
    file.sync_all()
        .map_err(|err| format!("failed to fsync {}: {err}", history_path.display()))
}

async fn delete_checkpoint_refs(worktree_path: &str, thread_id: &str) -> Result<(), String> {
    for checkpoint in list_checkpoints_for_thread(worktree_path, thread_id).await? {
        git_output(
            worktree_path,
            &["update-ref", "-d", &checkpoint.summary.git_ref],
        )
        .await?;
    }
    Ok(())
}

async fn git_is_busy(worktree_path: &str) -> Result<bool, String> {
    let git_dir = git_dir(worktree_path).await?;
    Ok(git_dir.join("rebase-merge").exists()
        || git_dir.join("MERGE_HEAD").exists()
        || git_dir.join("CHERRY_PICK_HEAD").exists()
        || git_dir.join("REVERT_HEAD").exists())
}

async fn git_dir(worktree_path: &str) -> Result<PathBuf, String> {
    let raw = git_output(worktree_path, &["rev-parse", "--git-dir"]).await?;
    let git_dir = PathBuf::from(raw);
    if git_dir.is_absolute() {
        Ok(git_dir)
    } else {
        Ok(Path::new(worktree_path).join(git_dir))
    }
}

async fn git_output(worktree_path: &str, args: &[&str]) -> Result<String, String> {
    git_command(worktree_path, args, &[], None).await
}

async fn git_output_env(
    worktree_path: &str,
    args: &[&str],
    env: &[(&str, &str)],
) -> Result<String, String> {
    git_command(worktree_path, args, env, None).await
}

async fn git_output_with_input(
    worktree_path: &str,
    args: &[&str],
    env: &[(&str, &str)],
    input: &str,
) -> Result<String, String> {
    git_command(worktree_path, args, env, Some(input)).await
}

async fn git_command(
    worktree_path: &str,
    args: &[&str],
    env: &[(&str, &str)],
    input: Option<&str>,
) -> Result<String, String> {
    let mut command = Command::new("git");
    command.arg("-C").arg(worktree_path).args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    if input.is_some() {
        command.stdin(std::process::Stdio::piped());
    }
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|err| format!("failed to run git {:?}: {err}", args))?;

    if let Some(input) = input {
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(input.as_bytes())
                .await
                .map_err(|err| format!("failed to write git {:?} stdin: {err}", args))?;
        }
    }

    let output = child
        .wait_with_output()
        .await
        .map_err(|err| format!("failed to wait for git {:?}: {err}", args))?;
    if !output.status.success() {
        return Err(format!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn checkpoint_ref_prefix(thread_id: &str) -> String {
    format!("{CHECKPOINT_REF_NAMESPACE}/{thread_id}")
}

fn checkpoint_ref(thread_id: &str, seq: u64) -> String {
    format!("{}/{seq}", checkpoint_ref_prefix(thread_id))
}

fn sanitize_optional_text(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint(seq: u64, timestamp: &str) -> StoredCheckpoint {
        StoredCheckpoint {
            _commit_oid: format!("oid-{seq}"),
            summary: protocol::CheckpointSummary {
                seq,
                git_ref: checkpoint_ref("thread-1", seq),
                timestamp: timestamp.to_string(),
                session_id: None,
                head_oid: format!("head-{seq}"),
                prompt_preview: None,
                message: None,
            },
            history_cursor: None,
        }
    }

    #[test]
    fn retention_prunes_oldest_and_expired_refs() {
        let checkpoints = vec![
            checkpoint(4, "2026-04-07T12:00:00Z"),
            checkpoint(3, "2026-04-06T12:00:00Z"),
            checkpoint(2, "2026-03-20T12:00:00Z"),
            checkpoint(1, "2026-03-10T12:00:00Z"),
        ];
        let config = CheckpointConfig {
            max_count: 2,
            max_age_days: Some(10),
        };

        let refs = refs_to_prune(
            &checkpoints,
            &config,
            DateTime::parse_from_rfc3339("2026-04-07T12:00:00Z")
                .expect("valid timestamp")
                .with_timezone(&Utc),
        );

        assert_eq!(
            refs,
            vec![checkpoint_ref("thread-1", 2), checkpoint_ref("thread-1", 1)]
        );
    }
}
