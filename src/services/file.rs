use std::{
    cmp::Ordering,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{protocol, AppState};

const MAX_FILE_SIZE_BYTES: u64 = 5 * 1024 * 1024;

pub struct FileService;

impl FileService {
    pub async fn list(
        state: Arc<AppState>,
        params: protocol::FileListParams,
    ) -> Result<protocol::FileListResult, String> {
        let directory = resolve_authorized_path(state, &params.path).await?;
        if !directory.is_dir() {
            return Err(format!("path is not a directory: {}", directory.display()));
        }

        let read_dir = fs::read_dir(&directory)
            .map_err(|err| format!("failed to read {}: {err}", directory.display()))?;

        let mut entries = Vec::new();
        for entry in read_dir {
            let entry = entry.map_err(|err| format!("failed to read directory entry: {err}"))?;
            let metadata = entry
                .metadata()
                .map_err(|err| format!("failed to inspect {}: {err}", entry.path().display()))?;

            let is_directory = metadata.is_dir();
            let full_path = entry.path();
            let full_path_str = full_path
                .to_str()
                .ok_or_else(|| format!("invalid utf-8 path: {}", full_path.display()))?
                .to_string();

            entries.push(protocol::FileEntry {
                name: entry.file_name().to_string_lossy().to_string(),
                path: full_path_str,
                is_directory,
                size: if is_directory { 0 } else { metadata.len() },
            });
        }

        entries.sort_by(
            |left, right| match (left.is_directory, right.is_directory) {
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                _ => left
                    .name
                    .to_lowercase()
                    .cmp(&right.name.to_lowercase())
                    .then_with(|| left.name.cmp(&right.name)),
            },
        );

        Ok(protocol::FileListResult { entries })
    }

    pub async fn read(
        state: Arc<AppState>,
        params: protocol::FileReadParams,
    ) -> Result<protocol::FileReadResult, String> {
        let path = resolve_authorized_path(state, &params.path).await?;
        let metadata = fs::metadata(&path)
            .map_err(|err| format!("failed to inspect {}: {err}", path.display()))?;

        if !metadata.is_file() {
            return Err(format!("path is not a file: {}", path.display()));
        }

        let size = metadata.len();
        if size > MAX_FILE_SIZE_BYTES {
            return Err(format!(
                "file is larger than 5MB: {} bytes ({})",
                size,
                path.display()
            ));
        }

        let bytes =
            fs::read(&path).map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        let content = String::from_utf8(bytes)
            .map_err(|_| format!("file is not valid UTF-8: {}", path.display()))?;

        Ok(protocol::FileReadResult { content, size })
    }
}

async fn resolve_authorized_path(
    state: Arc<AppState>,
    requested_path: &str,
) -> Result<PathBuf, String> {
    if requested_path.trim().is_empty() {
        return Err("path must not be empty".to_string());
    }

    let requested = PathBuf::from(requested_path);
    if !requested.is_absolute() {
        return Err("path must be absolute".to_string());
    }

    let canonical_requested = fs::canonicalize(&requested)
        .map_err(|err| format!("failed to resolve {}: {err}", requested.display()))?;
    let allowed_roots = allowed_roots(state).await;

    if allowed_roots
        .iter()
        .any(|root| path_is_within(&canonical_requested, root))
    {
        return Ok(canonical_requested);
    }

    Err(format!(
        "path is outside known project/worktree roots: {}",
        canonical_requested.display()
    ))
}

async fn allowed_roots(state: Arc<AppState>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let store = state.store.lock().await;

    for project in &store.data.projects {
        if let Ok(path) = fs::canonicalize(&project.path) {
            roots.push(path);
        }
    }

    for thread in &store.data.threads {
        if let Ok(path) = fs::canonicalize(&thread.worktree_path) {
            roots.push(path);
        }
    }

    roots
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}
