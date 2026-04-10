mod common;

use std::{
    env,
    ffi::OsString,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::json;
use tokio::process::Command;

async fn setup_test_server() -> common::TestHarness {
    common::setup_test_server().await
}

async fn add_project(harness: &mut common::TestHarness) -> (common::TestProject, String) {
    let project = common::create_git_project(None, false)
        .await
        .expect("create test git project");
    harness.register_cleanup_path(project.root_dir.clone());

    let added = harness
        .rpc(
            "project.add",
            json!({ "path": project.repo_path.to_string_lossy() }),
        )
        .await
        .expect("add project");

    let project_id = added["id"].as_str().expect("project id").to_string();
    (project, project_id)
}

async fn create_thread(
    harness: &mut common::TestHarness,
    project_id: &str,
) -> (String, String, String) {
    let created = harness
        .rpc(
            "thread.create",
            json!({
                "project_id": project_id,
                "name": common::unique_name("git-thread"),
                "source_type": "new_feature"
            }),
        )
        .await
        .expect("create thread");

    let thread_id = created["id"].as_str().expect("thread id").to_string();
    let branch = created["branch"]
        .as_str()
        .expect("thread branch")
        .to_string();
    let worktree_path = created["worktree_path"]
        .as_str()
        .expect("worktree path")
        .to_string();
    (thread_id, branch, worktree_path)
}

async fn wait_for_thread_ready(harness: &mut common::TestHarness, thread_id: &str) {
    loop {
        let event = harness
            .wait_for_event("thread.progress", Duration::from_secs(45))
            .await
            .expect("wait for thread.progress event");
        let params = &event["params"];
        if params["thread_id"] != thread_id {
            continue;
        }

        if let Some(error) = params["error"].as_str() {
            panic!("thread creation failed: {error}");
        }

        if params["step"] == "ready" {
            return;
        }
    }
}

async fn cleanup_thread_project(
    harness: &mut common::TestHarness,
    thread_id: &str,
    project_id: &str,
) {
    let _ = harness
        .rpc(
            "thread.close",
            json!({ "thread_id": thread_id, "mode": "close" }),
        )
        .await;
    let _ = harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await;
}

async fn run_git(repo_path: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    command.current_dir(repo_path).args(args);
    let output = command.output().await.expect("run git command");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn unique_temp_path(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ))
}

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set_var(key: &'static str, value: impl Into<OsString>) -> Self {
        let previous = env::var_os(key);
        #[allow(unused_unsafe)]
        unsafe {
            env::set_var(key, value.into());
        }
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        #[allow(unused_unsafe)]
        unsafe {
            match self.previous.take() {
                Some(value) => env::set_var(self.key, value),
                None => env::remove_var(self.key),
            }
        }
    }
}

fn install_fake_gh() -> (PathBuf, EnvVarGuard, EnvVarGuard) {
    let fake_bin_dir = unique_temp_path("fake-gh-bin");
    fs::create_dir_all(&fake_bin_dir).expect("create fake gh directory");

    let log_path = fake_bin_dir.join("gh.log");
    let gh_path = fake_bin_dir.join("gh");
    fs::write(
        &gh_path,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "{}"
if [ "$1" = "pr" ] && [ "$2" = "create" ]; then
  echo "https://github.com/example/repo/pull/42"
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  echo '{{"url":"https://github.com/example/repo/pull/42","number":42}}'
  exit 0
fi
echo "unexpected gh invocation: $*" >&2
exit 1
"#,
            log_path.display()
        ),
    )
    .expect("write fake gh script");
    let mut permissions = fs::metadata(&gh_path)
        .expect("stat fake gh script")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&gh_path, permissions).expect("chmod fake gh script");

    let previous_path = env::var_os("PATH").unwrap_or_default();
    let mut new_path = OsString::from(fake_bin_dir.as_os_str());
    if !previous_path.is_empty() {
        new_path.push(":");
        new_path.push(previous_path);
    }

    let path_guard = EnvVarGuard::set_var("PATH", new_path);
    let log_guard = EnvVarGuard::set_var("GH_TEST_LOG", log_path.as_os_str());
    (log_path, path_guard, log_guard)
}

#[tokio::test]
async fn git_status_summary_reports_branch_counts_and_file_states() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping git_status_summary_reports_branch_counts_and_file_states: tmux unavailable"
        );
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let (thread_id, branch, worktree_path) = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let worktree = PathBuf::from(&worktree_path);
    run_git(&worktree, &["push", "-u", "origin", &branch]).await;

    fs::write(worktree.join("ahead.txt"), "ahead\n").expect("write ahead file");
    run_git(&worktree, &["add", "ahead.txt"]).await;
    run_git(&worktree, &["commit", "-m", "ahead commit"]).await;

    fs::write(worktree.join("staged.txt"), "staged\n").expect("write staged file");
    run_git(&worktree, &["add", "staged.txt"]).await;

    let readme_path = worktree.join("README.md");
    fs::write(&readme_path, "readme staged\n").expect("write staged readme");
    run_git(&worktree, &["add", "README.md"]).await;
    fs::write(&readme_path, "readme unstaged too\n").expect("write unstaged readme");

    fs::write(worktree.join("untracked.txt"), "untracked\n").expect("write untracked file");

    let summary = harness
        .rpc("git.status_summary", json!({ "thread_id": thread_id }))
        .await
        .expect("git.status_summary succeeds");

    assert_eq!(summary["branch"], branch);
    assert_eq!(summary["ahead_count"], 1);
    assert_eq!(summary["behind_count"], 0);

    let files = summary["files"].as_array().expect("files array");
    let staged = files
        .iter()
        .find(|entry| entry["path"] == "staged.txt")
        .expect("staged file entry");
    assert_eq!(staged["staged"], true);
    assert_eq!(staged["unstaged"], false);
    assert_eq!(staged["untracked"], false);

    let mixed = files
        .iter()
        .find(|entry| entry["path"] == "README.md")
        .expect("mixed file entry");
    assert_eq!(mixed["staged"], true);
    assert_eq!(mixed["unstaged"], true);
    assert_eq!(mixed["untracked"], false);

    let untracked = files
        .iter()
        .find(|entry| entry["path"] == "untracked.txt")
        .expect("untracked file entry");
    assert_eq!(untracked["staged"], false);
    assert_eq!(untracked["unstaged"], false);
    assert_eq!(untracked["untracked"], true);

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn git_commit_can_limit_staged_paths() {
    if !common::tmux_available().await {
        eprintln!("skipping git_commit_can_limit_staged_paths: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let (thread_id, _branch, worktree_path) = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let worktree = PathBuf::from(&worktree_path);
    fs::write(worktree.join("first.txt"), "first\n").expect("write first file");
    fs::write(worktree.join("second.txt"), "second\n").expect("write second file");

    let commit = harness
        .rpc(
            "git.commit",
            json!({
                "thread_id": thread_id,
                "message": "add first file",
                "paths": ["first.txt"]
            }),
        )
        .await
        .expect("git.commit succeeds");

    let head = run_git(&worktree, &["rev-parse", "HEAD"]).await;
    assert_eq!(commit["commit_hash"], head);
    assert_eq!(commit["summary"], "add first file");

    let status = run_git(&worktree, &["status", "--porcelain=v1"]).await;
    assert_eq!(status, "?? second.txt");

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn git_commit_supports_amend() {
    if !common::tmux_available().await {
        eprintln!("skipping git_commit_supports_amend: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let (thread_id, _branch, worktree_path) = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let worktree = PathBuf::from(&worktree_path);
    fs::write(worktree.join("note.txt"), "initial\n").expect("write note file");
    let initial_commit = harness
        .rpc(
            "git.commit",
            json!({
                "thread_id": thread_id,
                "message": "initial note"
            }),
        )
        .await
        .expect("initial git.commit succeeds");

    fs::write(worktree.join("note.txt"), "amended\n").expect("rewrite note file");
    let amended_commit = harness
        .rpc(
            "git.commit",
            json!({
                "thread_id": thread_id,
                "message": "amended note",
                "amend": true
            }),
        )
        .await
        .expect("amended git.commit succeeds");

    assert_ne!(initial_commit["commit_hash"], amended_commit["commit_hash"]);
    assert_eq!(amended_commit["summary"], "amended note");
    assert_eq!(
        run_git(&worktree, &["log", "-1", "--format=%s"]).await,
        "amended note"
    );
    assert_eq!(
        run_git(&worktree, &["show", "HEAD:note.txt"]).await,
        "amended"
    );

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn git_push_sets_upstream_and_reports_remote_branch() {
    if !common::tmux_available().await {
        eprintln!("skipping git_push_sets_upstream_and_reports_remote_branch: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let (thread_id, branch, worktree_path) = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let worktree = PathBuf::from(&worktree_path);
    fs::write(worktree.join("push.txt"), "push me\n").expect("write push file");
    run_git(&worktree, &["add", "push.txt"]).await;
    run_git(&worktree, &["commit", "-m", "push commit"]).await;
    let expected_head = run_git(&worktree, &["rev-parse", "HEAD"]).await;

    let push = harness
        .rpc("git.push", json!({ "thread_id": thread_id }))
        .await
        .expect("git.push succeeds");

    assert_eq!(push["success"], true);
    assert_eq!(push["remote"], "origin");
    assert_eq!(push["branch"], branch);

    let upstream = run_git(
        &worktree,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    )
    .await;
    assert_eq!(upstream, format!("origin/{branch}"));

    let remote_head = run_git(&worktree, &["ls-remote", "--heads", "origin", &branch]).await;
    assert!(
        remote_head.starts_with(&expected_head),
        "expected remote branch to point at {expected_head}, got {remote_head}"
    );

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn git_create_pr_uses_gh_and_returns_metadata() {
    if !common::tmux_available().await {
        eprintln!("skipping git_create_pr_uses_gh_and_returns_metadata: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let (thread_id, _branch, _worktree_path) = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let (gh_log_path, _path_guard, _log_guard) = install_fake_gh();

    let pr = harness
        .rpc(
            "git.create_pr",
            json!({
                "thread_id": thread_id,
                "title": "Threadmill test PR",
                "body": "## Summary\n- test"
            }),
        )
        .await
        .expect("git.create_pr succeeds");

    assert_eq!(pr["pr_url"], "https://github.com/example/repo/pull/42");
    assert_eq!(pr["pr_number"], 42);

    let log = fs::read_to_string(&gh_log_path).expect("read gh invocation log");
    assert!(
        log.contains("pr create"),
        "expected pr create call, got: {log}"
    );
    assert!(
        log.contains("--title Threadmill test PR"),
        "expected title arg, got: {log}"
    );
    assert!(
        log.contains("--draft"),
        "expected default draft arg, got: {log}"
    );
    assert!(log.contains("pr view"), "expected pr view call, got: {log}");

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}
