mod common;

use std::time::Duration;

use serde_json::json;

const CHAT_AGENT_CONFIG: &str = r#"agents:
  mock:
    command: "./mock-chat-agent.sh"
"#;

async fn setup_test_server() -> common::TestHarness {
    common::setup_test_server().await
}

async fn add_project(harness: &mut common::TestHarness) -> (common::TestProject, String) {
    let project = common::create_git_project(Some(CHAT_AGENT_CONFIG), true)
        .await
        .expect("create test git project");
    std::fs::write(
        project.repo_path.join("mock-chat-agent.sh"),
        r#"#!/usr/bin/env python3
import json
import sys
import time

for raw in sys.stdin.buffer:
    text = raw.decode("utf-8", "replace").strip()
    try:
        msg = json.loads(text)
    except Exception:
        sys.stdout.buffer.write(raw)
        sys.stdout.flush()
        continue

    rid = msg.get("id")
    method = msg.get("method")
    if method == "initialize":
        time.sleep(0.2)
        result = {"protocolVersion": 1}
    elif method == "session/new":
        result = {
            "sessionId": "acp-session-1",
            "modes": {"availableModes": [], "currentModeId": None},
            "models": {"availableModels": [], "currentModelId": None},
            "configOptions": [],
        }
    elif method == "session/load":
        sid = (msg.get("params") or {}).get("sessionId", "acp-session-1")
        result = {
            "sessionId": sid,
            "modes": {"availableModes": [], "currentModeId": None},
            "models": {"availableModels": [], "currentModelId": None},
            "configOptions": [],
        }
    else:
        result = {}

    response = {"jsonrpc": "2.0", "id": rid, "result": result}
    sys.stdout.write(json.dumps(response) + "\n")
    sys.stdout.flush()
"#,
    )
    .expect("write mock chat agent script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = project.repo_path.join("mock-chat-agent.sh");
        let mut perms = std::fs::metadata(&path)
            .expect("read script metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod mock chat agent script");
    }

    let repo = project.repo_path.to_string_lossy().to_string();
    let add_output = std::process::Command::new("git")
        .args(["-C", &repo, "add", "mock-chat-agent.sh"])
        .output()
        .expect("git add mock chat agent script");
    assert!(
        add_output.status.success(),
        "git add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );

    let commit_output = std::process::Command::new("git")
        .args(["-C", &repo, "commit", "-m", "add mock chat agent"])
        .output()
        .expect("git commit mock chat agent script");
    assert!(
        commit_output.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&commit_output.stderr)
    );

    let push_output = std::process::Command::new("git")
        .args(["-C", &repo, "push", "origin", "main"])
        .output()
        .expect("git push mock chat agent script");
    assert!(
        push_output.status.success(),
        "git push failed: {}",
        String::from_utf8_lossy(&push_output.stderr)
    );

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
                "name": common::unique_name("chat"),
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

async fn wait_for_chat_ready(harness: &mut common::TestHarness, thread_id: &str, session_id: &str) {
    loop {
        let ready = harness
            .wait_for_event("chat.session_ready", Duration::from_secs(10))
            .await;
        if let Ok(event) = ready {
            if event["params"]["thread_id"] == thread_id && event["params"]["session_id"] == session_id {
                return;
            }
        }

        let failed = harness
            .wait_for_event("chat.session_failed", Duration::from_secs(1))
            .await;
        if let Ok(event) = failed {
            if event["params"]["thread_id"] == thread_id && event["params"]["session_id"] == session_id {
                panic!("chat session failed: {}", event["params"]["error"]);
            }
        }
    }
}

#[tokio::test]
async fn chat_start_emits_ready_and_lists_in_snapshot() {
    if !common::tmux_available().await {
        eprintln!("skipping chat_start_emits_ready_and_lists_in_snapshot: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let started = harness
        .rpc(
            "chat.start",
            json!({"thread_id": thread_id, "agent_name": "mock"}),
        )
        .await
        .expect("chat.start");
    let session_id = started["session_id"].as_str().expect("session_id").to_string();
    assert_eq!(started["status"], "starting");

    let created = harness
        .wait_for_event("chat.session_created", Duration::from_secs(5))
        .await
        .expect("chat.session_created");
    assert_eq!(created["params"]["thread_id"], thread_id);
    assert_eq!(created["params"]["session_id"], session_id);

    wait_for_chat_ready(&mut harness, &thread_id, &session_id).await;

    let listed = harness
        .rpc("chat.list", json!({"thread_id": thread_id}))
        .await
        .expect("chat.list");
    let sessions = listed.as_array().expect("chat.list array");
    let session = sessions
        .iter()
        .find(|entry| entry["session_id"] == session_id)
        .expect("session in chat.list");
    assert_eq!(session["agent_type"], "mock");
    assert_eq!(session["status"], "ready");

    let snapshot = harness
        .rpc("state.snapshot", json!({}))
        .await
        .expect("state.snapshot");
    let thread = snapshot["threads"]
        .as_array()
        .expect("threads array")
        .iter()
        .find(|thread| thread["id"] == thread_id)
        .expect("thread in snapshot");
    let chat_sessions = thread["chat_sessions"].as_array().expect("chat sessions array");
    assert!(chat_sessions.iter().any(|entry| entry["session_id"] == session_id));

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn chat_attach_queues_until_ready_and_fans_out_to_all_channels() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping chat_attach_queues_until_ready_and_fans_out_to_all_channels: tmux unavailable"
        );
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let started = harness
        .rpc(
            "chat.start",
            json!({"thread_id": thread_id, "agent_name": "mock"}),
        )
        .await
        .expect("chat.start");
    let session_id = started["session_id"].as_str().expect("session_id").to_string();

    let first_attach = harness
        .rpc(
            "chat.attach",
            json!({"thread_id": thread_id, "session_id": session_id}),
        )
        .await
        .expect("chat.attach while starting");
    let first_channel = first_attach["channel_id"].as_u64().expect("channel_id") as u16;

    let second_attach = harness
        .rpc(
            "chat.attach",
            json!({"thread_id": thread_id, "session_id": session_id}),
        )
        .await
        .expect("second chat.attach");
    let second_channel = second_attach["channel_id"].as_u64().expect("channel_id") as u16;
    assert_ne!(first_channel, second_channel);

    wait_for_chat_ready(&mut harness, &thread_id, &session_id).await;

    let marker = common::unique_name("chat-fanout");
    harness
        .send_binary(first_channel, format!("{marker}\n").as_bytes())
        .await
        .expect("send binary to first channel");

    let first_output = harness
        .wait_for_channel_output_contains(first_channel, marker.as_bytes(), Duration::from_secs(5))
        .await
        .expect("first channel output");
    let second_output = harness
        .wait_for_channel_output_contains(second_channel, marker.as_bytes(), Duration::from_secs(5))
        .await
        .expect("second channel output");
    assert!(String::from_utf8_lossy(&first_output).contains(&marker));
    assert!(String::from_utf8_lossy(&second_output).contains(&marker));

    harness
        .rpc("chat.detach", json!({"channel_id": second_channel}))
        .await
        .expect("chat.detach");
    let detached_marker = common::unique_name("chat-detached");
    harness
        .send_binary(first_channel, format!("{detached_marker}\n").as_bytes())
        .await
        .expect("send binary to first channel after detach");
    harness
        .wait_for_channel_output_contains(
            first_channel,
            detached_marker.as_bytes(),
            Duration::from_secs(5),
        )
        .await
        .expect("first channel output after detach");
    harness
        .expect_no_channel_output_contains(
            second_channel,
            detached_marker.as_bytes(),
            Duration::from_secs(2),
        )
        .await
        .expect("detached channel should stop receiving output");

    harness
        .rpc(
            "chat.stop",
            json!({"thread_id": thread_id, "session_id": session_id}),
        )
        .await
        .expect("chat.stop");
    let ended = harness
        .wait_for_event("chat.session_ended", Duration::from_secs(10))
        .await
        .expect("chat.session_ended");
    assert_eq!(ended["params"]["thread_id"], thread_id);

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn chat_load_restarts_archived_session() {
    if !common::tmux_available().await {
        eprintln!("skipping chat_load_restarts_archived_session: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let started = harness
        .rpc(
            "chat.start",
            json!({"thread_id": thread_id, "agent_name": "mock"}),
        )
        .await
        .expect("chat.start");
    let session_id = started["session_id"].as_str().expect("session_id").to_string();
    wait_for_chat_ready(&mut harness, &thread_id, &session_id).await;

    harness
        .rpc(
            "chat.stop",
            json!({"thread_id": thread_id, "session_id": session_id}),
        )
        .await
        .expect("chat.stop");
    let _ = harness
        .wait_for_event("chat.session_ended", Duration::from_secs(10))
        .await
        .expect("chat.session_ended");

    let loaded = harness
        .rpc(
            "chat.load",
            json!({"thread_id": thread_id, "session_id": session_id}),
        )
        .await
        .expect("chat.load");
    assert_eq!(loaded["status"], "starting");
    wait_for_chat_ready(&mut harness, &thread_id, &session_id).await;

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn thread_close_stops_chat_sessions() {
    if !common::tmux_available().await {
        eprintln!("skipping thread_close_stops_chat_sessions: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let started = harness
        .rpc(
            "chat.start",
            json!({"thread_id": thread_id, "agent_name": "mock"}),
        )
        .await
        .expect("chat.start");
    let session_id = started["session_id"].as_str().expect("session_id").to_string();
    wait_for_chat_ready(&mut harness, &thread_id, &session_id).await;

    harness
        .rpc(
            "thread.close",
            json!({ "thread_id": thread_id, "mode": "close" }),
        )
        .await
        .expect("thread.close");

    let ended = harness
        .wait_for_event("chat.session_ended", Duration::from_secs(10))
        .await
        .expect("chat.session_ended");
    assert_eq!(ended["params"]["thread_id"], thread_id);
    assert_eq!(ended["params"]["session_id"], session_id);

    let listed = harness
        .rpc("chat.list", json!({"thread_id": thread_id}))
        .await
        .expect("chat.list");
    assert!(listed.as_array().expect("chat.list array").is_empty());

    let _ = harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await;
}
