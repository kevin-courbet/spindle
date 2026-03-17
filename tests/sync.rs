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
                "name": common::unique_name("sync"),
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

#[tokio::test]
async fn state_snapshot_returns_valid_state() {
    let mut harness = setup_test_server().await;

    let snapshot = harness
        .rpc("state.snapshot", json!({}))
        .await
        .expect("state snapshot");

    assert!(snapshot["state_version"].is_u64());
    assert!(snapshot["projects"].is_array());
    assert!(snapshot["threads"].is_array());
}

#[tokio::test]
async fn thread_create_emits_state_delta_with_incremented_version() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping thread_create_emits_state_delta_with_incremented_version: tmux unavailable"
        );
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;

    let snapshot = harness
        .rpc("state.snapshot", json!({}))
        .await
        .expect("state snapshot before thread.create");
    let base_version = snapshot["state_version"].as_u64().expect("state version");

    let thread_id = create_thread(&mut harness, &project_id).await;

    let delta_event = loop {
        let event = harness
            .wait_for_event("state.delta", Duration::from_secs(45))
            .await
            .expect("wait for state delta event");
        let version = event["params"]["state_version"]
            .as_u64()
            .expect("state.delta version");
        if version > base_version {
            break event;
        }
    };

    let delta_version = delta_event["params"]["state_version"]
        .as_u64()
        .expect("state.delta version");
    assert!(delta_version > base_version);

    let operations = delta_event["params"]["operations"]
        .as_array()
        .expect("state.delta operations array");
    assert!(
        operations.iter().any(|operation| {
            operation["type"] == "thread.created" && operation["thread"]["id"] == thread_id
        }),
        "expected thread.created delta for new thread"
    );
    assert!(
        operations
            .iter()
            .all(|operation| operation["op_id"].as_str().is_some()),
        "all operations must include op_id"
    );

    wait_for_thread_ready(&mut harness, &thread_id).await;

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
