mod common;

use std::time::Duration;

use serde_json::json;

const PRESET_CONFIG: &str = r#"presets:
  watch:
    label: Watch
    commands:
      - "sleep 120"
    autostart: false
"#;

async fn setup_test_server() -> common::TestHarness {
    common::setup_test_server().await
}

async fn add_project(harness: &mut common::TestHarness) -> (common::TestProject, String) {
    let project = common::create_git_project(Some(PRESET_CONFIG), true)
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
                "name": common::unique_name("preset"),
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
async fn preset_start_stop_restart_lifecycle() {
    if !common::tmux_available().await {
        eprintln!("skipping preset_start_stop_restart_lifecycle: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let started = harness
        .rpc(
            "preset.start",
            json!({ "thread_id": thread_id, "preset": "watch" }),
        )
        .await
        .expect("start preset");
    assert_eq!(started["ok"], true);

    loop {
        let output_event = harness
            .wait_for_event("preset.output", Duration::from_secs(10))
            .await
            .expect("wait for preset.output");
        if output_event["params"]["thread_id"] != thread_id
            || output_event["params"]["preset"] != "watch"
        {
            continue;
        }

        assert_eq!(output_event["params"]["stream"], "stdout");
        assert!(output_event["params"]["chunk"]
            .as_str()
            .map(|chunk| !chunk.is_empty())
            .unwrap_or(false));
        break;
    }

    let attached = harness
        .rpc(
            "terminal.attach",
            json!({ "thread_id": thread_id, "preset": "watch" }),
        )
        .await
        .expect("attach watch preset");
    assert!(attached["channel_id"].as_u64().expect("channel id") > 0);

    harness
        .rpc(
            "terminal.detach",
            json!({ "thread_id": thread_id, "preset": "watch" }),
        )
        .await
        .expect("detach watch preset");

    let stopped = harness
        .rpc(
            "preset.stop",
            json!({ "thread_id": thread_id, "preset": "watch" }),
        )
        .await
        .expect("stop preset");
    assert_eq!(stopped["ok"], true);

    let attach_error = harness
        .rpc_expect_error(
            "terminal.attach",
            json!({ "thread_id": thread_id, "preset": "watch" }),
        )
        .await;
    assert!(
        attach_error.contains("tmux list-panes failed"),
        "{attach_error}"
    );

    let restarted = harness
        .rpc(
            "preset.restart",
            json!({ "thread_id": thread_id, "preset": "watch" }),
        )
        .await
        .expect("restart preset");
    assert_eq!(restarted["ok"], true);

    let reattached = harness
        .rpc(
            "terminal.attach",
            json!({ "thread_id": thread_id, "preset": "watch" }),
        )
        .await
        .expect("attach watch preset after restart");
    assert!(reattached["channel_id"].as_u64().expect("channel id") > 0);

    harness
        .rpc(
            "terminal.detach",
            json!({ "thread_id": thread_id, "preset": "watch" }),
        )
        .await
        .expect("detach watch preset after restart");

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}
