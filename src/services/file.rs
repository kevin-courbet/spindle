use std::{
    cmp::Ordering,
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Read,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Component, Path, PathBuf},
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

            let read_dir = fs::read_dir(fd_path(&opened.file)?)
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

    pub async fn diff_summary(
        state: Arc<AppState>,
        params: protocol::FileDiffSummaryParams,
    ) -> Result<protocol::FileDiffSummaryResult, String> {
        let worktree_root = resolve_thread_worktree_root(state, &params.thread_id).await?;
        let path_filter = resolve_optional_repo_path(&worktree_root, params.path.as_deref())?;
        collect_summary_entries(&worktree_root, &params.scope, path_filter.as_deref()).await
    }

    pub async fn diff(
        state: Arc<AppState>,
        params: protocol::FileDiffParams,
    ) -> Result<protocol::FileDiffResult, String> {
        let worktree_root = resolve_thread_worktree_root(state, &params.thread_id).await?;
        let path_filter = resolve_optional_repo_path(&worktree_root, params.path.as_deref())?;

        let mut diff_text =
            scoped_diff_text(&worktree_root, &params.scope, path_filter.as_deref()).await?;
        if params.scope == protocol::FileDiffScope::Working {
            for untracked_path in
                list_untracked_paths(&worktree_root, path_filter.as_deref()).await?
            {
                diff_text.push_str(&untracked_diff_text(&worktree_root, &untracked_path).await?);
            }
        }

        let (added, removed) = diff_stats_from_text(&diff_text)?;

        Ok(protocol::FileDiffResult {
            path: path_filter.unwrap_or_else(|| ".".to_string()),
            status: if diff_text.is_empty() {
                "clean".to_string()
            } else {
                "modified".to_string()
            },
            diff_text,
            stats: protocol::FileDiffStats { added, removed },
        })
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

struct SummaryStatusEntry {
    path: String,
    status: String,
}

async fn resolve_thread_worktree_root(
    state: Arc<AppState>,
    thread_id: &str,
) -> Result<PathBuf, String> {
    let raw_worktree_path = {
        let store = state.store.lock().await;
        let thread = store
            .thread_by_id(thread_id)
            .ok_or_else(|| format!("thread not found: {thread_id}"))?;
        thread.worktree_path.clone()
    };

    run_blocking("file.resolve_thread_worktree", move || {
        let canonical = fs::canonicalize(&raw_worktree_path)
            .map_err(|err| format!("failed to resolve {}: {err}", raw_worktree_path))?;
        let metadata = fs::metadata(&canonical)
            .map_err(|err| format!("failed to inspect {}: {err}", canonical.display()))?;
        if !metadata.is_dir() {
            return Err(format!(
                "thread worktree is not a directory: {}",
                canonical.display()
            ));
        }
        Ok(canonical)
    })
    .await
}

fn resolve_optional_repo_path(
    worktree_root: &Path,
    path: Option<&str>,
) -> Result<Option<String>, String> {
    path.map(|path| normalize_repo_path(worktree_root, path))
        .transpose()
}

fn normalize_repo_path(worktree_root: &Path, path: &str) -> Result<String, String> {
    if path.trim().is_empty() {
        return Err("path must not be empty".to_string());
    }

    let raw_path = Path::new(path);
    let relative_path = if raw_path.is_absolute() {
        raw_path
            .strip_prefix(worktree_root)
            .map_err(|_| format!("path is outside thread worktree: {}", raw_path.display()))?
            .to_path_buf()
    } else {
        raw_path.to_path_buf()
    };

    normalize_relative_path(&relative_path)
}

fn normalize_relative_path(path: &Path) -> Result<String, String> {
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
                    "path must stay within thread worktree: {}",
                    path.display()
                ));
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        return Err("path must not be empty".to_string());
    }

    normalized
        .to_str()
        .map(|value| value.to_string())
        .ok_or_else(|| format!("path is not valid UTF-8: {}", normalized.display()))
}

async fn collect_summary_entries(
    worktree_root: &Path,
    scope: &protocol::FileDiffScope,
    path_filter: Option<&str>,
) -> Result<protocol::FileDiffSummaryResult, String> {
    let status_output = run_scoped_git_diff_bytes(
        worktree_root,
        scope,
        &["--name-status", "-z"],
        path_filter,
        "file.diff_summary --name-status",
    )
    .await?;
    let numstat_output = run_scoped_git_diff_bytes(
        worktree_root,
        scope,
        &["--numstat", "-z"],
        path_filter,
        "file.diff_summary --numstat",
    )
    .await?;

    let statuses = parse_name_status_entries_z(&status_output)?;
    let stats_by_path = parse_numstat_entries_z(&numstat_output)?;

    let mut files = statuses
        .into_iter()
        .map(|entry| -> Result<protocol::FileDiffSummaryEntry, String> {
            let (insertions, deletions) = stats_by_path.get(&entry.path).copied().unwrap_or((0, 0));
            Ok(protocol::FileDiffSummaryEntry {
                path: entry.path,
                status: entry.status,
                added: to_u32(insertions, "insertions")?,
                removed: to_u32(deletions, "deletions")?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;

    if *scope == protocol::FileDiffScope::Working {
        for untracked_path in list_untracked_paths(worktree_root, path_filter).await? {
            let (insertions, deletions) =
                untracked_diff_stats(worktree_root, &untracked_path).await?;
            files.push(protocol::FileDiffSummaryEntry {
                path: untracked_path,
                status: "added".to_string(),
                added: to_u32(insertions, "insertions")?,
                removed: to_u32(deletions, "deletions")?,
            });
        }
    }

    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(protocol::FileDiffSummaryResult { files })
}

async fn scoped_diff_text(
    worktree_root: &Path,
    scope: &protocol::FileDiffScope,
    path_filter: Option<&str>,
) -> Result<String, String> {
    let stdout = run_scoped_git_diff_bytes(
        worktree_root,
        scope,
        &["--no-ext-diff"],
        path_filter,
        "file.diff",
    )
    .await?;

    String::from_utf8(stdout).map_err(|_| {
        format!(
            "git diff output was not valid UTF-8 in {}",
            worktree_root.display()
        )
    })
}

async fn list_untracked_paths(
    worktree_root: &Path,
    path_filter: Option<&str>,
) -> Result<Vec<String>, String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(worktree_root)
        .args(["ls-files", "--others", "--exclude-standard", "-z"]);

    if let Some(path_filter) = path_filter {
        command.arg("--").arg(path_filter);
    }

    let output = command.output().await.map_err(|err| {
        format!(
            "failed to list untracked files in {}: {err}",
            worktree_root.display()
        )
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!(
            "git ls-files failed in {}: {stderr}",
            worktree_root.display()
        ));
    }

    let mut paths = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .map(|record| parse_utf8_path(record, "untracked path"))
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    Ok(paths)
}

async fn untracked_diff_stats(worktree_root: &Path, path: &str) -> Result<(u64, u64), String> {
    let stdout = run_no_index_git_diff_bytes(
        worktree_root,
        &["--numstat", "-z"],
        path,
        "file.diff_summary untracked --numstat",
    )
    .await?;

    Ok(parse_numstat_entries_z(&stdout)?
        .remove(path)
        .unwrap_or((0, 0)))
}

async fn untracked_diff_text(worktree_root: &Path, path: &str) -> Result<String, String> {
    let stdout = run_no_index_git_diff_bytes(
        worktree_root,
        &["--no-ext-diff", "--src-prefix=a/", "--dst-prefix=b/"],
        path,
        "file.diff untracked",
    )
    .await?;

    String::from_utf8(stdout).map_err(|_| {
        format!(
            "git diff output was not valid UTF-8 in {}",
            worktree_root.display()
        )
    })
}

async fn run_scoped_git_diff_bytes(
    worktree_root: &Path,
    scope: &protocol::FileDiffScope,
    extra_args: &[&str],
    path_filter: Option<&str>,
    operation: &str,
) -> Result<Vec<u8>, String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(worktree_root)
        .arg("diff")
        .arg("--find-renames");

    match scope {
        protocol::FileDiffScope::Working => {}
        protocol::FileDiffScope::Staged => {
            command.arg("--cached");
        }
        protocol::FileDiffScope::Head => {
            command.arg("@{upstream}..HEAD");
        }
    }

    command.args(extra_args);

    if let Some(path_filter) = path_filter {
        command.arg("--").arg(path_filter);
    }

    let output = command.output().await.map_err(|err| {
        format!(
            "failed to run git diff in {} for {operation}: {err}",
            worktree_root.display()
        )
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!(
            "git diff failed in {} for {operation}: {stderr}",
            worktree_root.display()
        ));
    }

    Ok(output.stdout)
}

async fn run_no_index_git_diff_bytes(
    worktree_root: &Path,
    extra_args: &[&str],
    path: &str,
    operation: &str,
) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_root)
        .arg("diff")
        .arg("--no-index")
        .args(extra_args)
        .arg("--")
        .arg("/dev/null")
        .arg(path)
        .output()
        .await
        .map_err(|err| {
            format!(
                "failed to run git diff in {} for {operation}: {err}",
                worktree_root.display()
            )
        })?;

    if !(output.status.success() || output.status.code() == Some(1)) {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!(
            "git diff failed in {} for {operation}: {stderr}",
            worktree_root.display()
        ));
    }

    Ok(output.stdout)
}

fn parse_name_status_entries_z(stdout: &[u8]) -> Result<Vec<SummaryStatusEntry>, String> {
    let mut entries = Vec::new();
    let mut fields = stdout
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());

    while let Some(raw_status) = fields.next() {
        let raw_status = std::str::from_utf8(raw_status)
            .map_err(|_| "diff status is not valid UTF-8".to_string())?;
        let code = raw_status
            .chars()
            .next()
            .ok_or_else(|| "diff status must not be empty".to_string())?;
        let status = map_summary_status(code)
            .ok_or_else(|| format!("unsupported diff status '{raw_status}'"))?;

        let path = match code {
            'R' | 'C' => {
                let _old_path = fields
                    .next()
                    .ok_or_else(|| format!("missing old rename path for status '{raw_status}'"))?;
                let new_path = fields
                    .next()
                    .ok_or_else(|| format!("missing new rename path for status '{raw_status}'"))?;
                parse_utf8_path(new_path, "renamed path")?
            }
            _ => {
                let path = fields
                    .next()
                    .ok_or_else(|| format!("missing path for status '{raw_status}'"))?;
                parse_utf8_path(path, "diff path")?
            }
        };

        entries.push(SummaryStatusEntry {
            path,
            status: status.to_string(),
        });
    }

    Ok(entries)
}

fn map_summary_status(code: char) -> Option<&'static str> {
    match code {
        'A' => Some("added"),
        'D' => Some("deleted"),
        'R' | 'C' => Some("renamed"),
        'M' | 'T' | 'U' | 'X' | 'B' => Some("modified"),
        _ => None,
    }
}

fn parse_numstat_entries_z(stdout: &[u8]) -> Result<HashMap<String, (u64, u64)>, String> {
    let mut entries = HashMap::new();
    let mut fields = stdout
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());

    while let Some(record) = fields.next() {
        let mut parts = record.splitn(3, |byte| *byte == b'\t');
        let insertions_raw = parts
            .next()
            .ok_or_else(|| "missing insertion count".to_string())?;
        let deletions_raw = parts
            .next()
            .ok_or_else(|| "missing deletion count".to_string())?;
        let path_field = parts
            .next()
            .ok_or_else(|| "missing numstat path".to_string())?;

        let path = if path_field.is_empty() {
            let _old_path = fields
                .next()
                .ok_or_else(|| "missing old rename path for numstat entry".to_string())?;
            let new_path = fields
                .next()
                .ok_or_else(|| "missing new rename path for numstat entry".to_string())?;
            parse_utf8_path(new_path, "renamed numstat path")?
        } else {
            parse_utf8_path(path_field, "numstat path")?
        };

        let insertions = parse_numstat_count(insertions_raw)?;
        let deletions = parse_numstat_count(deletions_raw)?;
        entries.insert(path, (insertions, deletions));
    }

    Ok(entries)
}

fn parse_numstat_count(raw: &[u8]) -> Result<u64, String> {
    if raw == b"-" {
        return Ok(0);
    }

    let raw =
        std::str::from_utf8(raw).map_err(|_| "numstat count is not valid UTF-8".to_string())?;
    raw.parse::<u64>()
        .map_err(|err| format!("invalid numstat count '{raw}': {err}"))
}

fn parse_utf8_path(bytes: &[u8], context: &str) -> Result<String, String> {
    std::str::from_utf8(bytes)
        .map(|value| value.to_string())
        .map_err(|_| format!("{context} is not valid UTF-8"))
}

fn diff_stats_from_text(diff_text: &str) -> Result<(u32, u32), String> {
    let mut added = 0_u32;
    let mut removed = 0_u32;

    for line in diff_text.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added = added
                .checked_add(1)
                .ok_or_else(|| "diff added line count overflowed u32".to_string())?;
        } else if line.starts_with('-') {
            removed = removed
                .checked_add(1)
                .ok_or_else(|| "diff removed line count overflowed u32".to_string())?;
        }
    }

    Ok((added, removed))
}

fn to_u32(value: u64, label: &str) -> Result<u32, String> {
    u32::try_from(value).map_err(|_| format!("{label} count exceeds u32: {value}"))
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
    let path = fd_path(file)?;
    fs::canonicalize(path)
        .map_err(|err| format!("failed to resolve opened file descriptor: {err}"))
}

#[cfg(target_os = "linux")]
fn fd_path(file: &fs::File) -> Result<PathBuf, String> {
    Ok(PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd())))
}

#[cfg(target_os = "macos")]
fn fd_path(file: &fs::File) -> Result<PathBuf, String> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    // MAXPATHLEN on macOS is 1024; allocate a full buffer and trim trailing NULs.
    const MAXPATHLEN: usize = 1024;
    let mut buf = vec![0u8; MAXPATHLEN];
    let ret = unsafe {
        libc::fcntl(
            file.as_raw_fd(),
            libc::F_GETPATH,
            buf.as_mut_ptr() as *mut libc::c_void,
        )
    };
    if ret != 0 {
        return Err(format!(
            "fcntl F_GETPATH failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let nul_pos = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    buf.truncate(nul_pos);
    Ok(PathBuf::from(OsString::from_vec(buf)))
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

    #[test]
    fn parse_name_status_entries_z_uses_new_path_for_renames() {
        let parsed = parse_name_status_entries_z(b"R100\0old name.txt\0new name.txt\0")
            .expect("parse output");

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].path, "new name.txt");
        assert_eq!(parsed[0].status, "renamed");
    }

    #[test]
    fn parse_numstat_entries_z_handles_binary_and_renamed_paths() {
        let data = [
            b"-\t-\timage.bin\0".as_slice(),
            b"1\t0\t\0README.md\0README-renamed.md\0".as_slice(),
        ]
        .concat();

        let parsed = parse_numstat_entries_z(&data).expect("parse output");

        assert_eq!(parsed.get("image.bin"), Some(&(0, 0)));
        assert_eq!(parsed.get("README-renamed.md"), Some(&(1, 0)));
    }

    #[test]
    fn normalize_relative_path_rejects_escape() {
        let error = normalize_relative_path(Path::new("../README.md")).expect_err("reject escape");

        assert!(error.contains("escapes thread worktree"));
    }
}
