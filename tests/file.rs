mod common;

use std::{fs, process::Command};

use serde_json::json;

async fn setup_test_server() -> common::TestHarness {
    common::setup_test_server().await
}

#[tokio::test]
async fn file_list_and_read_enforce_workspace_rules() {
    let mut harness = setup_test_server().await;
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

    let fixture_path = project.repo_path.join("file-browser-fixture");
    fs::create_dir_all(fixture_path.join("src")).expect("create src fixture dir");
    fs::create_dir_all(fixture_path.join("ZDir")).expect("create second fixture dir");
    fs::write(fixture_path.join("a.txt"), "alpha\n").expect("write text fixture");
    fs::write(fixture_path.join("B.txt"), "bravo\n").expect("write second text fixture");
    fs::write(fixture_path.join("binary.bin"), vec![0xFF, 0xFE, 0x00])
        .expect("write binary fixture");
    fs::write(
        fixture_path.join("large.txt"),
        vec![b'x'; 5 * 1024 * 1024 + 1],
    )
    .expect("write large fixture");

    // Spindle canonicalises paths before returning entries (TOCTOU hardening).
    // On macOS, `/var/folders/…` resolves to `/private/var/folders/…`, so
    // compare against the canonicalised fixture path to stay platform-agnostic.
    let fixture_path = fs::canonicalize(&fixture_path).expect("canonicalise fixture path");

    let listed = harness
        .rpc(
            "file.list",
            json!({ "path": fixture_path.to_string_lossy() }),
        )
        .await
        .expect("list fixture directory");

    let entries = listed["entries"]
        .as_array()
        .expect("file.list returns entries array");
    let names: Vec<&str> = entries
        .iter()
        .map(|entry| entry["name"].as_str().expect("entry name"))
        .collect();
    assert_eq!(
        names,
        vec!["src", "ZDir", "a.txt", "B.txt", "binary.bin", "large.txt"],
        "directories should be listed first, then files sorted case-insensitively"
    );
    assert_eq!(entries[0]["isDirectory"], true);
    assert_eq!(
        entries[0]["path"],
        fixture_path.join("src").to_string_lossy().to_string()
    );

    let read_text = harness
        .rpc(
            "file.read",
            json!({ "path": fixture_path.join("a.txt").to_string_lossy() }),
        )
        .await
        .expect("read text file");
    assert_eq!(read_text["content"], "alpha\n");
    assert_eq!(read_text["size"], 6);

    let symlink_path = fixture_path.join("a-link.txt");
    std::os::unix::fs::symlink(fixture_path.join("a.txt"), &symlink_path)
        .expect("create symlink fixture");
    let symlink_error = harness
        .rpc_expect_error(
            "file.read",
            json!({ "path": symlink_path.to_string_lossy() }),
        )
        .await;
    assert!(
        symlink_error.to_lowercase().contains("symbolic link")
            || symlink_error.to_lowercase().contains("symlink"),
        "expected symlink rejection, got: {symlink_error}"
    );

    let binary_error = harness
        .rpc_expect_error(
            "file.read",
            json!({ "path": fixture_path.join("binary.bin").to_string_lossy() }),
        )
        .await;
    assert!(
        binary_error.to_lowercase().contains("utf-8"),
        "expected utf-8 error for binary files, got: {binary_error}"
    );

    let large_error = harness
        .rpc_expect_error(
            "file.read",
            json!({ "path": fixture_path.join("large.txt").to_string_lossy() }),
        )
        .await;
    assert!(
        large_error.contains("5MB") || large_error.contains("5 MB"),
        "expected size limit error, got: {large_error}"
    );

    let outside_error = harness
        .rpc_expect_error("file.list", json!({ "path": "/etc" }))
        .await;
    assert!(
        outside_error.to_lowercase().contains("outside")
            || outside_error.to_lowercase().contains("within"),
        "expected root validation error, got: {outside_error}"
    );

    let _ = harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await;
}

#[tokio::test]
async fn file_list_survives_broken_symlink_entries() {
    let mut harness = setup_test_server().await;
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

    let fixture_path = project.repo_path.join("file-list-broken-link");
    fs::create_dir_all(&fixture_path).expect("create fixture dir");
    fs::write(fixture_path.join("ok.txt"), "ok\n").expect("write fixture file");

    let broken_target = fixture_path.join("missing.txt");
    let broken_link = fixture_path.join("missing-link.txt");
    std::os::unix::fs::symlink(&broken_target, &broken_link).expect("create broken symlink");

    let listed = harness
        .rpc(
            "file.list",
            json!({ "path": fixture_path.to_string_lossy() }),
        )
        .await
        .expect("list fixture directory with broken symlink");

    let entries = listed["entries"]
        .as_array()
        .expect("file.list returns entries array");
    let names: Vec<&str> = entries
        .iter()
        .map(|entry| entry["name"].as_str().expect("entry name"))
        .collect();

    assert!(
        names.contains(&"ok.txt"),
        "directory listing should succeed despite broken symlink"
    );

    let _ = harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await;
}

#[tokio::test]
async fn file_list_marks_directory_symlinks_as_non_directories() {
    let mut harness = setup_test_server().await;
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

    let fixture_path = project.repo_path.join("file-list-symlinked-dirs");
    let real_dir = fixture_path.join("real-dir");
    let linked_dir = fixture_path.join("linked-dir");
    fs::create_dir_all(&real_dir).expect("create real dir");
    std::os::unix::fs::symlink(&real_dir, &linked_dir).expect("create directory symlink");

    let listed = harness
        .rpc(
            "file.list",
            json!({ "path": fixture_path.to_string_lossy() }),
        )
        .await
        .expect("list fixture directory with symlinked dir");

    let entries = listed["entries"]
        .as_array()
        .expect("file.list returns entries array");

    let real_entry = entries
        .iter()
        .find(|entry| entry["name"] == "real-dir")
        .expect("real directory entry present");
    assert_eq!(real_entry["isDirectory"], true);

    let linked_entry = entries
        .iter()
        .find(|entry| entry["name"] == "linked-dir")
        .expect("directory symlink entry present");
    assert_eq!(
        linked_entry["isDirectory"], false,
        "directory symlinks must not be advertised as traversable directories"
    );

    let _ = harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await;
}

#[tokio::test]
async fn file_list_reports_non_dangling_metadata_failures() {
    let mut harness = setup_test_server().await;
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

    let fixture_path = project.repo_path.join("file-list-metadata-errors");
    fs::create_dir_all(&fixture_path).expect("create fixture dir");
    fs::write(fixture_path.join("ok.txt"), "ok\n").expect("write fixture file");

    let loop_link = fixture_path.join("loop-link");
    std::os::unix::fs::symlink("loop-link", &loop_link).expect("create loop symlink");

    let list_error = harness
        .rpc_expect_error(
            "file.list",
            json!({ "path": fixture_path.to_string_lossy() }),
        )
        .await;

    assert!(
        list_error.to_lowercase().contains("metadata")
            || list_error.to_lowercase().contains("inspect")
            || list_error.to_lowercase().contains("level"),
        "expected metadata failure to surface, got: {list_error}"
    );

    let _ = harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await;
}

#[tokio::test]
async fn file_git_status_returns_modified_entries_for_worktree_files() {
    if !common::tmux_available().await {
        eprintln!("skipping file_git_status_returns_modified_entries_for_worktree_files: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
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

    let created = harness
        .rpc(
            "thread.create",
            json!({
                "project_id": project_id.clone(),
                "name": common::unique_name("git-status"),
                "source_type": "new_feature"
            }),
        )
        .await
        .expect("create thread");

    let thread_id = created["id"].as_str().expect("thread id").to_string();
    let worktree_path = created["worktree_path"]
        .as_str()
        .expect("thread worktree path")
        .to_string();

    loop {
        let event = harness
            .wait_for_event("thread.progress", std::time::Duration::from_secs(45))
            .await
            .expect("thread progress event");
        if event["params"]["thread_id"] == thread_id && event["params"]["step"] == "ready" {
            break;
        }
    }

    fs::write(
        std::path::Path::new(&worktree_path).join("README.md"),
        "spindle integration test\nchanged\n",
    )
    .expect("modify tracked file in worktree");

    let status = harness
        .rpc("file.git_status", json!({ "path": worktree_path }))
        .await
        .expect("fetch git status");

    assert_eq!(
        status["entries"]["README.md"], "modified",
        "expected modified tracked file status"
    );
}

#[tokio::test]
async fn file_git_status_handles_quoted_and_spaced_paths() {
    if !common::tmux_available().await {
        eprintln!("skipping file_git_status_handles_quoted_and_spaced_paths: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
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

    let created = harness
        .rpc(
            "thread.create",
            json!({
                "project_id": project_id,
                "name": common::unique_name("git-status-quotes"),
                "source_type": "new_feature"
            }),
        )
        .await
        .expect("create thread");

    let thread_id = created["id"].as_str().expect("thread id").to_string();
    let worktree_path = created["worktree_path"]
        .as_str()
        .expect("thread worktree path")
        .to_string();

    loop {
        let event = harness
            .wait_for_event("thread.progress", std::time::Duration::from_secs(45))
            .await
            .expect("thread progress event");
        if event["params"]["thread_id"] == thread_id && event["params"]["step"] == "ready" {
            break;
        }
    }

    let worktree = std::path::Path::new(&worktree_path);
    fs::create_dir_all(worktree.join("dir with spaces")).expect("create spaced dir");
    let quoted_path = "dir with spaces/file \"quoted\" name.txt";
    fs::write(worktree.join(quoted_path), "spindle integration test\n").expect("write quoted file");

    let status = harness
        .rpc("file.git_status", json!({ "path": worktree_path }))
        .await
        .expect("fetch git status");

    assert_eq!(
        status["entries"][quoted_path], "untracked",
        "expected untracked status for quoted/spaced path"
    );
}

#[tokio::test]
async fn file_git_status_reports_renamed_and_modified_entries() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping file_git_status_reports_renamed_and_modified_entries: tmux unavailable"
        );
        return;
    }

    let mut harness = setup_test_server().await;
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

    let created = harness
        .rpc(
            "thread.create",
            json!({
                "project_id": project_id,
                "name": common::unique_name("git-status-rename-modified"),
                "source_type": "new_feature"
            }),
        )
        .await
        .expect("create thread");

    let thread_id = created["id"].as_str().expect("thread id").to_string();
    let worktree_path = created["worktree_path"]
        .as_str()
        .expect("thread worktree path")
        .to_string();

    loop {
        let event = harness
            .wait_for_event("thread.progress", std::time::Duration::from_secs(45))
            .await
            .expect("thread progress event");
        if event["params"]["thread_id"] == thread_id && event["params"]["step"] == "ready" {
            break;
        }
    }

    let rename_output = Command::new("git")
        .arg("-C")
        .arg(&worktree_path)
        .args(["mv", "README.md", "README-renamed.md"])
        .output()
        .expect("rename tracked file");
    assert!(
        rename_output.status.success(),
        "git mv should succeed: {}",
        String::from_utf8_lossy(&rename_output.stderr)
    );

    fs::write(
        std::path::Path::new(&worktree_path).join("README-renamed.md"),
        "spindle integration test\nrenamed and modified\n",
    )
    .expect("modify renamed file in worktree");

    let status = harness
        .rpc("file.git_status", json!({ "path": worktree_path }))
        .await
        .expect("fetch git status");

    assert_eq!(
        status["entries"]["README-renamed.md"], "renamed",
        "expected renamed status for renamed and modified tracked file"
    );
}

async fn create_ready_thread(
    harness: &mut common::TestHarness,
    name: &str,
) -> (String, String, String) {
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

    let created = harness
        .rpc(
            "thread.create",
            json!({
                "project_id": project_id.clone(),
                "name": common::unique_name(name),
                "source_type": "new_feature"
            }),
        )
        .await
        .expect("create thread");

    let thread_id = created["id"].as_str().expect("thread id").to_string();
    let worktree_path = created["worktree_path"]
        .as_str()
        .expect("thread worktree path")
        .to_string();

    loop {
        let event = harness
            .wait_for_event("thread.progress", std::time::Duration::from_secs(45))
            .await
            .expect("thread progress event");
        if event["params"]["thread_id"] == thread_id && event["params"]["step"] == "ready" {
            break;
        }
    }

    (project_id, thread_id, worktree_path)
}

fn run_git(repo_path: &str, args: &[&str]) -> std::process::Output {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(args)
        .output()
        .expect("run git command");
    assert!(
        output.status.success(),
        "git {:?} should succeed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[tokio::test]
async fn file_diff_returns_working_scope_unified_diff_for_all_files_or_one_path() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping file_diff_returns_working_scope_unified_diff_for_all_files_or_one_path: tmux unavailable"
        );
        return;
    }

    let mut harness = setup_test_server().await;
    let (project_id, thread_id, worktree_path) =
        create_ready_thread(&mut harness, "file-diff-working").await;
    let worktree = std::path::Path::new(&worktree_path);

    fs::write(
        worktree.join("README.md"),
        "spindle integration test\nchanged\n",
    )
    .expect("modify tracked file");
    fs::write(worktree.join("notes.txt"), "alpha\nbeta\n").expect("write untracked file");

    let diff = harness
        .rpc(
            "file.diff",
            json!({
                "thread_id": thread_id.clone(),
                "scope": "working"
            }),
        )
        .await
        .expect("fetch working diff");

    let diff_text = diff["diff_text"].as_str().expect("diff string");
    assert!(diff_text.contains("diff --git a/README.md b/README.md"));
    assert!(diff_text.contains("diff --git a/notes.txt b/notes.txt"));
    assert!(diff_text.contains("+changed"));
    assert!(diff_text.contains("+alpha"));

    let filtered = harness
        .rpc(
            "file.diff",
            json!({
                "thread_id": thread_id,
                "path": "notes.txt",
                "scope": "working"
            }),
        )
        .await
        .expect("fetch filtered diff");

    let filtered_text = filtered["diff_text"]
        .as_str()
        .expect("filtered diff string");
    assert!(filtered_text.contains("diff --git a/notes.txt b/notes.txt"));
    assert!(!filtered_text.contains("README.md"));

    let _ = harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await;
}

#[tokio::test]
async fn file_diff_summary_reports_staged_statuses_and_counts() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping file_diff_summary_reports_staged_statuses_and_counts: tmux unavailable"
        );
        return;
    }

    let mut harness = setup_test_server().await;
    let (project_id, thread_id, worktree_path) =
        create_ready_thread(&mut harness, "file-diff-summary").await;
    let worktree = std::path::Path::new(&worktree_path);

    fs::write(worktree.join("obsolete.txt"), "remove me\n").expect("write obsolete file");
    fs::write(worktree.join("changed.txt"), "before\n").expect("write changed file");
    run_git(&worktree_path, &["add", "obsolete.txt", "changed.txt"]);
    run_git(&worktree_path, &["commit", "-m", "baseline"]);

    run_git(&worktree_path, &["mv", "README.md", "README-renamed.md"]);
    fs::write(
        worktree.join("README-renamed.md"),
        "spindle integration test\nrenamed and modified\n",
    )
    .expect("modify renamed file");
    fs::write(worktree.join("changed.txt"), "before\nafter\n").expect("modify tracked file");
    fs::write(worktree.join("notes.txt"), "alpha\nbeta\n").expect("write staged file");
    fs::remove_file(worktree.join("obsolete.txt")).expect("delete tracked file");
    run_git(&worktree_path, &["add", "-A"]);

    let summary = harness
        .rpc(
            "file.diff_summary",
            json!({
                "thread_id": thread_id,
                "scope": "staged"
            }),
        )
        .await
        .expect("fetch diff summary");

    let files = summary["files"].as_array().expect("summary array");
    let renamed = files
        .iter()
        .find(|entry| entry["path"] == "README-renamed.md")
        .expect("renamed entry present");
    assert_eq!(renamed["status"], "renamed");
    assert_eq!(renamed["added"], 1);
    assert_eq!(renamed["removed"], 0);

    let modified = files
        .iter()
        .find(|entry| entry["path"] == "changed.txt")
        .expect("modified entry present");
    assert_eq!(modified["status"], "modified");
    assert_eq!(modified["added"], 1);
    assert_eq!(modified["removed"], 0);

    let added = files
        .iter()
        .find(|entry| entry["path"] == "notes.txt")
        .expect("added entry present");
    assert_eq!(added["status"], "added");
    assert_eq!(added["added"], 2);
    assert_eq!(added["removed"], 0);

    let deleted = files
        .iter()
        .find(|entry| entry["path"] == "obsolete.txt")
        .expect("deleted entry present");
    assert_eq!(deleted["status"], "deleted");
    assert_eq!(deleted["added"], 0);
    assert_eq!(deleted["removed"], 1);

    let _ = harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await;
}

#[tokio::test]
async fn file_diff_and_summary_report_head_scope_changes_against_upstream() {
    if !common::tmux_available().await {
        eprintln!("skipping file_diff_and_summary_report_head_scope_changes_against_upstream: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (project_id, thread_id, worktree_path) =
        create_ready_thread(&mut harness, "file-diff-head").await;
    let worktree = std::path::Path::new(&worktree_path);

    fs::write(
        worktree.join("README.md"),
        "spindle integration test\nhead change\n",
    )
    .expect("modify tracked file");
    run_git(&worktree_path, &["add", "README.md"]);
    run_git(&worktree_path, &["commit", "-m", "head change"]);

    let diff = harness
        .rpc(
            "file.diff",
            json!({
                "thread_id": thread_id.clone(),
                "scope": "head"
            }),
        )
        .await
        .expect("fetch head diff");
    let diff_text = diff["diff_text"].as_str().expect("head diff string");
    assert!(diff_text.contains("diff --git a/README.md b/README.md"));
    assert!(diff_text.contains("+head change"));

    let summary = harness
        .rpc(
            "file.diff_summary",
            json!({
                "thread_id": thread_id,
                "scope": "head"
            }),
        )
        .await
        .expect("fetch head summary");
    let files = summary["files"].as_array().expect("head summary array");
    let readme = files
        .iter()
        .find(|entry| entry["path"] == "README.md")
        .expect("head summary entry present");
    assert_eq!(readme["status"], "modified");
    assert_eq!(readme["added"], 1);
    assert_eq!(readme["removed"], 0);

    let _ = harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await;
}

#[tokio::test]
async fn file_diff_rejects_invalid_scope() {
    let mut harness = setup_test_server().await;

    let error = harness
        .rpc_expect_error(
            "file.diff",
            json!({
                "thread_id": "thread-1",
                "scope": "bogus"
            }),
        )
        .await;
    assert!(
        error.contains("working") && error.contains("staged") && error.contains("head"),
        "expected invalid scope error, got: {error}"
    );
}
