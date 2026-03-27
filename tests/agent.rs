mod common;

use std::time::Duration;

use serde_json::json;

#[tokio::test]
async fn agent_start_relay_stop_lifecycle() {
    let mut harness = common::setup_test_server().await;

    let config = r#"agents:
  echo:
    command: cat
"#;

    let project = common::create_git_project(Some(config), false)
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

    let listed = harness
        .rpc("project.list", json!({}))
        .await
        .expect("list projects");
    let project_row = listed
        .as_array()
        .expect("project.list returns array")
        .iter()
        .find(|project| project["id"] == project_id)
        .expect("project present in list");
    let agents = project_row["agents"]
        .as_array()
        .expect("project includes agents array");
    assert!(agents.iter().any(|agent| {
        agent["name"] == "echo" && agent["command"] == "cat" && agent["cwd"].is_null()
    }));

    let started = harness
        .rpc(
            "agent.start",
            json!({ "project_id": project_id, "agent_name": "echo" }),
        )
        .await
        .expect("start agent");
    let channel_id = started["channel_id"]
        .as_u64()
        .expect("channel_id in response") as u16;
    assert!(channel_id > 0);

    let started_event = harness
        .wait_for_event("agent.status_changed", Duration::from_secs(5))
        .await
        .expect("wait for agent started event");
    assert_eq!(started_event["params"]["channel_id"], channel_id);
    assert_eq!(started_event["params"]["event"], "started");

    let marker = common::unique_name("agent-echo");
    harness
        .send_binary(channel_id, format!("{marker}\n").as_bytes())
        .await
        .expect("send agent stdin payload");

    harness
        .wait_for_channel_output_contains(channel_id, marker.as_bytes(), Duration::from_secs(10))
        .await
        .expect("receive agent stdout payload");

    harness
        .rpc("agent.stop", json!({ "channel_id": channel_id }))
        .await
        .expect("stop agent");

    let stopped_event = harness
        .wait_for_event("agent.status_changed", Duration::from_secs(5))
        .await
        .expect("wait for agent stopped event");
    assert_eq!(stopped_event["params"]["channel_id"], channel_id);
    assert!(
        stopped_event["params"]["event"] == "exited"
            || stopped_event["params"]["event"] == "crashed"
    );
}
