mod common;

use std::{fs, path::PathBuf, time::Duration};

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
                "name": common::unique_name("workflow"),
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

async fn wait_for_chat_ready(harness: &mut common::TestHarness, thread_id: &str, session_id: &str) {
    loop {
        let ready = harness
            .wait_for_event("chat.session_ready", Duration::from_secs(10))
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

async fn assert_worker_status(
    harness: &mut common::TestHarness,
    workflow_id: &str,
    worker_id: &str,
    expected_status: &str,
) {
    let status = harness
        .rpc("workflow.status", json!({ "workflow_id": workflow_id }))
        .await
        .expect("workflow.status for worker");
    assert!(
        status["workers"]
            .as_array()
            .expect("workers array")
            .iter()
            .any(|worker| {
                worker["worker_id"] == worker_id && worker["status"] == expected_status
            }),
        "expected worker {worker_id} to be {expected_status}: {status}"
    );
}

async fn assert_reviewer_status(
    harness: &mut common::TestHarness,
    workflow_id: &str,
    reviewer_id: &str,
    expected_status: &str,
) {
    let status = harness
        .rpc("workflow.status", json!({ "workflow_id": workflow_id }))
        .await
        .expect("workflow.status for reviewer");
    assert!(
        status["reviewers"]
            .as_array()
            .expect("reviewers array")
            .iter()
            .any(|reviewer| {
                reviewer["reviewer_id"] == reviewer_id && reviewer["status"] == expected_status
            }),
        "expected reviewer {reviewer_id} to be {expected_status}: {status}"
    );
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

async fn create_workflow(harness: &mut common::TestHarness, thread_id: &str) -> String {
    let created = harness
        .rpc(
            "workflow.create",
            json!({
                "thread_id": thread_id,
                "prd_issue_url": "https://github.com/kevin-courbet/threadmill/issues/49",
                "implementation_issue_urls": [
                    "https://github.com/kevin-courbet/threadmill/issues/50"
                ]
            }),
        )
        .await
        .expect("workflow.create");

    created["workflow_id"]
        .as_str()
        .expect("workflow id in create response")
        .to_string()
}

fn workflows_path() -> PathBuf {
    let config_home = std::env::var("XDG_CONFIG_HOME").expect("XDG_CONFIG_HOME set by harness");
    PathBuf::from(config_home)
        .join("threadmill")
        .join("workflows.json")
}

#[tokio::test]
async fn workflow_create_persists_and_appears_in_state_snapshot() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping workflow_create_persists_and_appears_in_state_snapshot: tmux unavailable"
        );
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let workflow_id = create_workflow(&mut harness, &thread_id).await;
    let created_event = harness
        .wait_for_event("workflow.created", Duration::from_secs(5))
        .await
        .expect("workflow.created event");
    assert_eq!(
        created_event["params"]["workflow"]["workflow_id"],
        workflow_id
    );

    let snapshot = harness
        .rpc("state.snapshot", json!({}))
        .await
        .expect("state.snapshot");
    let workflows = snapshot["workflows"]
        .as_array()
        .expect("workflows array in state snapshot");
    assert!(
        workflows
            .iter()
            .any(|workflow| workflow["workflow_id"] == workflow_id),
        "workflow should appear in state snapshot"
    );

    let raw = fs::read_to_string(workflows_path()).expect("read workflows.json");
    assert!(
        raw.contains(&workflow_id),
        "workflow should persist to workflows.json"
    );

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn workflow_transition_rejects_invalid_transition_without_force_skip() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping workflow_transition_rejects_invalid_transition_without_force_skip: tmux unavailable"
        );
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;
    let workflow_id = create_workflow(&mut harness, &thread_id).await;

    let error = harness
        .rpc_expect_error(
            "workflow.transition",
            json!({
                "workflow_id": workflow_id,
                "phase": "REVIEWING"
            }),
        )
        .await;
    assert!(
        error.contains("invalid workflow phase transition"),
        "unexpected error: {error}"
    );

    let transitioned = harness
        .rpc(
            "workflow.transition",
            json!({
                "workflow_id": workflow_id,
                "phase": "IMPLEMENTING"
            }),
        )
        .await
        .expect("workflow.transition to IMPLEMENTING");
    assert_eq!(transitioned["phase"], "IMPLEMENTING");

    let phase_event = harness
        .wait_for_event("workflow.phase_changed", Duration::from_secs(5))
        .await
        .expect("workflow.phase_changed event");
    assert_eq!(phase_event["params"]["workflow_id"], workflow_id);
    assert_eq!(phase_event["params"]["new_phase"], "IMPLEMENTING");
    assert_eq!(phase_event["params"]["forced"], false);

    let force_skipped = harness
        .rpc(
            "workflow.transition",
            json!({
                "workflow_id": workflow_id,
                "phase": "COMPLETE",
                "force": true
            }),
        )
        .await
        .expect("workflow.transition force skip");
    assert_eq!(force_skipped["phase"], "COMPLETE");

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn workflow_spawn_worker_records_handoff_and_updates_on_external_end() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping workflow_spawn_worker_records_handoff_and_updates_on_external_end: tmux unavailable"
        );
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;
    let workflow_id = create_workflow(&mut harness, &thread_id).await;

    harness
        .rpc(
            "workflow.transition",
            json!({
                "workflow_id": workflow_id,
                "phase": "IMPLEMENTING"
            }),
        )
        .await
        .expect("transition workflow to IMPLEMENTING");

    let spawned = harness
        .rpc(
            "workflow.spawn_worker",
            json!({
                "workflow_id": workflow_id,
                "agent_name": "mock",
                "parent_session_id": "orchestrator-session",
                "system_prompt": "You are worker W001",
                "initial_prompt": "Implement workflow engine",
                "display_name": "Worker: W001"
            }),
        )
        .await
        .expect("workflow.spawn_worker");
    let first_worker = spawned["worker"].clone();
    let first_session_id = first_worker["session_id"]
        .as_str()
        .expect("worker session id")
        .to_string();
    assert_eq!(first_worker["worker_id"], "W001");

    wait_for_chat_ready(&mut harness, &thread_id, &first_session_id).await;
    assert_worker_status(&mut harness, &workflow_id, "W001", "RUNNING").await;

    let handoff = harness
        .rpc(
            "workflow.record_handoff",
            json!({
                "workflow_id": workflow_id,
                "worker_id": "W001",
                "stop_reason": "DONE",
                "progress": ["Implemented workflow service"],
                "next_steps": ["Run full spindle validation"],
                "context": "Worker finished implementation details.",
                "blockers": null
            }),
        )
        .await
        .expect("workflow.record_handoff");
    assert_eq!(handoff["status"], "COMPLETED");
    assert_eq!(handoff["handoff"]["stop_reason"], "DONE");

    let status = harness
        .rpc("workflow.status", json!({ "workflow_id": workflow_id }))
        .await
        .expect("workflow.status after handoff");
    assert!(
        status["workers"]
            .as_array()
            .expect("workers array")
            .iter()
            .any(|worker| worker["worker_id"] == "W001" && worker["status"] == "COMPLETED"),
        "W001 should be completed after handoff"
    );

    let spawned = harness
        .rpc(
            "workflow.spawn_worker",
            json!({
                "workflow_id": workflow_id,
                "agent_name": "mock",
                "parent_session_id": "orchestrator-session",
                "system_prompt": "You are worker W002",
                "initial_prompt": "Investigate edge case",
                "display_name": "Worker: W002"
            }),
        )
        .await
        .expect("workflow.spawn_worker W002");
    let second_session_id = spawned["worker"]["session_id"]
        .as_str()
        .expect("second worker session id")
        .to_string();
    wait_for_chat_ready(&mut harness, &thread_id, &second_session_id).await;

    let chat_status = harness
        .rpc("chat.status", json!({ "session_id": second_session_id }))
        .await
        .expect("chat.status for workflow worker session");
    assert_eq!(chat_status["parent_session_id"], "orchestrator-session");
    assert_eq!(chat_status["display_name"], "Worker: W002");

    harness
        .rpc(
            "chat.stop",
            json!({
                "thread_id": thread_id,
                "session_id": chat_status["session_id"].as_str().expect("chat session id")
            }),
        )
        .await
        .expect("chat.stop workflow worker session");

    let worker_event = loop {
        let event = harness
            .wait_for_event("workflow.worker_completed", Duration::from_secs(5))
            .await
            .expect("workflow.worker_completed event");
        if event["params"]["worker"]["worker_id"] == "W002" {
            break event;
        }
    };
    assert_eq!(worker_event["params"]["worker"]["worker_id"], "W002");
    assert_eq!(worker_event["params"]["worker"]["status"], "FAILED");

    let status = harness
        .rpc("workflow.status", json!({ "workflow_id": workflow_id }))
        .await
        .expect("workflow.status after external session end");
    assert!(
        status["workers"]
            .as_array()
            .expect("workers array")
            .iter()
            .any(|worker| worker["worker_id"] == "W002" && worker["status"] == "FAILED"),
        "W002 should fail when its chat session ends externally"
    );

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn workflow_restart_reconciliation_keeps_idle_active_phase() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping workflow_restart_reconciliation_keeps_idle_active_phase: tmux unavailable"
        );
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;
    let workflow_id = create_workflow(&mut harness, &thread_id).await;

    harness
        .rpc(
            "workflow.transition",
            json!({
                "workflow_id": workflow_id,
                "phase": "IMPLEMENTING"
            }),
        )
        .await
        .expect("transition workflow to IMPLEMENTING");

    let config_home = harness.preserve_config_home();
    drop(harness);

    let mut harness = common::setup_test_server_with_config_home(config_home).await;
    let snapshot = harness
        .rpc("state.snapshot", json!({}))
        .await
        .expect("state.snapshot after restart");
    let restarted = snapshot["workflows"]
        .as_array()
        .expect("workflows array after restart")
        .iter()
        .find(|workflow| workflow["workflow_id"] == workflow_id)
        .cloned()
        .expect("workflow in restarted snapshot");
    assert_eq!(restarted["phase"], "IMPLEMENTING");

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn workflow_start_review_rolls_back_partial_reviewer_spawns_on_error() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping workflow_start_review_rolls_back_partial_reviewer_spawns_on_error: tmux unavailable"
        );
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;
    let workflow_id = create_workflow(&mut harness, &thread_id).await;

    harness
        .rpc(
            "workflow.transition",
            json!({
                "workflow_id": workflow_id,
                "phase": "IMPLEMENTING"
            }),
        )
        .await
        .expect("transition workflow to IMPLEMENTING");
    harness
        .rpc(
            "workflow.transition",
            json!({
                "workflow_id": workflow_id,
                "phase": "TESTING"
            }),
        )
        .await
        .expect("transition workflow to TESTING");

    let error = harness
        .rpc_expect_error(
            "workflow.start_review",
            json!({
                "workflow_id": workflow_id,
                "reviewers": [
                    {
                        "agent_name": "mock",
                        "parent_session_id": "orchestrator-session",
                        "system_prompt": "Review worker output",
                        "initial_prompt": "Review implementation",
                        "display_name": "Reviewer: R001"
                    },
                    {
                        "agent_name": "missing-agent",
                        "parent_session_id": "orchestrator-session",
                        "system_prompt": "Review worker output",
                        "initial_prompt": "Review implementation",
                        "display_name": "Reviewer: R002"
                    }
                ]
            }),
        )
        .await;
    assert!(error.contains("agent not found: missing-agent"));

    let status = harness
        .rpc("workflow.status", json!({ "workflow_id": workflow_id }))
        .await
        .expect("workflow.status after failed review start");
    assert_eq!(status["phase"], "TESTING");
    assert!(status["review_started_at"].is_null());

    let reviewers = harness
        .rpc(
            "workflow.list_reviewers",
            json!({ "workflow_id": workflow_id }),
        )
        .await
        .expect("workflow.list_reviewers after failed review start");
    assert!(reviewers
        .as_array()
        .expect("reviewers array from workflow.list_reviewers")
        .is_empty());

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn workflow_review_records_findings_and_reconciles_after_restart() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping workflow_review_records_findings_and_reconciles_after_restart: tmux unavailable"
        );
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;
    let workflow_id = create_workflow(&mut harness, &thread_id).await;

    harness
        .rpc(
            "workflow.transition",
            json!({
                "workflow_id": workflow_id,
                "phase": "IMPLEMENTING"
            }),
        )
        .await
        .expect("transition workflow to IMPLEMENTING");
    harness
        .rpc(
            "workflow.transition",
            json!({
                "workflow_id": workflow_id,
                "phase": "TESTING"
            }),
        )
        .await
        .expect("transition workflow to TESTING");

    let started = harness
        .rpc(
            "workflow.start_review",
            json!({
                "workflow_id": workflow_id,
                "reviewers": [
                    {
                        "agent_name": "mock",
                        "parent_session_id": "orchestrator-session",
                        "system_prompt": "Review worker output",
                        "initial_prompt": "Review implementation",
                        "display_name": "Reviewer: R001"
                    }
                ]
            }),
        )
        .await
        .expect("workflow.start_review");
    let reviewer_session_id = started["reviewers"][0]["session_id"]
        .as_str()
        .expect("reviewer session id")
        .to_string();
    wait_for_chat_ready(&mut harness, &thread_id, &reviewer_session_id).await;
    assert_reviewer_status(&mut harness, &workflow_id, "R001", "RUNNING").await;

    let reviewers = harness
        .rpc(
            "workflow.list_reviewers",
            json!({ "workflow_id": workflow_id }),
        )
        .await
        .expect("workflow.list_reviewers");
    assert_eq!(reviewers.as_array().expect("reviewers array").len(), 1);
    assert_eq!(reviewers[0]["reviewer_id"], "R001");

    let findings = harness
        .rpc(
            "workflow.record_findings",
            json!({
                "workflow_id": workflow_id,
                "findings": [
                    {
                        "severity": "HIGH",
                        "summary": "Missing restart reconciliation",
                        "details": "Worker sessions were not reconciled on daemon restart.",
                        "source_reviewers": ["R001"],
                        "file_path": "src/services/workflow.rs",
                        "line": 42
                    },
                    {
                        "severity": "MEDIUM",
                        "summary": "Need workflow delta sync",
                        "details": "state.snapshot should include workflow data.",
                        "source_reviewers": ["R001"],
                        "file_path": "src/protocol.rs",
                        "line": 348
                    }
                ]
            }),
        )
        .await
        .expect("workflow.record_findings");
    assert_eq!(findings[0]["finding_id"], "H1");
    assert_eq!(findings[1]["finding_id"], "H2");

    let config_home = harness.preserve_config_home();
    drop(harness);

    let mut harness = common::setup_test_server_with_config_home(config_home).await;
    let snapshot = harness
        .rpc("state.snapshot", json!({}))
        .await
        .expect("state.snapshot after restart");
    let restarted = snapshot["workflows"]
        .as_array()
        .expect("workflows array after restart")
        .iter()
        .find(|workflow| workflow["workflow_id"] == workflow_id)
        .cloned()
        .expect("workflow in restarted snapshot");
    assert_eq!(restarted["phase"], "BLOCKED");
    assert!(
        restarted["reviewers"]
            .as_array()
            .expect("reviewers array after restart")
            .iter()
            .any(|reviewer| reviewer["reviewer_id"] == "R001" && reviewer["status"] == "FAILED"),
        "active reviewer should fail during restart reconciliation"
    );

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn workflow_add_linked_issue_appends_url_and_emits_delta() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping workflow_add_linked_issue_appends_url_and_emits_delta: tmux unavailable"
        );
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;
    let workflow_id = create_workflow(&mut harness, &thread_id).await;

    let first = harness
        .rpc(
            "workflow.add_linked_issue",
            json!({
                "workflow_id": workflow_id,
                "url": "threadmill-local://42",
            }),
        )
        .await
        .expect("workflow.add_linked_issue first call");
    let urls = first["workflow"]["implementation_issue_urls"]
        .as_array()
        .expect("implementation_issue_urls array");
    assert!(
        urls.iter().any(|url| url == "threadmill-local://42"),
        "new URL should be appended; got {urls:?}"
    );

    // WorkflowUpsert delta should have been broadcast. Earlier deltas from
    // project/thread/workflow creation are buffered, so keep draining until we
    // find the upsert that carries our appended URL.
    let mut found_upsert = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline && !found_upsert {
        let delta = match harness
            .wait_for_event("state.delta", Duration::from_secs(1))
            .await
        {
            Ok(value) => value,
            Err(_) => break,
        };
        let Some(ops) = delta["params"]["operations"].as_array() else {
            continue;
        };
        for op in ops {
            if op["type"] != "workflow.upsert" {
                continue;
            }
            if op["workflow"]["workflow_id"] != workflow_id {
                continue;
            }
            let urls = op["workflow"]["implementation_issue_urls"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if urls.iter().any(|url| url == "threadmill-local://42") {
                found_upsert = true;
                break;
            }
        }
    }
    assert!(
        found_upsert,
        "expected workflow.upsert delta carrying threadmill-local://42"
    );

    // Second call with the same URL is idempotent — no duplicate.
    let second = harness
        .rpc(
            "workflow.add_linked_issue",
            json!({
                "workflow_id": workflow_id,
                "url": "threadmill-local://42",
            }),
        )
        .await
        .expect("workflow.add_linked_issue second call");
    let urls_after = second["workflow"]["implementation_issue_urls"]
        .as_array()
        .expect("implementation_issue_urls array after second call");
    let matching = urls_after
        .iter()
        .filter(|url| *url == "threadmill-local://42")
        .count();
    assert_eq!(matching, 1, "idempotent re-add should not duplicate");

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn issue_create_with_link_to_workflow_appends_url() {
    if !common::tmux_available().await {
        eprintln!("skipping issue_create_with_link_to_workflow_appends_url: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;
    let workflow_id = create_workflow(&mut harness, &thread_id).await;

    let created = harness
        .rpc(
            "issue.create",
            json!({
                "project_id": project_id,
                "draft": {
                    "title": "Follow-up work",
                    "body": "## Problem\nAuto-linked from the test.\n",
                    "labels": ["impl"],
                    "assignees": []
                },
                "link_to_workflow_id": workflow_id,
            }),
        )
        .await
        .expect("issue.create with link");
    let created_url = created["issue_ref"]["url"]
        .as_str()
        .expect("issue_ref.url")
        .to_string();
    assert_eq!(created["linked_workflow_id"], workflow_id);
    assert!(
        created_url.starts_with("threadmill-local://"),
        "LocalTransport should mint local URL, got {created_url}"
    );

    // workflow.issue_created direct event fires. The event is the project-level
    // lifecycle signal and intentionally does not carry workflow ownership —
    // linkage is observable via the concurrent WorkflowUpsert state delta.
    let event = harness
        .wait_for_event("workflow.issue_created", Duration::from_secs(5))
        .await
        .expect("workflow.issue_created event");
    assert!(
        event["params"].get("linked_workflow_id").is_none(),
        "workflow.issue_created must not carry linked_workflow_id"
    );
    assert_eq!(event["params"]["issue_ref"]["url"], created_url);

    let status = harness
        .rpc("workflow.status", json!({ "workflow_id": workflow_id }))
        .await
        .expect("workflow.status after linked issue.create");
    assert!(
        status["implementation_issue_urls"]
            .as_array()
            .expect("implementation_issue_urls array")
            .iter()
            .any(|url| url == &created_url),
        "implementation_issue_urls should include newly-created url"
    );

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}
