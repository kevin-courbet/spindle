mod common;

use std::time::Duration;

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

async fn cleanup_thread_project(harness: &mut common::TestHarness, thread_id: &str, project_id: &str) {
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
async fn thread_hide_marks_thread_hidden() {
    if !common::tmux_available().await {
        eprintln!("skipping thread_hide_marks_thread_hidden: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
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
