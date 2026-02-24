mod common;

use std::time::Duration;

use serde_json::json;

const MULTI_CHANNEL_CONFIG: &str = r#"presets:
  terminal:
    label: Terminal
    commands:
      - "$SHELL"
    autostart: true
  aux:
    label: Aux
    commands:
      - "$SHELL"
    autostart: true
"#;

async fn setup_test_server() -> common::TestHarness {
    common::setup_test_server().await
}

async fn add_project(harness: &mut common::TestHarness) -> (common::TestProject, String) {
    let project = common::create_git_project(Some(MULTI_CHANNEL_CONFIG), true)
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
                "name": common::unique_name("binary"),
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
async fn binary_frames_route_to_matching_channels() {
    if !common::tmux_available().await {
        eprintln!("skipping binary_frames_route_to_matching_channels: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let terminal_attach = harness
        .rpc(
            "terminal.attach",
            json!({ "thread_id": thread_id, "preset": "terminal" }),
        )
        .await
        .expect("attach terminal preset");
    let aux_attach = harness
        .rpc(
            "terminal.attach",
            json!({ "thread_id": thread_id, "preset": "aux" }),
        )
        .await
        .expect("attach aux preset");

    let terminal_channel = terminal_attach["channel_id"].as_u64().expect("channel id") as u16;
    let aux_channel = aux_attach["channel_id"].as_u64().expect("channel id") as u16;
    assert!(terminal_channel > 0);
    assert!(aux_channel > 0);
    assert_ne!(terminal_channel, aux_channel);

    let terminal_marker = common::unique_name("ch-terminal");
    let aux_marker = common::unique_name("ch-aux");

    harness
        .send_binary(
            terminal_channel,
            format!("printf '{terminal_marker}\\n'\n").as_bytes(),
        )
        .await
        .expect("send terminal channel input");
    harness
        .send_binary(aux_channel, format!("printf '{aux_marker}\\n'\n").as_bytes())
        .await
        .expect("send aux channel input");

    let terminal_output = harness
        .wait_for_channel_output_contains(
            terminal_channel,
            terminal_marker.as_bytes(),
            Duration::from_secs(20),
        )
        .await
        .expect("terminal channel output");
    let aux_output = harness
        .wait_for_channel_output_contains(aux_channel, aux_marker.as_bytes(), Duration::from_secs(20))
        .await
        .expect("aux channel output");

    assert!(
        !String::from_utf8_lossy(&terminal_output).contains(&aux_marker),
        "terminal channel should not contain aux marker"
    );
    assert!(
        !String::from_utf8_lossy(&aux_output).contains(&terminal_marker),
        "aux channel should not contain terminal marker"
    );

    let _ = harness
        .rpc(
            "terminal.detach",
            json!({ "thread_id": thread_id, "preset": "terminal" }),
        )
        .await;
    let _ = harness
        .rpc(
            "terminal.detach",
            json!({ "thread_id": thread_id, "preset": "aux" }),
        )
        .await;

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn binary_channel_zero_is_rejected() {
    if !common::tmux_available().await {
        eprintln!("skipping binary_channel_zero_is_rejected: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let marker = common::unique_name("channel-zero");
    harness
        .send_binary(0, format!("printf '{marker}\\n'\n").as_bytes())
        .await
        .expect("send channel 0 frame");
    harness
        .expect_no_channel_output_contains(0, marker.as_bytes(), Duration::from_secs(2))
        .await
        .expect("channel 0 should not produce output");

    let pong = harness
        .rpc("ping", json!({}))
        .await
        .expect("ping after channel 0 frame");
    assert_eq!(pong, "pong");

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn binary_input_on_detached_channel_is_ignored() {
    if !common::tmux_available().await {
        eprintln!("skipping binary_input_on_detached_channel_is_ignored: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let attached = harness
        .rpc(
            "terminal.attach",
            json!({ "thread_id": thread_id, "preset": "terminal" }),
        )
        .await
        .expect("attach terminal");
    let channel_id = attached["channel_id"].as_u64().expect("channel id") as u16;

    harness
        .rpc(
            "terminal.detach",
            json!({ "thread_id": thread_id, "preset": "terminal" }),
        )
        .await
        .expect("detach terminal");

    let marker = common::unique_name("detached-channel");
    harness
        .send_binary(channel_id, format!("printf '{marker}\\n'\n").as_bytes())
        .await
        .expect("send detached-channel frame");
    harness
        .expect_no_channel_output_contains(channel_id, marker.as_bytes(), Duration::from_secs(3))
        .await
        .expect("detached channel should not produce output");

    let pong = harness
        .rpc("ping", json!({}))
        .await
        .expect("ping after detached-channel frame");
    assert_eq!(pong, "pong");

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}
