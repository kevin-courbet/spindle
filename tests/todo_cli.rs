mod common;

use std::{process::Output, time::Duration};

use serde_json::{json, Value};
use tokio::process::Command;

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
                "name": common::unique_name("todo-cli"),
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

async fn run_cli(harness: &common::TestHarness, thread_id: &str, args: &[&str]) -> Output {
    Command::new("cargo")
        .args(["run", "--quiet", "--bin", "threadmill-cli", "--"])
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("THREADMILL_CLI_WS_URL", harness.ws_url())
        .env("THREADMILL_THREAD", thread_id)
        .output()
        .await
        .expect("run threadmill-cli todo command")
}

fn parse_stdout_json(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "threadmill-cli failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    serde_json::from_slice(&output.stdout).expect("threadmill-cli stdout json")
}

#[tokio::test]
async fn todo_cli_manages_current_thread_todos() {
    if !common::tmux_available().await {
        eprintln!("skipping todo_cli_manages_current_thread_todos: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let added =
        parse_stdout_json(&run_cli(&harness, &thread_id, &["todo", "add", "Write CLI test"]).await);
    let todo_id = added["id"].as_str().expect("todo id from cli").to_string();
    assert_eq!(added["content"], "Write CLI test");

    let listed = parse_stdout_json(&run_cli(&harness, &thread_id, &["todo", "list"]).await);
    let listed = listed.as_array().expect("todo list array from cli");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], todo_id);

    let completed =
        parse_stdout_json(&run_cli(&harness, &thread_id, &["todo", "done", &todo_id]).await);
    assert_eq!(completed["completed"], true);

    let active = parse_stdout_json(&run_cli(&harness, &thread_id, &["todo", "list"]).await);
    assert!(active
        .as_array()
        .expect("active todo list array")
        .is_empty());

    let removed =
        parse_stdout_json(&run_cli(&harness, &thread_id, &["todo", "remove", &todo_id]).await);
    assert_eq!(removed["success"], true);

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}

#[tokio::test]
async fn todo_cli_updates_and_reorders_current_thread_todos() {
    if !common::tmux_available().await {
        eprintln!("skipping todo_cli_updates_and_reorders_current_thread_todos: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let first = parse_stdout_json(
        &run_cli(
            &harness,
            &thread_id,
            &["todo", "add", "Write CLI test", "--priority", "high"],
        )
        .await,
    );
    let first_id = first["id"].as_str().expect("first todo id").to_string();

    let second =
        parse_stdout_json(&run_cli(&harness, &thread_id, &["todo", "add", "Sort todos"]).await);
    let second_id = second["id"].as_str().expect("second todo id").to_string();

    let updated = parse_stdout_json(
        &run_cli(
            &harness,
            &thread_id,
            &[
                "todo",
                "update",
                &first_id,
                "--content",
                "Updated CLI test",
                "--priority",
                "medium",
            ],
        )
        .await,
    );
    assert_eq!(updated["content"], "Updated CLI test");
    assert_eq!(updated["priority"], "medium");

    let reordered = parse_stdout_json(
        &run_cli(
            &harness,
            &thread_id,
            &["todo", "reorder", &second_id, &first_id],
        )
        .await,
    );
    assert_eq!(reordered["success"], true);

    let listed = parse_stdout_json(
        &run_cli(&harness, &thread_id, &["todo", "list", "--filter", "all"]).await,
    );
    let listed = listed.as_array().expect("todo list array from cli");
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0]["id"], second_id);
    assert_eq!(listed[1]["id"], first_id);

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}
