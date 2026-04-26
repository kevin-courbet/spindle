mod common;

use std::{path::Path, time::Duration};

use serde_json::json;

async fn setup_test_server() -> common::TestHarness {
    common::setup_test_server().await
}

async fn add_project(harness: &mut common::TestHarness) -> (common::TestProject, String) {
    let project = common::create_git_project(None, true)
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

    let project_id = added["id"]
        .as_str()
        .expect("project id in add response")
        .to_string();
    (project, project_id)
}

async fn create_thread(harness: &mut common::TestHarness, project_id: &str) -> String {
    let created = harness
        .rpc(
            "thread.create",
            json!({
                "project_id": project_id,
                "name": common::unique_name("thread"),
                "source_type": "new_feature"
            }),
        )
        .await
        .expect("create thread");

    created["id"]
        .as_str()
        .expect("thread id in create response")
        .to_string()
}

async fn remove_origin_remote(repo_path: &std::path::Path) {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["remote", "remove", "origin"])
        .output()
        .await
        .expect("run git remote remove origin");

    assert!(
        output.status.success(),
        "git remote remove origin failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
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

async fn add_project_with_config(
    harness: &mut common::TestHarness,
    threadmill_config: &str,
) -> (common::TestProject, String) {
    let project = common::create_git_project(Some(threadmill_config), true)
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

    let project_id = added["id"]
        .as_str()
        .expect("project id in add response")
        .to_string();
    (project, project_id)
}

#[tokio::test]
async fn thread_create_emits_progress_and_lists_thread() {
    if !common::tmux_available().await {
        eprintln!("skipping thread_create_emits_progress_and_lists_thread: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;

    wait_for_thread_ready(&mut harness, &thread_id).await;

    let listed = harness
        .rpc("thread.list", json!({ "project_id": project_id }))
        .await
        .expect("list threads");
    let threads = listed.as_array().expect("thread.list returns array");
    assert!(threads.iter().any(|thread| thread["id"] == thread_id));

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn thread_create_assigns_distinct_port_offsets() {
    if !common::tmux_available().await {
        eprintln!("skipping thread_create_assigns_distinct_port_offsets: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;

    let first = harness
        .rpc(
            "thread.create",
            json!({
                "project_id": project_id,
                "name": common::unique_name("offset-a"),
                "source_type": "new_feature"
            }),
        )
        .await
        .expect("create first thread");
    let first_id = first["id"].as_str().expect("first thread id").to_string();
    let first_offset = first["port_offset"].as_u64().expect("first port_offset");
    wait_for_thread_ready(&mut harness, &first_id).await;

    let second = harness
        .rpc(
            "thread.create",
            json!({
                "project_id": project_id,
                "name": common::unique_name("offset-b"),
                "source_type": "new_feature"
            }),
        )
        .await
        .expect("create second thread");
    let second_id = second["id"].as_str().expect("second thread id").to_string();
    let second_offset = second["port_offset"].as_u64().expect("second port_offset");

    assert_ne!(first_offset, second_offset);

    wait_for_thread_ready(&mut harness, &second_id).await;

    let listed = harness
        .rpc("thread.list", json!({ "project_id": project_id.clone() }))
        .await
        .expect("list threads");
    let threads = listed.as_array().expect("thread.list returns array");

    let first_listed = threads
        .iter()
        .find(|thread| thread["id"] == first_id)
        .expect("first thread listed");
    let second_listed = threads
        .iter()
        .find(|thread| thread["id"] == second_id)
        .expect("second thread listed");

    assert_eq!(first_listed["port_offset"].as_u64(), Some(first_offset));
    assert_eq!(second_listed["port_offset"].as_u64(), Some(second_offset));

    let _ = harness
        .rpc(
            "thread.close",
            json!({ "thread_id": first_id, "mode": "close" }),
        )
        .await;
    let _ = harness
        .rpc(
            "thread.close",
            json!({ "thread_id": second_id, "mode": "close" }),
        )
        .await;
    let _ = harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await;
}

#[tokio::test]
async fn thread_create_succeeds_without_origin_remote() {
    if !common::tmux_available().await {
        eprintln!("skipping thread_create_succeeds_without_origin_remote: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (project, project_id) = add_project(&mut harness).await;
    remove_origin_remote(&project.repo_path).await;

    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let listed = harness
        .rpc("thread.list", json!({ "project_id": project_id.clone() }))
        .await
        .expect("list threads");
    let threads = listed.as_array().expect("thread.list returns array");
    assert!(threads.iter().any(|thread| thread["id"] == thread_id));

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn thread_close_marks_thread_closed_or_removed() {
    if !common::tmux_available().await {
        eprintln!("skipping thread_close_marks_thread_closed_or_removed: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;

    wait_for_thread_ready(&mut harness, &thread_id).await;
    harness
        .rpc(
            "thread.close",
            json!({ "thread_id": thread_id, "mode": "close" }),
        )
        .await
        .expect("close thread");

    let listed = harness
        .rpc("thread.list", json!({ "project_id": project_id.clone() }))
        .await
        .expect("list threads");
    let threads = listed.as_array().expect("thread.list returns array");
    let maybe_thread = threads.iter().find(|thread| thread["id"] == thread_id);
    if let Some(thread) = maybe_thread {
        assert_eq!(thread["status"], "closed");
    }

    let _ = harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await;
}

#[tokio::test]
async fn thread_close_main_checkout_keeps_project_root() {
    if !common::tmux_available().await {
        eprintln!("skipping thread_close_main_checkout_keeps_project_root: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (project, project_id) = add_project(&mut harness).await;

    let created = harness
        .rpc(
            "thread.create",
            json!({
                "project_id": project_id,
                "name": "main",
                "source_type": "main_checkout"
            }),
        )
        .await
        .expect("create main-checkout thread");
    assert_eq!(created["name"], "main");
    assert_eq!(created["source_type"], "main_checkout");
    let thread_id = created["id"].as_str().expect("thread id").to_string();
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let project_path = project.repo_path.to_string_lossy().to_string();
    assert!(Path::new(&project_path).exists());

    harness
        .rpc(
            "thread.close",
            json!({ "thread_id": thread_id.clone(), "mode": "close" }),
        )
        .await
        .expect("close main-checkout thread");

    assert!(Path::new(&project_path).exists());

    let listed = harness
        .rpc("thread.list", json!({ "project_id": project_id.clone() }))
        .await
        .expect("list threads");
    let listed_thread = listed
        .as_array()
        .expect("thread.list returns array")
        .iter()
        .find(|entry| entry["id"] == thread_id)
        .expect("main-checkout thread listed");
    assert_eq!(listed_thread["status"], "closed");

    let _ = harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await;
}

#[tokio::test]
async fn thread_hide_marks_thread_hidden() {
    if !common::tmux_available().await {
        eprintln!("skipping thread_hide_marks_thread_hidden: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;

    wait_for_thread_ready(&mut harness, &thread_id).await;
    let hidden = harness
        .rpc("thread.hide", json!({ "thread_id": thread_id }))
        .await
        .expect("hide thread");
    assert_eq!(hidden["status"], "hidden");

    let listed = harness
        .rpc("thread.list", json!({ "project_id": project_id.clone() }))
        .await
        .expect("list threads");
    let threads = listed.as_array().expect("thread.list returns array");
    let thread = threads
        .iter()
        .find(|thread| thread["id"] == thread_id)
        .expect("thread remains listed");
    assert_eq!(thread["status"], "hidden");
    assert!(project.repo_path.exists());

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn thread_close_base_checkout_runs_teardown_without_removing_project_root() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping thread_close_base_checkout_runs_teardown_without_removing_project_root: tmux unavailable"
        );
        return;
    }

    let mut harness = setup_test_server().await;
    let (project, project_id) = add_project_with_config(
        &mut harness,
        r#"teardown:
  - "touch teardown-ran.txt"
"#,
    )
    .await;

    let created = harness
        .rpc(
            "thread.create",
            json!({
                "project_id": project_id,
                "name": common::unique_name("base-close"),
                "source_type": "new_feature"
            }),
        )
        .await
        .expect("create base-checkout thread");
    assert!(created["worktree_path"].is_null());

    let thread_id = created["id"].as_str().expect("thread id").to_string();
    wait_for_thread_ready(&mut harness, &thread_id).await;

    harness
        .rpc(
            "thread.close",
            json!({ "thread_id": thread_id.clone(), "mode": "close" }),
        )
        .await
        .expect("close base-checkout thread");

    assert!(project.repo_path.exists());
    assert!(project.repo_path.join("teardown-ran.txt").exists());

    let _ = harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await;
}

#[tokio::test]
async fn thread_create_rejects_duplicate_active_name() {
    if !common::tmux_available().await {
        eprintln!("skipping thread_create_rejects_duplicate_active_name: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let name = common::unique_name("dup-thread");
    let created = harness
        .rpc(
            "thread.create",
            json!({
                "project_id": project_id,
                "name": name,
                "source_type": "new_feature"
            }),
        )
        .await
        .expect("create first thread");
    let thread_id = created["id"].as_str().expect("thread id").to_string();

    wait_for_thread_ready(&mut harness, &thread_id).await;

    let error = harness
        .rpc_expect_error(
            "thread.create",
            json!({
                "project_id": project_id,
                "name": name,
                "source_type": "new_feature"
            }),
        )
        .await;

    assert!(error.contains("thread name already exists"), "{error}");

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn thread_create_nonexistent_project_returns_error() {
    let mut harness = setup_test_server().await;

    let error = harness
        .rpc_expect_error(
            "thread.create",
            json!({
                "project_id": "does-not-exist",
                "name": "broken-thread",
                "source_type": "new_feature"
            }),
        )
        .await;

    assert!(error.contains("project not found"), "{error}");
}

#[tokio::test]
async fn thread_create_repo_without_remote_succeeds() {
    if !common::tmux_available().await {
        eprintln!("skipping thread_create_repo_without_remote_succeeds: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let project = common::create_git_project_without_remote(None)
        .await
        .expect("create test git project without remote");
    harness.register_cleanup_path(project.root_dir.clone());

    let added = harness
        .rpc(
            "project.add",
            json!({ "path": project.repo_path.to_string_lossy() }),
        )
        .await
        .expect("add project");
    let project_id = added["id"]
        .as_str()
        .expect("project id in add response")
        .to_string();

    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn thread_reopen_hidden_thread() {
    if !common::tmux_available().await {
        eprintln!("skipping thread_reopen_hidden_thread: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;

    wait_for_thread_ready(&mut harness, &thread_id).await;
    harness
        .rpc("thread.hide", json!({ "thread_id": thread_id }))
        .await
        .expect("hide thread");

    let reopened = harness
        .rpc("thread.reopen", json!({ "thread_id": thread_id }))
        .await
        .expect("reopen hidden thread");
    assert_eq!(reopened["status"], "active");

    let listed = harness
        .rpc("thread.list", json!({ "project_id": project_id.clone() }))
        .await
        .expect("list threads");
    let thread = listed
        .as_array()
        .expect("thread.list returns array")
        .iter()
        .find(|entry| entry["id"] == thread_id)
        .expect("thread listed after reopen");
    assert_eq!(thread["status"], "active");

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn thread_cancel_inflight_creation_marks_failed_and_cleans_worktree() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping thread_cancel_inflight_creation_marks_failed_and_cleans_worktree: tmux unavailable"
        );
        return;
    }

    let mut harness = setup_test_server().await;
    let project = common::create_git_project(
        Some(
            r#"setup:
  - \"sleep 10\"
"#,
        ),
        false,
    )
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
                "name": common::unique_name("cancel-thread"),
                "source_type": "new_feature",
                "sandbox": true
            }),
        )
        .await
        .expect("create thread");
    let thread_id = created["id"].as_str().expect("thread id").to_string();
    let worktree_path = created["worktree_path"]
        .as_str()
        .expect("worktree path")
        .to_string();

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
            panic!("thread creation failed before cancel: {error}");
        }

        if params["step"] == "running_hooks" {
            break;
        }
    }

    let cancelled = harness
        .rpc("thread.cancel", json!({ "thread_id": thread_id }))
        .await
        .expect("cancel creating thread");
    assert_eq!(cancelled["status"], "failed");

    // Status transitions arrive inside state.delta operations — see
    // `StateDeltaOperationPayload::ThreadStatusChanged` in src/protocol.rs.
    // `thread.status_changed` is no longer a top-level event (commit bce295f
    // consolidated status emission onto state.delta as the sole replication
    // channel).
    loop {
        let event = harness
            .wait_for_event("state.delta", Duration::from_secs(45))
            .await
            .expect("wait for state.delta with thread status change");
        let operations = event["params"]["operations"]
            .as_array()
            .expect("state.delta operations array");
        let observed_failed = operations.iter().any(|operation| {
            operation["type"] == "thread.status_changed"
                && operation["thread_id"] == thread_id
                && operation["new"] == "failed"
        });
        if observed_failed {
            break;
        }
    }

    let listed = harness
        .rpc("thread.list", json!({ "project_id": project_id.clone() }))
        .await
        .expect("list threads");
    let threads = listed.as_array().expect("thread.list returns array");
    if let Some(thread) = threads.iter().find(|thread| thread["id"] == thread_id) {
        assert_eq!(thread["status"], "failed");
    }

    for _ in 0..50 {
        if !Path::new(&worktree_path).exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !Path::new(&worktree_path).exists(),
        "worktree should be removed after cancel"
    );

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

#[tokio::test]
async fn thread_create_from_pr_url_resolves_branch() {
    if !common::tmux_available().await {
        eprintln!("skipping thread_create_from_pr_url_resolves_branch: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;

    let expected_branch = "feature/integration-test";
    let name = common::unique_name("pr-test");
    let result = harness
        .rpc(
            "thread.create",
            json!({
                "project_id": project_id,
                "name": name,
                "source_type": "pull_request",
                "pr_url": "https://example.com/acme/repo/pull/42/head:feature/integration-test",
                "sandbox": true
            }),
        )
        .await;

    match result {
        Ok(created) => {
            if let Some(thread_id) = created["id"].as_str() {
                let _ = harness
                    .rpc(
                        "thread.close",
                        json!({ "thread_id": thread_id, "mode": "close" }),
                    )
                    .await;
            }
            let _ = harness
                .rpc("project.remove", json!({ "project_id": project_id }))
                .await;

            let branch = created["branch"].as_str().unwrap_or_default();
            assert_eq!(
                branch, expected_branch,
                "pull_request source should resolve branch from pr_url instead of defaulting to thread name"
            );
        }
        Err(error) => {
            let _ = harness
                .rpc("project.remove", json!({ "project_id": project_id }))
                .await;

            let lower = error.to_lowercase();
            assert!(
                !lower.contains("invalid thread.create params"),
                "expected PR-resolution error, got generic param error: {error}"
            );
            assert!(
                lower.contains("pull request")
                    || lower.contains("pr url")
                    || lower.contains("resolve"),
                "expected clear PR resolution error, got: {error}"
            );
        }
    }
}

#[tokio::test]
async fn thread_create_without_sandbox_rejects_non_current_branch() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping thread_create_without_sandbox_rejects_non_current_branch: tmux unavailable"
        );
        return;
    }

    let mut harness = setup_test_server().await;
    let (project, project_id) = add_project(&mut harness).await;
    let branch = project
        .feature_branch
        .clone()
        .expect("feature branch exists in test repository");

    let error = harness
        .rpc_expect_error(
            "thread.create",
            json!({
                "project_id": project_id.clone(),
                "name": common::unique_name("base-thread-mismatch"),
                "source_type": "existing_branch",
                "branch": branch
            }),
        )
        .await;

    let lower = error.to_lowercase();
    assert!(
        lower.contains("sandbox") || lower.contains("worktree"),
        "{error}"
    );
    assert!(lower.contains("branch"), "{error}");

    let listed = harness
        .rpc("thread.list", json!({ "project_id": project_id.clone() }))
        .await
        .expect("list threads");
    assert!(
        listed
            .as_array()
            .expect("thread.list returns array")
            .is_empty(),
        "rejected create should not persist thread"
    );

    let _ = harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await;
}

#[tokio::test]
async fn thread_create_without_sandbox_returns_null_worktree_path_and_can_close() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping thread_create_without_sandbox_returns_null_worktree_path_and_can_close: tmux unavailable"
        );
        return;
    }

    let mut harness = setup_test_server().await;
    let (project, project_id) = add_project(&mut harness).await;

    let created = harness
        .rpc(
            "thread.create",
            json!({
                "project_id": project_id,
                "name": common::unique_name("base-thread"),
                "source_type": "new_feature"
            }),
        )
        .await
        .expect("create non-sandboxed thread");
    assert!(created["worktree_path"].is_null());

    let thread_id = created["id"].as_str().expect("thread id").to_string();
    wait_for_thread_ready(&mut harness, &thread_id).await;

    harness
        .rpc(
            "thread.close",
            json!({ "thread_id": thread_id.clone(), "mode": "close" }),
        )
        .await
        .expect("close base-checkout thread");

    assert!(project.repo_path.exists());

    let listed = harness
        .rpc("thread.list", json!({ "project_id": project_id.clone() }))
        .await
        .expect("list threads");
    let thread = listed
        .as_array()
        .expect("thread.list returns array")
        .iter()
        .find(|entry| entry["id"] == thread_id)
        .expect("closed thread listed");
    assert_eq!(thread["status"], "closed");

    let _ = harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await;
}

#[tokio::test]
async fn thread_promote_and_demote_update_worktree_path_over_rpc() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping thread_promote_and_demote_update_worktree_path_over_rpc: tmux unavailable"
        );
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;

    let created = harness
        .rpc(
            "thread.create",
            json!({
                "project_id": project_id,
                "name": common::unique_name("promote-thread"),
                "source_type": "existing_branch",
                "branch": "main"
            }),
        )
        .await
        .expect("create base-checkout thread");
    let thread_id = created["id"].as_str().expect("thread id").to_string();
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let promoted = harness
        .rpc(
            "thread.promoteToWorktree",
            json!({ "thread_id": thread_id.clone() }),
        )
        .await
        .expect("promote thread to worktree");
    let promoted_path = promoted["worktree_path"]
        .as_str()
        .expect("promoted worktree path")
        .to_string();
    assert!(Path::new(&promoted_path).is_dir());

    let demoted = harness
        .rpc(
            "thread.demoteToBase",
            json!({ "thread_id": thread_id.clone() }),
        )
        .await
        .expect("demote thread to base checkout");
    assert!(demoted["worktree_path"].is_null());
    assert!(!Path::new(&promoted_path).exists());

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn thread_demote_to_base_rejects_branch_mismatch_and_keeps_worktree() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping thread_demote_to_base_rejects_branch_mismatch_and_keeps_worktree: tmux unavailable"
        );
        return;
    }

    let mut harness = setup_test_server().await;
    let (project, project_id) = add_project(&mut harness).await;
    let branch = project
        .feature_branch
        .clone()
        .expect("feature branch exists in test repository");

    let created = harness
        .rpc(
            "thread.create",
            json!({
                "project_id": project_id.clone(),
                "name": common::unique_name("demote-mismatch"),
                "source_type": "existing_branch",
                "branch": branch,
                "sandbox": true
            }),
        )
        .await
        .expect("create sandboxed feature thread");
    let thread_id = created["id"].as_str().expect("thread id").to_string();
    let worktree_path = created["worktree_path"]
        .as_str()
        .expect("worktree path")
        .to_string();
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let error = harness
        .rpc_expect_error(
            "thread.demoteToBase",
            json!({ "thread_id": thread_id.clone() }),
        )
        .await;

    let lower = error.to_lowercase();
    assert!(lower.contains("branch"), "{error}");
    assert!(
        lower.contains("base") || lower.contains("checkout"),
        "{error}"
    );
    assert!(
        Path::new(&worktree_path).is_dir(),
        "worktree should remain after rejection"
    );

    let listed = harness
        .rpc("thread.list", json!({ "project_id": project_id.clone() }))
        .await
        .expect("list threads");
    let thread = listed
        .as_array()
        .expect("thread.list returns array")
        .iter()
        .find(|entry| entry["id"] == thread_id)
        .expect("thread listed after demote rejection");
    assert_eq!(thread["status"], "active");
    assert_eq!(thread["worktree_path"], worktree_path);

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}
