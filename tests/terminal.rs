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
                "name": common::unique_name("terminal"),
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
async fn terminal_attach_binary_resize_detach_lifecycle() {
    if !common::tmux_available().await {
        eprintln!("skipping terminal_attach_binary_resize_detach_lifecycle: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
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
    let channel_id = attach_result["channel_id"]
        .as_u64()
        .expect("channel id in attach response") as u16;
    assert!(channel_id > 0);

    let marker = common::unique_name("terminal-io");
    let command = format!("printf '{marker}\\n'\n");
    harness
        .send_binary(channel_id, command.as_bytes())
        .await
        .expect("send binary input");

    harness
        .wait_for_channel_output_contains(channel_id, marker.as_bytes(), Duration::from_secs(15))
        .await
        .expect("receive binary output for channel");

    let resize = harness
        .rpc(
            "terminal.resize",
            json!({
                "thread_id": thread_id,
                "preset": "terminal",
                "cols": 120,
                "rows": 40
            }),
        )
        .await
        .expect("resize terminal");
    assert_eq!(resize["resized"], true);

    let detach = harness
        .rpc(
            "terminal.detach",
            json!({ "thread_id": thread_id, "preset": "terminal" }),
        )
        .await
        .expect("detach terminal");
    assert_eq!(detach["detached"], true);

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn terminal_attach_nonexistent_thread_returns_error() {
    let mut harness = setup_test_server().await;

    let error = harness
        .rpc_expect_error(
            "terminal.attach",
            json!({ "thread_id": "missing", "preset": "terminal" }),
        )
        .await;

    assert!(error.contains("thread not found"), "{error}");
}

#[tokio::test]
async fn terminal_double_attach_returns_same_channel() {
    if !common::tmux_available().await {
        eprintln!("skipping terminal_double_attach_returns_same_channel: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let first = harness
        .rpc(
            "terminal.attach",
            json!({ "thread_id": thread_id, "preset": "terminal" }),
        )
        .await
        .expect("first attach");
    let second = harness
        .rpc(
            "terminal.attach",
            json!({ "thread_id": thread_id, "preset": "terminal" }),
        )
        .await
        .expect("second attach");

    assert_eq!(first["channel_id"], second["channel_id"]);

    let _ = harness
        .rpc(
            "terminal.detach",
            json!({ "thread_id": thread_id, "preset": "terminal" }),
        )
        .await;

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn reattach_receives_scrollback_replay() {
    if !common::tmux_available().await {
        eprintln!("skipping reattach_receives_scrollback_replay: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let first_attach = harness
        .rpc(
            "terminal.attach",
            json!({ "thread_id": thread_id, "preset": "terminal" }),
        )
        .await
        .expect("attach terminal first time");
    let first_channel_id = first_attach["channel_id"]
        .as_u64()
        .expect("channel id in first attach response") as u16;
    assert!(first_channel_id > 0);

    let marker = common::unique_name("hello-scrollback");
    let command = format!("printf '{marker}\\n'\\n");
    harness
        .send_binary(first_channel_id, command.as_bytes())
        .await
        .expect("send binary input on first attach");

    harness
        .wait_for_channel_output_contains(
            first_channel_id,
            marker.as_bytes(),
            Duration::from_secs(15),
        )
        .await
        .expect("receive marker output before detach");

    let detach = harness
        .rpc(
            "terminal.detach",
            json!({ "thread_id": thread_id, "preset": "terminal" }),
        )
        .await
        .expect("detach terminal");
    assert_eq!(detach["detached"], true);

    let second_attach = harness
        .rpc(
            "terminal.attach",
            json!({ "thread_id": thread_id, "preset": "terminal" }),
        )
        .await
        .expect("attach terminal second time");
    let second_channel_id = second_attach["channel_id"]
        .as_u64()
        .expect("channel id in second attach response") as u16;
    assert!(second_channel_id > 0);

    let replay_result = harness
        .wait_for_channel_output_contains(
            second_channel_id,
            marker.as_bytes(),
            Duration::from_secs(2),
        )
        .await;

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;

    let replay_bytes = replay_result.expect("reattach should replay recent scrollback output");

    // Scrollback replay must use CR+LF line endings so terminals render
    // correctly. Bare LF causes diagonal/staircase text in raw-mode PTYs.
    for (i, &byte) in replay_bytes.iter().enumerate() {
        if byte == b'\n' {
            assert!(
                i > 0 && replay_bytes[i - 1] == b'\r',
                "bare LF at byte offset {i} in scrollback replay (expected CR+LF). \
                 Context: {:?}",
                String::from_utf8_lossy(
                    &replay_bytes[i.saturating_sub(20)..replay_bytes.len().min(i + 20)]
                )
            );
        }
    }
}
