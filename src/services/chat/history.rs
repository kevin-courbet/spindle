use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

use crate::protocol;

use super::runtime::ChatSessionRuntime;

const CHAT_HISTORY_BATCH_SIZE: usize = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct PersistedChatSessionMetadata {
    pub(super) acp_session_id: Option<String>,
}

pub(crate) fn history_path_for_session(
    history_root: &Path,
    thread_id: &str,
    session_id: &str,
) -> PathBuf {
    history_root
        .join(thread_id)
        .join(format!("{session_id}.jsonl"))
}

pub(super) fn history_metadata_path_for_session(
    history_root: &Path,
    thread_id: &str,
    session_id: &str,
) -> PathBuf {
    history_root
        .join(thread_id)
        .join(format!("{session_id}.metadata.json"))
}

fn restored_session_marker_path_for_session(
    history_root: &Path,
    thread_id: &str,
    session_id: &str,
) -> PathBuf {
    history_root
        .join(thread_id)
        .join(format!("{session_id}.restore-session-new"))
}

pub(super) fn persist_restored_session_marker(
    history_root: &Path,
    thread_id: &str,
    session_id: &str,
) -> Result<(), String> {
    let marker_path = restored_session_marker_path_for_session(history_root, thread_id, session_id);
    let parent = marker_path.parent().ok_or_else(|| {
        format!(
            "invalid restored session marker path: {}",
            marker_path.display()
        )
    })?;
    fs::create_dir_all(parent)
        .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;

    let mut file = fs::File::create(&marker_path)
        .map_err(|err| format!("failed to create {}: {err}", marker_path.display()))?;
    file.write_all(b"session/new\n")
        .map_err(|err| format!("failed to write {}: {err}", marker_path.display()))?;
    file.sync_all()
        .map_err(|err| format!("failed to fsync {}: {err}", marker_path.display()))
}

pub(super) fn restored_session_marker_exists(
    history_root: &Path,
    thread_id: &str,
    session_id: &str,
) -> Result<bool, String> {
    restored_session_marker_path_for_session(history_root, thread_id, session_id)
        .try_exists()
        .map_err(|err| {
            format!(
                "failed to stat restored session marker for {}: {err}",
                session_id
            )
        })
}

pub(super) fn clear_restored_session_marker(
    history_root: &Path,
    thread_id: &str,
    session_id: &str,
) -> Result<(), String> {
    let marker_path = restored_session_marker_path_for_session(history_root, thread_id, session_id);
    match fs::remove_file(&marker_path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("failed to remove {}: {err}", marker_path.display())),
    }
}

pub(crate) fn persist_session_metadata(
    history_root: &Path,
    thread_id: &str,
    session_id: &str,
    acp_session_id: Option<&str>,
) -> Result<(), String> {
    let metadata_path = history_metadata_path_for_session(history_root, thread_id, session_id);
    let parent = metadata_path.parent().ok_or_else(|| {
        format!(
            "invalid chat session metadata path: {}",
            metadata_path.display()
        )
    })?;
    fs::create_dir_all(parent)
        .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;

    let metadata = PersistedChatSessionMetadata {
        acp_session_id: acp_session_id.map(ToOwned::to_owned),
    };
    let mut file = fs::File::create(&metadata_path)
        .map_err(|err| format!("failed to create {}: {err}", metadata_path.display()))?;
    serde_json::to_writer(&mut file, &metadata)
        .map_err(|err| format!("failed to encode {}: {err}", metadata_path.display()))?;
    file.write_all(b"\n")
        .map_err(|err| format!("failed to finalize {}: {err}", metadata_path.display()))?;
    file.sync_all()
        .map_err(|err| format!("failed to fsync {}: {err}", metadata_path.display()))
}

pub(super) fn read_persisted_session_metadata(
    history_root: &Path,
    thread_id: &str,
    session_id: &str,
) -> Result<Option<PersistedChatSessionMetadata>, String> {
    let metadata_path = history_metadata_path_for_session(history_root, thread_id, session_id);
    if !metadata_path.exists() {
        return Ok(None);
    }

    let file = fs::File::open(&metadata_path)
        .map_err(|err| format!("failed to open {}: {err}", metadata_path.display()))?;
    let reader = BufReader::new(file);
    serde_json::from_reader(reader)
        .map(Some)
        .map_err(|err| format!("failed to parse {}: {err}", metadata_path.display()))
}

fn read_history_acp_session_id(history_path: &Path) -> Option<String> {
    fs::File::open(history_path).ok().and_then(|file| {
        let reader = BufReader::new(file);
        reader
            .lines()
            .next()?
            .ok()
            .and_then(|line| serde_json::from_str::<Value>(&line).ok())
            .and_then(|value| value.get("sessionId")?.as_str().map(ToOwned::to_owned))
    })
}

pub(super) fn is_safe_history_component(value: &str) -> bool {
    !value.is_empty() && !value.contains('/') && !value.contains('\\') && !value.contains("..")
}

pub(super) fn count_lines(path: &Path) -> Result<u64, String> {
    let file =
        fs::File::open(path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    let reader = BufReader::new(file);
    let mut count = 0_u64;
    for line in reader.lines() {
        line.map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        count += 1;
    }
    Ok(count)
}

pub(super) fn append_updates_to_history(
    history_path: &Path,
    updates: &[Value],
) -> Result<(), String> {
    if updates.is_empty() {
        return Ok(());
    }

    let parent = history_path
        .parent()
        .ok_or_else(|| format!("invalid chat history path: {}", history_path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(history_path)
        .map_err(|err| format!("failed to open {}: {err}", history_path.display()))?;

    for update in updates {
        serde_json::to_writer(&mut file, update)
            .map_err(|err| format!("failed to encode history update: {err}"))?;
        file.write_all(b"\n").map_err(|err| {
            format!(
                "failed to append newline to {}: {err}",
                history_path.display()
            )
        })?;
    }

    file.sync_data()
        .map_err(|err| format!("failed to fsync {}: {err}", history_path.display()))
}

pub(super) fn read_history_page(
    history_path: &Path,
    cursor: Option<u64>,
) -> Result<protocol::ChatHistoryResult, String> {
    if !history_path.exists() {
        return Ok(protocol::ChatHistoryResult {
            updates: Vec::new(),
            next_cursor: None,
        });
    }

    let start_idx = cursor.unwrap_or(0) as usize;
    let file = fs::File::open(history_path)
        .map_err(|err| format!("failed to open {}: {err}", history_path.display()))?;
    let reader = BufReader::new(file);

    let mut updates = Vec::new();
    let mut line_index = 0_usize;
    let mut has_more = false;

    for line in reader.lines() {
        let line =
            line.map_err(|err| format!("failed to read {}: {err}", history_path.display()))?;
        if line_index < start_idx {
            line_index += 1;
            continue;
        }

        if updates.len() >= CHAT_HISTORY_BATCH_SIZE {
            has_more = true;
            break;
        }

        match serde_json::from_str::<Value>(&line) {
            Ok(value) => updates.push(value),
            Err(error) => {
                warn!(
                    path = %history_path.display(),
                    line_index,
                    error = %error,
                    "failed to parse chat history line"
                );
            }
        }
        line_index += 1;
    }

    let next_cursor = if has_more {
        Some(start_idx as u64 + CHAT_HISTORY_BATCH_SIZE as u64)
    } else {
        None
    };

    Ok(protocol::ChatHistoryResult {
        updates,
        next_cursor,
    })
}

pub(super) fn discover_history_sessions(
    history_root: &Path,
    known_threads: &HashSet<String>,
) -> Result<Vec<ChatSessionRuntime>, String> {
    if !history_root.exists() {
        return Ok(Vec::new());
    }

    let mut recovered = Vec::new();
    let thread_dirs = fs::read_dir(history_root)
        .map_err(|err| format!("failed to read {}: {err}", history_root.display()))?;

    for thread_dir in thread_dirs {
        let thread_dir =
            thread_dir.map_err(|err| format!("failed to read chat thread dir entry: {err}"))?;
        let file_type = thread_dir
            .file_type()
            .map_err(|err| format!("failed to inspect {}: {err}", thread_dir.path().display()))?;
        if !file_type.is_dir() {
            continue;
        }

        let thread_id = thread_dir.file_name().to_string_lossy().to_string();
        if !known_threads.contains(&thread_id) {
            continue;
        }

        let entries = fs::read_dir(thread_dir.path())
            .map_err(|err| format!("failed to read {}: {err}", thread_dir.path().display()))?;
        for entry in entries {
            let entry =
                entry.map_err(|err| format!("failed to read chat history file entry: {err}"))?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }

            let Some(stem) = path.file_stem().and_then(|name| name.to_str()) else {
                continue;
            };
            let created_at = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .map(chrono::DateTime::<Utc>::from)
                .unwrap_or_else(Utc::now)
                .to_rfc3339();

            let acp_session_id =
                match restored_session_marker_exists(history_root, &thread_id, stem) {
                    Ok(true) => None,
                    Ok(false) => {
                        match read_persisted_session_metadata(history_root, &thread_id, stem) {
                            Ok(Some(metadata)) => metadata.acp_session_id,
                            Ok(None) => read_history_acp_session_id(&path),
                            Err(error) => {
                                warn!(
                                    thread_id,
                                    session_id = stem,
                                    error = %error,
                                    "failed to read persisted chat session metadata"
                                );
                                None
                            }
                        }
                    }
                    Err(error) => {
                        warn!(
                            thread_id,
                            session_id = stem,
                            error = %error,
                            "failed to read restored chat session marker"
                        );
                        None
                    }
                };

            let summary = protocol::ChatSessionSummary {
                session_id: stem.to_string(),
                agent_type: "unknown".to_string(),
                status: protocol::ChatSessionStatus::Ended,
                agent_status: protocol::AgentStatus::Idle,
                worker_count: 0,
                title: None,
                model_id: None,
                created_at,
                display_name: None,
                parent_session_id: None,
            };
            let mut runtime = ChatSessionRuntime::new(summary, thread_id.clone(), path);
            runtime.acp_session_id = acp_session_id;
            runtime.ended_emitted = true;
            recovered.push(runtime);
        }
    }

    Ok(recovered)
}

pub(super) fn remove_history_file(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }

    fs::remove_file(path).map_err(|err| format!("failed to remove {}: {err}", path.display()))
}

pub(super) fn remove_thread_history_dir(
    history_root: &Path,
    thread_id: &str,
) -> Result<(), String> {
    let thread_dir = history_root.join(thread_id);
    if !thread_dir.exists() {
        return Ok(());
    }

    fs::remove_dir_all(&thread_dir)
        .map_err(|err| format!("failed to remove {}: {err}", thread_dir.display()))
}
