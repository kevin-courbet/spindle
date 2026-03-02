use std::{
    cmp::Ordering,
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Read,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{protocol, AppState};
use tokio::process::Command;

const MAX_FILE_SIZE_BYTES: u64 = 5 * 1024 * 1024;

pub struct FileService;

impl FileService {
    pub async fn list(
        state: Arc<AppState>,
        params: protocol::FileListParams,
    ) -> Result<protocol::FileListResult, String> {
        let authorized = authorize_requested_path(state, &params.path).await?;
        let opened = open_authorized_path(&authorized)?;
        let metadata = opened
            .file
            .metadata()
            .map_err(|err| format!("failed to inspect {}: {err}", opened.canonical.display()))?;
        if !metadata.is_dir() {
            return Err(format!(
                "path is not a directory: {}",
                opened.canonical.display()
            ));
        }

        let read_dir = fs::read_dir(proc_fd_path(&opened.file))
            .map_err(|err| format!("failed to read {}: {err}", opened.canonical.display()))?;

        let mut entries = Vec::new();
        for entry in read_dir {
            let entry = entry.map_err(|err| format!("failed to read directory entry: {err}"))?;
            let metadata = entry
                .metadata()
                .map_err(|err| format!("failed to inspect {}: {err}", entry.path().display()))?;

            let is_directory = metadata.is_dir();
            let full_path = opened.canonical.join(entry.file_name());
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
        let authorized = authorize_requested_path(state, &params.path).await?;
        let mut opened = open_authorized_path(&authorized)?;
        let metadata = opened
            .file
            .metadata()
            .map_err(|err| format!("failed to inspect {}: {err}", opened.canonical.display()))?;

        if !metadata.is_file() {
            return Err(format!(
                "path is not a file: {}",
                opened.canonical.display()
            ));
        }

        let size = metadata.len();
        if size > MAX_FILE_SIZE_BYTES {
            return Err(format!(
                "file is larger than 5MB: {} bytes ({})",
                size,
                opened.canonical.display()
            ));
        }

        let mut bytes = Vec::with_capacity(size as usize);
        opened
            .file
            .read_to_end(&mut bytes)
            .map_err(|err| format!("failed to read {}: {err}", opened.canonical.display()))?;
        let content = String::from_utf8(bytes)
            .map_err(|_| format!("file is not valid UTF-8: {}", opened.canonical.display()))?;

        Ok(protocol::FileReadResult { content, size })
    }

    pub async fn git_status(
        state: Arc<AppState>,
        params: protocol::FileGitStatusParams,
    ) -> Result<protocol::FileGitStatusResult, String> {
        let authorized = authorize_requested_path(state, &params.path).await?;
        let metadata = fs::metadata(&authorized.canonical).map_err(|err| {
            format!(
                "failed to inspect {}: {err}",
                authorized.canonical.display()
            )
        })?;
        if !metadata.is_dir() {
            return Err(format!(
                "path is not a directory: {}",
                authorized.canonical.display()
            ));
        }

        let output = Command::new("git")
            .arg("-C")
            .arg(&authorized.canonical)
            .args(["status", "--porcelain=v1", "-uall"])
            .output()
            .await
            .map_err(|err| {
                format!(
                    "failed to run git status in {}: {err}",
                    authorized.canonical.display()
                )
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(format!(
                "git status failed in {}: {stderr}",
                authorized.canonical.display()
            ));
        }

        let stdout = String::from_utf8(output.stdout).map_err(|_| {
            format!(
                "git status output is not valid UTF-8 in {}",
                authorized.canonical.display()
            )
        })?;

        let mut entries = HashMap::new();
        for line in stdout.lines() {
            if line.len() < 4 {
                continue;
            }

            let Some(path) = parse_porcelain_path(line) else {
                continue;
            };
            let Some(status) = map_porcelain_status(&line[0..2]) else {
                continue;
            };

            entries.insert(path, status.to_string());
        }

        Ok(protocol::FileGitStatusResult { entries })
    }
}

fn map_porcelain_status(xy: &str) -> Option<&'static str> {
    match xy {
        " M" | "M " | "MM" => Some("modified"),
        "A " => Some("added"),
        "D " | " D" => Some("deleted"),
        "R " => Some("renamed"),
        "??" => Some("untracked"),
        "UU" | "AA" | "DD" => Some("conflicted"),
        _ => None,
    }
}

fn parse_porcelain_path(line: &str) -> Option<String> {
    let raw = line.get(3..)?;
    let path = raw
        .split_once(" -> ")
        .map(|(_, renamed_path)| renamed_path)
        .unwrap_or(raw);
    Some(path.to_string())
}

struct AuthorizedPath {
    requested: PathBuf,
    canonical: PathBuf,
    allowed_roots: Vec<PathBuf>,
}

struct OpenedAuthorizedPath {
    file: fs::File,
    canonical: PathBuf,
}

async fn authorize_requested_path(
    state: Arc<AppState>,
    requested_path: &str,
) -> Result<AuthorizedPath, String> {
    if requested_path.trim().is_empty() {
        return Err("path must not be empty".to_string());
    }

    let requested = PathBuf::from(requested_path);
    if !requested.is_absolute() {
        return Err("path must be absolute".to_string());
    }

    let canonical = fs::canonicalize(&requested)
        .map_err(|err| format!("failed to resolve {}: {err}", requested.display()))?;
    let allowed_roots = allowed_roots(state).await;

    if !allowed_roots
        .iter()
        .any(|root| path_is_within(&canonical, root))
    {
        return Err(format!(
            "path is outside known project/worktree roots: {}",
            canonical.display()
        ));
    }

    Ok(AuthorizedPath {
        requested,
        canonical,
        allowed_roots,
    })
}

fn open_authorized_path(authorized: &AuthorizedPath) -> Result<OpenedAuthorizedPath, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&authorized.requested)
        .map_err(|err| format!("failed to open {}: {err}", authorized.requested.display()))?;

    let opened_canonical = canonicalize_open_file(&file)?;
    if !authorized
        .allowed_roots
        .iter()
        .any(|root| path_is_within(&opened_canonical, root))
    {
        return Err(format!(
            "path is outside known project/worktree roots: {}",
            opened_canonical.display()
        ));
    }

    let recanonicalized = fs::canonicalize(&authorized.requested).map_err(|err| {
        format!(
            "failed to re-resolve {} after open: {err}",
            authorized.requested.display()
        )
    })?;

    if recanonicalized != authorized.canonical || opened_canonical != recanonicalized {
        return Err(format!(
            "path changed during access; refusing to read {}",
            authorized.requested.display()
        ));
    }

    Ok(OpenedAuthorizedPath {
        file,
        canonical: opened_canonical,
    })
}

fn canonicalize_open_file(file: &fs::File) -> Result<PathBuf, String> {
    fs::canonicalize(proc_fd_path(file))
        .map_err(|err| format!("failed to resolve opened file descriptor: {err}"))
}

fn proc_fd_path(file: &fs::File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
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
