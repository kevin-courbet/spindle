mod common;

use std::fs;

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
                "project_id": project_id,
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
