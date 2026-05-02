mod common;

use std::{path::PathBuf, time::Duration};

use serde_json::{json, Value};

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
import os
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
    with open(os.path.join(os.getcwd(), "mock-chat-agent-log.jsonl"), "a", encoding="utf-8") as fh:
        fh.write(json.dumps({
            "id": rid,
            "method": method,
            "params": msg.get("params"),
            "result": msg.get("result"),
            "error": msg.get("error"),
        }) + "\n")
    if method is None:
        continue
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
    elif method == "threadmill/testUpdate":
        marker = (msg.get("params") or {}).get("marker", "")
        update = {
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "acp-session-1",
                "update": {
                    "kind": "agent_message_chunk",
                    "content": marker,
                },
            },
        }
        sys.stdout.write(json.dumps(update) + "\n")
        sys.stdout.flush()
        result = {"ok": True}
    elif method == "threadmill/emitPermission":
        params = msg.get("params") or {}
        request_id = params.get("requestId", "agent-permission")
        request = {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": params.get("method", "session/request_permission"),
            "params": {
                "sessionId": "acp-session-1",
                "message": params.get("message", "Allow shell command?"),
                "options": params.get("options", [{"optionId": "run", "name": "Run", "kind": "allow"}])
            },
        }
        sys.stdout.write(json.dumps(request) + "\n")
        sys.stdout.flush()
        result = {"ok": True}
    elif method == "threadmill/emitElicitation":
        params = msg.get("params") or {}
        request_id = params.get("requestId", "agent-elicitation")
        request = {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": params.get("method", "session/elicitation"),
            "params": {
                "sessionId": "acp-session-1",
                "message": params.get("message", "Continue?"),
                "requestedSchema": params.get("requestedSchema", {
                    "type": "object",
                    "properties": {
                        "decision": {
                            "type": "string",
                            "title": "Decision",
                            "oneOf": [
                                {"const": "yes", "title": "Yes"},
                                {"const": "no", "title": "No"}
                            ]
                        }
                    },
                    "required": ["decision"]
                })
            },
        }
        sys.stdout.write(json.dumps(request) + "\n")
        sys.stdout.flush()
        result = {"ok": True}
    elif method == "session/prompt" and "TRIGGER_PERMISSION" in json.dumps(msg.get("params") or {}):
        request = {
            "jsonrpc": "2.0",
            "id": "agent-permission-before-attach",
            "method": "request_permission",
            "params": {
                "message": "Allow shell command?",
                "options": [{"optionId": "run", "name": "Run", "kind": "allow"}]
            },
        }
        sys.stdout.write(json.dumps(request) + "\n")
        sys.stdout.flush()
        result = {}
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

async fn create_thread_with_worktree(
    harness: &mut common::TestHarness,
    project_id: &str,
) -> (String, PathBuf) {
    let created = harness
        .rpc(
            "thread.create",
            json!({
                "project_id": project_id,
                "name": common::unique_name("chat"),
                "source_type": "new_feature",
                "sandbox": true
            }),
        )
        .await
        .expect("create thread");

    (
        created["id"]
            .as_str()
            .expect("thread id in create response")
            .to_string(),
        PathBuf::from(
            created["worktree_path"]
                .as_str()
                .expect("worktree path in create response"),
        ),
    )
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
            .wait_for_event("chat.session_ready", Duration::from_secs(5))
            .await;
        if let Ok(event) = ready {
            if event["params"]["thread_id"] == thread_id
                && event["params"]["session_id"] == session_id
            {
                return;
            }
        }

        let failed = harness
            .wait_for_event("chat.session_failed", Duration::from_secs(1))
            .await;
        if let Ok(event) = failed {
            if event["params"]["thread_id"] == thread_id
                && event["params"]["session_id"] == session_id
            {
                panic!("chat session failed: {}", event["params"]["error"]);
            }
        }
    }
}

async fn send_test_update(harness: &mut common::TestHarness, channel_id: u16, marker: &str) {
    let request = json!({
        "jsonrpc": "2.0",
        "id": 42,
        "method": "threadmill/testUpdate",
        "params": {
            "marker": marker,
        }
    });
    harness
        .send_binary(channel_id, format!("{}\n", request).as_bytes())
        .await
        .expect("send test update request");
}

async fn wait_for_injection_complete(
    harness: &mut common::TestHarness,
    thread_id: &str,
    session_id: &str,
) {
    loop {
        let event = harness
            .wait_for_event("chat.injection_complete", Duration::from_secs(5))
            .await
            .expect("chat.injection_complete");
        if event["params"]["thread_id"] == thread_id && event["params"]["session_id"] == session_id
        {
            return;
        }
    }
}

fn chat_history_path(thread_id: &str, session_id: &str) -> std::path::PathBuf {
    let config_home = std::env::var("XDG_CONFIG_HOME").expect("XDG_CONFIG_HOME set by harness");
    std::path::Path::new(&config_home)
        .join("threadmill")
        .join("chat")
        .join(thread_id)
        .join(format!("{session_id}.jsonl"))
}

fn chat_metadata_path(thread_id: &str, session_id: &str) -> std::path::PathBuf {
    let config_home = std::env::var("XDG_CONFIG_HOME").expect("XDG_CONFIG_HOME set by harness");
    std::path::Path::new(&config_home)
        .join("threadmill")
        .join("chat")
        .join(thread_id)
        .join(format!("{session_id}.metadata.json"))
}

fn read_agent_log(worktree_path: &std::path::Path) -> Vec<Value> {
    let path = worktree_path.join("mock-chat-agent-log.jsonl");
    let Ok(log) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    log.lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("parse agent log line"))
        .collect()
}

fn agent_log_has_response(worktree_path: &std::path::Path, request_id: &str) -> bool {
    read_agent_log(worktree_path).iter().any(|entry| {
        entry["id"] == request_id && (entry.get("result").is_some() || entry.get("error").is_some())
    })
}

async fn wait_for_agent_response(worktree_path: &std::path::Path, request_id: &str) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(entry) = read_agent_log(worktree_path).into_iter().find(|entry| {
            entry["id"] == request_id
                && (entry.get("result").is_some() || entry.get("error").is_some())
        }) {
            return entry;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for agent response {request_id}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
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
    let session_id = started["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();
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
    let chat_sessions = thread["chat_sessions"]
        .as_array()
        .expect("chat sessions array");
    assert!(chat_sessions
        .iter()
        .any(|entry| entry["session_id"] == session_id));

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn capable_chat_start_captures_initial_prompt_permission_before_first_attach() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping capable_chat_start_captures_initial_prompt_permission_before_first_attach: tmux unavailable"
        );
        return;
    }

    let mut harness = common::setup_test_server_with_capabilities(&[
        "state.delta.operations.v1",
        "preset.output.v1",
        "rpc.errors.structured.v1",
        "chat.blocked_requests.v1",
    ])
    .await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let started = harness
        .rpc(
            "chat.start",
            json!({
                "thread_id": thread_id,
                "agent_name": "mock",
                "initial_prompt": "TRIGGER_PERMISSION before any attach"
            }),
        )
        .await
        .expect("chat.start");
    let session_id = started["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();

    wait_for_chat_ready(&mut harness, &thread_id, &session_id).await;
    let blocked = harness
        .wait_for_event("chat.blocked_request.added", Duration::from_secs(5))
        .await
        .expect("blocked request from initial prompt");
    assert_eq!(
        blocked["params"]["request"]["request_id"],
        "agent-permission-before-attach"
    );

    let listed = harness
        .rpc("chat.list", json!({"thread_id": thread_id}))
        .await
        .expect("chat.list");
    let session = listed
        .as_array()
        .expect("chat.list array")
        .iter()
        .find(|entry| entry["session_id"] == session_id)
        .expect("session in chat.list");
    assert_eq!(
        session["pending_blocked_requests"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let attached = harness
        .rpc(
            "chat.attach",
            json!({"thread_id": thread_id, "session_id": session_id}),
        )
        .await
        .expect("chat.attach");
    assert_eq!(
        attached["pending_blocked_requests"][0]["request_id"],
        "agent-permission-before-attach"
    );

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn capable_chat_answers_synthetic_elicitation_and_permission_after_rpc_and_reconnect() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping capable_chat_answers_synthetic_elicitation_and_permission_after_rpc_and_reconnect: tmux unavailable"
        );
        return;
    }

    let capabilities = [
        "state.delta.operations.v1",
        "preset.output.v1",
        "rpc.errors.structured.v1",
        "chat.blocked_requests.v1",
    ];
    let mut harness = common::setup_test_server_with_capabilities(&capabilities).await;
    let (_project, project_id) = add_project(&mut harness).await;
    let (thread_id, worktree_path) = create_thread_with_worktree(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let started = harness
        .rpc(
            "chat.start",
            json!({"thread_id": thread_id, "agent_name": "mock"}),
        )
        .await
        .expect("chat.start");
    let session_id = started["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();
    wait_for_chat_ready(&mut harness, &thread_id, &session_id).await;
    let initialize = read_agent_log(&worktree_path)
        .into_iter()
        .find(|entry| entry["method"] == "initialize")
        .expect("agent initialize request logged");
    assert!(
        initialize["params"]["clientCapabilities"]
            .get("elicitation")
            .is_some(),
        "blocked-request-capable clients should advertise ACP elicitation through daemon capture"
    );
    let attached = harness
        .rpc(
            "chat.attach",
            json!({"thread_id": thread_id, "session_id": session_id}),
        )
        .await
        .expect("chat.attach");
    let channel_id = attached["channel_id"].as_u64().expect("channel_id") as u16;

    let requested_schema = json!({
        "type": "object",
        "properties": {
            "decision": {
                "type": "string",
                "title": "Decision",
                "oneOf": [
                    {"const": "yes", "title": "Yes"},
                    {"const": "no", "title": "No"}
                ]
            }
        },
        "required": ["decision"]
    });
    for (request_id, action, expected_action, answer_content) in [
        (
            "elicitation-accept",
            protocol_action("accept"),
            "accept",
            Some(json!({"decision": "yes"})),
        ),
        (
            "elicitation-decline",
            protocol_action("decline"),
            "decline",
            None,
        ),
        (
            "elicitation-cancel",
            protocol_action("cancel"),
            "cancel",
            None,
        ),
    ] {
        harness
            .send_binary(
                channel_id,
                format!(
                    "{}\n",
                    json!({
                        "jsonrpc": "2.0",
                        "id": format!("emit-{request_id}"),
                        "method": "threadmill/emitElicitation",
                        "params": {
                            "requestId": request_id,
                            "message": format!("Question {request_id}?"),
                            "requestedSchema": requested_schema.clone()
                        }
                    })
                )
                .as_bytes(),
            )
            .await
            .expect("send synthetic elicitation trigger");
        let blocked = harness
            .wait_for_event("chat.blocked_request.added", Duration::from_secs(5))
            .await
            .expect("elicitation blocked request added");
        assert_eq!(blocked["params"]["request"]["request_id"], request_id);
        assert_eq!(blocked["params"]["request"]["kind"], "question");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !agent_log_has_response(&worktree_path, request_id),
            "agent must not receive elicitation response before answer RPC"
        );

        let mut answer_params = json!({
            "thread_id": thread_id,
            "session_id": session_id,
            "request_id": request_id,
            "action": action,
        });
        if let Some(content) = answer_content.clone() {
            answer_params["content"] = content;
        }
        let answered = harness
            .rpc("chat.answer_blocked_request", answer_params)
            .await
            .expect("answer elicitation blocked request");
        assert_eq!(answered["request_id"], request_id);
        let response = wait_for_agent_response(&worktree_path, request_id).await;
        assert_elicitation_response_matches_schema(
            &response,
            expected_action,
            &requested_schema,
            answer_content.as_ref(),
        );
    }

    harness
        .send_binary(
            channel_id,
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": "emit-permission-option",
                    "method": "threadmill/emitPermission",
                    "params": {
                        "requestId": "permission-option",
                        "options": [
                            {"optionId": "allow_once", "name": "Allow once", "kind": "allow"},
                            {"optionId": "deny", "name": "Deny", "kind": "deny"}
                        ]
                    }
                })
            )
            .as_bytes(),
        )
        .await
        .expect("send synthetic permission trigger");
    let permission = harness
        .wait_for_event("chat.blocked_request.added", Duration::from_secs(5))
        .await
        .expect("permission blocked request added");
    assert_eq!(
        permission["params"]["request"]["request_id"],
        "permission-option"
    );
    assert_eq!(permission["params"]["request"]["kind"], "permission");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !agent_log_has_response(&worktree_path, "permission-option"),
        "agent must not receive permission response before answer RPC"
    );
    harness
        .rpc(
            "chat.answer_blocked_request",
            json!({
                "thread_id": thread_id,
                "session_id": session_id,
                "request_id": "permission-option",
                "option_id": "allow_once",
            }),
        )
        .await
        .expect("answer permission blocked request");
    let permission_response = wait_for_agent_response(&worktree_path, "permission-option").await;
    assert_eq!(
        permission_response["result"]["outcome"]["optionId"],
        "allow_once"
    );

    harness
        .send_binary(
            channel_id,
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": "emit-permission-reconnect",
                    "method": "threadmill/emitPermission",
                    "params": {"requestId": "permission-reconnect"}
                })
            )
            .as_bytes(),
        )
        .await
        .expect("send reconnect permission trigger");
    harness
        .wait_for_event("chat.blocked_request.added", Duration::from_secs(5))
        .await
        .expect("reconnect permission blocked request added");
    harness
        .reconnect_with_capabilities(&capabilities)
        .await
        .expect("reconnect websocket");
    let reattached = harness
        .rpc(
            "chat.attach",
            json!({"thread_id": thread_id, "session_id": session_id}),
        )
        .await
        .expect("reattach after reconnect");
    assert!(reattached["pending_blocked_requests"]
        .as_array()
        .expect("pending blocked requests")
        .iter()
        .any(|request| request["request_id"] == "permission-reconnect"));
    harness
        .rpc(
            "chat.answer_blocked_request",
            json!({
                "thread_id": thread_id,
                "session_id": session_id,
                "request_id": "permission-reconnect",
                "option_id": "run",
            }),
        )
        .await
        .expect("answer reconnect permission");
    let reconnect_response = wait_for_agent_response(&worktree_path, "permission-reconnect").await;
    assert_eq!(reconnect_response["result"]["outcome"]["optionId"], "run");

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn noncapable_chat_start_does_not_advertise_acp_elicitation() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping noncapable_chat_start_does_not_advertise_acp_elicitation: tmux unavailable"
        );
        return;
    }

    let threadmill_capabilities = [
        "state.delta.operations.v1",
        "preset.output.v1",
        "rpc.errors.structured.v1",
    ];
    let mut harness = common::setup_test_server_with_capabilities(&threadmill_capabilities).await;
    let (_project, project_id) = add_project(&mut harness).await;
    let (thread_id, worktree_path) = create_thread_with_worktree(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let started = harness
        .rpc(
            "chat.start",
            json!({"thread_id": thread_id, "agent_name": "mock"}),
        )
        .await
        .expect("chat.start");
    let session_id = started["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();
    wait_for_chat_ready(&mut harness, &thread_id, &session_id).await;

    let initialize = read_agent_log(&worktree_path)
        .into_iter()
        .find(|entry| entry["method"] == "initialize")
        .expect("agent initialize request logged");
    assert!(
        initialize["params"]["clientCapabilities"]
            .get("elicitation")
            .is_none(),
        "Threadmill-current clients must not advertise ACP elicitation unless daemon-owned blocked request capture is enabled"
    );

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

fn protocol_action(action: &str) -> Value {
    Value::String(action.to_string())
}

fn assert_elicitation_response_matches_schema(
    response: &Value,
    expected_action: &str,
    requested_schema: &Value,
    expected_content: Option<&Value>,
) {
    let action = response["result"]["action"]
        .as_object()
        .expect("ACP elicitation response action must be nested object");
    assert_eq!(
        action.get("action").and_then(Value::as_str),
        Some(expected_action)
    );
    if expected_action != "accept" {
        assert!(
            action.get("content").is_none(),
            "non-accept elicitation actions must not include content"
        );
        return;
    }
    let content = action
        .get("content")
        .expect("accepted elicitation response must include content");
    assert_eq!(Some(content), expected_content);
    let content_object = content
        .as_object()
        .expect("accepted elicitation content must be an object");
    for required in requested_schema["required"]
        .as_array()
        .expect("requested schema required array")
    {
        let required = required.as_str().expect("required field name");
        assert!(
            content_object.contains_key(required),
            "accepted elicitation content must satisfy required schema field {required}"
        );
    }
    let decision = content["decision"]
        .as_str()
        .expect("decision content must be string");
    let one_of = requested_schema["properties"]["decision"]["oneOf"]
        .as_array()
        .expect("decision oneOf options");
    assert!(
        one_of
            .iter()
            .any(|option| option["const"].as_str() == Some(decision)),
        "decision content must match requested schema oneOf"
    );
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
    let session_id = started["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();

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
    let session_id = started["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();
    wait_for_chat_ready(&mut harness, &thread_id, &session_id).await;

    harness
        .rpc(
            "chat.stop",
            json!({"thread_id": thread_id, "session_id": session_id}),
        )
        .await
        .expect("chat.stop");
    let _ = harness
        .wait_for_event("chat.session_ended", Duration::from_secs(5))
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
async fn chat_load_recovers_session_without_acp_session_id_via_session_new() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping chat_load_recovers_session_without_acp_session_id_via_session_new: tmux unavailable"
        );
        return;
    }

    let mut harness = setup_test_server().await;
    let (project, project_id) = add_project(&mut harness).await;
    let (thread_id, worktree_path) = create_thread_with_worktree(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let started = harness
        .rpc(
            "chat.start",
            json!({"thread_id": thread_id, "agent_name": "mock"}),
        )
        .await
        .expect("chat.start");
    let session_id = started["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();
    wait_for_chat_ready(&mut harness, &thread_id, &session_id).await;

    let attached = harness
        .rpc(
            "chat.attach",
            json!({"thread_id": thread_id, "session_id": session_id}),
        )
        .await
        .expect("chat.attach");
    let channel_id = attached["channel_id"].as_u64().expect("channel_id") as u16;

    send_test_update(&mut harness, channel_id, "persisted-before-recovery").await;
    harness
        .wait_for_channel_output_contains(
            channel_id,
            b"persisted-before-recovery",
            Duration::from_secs(5),
        )
        .await
        .expect("wait for persisted output");

    harness
        .rpc(
            "chat.stop",
            json!({"thread_id": thread_id, "session_id": session_id}),
        )
        .await
        .expect("chat.stop");
    harness
        .wait_for_event("chat.session_ended", Duration::from_secs(5))
        .await
        .expect("chat.session_ended");

    let config_home = harness.preserve_config_home();
    harness.preserve_path(&project.root_dir);
    std::fs::write(
        config_home.join("threadmill").join("system-prompt.md"),
        "platform prompt marker\n",
    )
    .expect("write system prompt");
    let history_path = chat_history_path(&thread_id, &session_id);
    let history = std::fs::read_to_string(&history_path).expect("read chat history");
    let mut lines = history.lines();
    let first_line = lines.next().expect("history first line");
    let mut first_entry: serde_json::Value =
        serde_json::from_str(first_line).expect("parse history first line");
    first_entry
        .as_object_mut()
        .expect("history entry object")
        .remove("sessionId");
    let mut rewritten = vec![serde_json::to_string(&first_entry).expect("serialize history entry")];
    rewritten.extend(lines.map(str::to_string));
    std::fs::write(&history_path, format!("{}\n", rewritten.join("\n")))
        .expect("rewrite chat history without ACP session id");
    std::fs::write(
        chat_metadata_path(&thread_id, &session_id),
        "{\"acp_session_id\":null}\n",
    )
    .expect("rewrite chat metadata without ACP session id");

    drop(harness);

    let mut recovered = common::setup_test_server_with_config_home(config_home).await;
    recovered.register_cleanup_path(project.root_dir.clone());
    let listed = recovered
        .rpc("chat.list", json!({"thread_id": thread_id}))
        .await
        .expect("chat.list after recovery");
    assert!(listed
        .as_array()
        .expect("chat.list array")
        .iter()
        .any(|entry| entry["session_id"] == session_id));

    let loaded = recovered
        .rpc(
            "chat.load",
            json!({"thread_id": thread_id, "session_id": session_id, "agent_name": "mock"}),
        )
        .await
        .expect("chat.load");
    assert_eq!(loaded["session_id"], session_id);
    assert_eq!(loaded["status"], "starting");
    wait_for_chat_ready(&mut recovered, &thread_id, &session_id).await;
    wait_for_injection_complete(&mut recovered, &thread_id, &session_id).await;

    let reattached = recovered
        .rpc(
            "chat.attach",
            json!({"thread_id": thread_id, "session_id": session_id}),
        )
        .await
        .expect("chat.attach after recovery load");
    assert_eq!(reattached["acp_session_id"], "acp-session-1");

    let agent_log = std::fs::read_to_string(worktree_path.join("mock-chat-agent-log.jsonl"))
        .expect("read mock chat agent log");
    let log_entries = agent_log
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("parse agent log entry"))
        .collect::<Vec<_>>();
    let session_new_count = log_entries
        .iter()
        .filter(|entry| entry["method"] == "session/new")
        .count();
    let session_load_count = log_entries
        .iter()
        .filter(|entry| entry["method"] == "session/load")
        .count();
    assert_eq!(
        session_new_count, 2,
        "start + recovery load should both use session/new"
    );
    assert_eq!(
        session_load_count, 0,
        "missing ACP session id must not use session/load"
    );
    assert!(log_entries.iter().any(|entry| {
        entry["method"] == "session/prompt"
            && entry["params"]["prompt"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|block| block["text"].as_str())
                .any(|text| text.contains("platform prompt marker"))
    }));

    cleanup_thread_project(&mut recovered, &thread_id, &project_id).await;
}

#[tokio::test]
async fn chat_history_persists_session_updates_with_cursor_pagination() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping chat_history_persists_session_updates_with_cursor_pagination: tmux unavailable"
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
    let session_id = started["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();
    wait_for_chat_ready(&mut harness, &thread_id, &session_id).await;

    let attached = harness
        .rpc(
            "chat.attach",
            json!({"thread_id": thread_id, "session_id": session_id}),
        )
        .await
        .expect("chat.attach");
    let channel_id = attached["channel_id"].as_u64().expect("channel_id") as u16;

    for idx in 0..105 {
        let marker = format!("history-marker-{idx}");
        send_test_update(&mut harness, channel_id, &marker).await;
    }

    harness
        .wait_for_channel_output_contains(
            channel_id,
            b"history-marker-104",
            Duration::from_secs(10),
        )
        .await
        .expect("wait for history marker output");

    let first_page = harness
        .rpc(
            "chat.history",
            json!({"thread_id": thread_id, "session_id": session_id}),
        )
        .await
        .expect("chat.history first page");
    let first_updates = first_page["updates"].as_array().expect("updates array");
    assert_eq!(first_updates.len(), 100);
    assert_eq!(first_page["next_cursor"], 100);

    let second_page = harness
        .rpc(
            "chat.history",
            json!({"thread_id": thread_id, "session_id": session_id, "cursor": 100}),
        )
        .await
        .expect("chat.history second page");
    let second_updates = second_page["updates"].as_array().expect("updates array");
    assert_eq!(second_updates.len(), 5);
    assert!(second_page["next_cursor"].is_null());
    let last_content = second_updates
        .last()
        .and_then(|entry| entry["update"]["content"].as_str())
        .expect("last update content");
    assert_eq!(last_content, "history-marker-104");

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn chat_history_nonexistent_session_returns_empty() {
    if !common::tmux_available().await {
        eprintln!("skipping chat_history_nonexistent_session_returns_empty: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let history = harness
        .rpc(
            "chat.history",
            json!({
                "thread_id": thread_id,
                "session_id": "does-not-exist",
            }),
        )
        .await
        .expect("chat.history for missing session");
    assert!(history["updates"]
        .as_array()
        .expect("updates array")
        .is_empty());
    assert!(history["next_cursor"].is_null());

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn thread_close_removes_chat_history_files() {
    if !common::tmux_available().await {
        eprintln!("skipping thread_close_removes_chat_history_files: tmux unavailable");
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
    let session_id = started["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();
    wait_for_chat_ready(&mut harness, &thread_id, &session_id).await;

    let attached = harness
        .rpc(
            "chat.attach",
            json!({"thread_id": thread_id, "session_id": session_id}),
        )
        .await
        .expect("chat.attach");
    let channel_id = attached["channel_id"].as_u64().expect("channel_id") as u16;

    send_test_update(&mut harness, channel_id, "persisted-before-close").await;
    harness
        .wait_for_channel_output_contains(
            channel_id,
            b"persisted-before-close",
            Duration::from_secs(10),
        )
        .await
        .expect("wait for persisted marker output");

    let history_path = chat_history_path(&thread_id, &session_id);
    assert!(
        history_path.exists(),
        "history file should exist before close"
    );

    harness
        .rpc(
            "thread.close",
            json!({ "thread_id": thread_id, "mode": "close" }),
        )
        .await
        .expect("thread.close");

    assert!(
        !history_path.exists(),
        "history file should be removed when thread closes"
    );

    let _ = harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await;
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
    let session_id = started["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();
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

#[tokio::test]
async fn chat_history_jsonl_preserves_acp_session_id_for_recovery() {
    if !common::tmux_available().await {
        eprintln!("skipping: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    // Start a session
    let started = harness
        .rpc(
            "chat.start",
            json!({"thread_id": thread_id, "agent_name": "mock"}),
        )
        .await
        .expect("chat.start");
    let session_id = started["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();
    wait_for_chat_ready(&mut harness, &thread_id, &session_id).await;

    // Attach to get ACP session ID and channel
    let attached = harness
        .rpc(
            "chat.attach",
            json!({"thread_id": thread_id, "session_id": session_id}),
        )
        .await
        .expect("chat.attach");
    let channel_id = attached["channel_id"].as_u64().expect("channel_id") as u16;
    let acp_session_id = attached["acp_session_id"]
        .as_str()
        .expect("acp_session_id")
        .to_string();

    // Generate history by sending a test update through the agent
    send_test_update(&mut harness, channel_id, "recovery-test-marker").await;
    harness
        .wait_for_channel_output_contains(
            channel_id,
            b"recovery-test-marker",
            Duration::from_secs(5),
        )
        .await
        .expect("wait for marker output");

    // Verify JSONL first line contains ACP session ID (not Spindle session ID)
    let history_path = chat_history_path(&thread_id, &session_id);
    assert!(history_path.exists(), "history file must exist");
    let first_line = std::fs::read_to_string(&history_path)
        .expect("read history file")
        .lines()
        .next()
        .expect("at least one line")
        .to_string();
    let first_entry: serde_json::Value =
        serde_json::from_str(&first_line).expect("parse first JSONL line");
    let stored_session_id = first_entry["sessionId"]
        .as_str()
        .expect("sessionId in JSONL");

    // The ACP session ID in JSONL should match what chat.attach returned
    assert_eq!(
        stored_session_id, acp_session_id,
        "JSONL should contain ACP session ID, not Spindle session ID. \
         Got stored={stored_session_id}, expected acp={acp_session_id}, spindle={session_id}"
    );

    // Now test the full load cycle: stop, then load
    harness
        .rpc(
            "chat.stop",
            json!({"thread_id": thread_id, "session_id": session_id}),
        )
        .await
        .expect("chat.stop");
    harness
        .wait_for_event("chat.session_ended", Duration::from_secs(5))
        .await
        .expect("chat.session_ended");

    // chat.load should pass the ACP session ID to session/load
    let loaded = harness
        .rpc(
            "chat.load",
            json!({"thread_id": thread_id, "session_id": session_id}),
        )
        .await
        .expect("chat.load");
    assert_eq!(loaded["status"], "starting");
    wait_for_chat_ready(&mut harness, &thread_id, &session_id).await;

    // Re-attach and verify ACP session ID is preserved
    let reattached = harness
        .rpc(
            "chat.attach",
            json!({"thread_id": thread_id, "session_id": session_id}),
        )
        .await
        .expect("chat.attach after load");
    let reattached_acp = reattached["acp_session_id"]
        .as_str()
        .expect("acp_session_id after load");
    assert_eq!(
        reattached_acp, acp_session_id,
        "ACP session ID should be preserved across load"
    );

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}
