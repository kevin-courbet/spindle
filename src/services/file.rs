use std::{
    cmp::Ordering,
    fs::{self, OpenOptions},
    io::Read,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
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
