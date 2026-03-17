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
use tokio::{process::Command, task};

const MAX_FILE_SIZE_BYTES: u64 = 5 * 1024 * 1024;

pub struct FileService;

impl FileService {
    pub async fn list(
        state: Arc<AppState>,
        params: protocol::FileListParams,
    ) -> Result<protocol::FileListResult, String> {
        let authorized = authorize_requested_path(state, &params.path).await?;
        run_blocking("file.list", move || {
            let opened = open_authorized_path(&authorized)?;
            let metadata = opened.file.metadata().map_err(|err| {
                format!("failed to inspect {}: {err}", opened.canonical.display())
            })?;
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
                let entry =
                    entry.map_err(|err| format!("failed to read directory entry: {err}"))?;
                let file_name = entry.file_name();
                let full_path = opened.canonical.join(&file_name);
                let metadata = match fs::metadata(&full_path) {
                    Ok(metadata) => match fs::symlink_metadata(&full_path) {
                        Ok(link_metadata) if link_metadata.file_type().is_symlink() => None,
                        Ok(_) => Some(metadata),
                        Err(link_err) if link_err.kind() == std::io::ErrorKind::NotFound => None,
                        Err(link_err) => {
                            return Err(format!(
                                "failed to inspect metadata for {}: {link_err}",
                                full_path.display()
                            ));
                        }
                    },
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        match fs::symlink_metadata(&full_path) {
                            Ok(metadata) if metadata.file_type().is_symlink() => None,
                            Ok(metadata) => Some(metadata),
                            Err(link_err) if link_err.kind() == std::io::ErrorKind::NotFound => {
                                None
                            }
                            Err(link_err) => {
                                return Err(format!(
                                    "failed to inspect metadata for {}: {link_err}",
                                    full_path.display()
                                ));
                            }
                        }
                    }
                    Err(err) => {
                        return Err(format!(
                            "failed to inspect metadata for {}: {err}",
                            full_path.display()
                        ));
                    }
                };

                let (is_directory, size) = match metadata {
                    Some(metadata) => {
                        let is_directory = metadata.is_dir();
                        let size = if is_directory { 0 } else { metadata.len() };
                        (is_directory, size)
                    }
                    None => (false, 0),
                };
                let full_path_str = full_path
                    .to_str()
                    .ok_or_else(|| format!("invalid utf-8 path: {}", full_path.display()))?
                    .to_string();

                entries.push(protocol::FileEntry {
                    name: file_name.to_string_lossy().to_string(),
                    path: full_path_str,
                    is_directory,
                    size,
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
        })
        .await
    }

    pub async fn read(
        state: Arc<AppState>,
        params: protocol::FileReadParams,
    ) -> Result<protocol::FileReadResult, String> {
        let authorized = authorize_requested_path(state, &params.path).await?;
        run_blocking("file.read", move || {
            let mut opened = open_authorized_path(&authorized)?;
            let metadata = opened.file.metadata().map_err(|err| {
                format!("failed to inspect {}: {err}", opened.canonical.display())
            })?;

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
        })
        .await
    }

    pub async fn git_status(
        state: Arc<AppState>,
        params: protocol::FileGitStatusParams,
    ) -> Result<protocol::FileGitStatusResult, String> {
        let authorized = authorize_requested_path(state, &params.path).await?;
        let canonical_for_check = authorized.canonical.clone();
        run_blocking("file.git_status.metadata", move || {
            let metadata = fs::metadata(&canonical_for_check).map_err(|err| {
                format!("failed to inspect {}: {err}", canonical_for_check.display())
            })?;
            if !metadata.is_dir() {
                return Err(format!(
                    "path is not a directory: {}",
                    canonical_for_check.display()
                ));
            }
            Ok(())
        })
        .await?;

        let output = Command::new("git")
            .arg("-C")
            .arg(&authorized.canonical)
            .args(["status", "--porcelain=v1", "-z", "-uall"])
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

        let entries = parse_porcelain_entries(&output.stdout).map_err(|err| {
            format!(
                "failed to parse git status output in {}: {err}",
                authorized.canonical.display()
            )
        })?;

        Ok(protocol::FileGitStatusResult { entries })
    }
}

async fn run_blocking<T, F>(operation: &'static str, blocking_fn: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    task::spawn_blocking(blocking_fn)
        .await
        .map_err(|err| format!("{operation} task failed: {err}"))?
}

fn map_porcelain_status(xy: &str) -> Option<&'static str> {
    if xy == "??" {
        return Some("untracked");
    }

    let bytes = xy.as_bytes();
    if bytes.len() != 2 {
        return None;
    }

    let x = bytes[0] as char;
    let y = bytes[1] as char;

    if (matches!(x, 'U' | 'A' | 'D') && y == 'U') || (x == 'U' && matches!(y, 'U' | 'A' | 'D')) {
        return Some("conflicted");
    }

    if matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C') {
        return Some("renamed");
    }

    if x == 'D' || y == 'D' {
        return Some("deleted");
    }

    if x == 'A' || y == 'A' {
        return Some("added");
    }

    if x == 'M' || y == 'M' || x == 'T' || y == 'T' {
        return Some("modified");
    }

    None
}

fn parse_porcelain_entries(stdout: &[u8]) -> Result<HashMap<String, String>, String> {
    let mut entries = HashMap::new();
    let mut records = stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty());

    while let Some(record) = records.next() {
        if record.len() < 4 || record[2] != b' ' {
            continue;
        }

        let xy = std::str::from_utf8(&record[0..2])
            .map_err(|_| "status marker is not valid UTF-8".to_string())?;
        let Some(status) = map_porcelain_status(xy) else {
            continue;
        };

        let path = String::from_utf8(record[3..].to_vec())
            .map_err(|_| "path is not valid UTF-8".to_string())?;
        entries.insert(path, status.to_string());

        if xy
            .as_bytes()
            .iter()
            .any(|byte| *byte == b'R' || *byte == b'C')
        {
            let _ = records.next();
        }
    }

    Ok(entries)
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

    let requested_for_resolve = requested.clone();
    let canonical = run_blocking("file.authorize.canonicalize", move || {
        fs::canonicalize(&requested_for_resolve).map_err(|err| {
            format!(
                "failed to resolve {}: {err}",
                requested_for_resolve.display()
            )
        })
    })
    .await?;
    let allowed_roots = allowed_roots(state).await?;

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

async fn allowed_roots(state: Arc<AppState>) -> Result<Vec<PathBuf>, String> {
    let candidate_paths: Vec<PathBuf> = {
        let store = state.store.lock().await;
        store
            .data
            .projects
            .iter()
            .map(|project| PathBuf::from(&project.path))
            .chain(
                store
                    .data
                    .threads
                    .iter()
                    .map(|thread| PathBuf::from(&thread.worktree_path)),
            )
            .collect()
    };

    run_blocking("file.allowed_roots", move || {
        let mut roots = Vec::new();
        for candidate in candidate_paths {
            if let Ok(path) = fs::canonicalize(candidate) {
                roots.push(path);
            }
        }
        Ok(roots)
    })
    .await
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_porcelain_entries_handles_quoted_paths_without_escaping() {
        let stdout = b"?? dir with spaces/file \"quoted\" name.txt\0";

        let parsed = parse_porcelain_entries(stdout).expect("parse output");

        assert_eq!(
            parsed.get("dir with spaces/file \"quoted\" name.txt"),
            Some(&"untracked".to_string())
        );
    }

    #[test]
    fn parse_porcelain_entries_handles_rename_records() {
        let stdout = b"R  new name.txt\0old name.txt\0";

        let parsed = parse_porcelain_entries(stdout).expect("parse output");

        assert_eq!(parsed.get("new name.txt"), Some(&"renamed".to_string()));
        assert!(!parsed.contains_key("old name.txt"));
    }
}
