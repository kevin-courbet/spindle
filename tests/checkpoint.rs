mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::json;
use tokio::{process::Command, time::sleep};

const CHAT_AGENT_CONFIG: &str = r#"agents:
  mock:
    command: "./mock-chat-agent.sh"
"#;

async fn setup_test_server() -> common::TestHarness {
    common::setup_test_server().await
}

async fn add_project(
    harness: &mut common::TestHarness,
    threadmill_config: Option<&str>,
) -> (common::TestProject, String) {
    let project = common::create_git_project(threadmill_config, true)
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

async fn add_chat_project(harness: &mut common::TestHarness) -> (common::TestProject, String) {
    let project = common::create_git_project(Some(CHAT_AGENT_CONFIG), true)
        .await
        .expect("create test git project");
    write_mock_chat_agent(&project.repo_path);
    commit_mock_chat_agent(&project.repo_path).await;
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

fn write_mock_chat_agent(repo_path: &Path) {
    fs::write(
        repo_path.join("mock-chat-agent.sh"),
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
            "method": method,
            "params": msg.get("params"),
        }) + "\n")
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

        let path = repo_path.join("mock-chat-agent.sh");
        let mut perms = fs::metadata(&path)
            .expect("read script metadata")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).expect("chmod mock chat agent script");
    }
}

async fn commit_mock_chat_agent(repo_path: &Path) {
    run_git(repo_path, &["add", "mock-chat-agent.sh"]).await;
    run_git(repo_path, &["commit", "-m", "add mock chat agent"]).await;
    run_git(repo_path, &["push", "origin", "main"]).await;
}

async fn create_thread(harness: &mut common::TestHarness, project_id: &str) -> serde_json::Value {
    harness
        .rpc(
            "thread.create",
            json!({
                "project_id": project_id,
                "name": common::unique_name("checkpoint-thread"),
                "source_type": "new_feature"
            }),
        )
        .await
        .expect("create thread")
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

async fn wait_for_checkpoint_count(
    harness: &mut common::TestHarness,
    thread_id: &str,
    session_id: Option<&str>,
    expected: usize,
) -> serde_json::Value {
    for _ in 0..50 {
        let mut params = json!({ "thread_id": thread_id });
        if let Some(session_id) = session_id {
            params["session_id"] = json!(session_id);
        }
        let listed = harness
            .rpc("checkpoint.list", params)
            .await
            .expect("list checkpoints while waiting");
        let checkpoints = listed.as_array().expect("checkpoint list array");
        if checkpoints.len() == expected {
            return listed;
        }
        sleep(Duration::from_millis(100)).await;
    }

    panic!("timed out waiting for {expected} checkpoints");
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

async fn send_user_prompt(
    harness: &mut common::TestHarness,
    channel_id: u16,
    session_id: &str,
    prompt: &str,
) {
    let request = json!({
        "jsonrpc": "2.0",
        "id": 43,
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": prompt }]
        }
    });
    harness
        .send_binary(channel_id, format!("{}\n", request).as_bytes())
        .await
        .expect("send session prompt request");
}

fn read_mock_chat_agent_log(worktree_path: &Path) -> Vec<serde_json::Value> {
    let log_path = worktree_path.join("mock-chat-agent-log.jsonl");
    let Ok(raw) = fs::read_to_string(&log_path) else {
        return Vec::new();
    };

    raw.lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("parse agent log entry"))
        .collect()
}

async fn wait_for_mock_chat_agent_log(
    worktree_path: &Path,
    predicate: impl Fn(&[serde_json::Value]) -> bool,
) -> Vec<serde_json::Value> {
    for _ in 0..50 {
        let entries = read_mock_chat_agent_log(worktree_path);
        if predicate(&entries) {
            return entries;
        }
        sleep(Duration::from_millis(100)).await;
    }

    panic!("timed out waiting for mock chat agent log state");
}

async fn run_git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .await
        .expect("run git command");

    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

async fn git_dir(repo: &Path) -> PathBuf {
    let raw = run_git(repo, &["rev-parse", "--git-dir"]).await;
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        repo.join(path)
    }
}

async fn checkpoint_refs(repo: &Path, thread_id: &str) -> Vec<String> {
    let output = run_git(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname)",
            &format!("refs/threadmill-checkpoints/{thread_id}"),
        ],
    )
    .await;
    if output.is_empty() {
        Vec::new()
    } else {
        output.lines().map(ToOwned::to_owned).collect()
    }
}

fn session_metadata_path(thread_id: &str, session_id: &str) -> PathBuf {
    PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").expect("XDG_CONFIG_HOME set"))
        .join("threadmill")
        .join("chat")
        .join(thread_id)
        .join(format!("{session_id}.metadata.json"))
}

#[cfg(unix)]
fn set_read_only(path: &Path, read_only: bool) {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)
        .expect("read metadata permissions")
        .permissions();
    permissions.set_mode(if read_only { 0o444 } else { 0o644 });
    fs::set_permissions(path, permissions).expect("update metadata permissions");
}

#[tokio::test]
async fn checkpoint_save_list_diff_and_restore_round_trip() {
    if !common::tmux_available().await {
        eprintln!("skipping checkpoint_save_list_diff_and_restore_round_trip: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness, None).await;
    let created = create_thread(&mut harness, &project_id).await;
    let thread_id = created["id"].as_str().expect("thread id").to_string();
    let worktree_path = PathBuf::from(created["worktree_path"].as_str().expect("worktree path"));

    wait_for_thread_ready(&mut harness, &thread_id).await;

    let readme = worktree_path.join("README.md");
    fs::write(&readme, "checkpoint one\n").expect("write first checkpoint content");
    fs::write(worktree_path.join("saved.txt"), "saved at checkpoint one\n")
        .expect("write untracked checkpoint file");

    let first = harness
        .rpc(
            "checkpoint.save",
            json!({
                "thread_id": &thread_id,
                "session_id": "session-a",
                "message": "manual checkpoint one",
                "prompt_preview": "touch readme"
            }),
        )
        .await
        .expect("save first checkpoint");
    assert_eq!(first["skipped"], false);
    assert_eq!(first["checkpoint"]["seq"], 1);
    assert_eq!(first["checkpoint"]["session_id"], "session-a");
    assert_eq!(
        first["checkpoint"]["git_ref"],
        format!("refs/threadmill-checkpoints/{thread_id}/1")
    );

    fs::write(&readme, "checkpoint two\n").expect("write second checkpoint content");
    fs::write(worktree_path.join("later.txt"), "added later\n").expect("write later file");

    let second = harness
        .rpc(
            "checkpoint.save",
            json!({
                "thread_id": &thread_id,
                "message": "manual checkpoint two"
            }),
        )
        .await
        .expect("save second checkpoint");
    assert_eq!(second["skipped"], false);
    assert_eq!(second["checkpoint"]["seq"], 2);

    let listed = harness
        .rpc("checkpoint.list", json!({ "thread_id": &thread_id }))
        .await
        .expect("list checkpoints");
    let checkpoints = listed.as_array().expect("checkpoint list array");
    assert_eq!(checkpoints.len(), 2);
    assert_eq!(checkpoints[0]["seq"], 2);
    assert_eq!(checkpoints[1]["seq"], 1);
    assert_eq!(checkpoints[1]["prompt_preview"], "touch readme");

    let filtered = harness
        .rpc(
            "checkpoint.list",
            json!({ "thread_id": &thread_id, "session_id": "session-a" }),
        )
        .await
        .expect("list checkpoints filtered by session");
    assert_eq!(filtered.as_array().expect("filtered list array").len(), 1);

    let diff = harness
        .rpc(
            "checkpoint.diff",
            json!({ "thread_id": &thread_id, "base_seq": 1, "target_seq": 2 }),
        )
        .await
        .expect("diff checkpoints");
    let diff_text = diff["diff_text"].as_str().expect("diff text");
    assert!(diff_text.contains("checkpoint one"));
    assert!(diff_text.contains("checkpoint two"));
    assert!(diff_text.contains("later.txt"));

    fs::write(&readme, "dirty after checkpoint\n").expect("write dirty content");
    fs::write(worktree_path.join("after-restore.txt"), "remove me\n")
        .expect("write dirty untracked file");

    let restored = harness
        .rpc(
            "checkpoint.restore",
            json!({ "thread_id": &thread_id, "seq": 1 }),
        )
        .await
        .expect("restore checkpoint");
    assert_eq!(restored["restored"], true);
    assert_eq!(restored["checkpoint"]["seq"], 1);

    assert_eq!(
        fs::read_to_string(&readme).expect("read restored readme"),
        "checkpoint one\n"
    );
    assert!(
        worktree_path.join("saved.txt").exists(),
        "restore should recover untracked files captured in the checkpoint"
    );
    assert!(
        !worktree_path.join("later.txt").exists(),
        "restore should remove files introduced after the checkpoint"
    );
    assert!(
        !worktree_path.join("after-restore.txt").exists(),
        "restore should clean dirty untracked files"
    );

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn checkpoint_save_skips_when_git_is_busy() {
    if !common::tmux_available().await {
        eprintln!("skipping checkpoint_save_skips_when_git_is_busy: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness, None).await;
    let created = create_thread(&mut harness, &project_id).await;
    let thread_id = created["id"].as_str().expect("thread id").to_string();
    let worktree_path = PathBuf::from(created["worktree_path"].as_str().expect("worktree path"));

    wait_for_thread_ready(&mut harness, &thread_id).await;

    let git_dir = git_dir(&worktree_path).await;
    fs::write(git_dir.join("MERGE_HEAD"), "busy\n").expect("write merge head");

    let result = harness
        .rpc("checkpoint.save", json!({ "thread_id": &thread_id }))
        .await
        .expect("save busy checkpoint");
    assert_eq!(result["skipped"], true);
    assert!(result["checkpoint"].is_null());

    let listed = harness
        .rpc("checkpoint.list", json!({ "thread_id": &thread_id }))
        .await
        .expect("list checkpoints after busy save");
    assert!(listed.as_array().expect("checkpoint list array").is_empty());

    fs::remove_file(git_dir.join("MERGE_HEAD")).expect("remove merge head");
    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn checkpoint_save_prunes_oldest_refs_beyond_max_count() {
    if !common::tmux_available().await {
        eprintln!("skipping checkpoint_save_prunes_oldest_refs_beyond_max_count: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(
        &mut harness,
        Some(
            r#"checkpoints:
  max_count: 2
"#,
        ),
    )
    .await;
    let created = create_thread(&mut harness, &project_id).await;
    let thread_id = created["id"].as_str().expect("thread id").to_string();
    let worktree_path = PathBuf::from(created["worktree_path"].as_str().expect("worktree path"));

    wait_for_thread_ready(&mut harness, &thread_id).await;
    fs::write(
        worktree_path.join(".threadmill.yml"),
        "checkpoints:\n  max_count: 2\n",
    )
    .expect("write worktree checkpoint config");

    for idx in 1..=3 {
        fs::write(
            worktree_path.join("README.md"),
            format!("checkpoint {idx}\n"),
        )
        .expect("write checkpoint content");
        harness
            .rpc(
                "checkpoint.save",
                json!({ "thread_id": &thread_id, "message": format!("checkpoint {idx}") }),
            )
            .await
            .expect("save checkpoint for retention test");
    }

    let listed = harness
        .rpc("checkpoint.list", json!({ "thread_id": &thread_id }))
        .await
        .expect("list checkpoints after retention pruning");
    let checkpoints = listed.as_array().expect("checkpoint list array");
    assert_eq!(checkpoints.len(), 2);
    assert_eq!(checkpoints[0]["seq"], 3);
    assert_eq!(checkpoints[1]["seq"], 2);

    let refs = checkpoint_refs(&worktree_path, &thread_id).await;
    assert_eq!(refs.len(), 2);
    assert!(refs.iter().all(|git_ref| !git_ref.ends_with("/1")));

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn chat_prompt_auto_saves_checkpoint() {
    if !common::tmux_available().await {
        eprintln!("skipping chat_prompt_auto_saves_checkpoint: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_chat_project(&mut harness).await;
    let created = create_thread(&mut harness, &project_id).await;
    let thread_id = created["id"].as_str().expect("thread id").to_string();

    wait_for_thread_ready(&mut harness, &thread_id).await;

    let started = harness
        .rpc(
            "chat.start",
            json!({ "thread_id": &thread_id, "agent_name": "mock" }),
        )
        .await
        .expect("chat.start");
    let session_id = started["session_id"]
        .as_str()
        .expect("session id")
        .to_string();
    wait_for_chat_ready(&mut harness, &thread_id, &session_id).await;

    let attached = harness
        .rpc(
            "chat.attach",
            json!({ "thread_id": &thread_id, "session_id": &session_id }),
        )
        .await
        .expect("chat.attach");
    let channel_id = attached["channel_id"].as_u64().expect("channel id") as u16;

    harness
        .send_binary(
            channel_id,
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "session/prompt",
                    "params": {
                        "sessionId": &session_id,
                        "prompt": [{ "type": "text", "text": "Implement checkpoint auto-save" }]
                    }
                })
            )
            .as_bytes(),
        )
        .await
        .expect("send prompt");

    let listed = wait_for_checkpoint_count(&mut harness, &thread_id, Some(&session_id), 1).await;
    let checkpoint = listed
        .as_array()
        .expect("checkpoint list array")
        .first()
        .expect("auto checkpoint present");
    assert_eq!(checkpoint["session_id"], session_id);
    assert_eq!(
        checkpoint["prompt_preview"],
        "Implement checkpoint auto-save"
    );
    assert_eq!(checkpoint["message"], "Auto-checkpoint before prompt 1");

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn checkpoint_restore_truncates_chat_history_to_checkpoint() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping checkpoint_restore_truncates_chat_history_to_checkpoint: tmux unavailable"
        );
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_chat_project(&mut harness).await;
    let created = create_thread(&mut harness, &project_id).await;
    let thread_id = created["id"].as_str().expect("thread id").to_string();

    wait_for_thread_ready(&mut harness, &thread_id).await;

    let started = harness
        .rpc(
            "chat.start",
            json!({ "thread_id": &thread_id, "agent_name": "mock" }),
        )
        .await
        .expect("chat.start");
    let session_id = started["session_id"]
        .as_str()
        .expect("session id")
        .to_string();
    wait_for_chat_ready(&mut harness, &thread_id, &session_id).await;

    let attached = harness
        .rpc(
            "chat.attach",
            json!({ "thread_id": &thread_id, "session_id": &session_id }),
        )
        .await
        .expect("chat.attach");
    let channel_id = attached["channel_id"].as_u64().expect("channel id") as u16;

    send_test_update(&mut harness, channel_id, "history-before-checkpoint").await;
    harness
        .wait_for_channel_output_contains(
            channel_id,
            b"history-before-checkpoint",
            Duration::from_secs(5),
        )
        .await
        .expect("wait for first history marker");

    harness
        .rpc(
            "checkpoint.save",
            json!({
                "thread_id": &thread_id,
                "session_id": &session_id,
                "message": "checkpoint after first history marker"
            }),
        )
        .await
        .expect("checkpoint.save after first marker");

    send_test_update(&mut harness, channel_id, "history-after-checkpoint").await;
    harness
        .wait_for_channel_output_contains(
            channel_id,
            b"history-after-checkpoint",
            Duration::from_secs(5),
        )
        .await
        .expect("wait for second history marker");

    let before_restore = harness
        .rpc(
            "chat.history",
            json!({ "thread_id": &thread_id, "session_id": &session_id }),
        )
        .await
        .expect("chat.history before restore");
    let before_updates = before_restore["updates"].as_array().expect("updates array");
    assert_eq!(before_updates.len(), 2);

    harness
        .rpc(
            "checkpoint.restore",
            json!({ "thread_id": &thread_id, "seq": 1 }),
        )
        .await
        .expect("checkpoint.restore");

    let after_restore = harness
        .rpc(
            "chat.history",
            json!({ "thread_id": &thread_id, "session_id": &session_id }),
        )
        .await
        .expect("chat.history after restore");
    let after_updates = after_restore["updates"].as_array().expect("updates array");
    assert_eq!(after_updates.len(), 1);
    assert_eq!(
        after_updates[0]["update"]["content"].as_str(),
        Some("history-before-checkpoint")
    );

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn checkpoint_restore_reloads_chat_from_truncated_context_only() {
    if !common::tmux_available().await {
        eprintln!("skipping checkpoint_restore_reloads_chat_from_truncated_context_only: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_chat_project(&mut harness).await;
    let created = create_thread(&mut harness, &project_id).await;
    let thread_id = created["id"].as_str().expect("thread id").to_string();
    let worktree_path = PathBuf::from(created["worktree_path"].as_str().expect("worktree path"));

    wait_for_thread_ready(&mut harness, &thread_id).await;

    let started = harness
        .rpc(
            "chat.start",
            json!({ "thread_id": &thread_id, "agent_name": "mock" }),
        )
        .await
        .expect("chat.start");
    let session_id = started["session_id"]
        .as_str()
        .expect("session id")
        .to_string();
    wait_for_chat_ready(&mut harness, &thread_id, &session_id).await;

    let attached = harness
        .rpc(
            "chat.attach",
            json!({ "thread_id": &thread_id, "session_id": &session_id }),
        )
        .await
        .expect("chat.attach");
    let channel_id = attached["channel_id"].as_u64().expect("channel id") as u16;

    send_test_update(&mut harness, channel_id, "history-before-checkpoint").await;
    harness
        .wait_for_channel_output_contains(
            channel_id,
            b"history-before-checkpoint",
            Duration::from_secs(5),
        )
        .await
        .expect("wait for first history marker");

    harness
        .rpc(
            "checkpoint.save",
            json!({
                "thread_id": &thread_id,
                "session_id": &session_id,
                "message": "checkpoint after first history marker"
            }),
        )
        .await
        .expect("checkpoint.save after first marker");

    send_test_update(&mut harness, channel_id, "history-after-checkpoint").await;
    harness
        .wait_for_channel_output_contains(
            channel_id,
            b"history-after-checkpoint",
            Duration::from_secs(5),
        )
        .await
        .expect("wait for second history marker");

    harness
        .rpc(
            "checkpoint.restore",
            json!({ "thread_id": &thread_id, "seq": 1 }),
        )
        .await
        .expect("checkpoint.restore");

    let loaded = harness
        .rpc(
            "chat.load",
            json!({ "thread_id": &thread_id, "session_id": &session_id, "agent_name": "mock" }),
        )
        .await
        .expect("chat.load after restore");
    assert_eq!(loaded["status"], "starting");
    wait_for_chat_ready(&mut harness, &thread_id, &session_id).await;

    let reattached = harness
        .rpc(
            "chat.attach",
            json!({ "thread_id": &thread_id, "session_id": &session_id }),
        )
        .await
        .expect("chat.attach after restore load");
    let reattached_channel_id = reattached["channel_id"].as_u64().expect("channel id") as u16;

    send_user_prompt(
        &mut harness,
        reattached_channel_id,
        &session_id,
        "resume from checkpoint",
    )
    .await;

    let log_entries = wait_for_mock_chat_agent_log(&worktree_path, |entries| {
        entries
            .iter()
            .any(|entry| entry["method"] == "session/prompt")
    })
    .await;

    let session_new_count = log_entries
        .iter()
        .filter(|entry| entry["method"] == "session/new")
        .count();
    let session_load_count = log_entries
        .iter()
        .filter(|entry| entry["method"] == "session/load")
        .count();
    assert!(
        session_new_count >= 2,
        "restore flow should use session/new for initial start and reload"
    );
    assert_eq!(
        session_load_count, 0,
        "restore reload must not use session/load"
    );

    let injected_prompt = log_entries
        .iter()
        .rev()
        .find(|entry| entry["method"] == "session/prompt")
        .and_then(|entry| entry["params"]["prompt"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|block| block["text"].as_str())
        .collect::<String>();
    assert!(injected_prompt.contains("<conversation-history>"));
    assert!(injected_prompt.contains("history-before-checkpoint"));
    assert!(!injected_prompt.contains("history-after-checkpoint"));
    assert!(injected_prompt.contains("resume from checkpoint"));

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn checkpoint_restore_restart_before_load_uses_session_new() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping checkpoint_restore_restart_before_load_uses_session_new: tmux unavailable"
        );
        return;
    }

    let mut harness = setup_test_server().await;
    let (project, project_id) = add_chat_project(&mut harness).await;
    let created = create_thread(&mut harness, &project_id).await;
    let thread_id = created["id"].as_str().expect("thread id").to_string();
    let worktree_path = PathBuf::from(created["worktree_path"].as_str().expect("worktree path"));

    wait_for_thread_ready(&mut harness, &thread_id).await;

    let started = harness
        .rpc(
            "chat.start",
            json!({ "thread_id": &thread_id, "agent_name": "mock" }),
        )
        .await
        .expect("chat.start");
    let session_id = started["session_id"]
        .as_str()
        .expect("session id")
        .to_string();
    wait_for_chat_ready(&mut harness, &thread_id, &session_id).await;

    let attached = harness
        .rpc(
            "chat.attach",
            json!({ "thread_id": &thread_id, "session_id": &session_id }),
        )
        .await
        .expect("chat.attach");
    let channel_id = attached["channel_id"].as_u64().expect("channel id") as u16;

    send_test_update(&mut harness, channel_id, "history-before-checkpoint").await;
    harness
        .wait_for_channel_output_contains(
            channel_id,
            b"history-before-checkpoint",
            Duration::from_secs(5),
        )
        .await
        .expect("wait for first history marker");

    harness
        .rpc(
            "checkpoint.save",
            json!({
                "thread_id": &thread_id,
                "session_id": &session_id,
                "message": "checkpoint before restart"
            }),
        )
        .await
        .expect("checkpoint.save");

    send_test_update(&mut harness, channel_id, "history-after-checkpoint").await;
    harness
        .wait_for_channel_output_contains(
            channel_id,
            b"history-after-checkpoint",
            Duration::from_secs(5),
        )
        .await
        .expect("wait for second history marker");

    harness
        .rpc(
            "checkpoint.restore",
            json!({ "thread_id": &thread_id, "seq": 1 }),
        )
        .await
        .expect("checkpoint.restore");

    let config_home = harness.preserve_config_home();
    harness.preserve_path(&project.root_dir);
    drop(harness);

    let mut recovered = common::setup_test_server_with_config_home(config_home).await;
    recovered.register_cleanup_path(project.root_dir.clone());

    let listed = recovered
        .rpc("chat.list", json!({ "thread_id": &thread_id }))
        .await
        .expect("chat.list after restore restart");
    assert!(listed
        .as_array()
        .expect("chat.list array")
        .iter()
        .any(|entry| entry["session_id"] == session_id));

    let loaded = recovered
        .rpc(
            "chat.load",
            json!({ "thread_id": &thread_id, "session_id": &session_id, "agent_name": "mock" }),
        )
        .await
        .expect("chat.load after restart");
    assert_eq!(loaded["status"], "starting");
    wait_for_chat_ready(&mut recovered, &thread_id, &session_id).await;

    let reattached = recovered
        .rpc(
            "chat.attach",
            json!({ "thread_id": &thread_id, "session_id": &session_id }),
        )
        .await
        .expect("chat.attach after restart load");
    let reattached_channel_id = reattached["channel_id"].as_u64().expect("channel id") as u16;

    send_user_prompt(
        &mut recovered,
        reattached_channel_id,
        &session_id,
        "resume from checkpoint after restart",
    )
    .await;

    let log_entries = wait_for_mock_chat_agent_log(&worktree_path, |entries| {
        entries
            .iter()
            .any(|entry| entry["method"] == "session/prompt")
    })
    .await;

    let session_new_count = log_entries
        .iter()
        .filter(|entry| entry["method"] == "session/new")
        .count();
    let session_load_count = log_entries
        .iter()
        .filter(|entry| entry["method"] == "session/load")
        .count();
    assert!(
        session_new_count >= 2,
        "restore flow should use session/new before and after daemon restart"
    );
    assert_eq!(
        session_load_count, 0,
        "restored session must not use session/load after daemon restart"
    );

    let injected_prompt = log_entries
        .iter()
        .rev()
        .find(|entry| entry["method"] == "session/prompt")
        .and_then(|entry| entry["params"]["prompt"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|block| block["text"].as_str())
        .collect::<String>();
    assert!(injected_prompt.contains("<conversation-history>"));
    assert!(injected_prompt.contains("history-before-checkpoint"));
    assert!(!injected_prompt.contains("history-after-checkpoint"));
    assert!(injected_prompt.contains("resume from checkpoint after restart"));

    cleanup_thread_project(&mut recovered, &thread_id, &project_id).await;
}

#[tokio::test]
async fn checkpoint_restore_returns_error_when_restored_metadata_persist_fails() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping checkpoint_restore_returns_error_when_restored_metadata_persist_fails: tmux unavailable"
        );
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_chat_project(&mut harness).await;
    let created = create_thread(&mut harness, &project_id).await;
    let thread_id = created["id"].as_str().expect("thread id").to_string();

    wait_for_thread_ready(&mut harness, &thread_id).await;

    let started = harness
        .rpc(
            "chat.start",
            json!({ "thread_id": &thread_id, "agent_name": "mock" }),
        )
        .await
        .expect("chat.start");
    let session_id = started["session_id"]
        .as_str()
        .expect("session id")
        .to_string();
    wait_for_chat_ready(&mut harness, &thread_id, &session_id).await;

    let attached = harness
        .rpc(
            "chat.attach",
            json!({ "thread_id": &thread_id, "session_id": &session_id }),
        )
        .await
        .expect("chat.attach");
    let channel_id = attached["channel_id"].as_u64().expect("channel id") as u16;

    send_test_update(&mut harness, channel_id, "history-before-checkpoint").await;
    harness
        .wait_for_channel_output_contains(
            channel_id,
            b"history-before-checkpoint",
            Duration::from_secs(5),
        )
        .await
        .expect("wait for first history marker");

    harness
        .rpc(
            "checkpoint.save",
            json!({
                "thread_id": &thread_id,
                "session_id": &session_id,
                "message": "checkpoint before metadata write failure"
            }),
        )
        .await
        .expect("checkpoint.save");

    send_test_update(&mut harness, channel_id, "history-after-checkpoint").await;
    harness
        .wait_for_channel_output_contains(
            channel_id,
            b"history-after-checkpoint",
            Duration::from_secs(5),
        )
        .await
        .expect("wait for second history marker");

    let metadata_path = session_metadata_path(&thread_id, &session_id);
    #[cfg(unix)]
    set_read_only(&metadata_path, true);

    let error = harness
        .rpc_expect_error(
            "checkpoint.restore",
            json!({ "thread_id": &thread_id, "seq": 1 }),
        )
        .await;
    assert!(error.contains(&metadata_path.display().to_string()));

    #[cfg(unix)]
    set_read_only(&metadata_path, false);

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn thread_hide_cleans_checkpoint_refs() {
    if !common::tmux_available().await {
        eprintln!("skipping thread_hide_cleans_checkpoint_refs: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness, None).await;
    let created = create_thread(&mut harness, &project_id).await;
    let thread_id = created["id"].as_str().expect("thread id").to_string();
    let worktree_path = PathBuf::from(created["worktree_path"].as_str().expect("worktree path"));

    wait_for_thread_ready(&mut harness, &thread_id).await;
    fs::write(worktree_path.join("README.md"), "checkpoint one\n").expect("write readme");
    harness
        .rpc("checkpoint.save", json!({ "thread_id": &thread_id }))
        .await
        .expect("save checkpoint before hide");
    assert_eq!(checkpoint_refs(&worktree_path, &thread_id).await.len(), 1);

    harness
        .rpc("thread.hide", json!({ "thread_id": &thread_id }))
        .await
        .expect("thread.hide");
    assert!(checkpoint_refs(&worktree_path, &thread_id).await.is_empty());

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
