mod common;

use std::time::Duration;

use serde_json::json;

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
                "name": common::unique_name("spawn"),
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

#[tokio::test]
async fn terminal_spawn_attach_binary_output_and_detach() {
    if !common::tmux_available().await {
        eprintln!("skipping terminal_spawn_attach_binary_output_and_detach: tmux unavailable");
        return;
    }

    let mut harness = common::setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let attach_result = harness
        .rpc(
            "terminal.attach",
            json!({ "thread_id": thread_id, "preset": "terminal" }),
        )
        .await
        .expect("attach terminal");
    let channel_id = attach_result["channel_id"].as_u64().expect("channel id") as u16;
    assert!(channel_id > 0);

    harness
        .send_binary(channel_id, b"echo hello\n")
        .await
        .expect("send terminal input");

    harness
        .wait_for_channel_output_contains(channel_id, b"hello", Duration::from_secs(15))
        .await
        .expect("receive terminal output");

    let detach_result = harness
        .rpc(
            "terminal.detach",
            json!({ "thread_id": thread_id, "preset": "terminal" }),
        )
        .await
        .expect("detach terminal");
    assert_eq!(detach_result["detached"], true);

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}
