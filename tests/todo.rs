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

    let project_id = added["id"].as_str().expect("project id").to_string();
    (project, project_id)
}

async fn create_thread(harness: &mut common::TestHarness, project_id: &str) -> String {
    let created = harness
        .rpc(
            "thread.create",
            json!({
                "project_id": project_id,
                "name": common::unique_name("todo-thread"),
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

async fn wait_for_todo_event(
    harness: &mut common::TestHarness,
    method: &str,
    thread_id: &str,
) -> serde_json::Value {
    loop {
        let event = harness
            .wait_for_event(method, Duration::from_secs(10))
            .await
            .expect("wait for todo event");
        if event["params"]["thread_id"] == thread_id {
            return event;
        }
    }
}

#[tokio::test]
async fn todo_rpc_crud_reorder_and_events_round_trip() {
    if !common::tmux_available().await {
        eprintln!("skipping todo_rpc_crud_reorder_and_events_round_trip: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let (_project, project_id) = add_project(&mut harness).await;
    let thread_id = create_thread(&mut harness, &project_id).await;
    wait_for_thread_ready(&mut harness, &thread_id).await;

    let first = harness
        .rpc(
            "todo.add",
            json!({
                "thread_id": &thread_id,
                "content": "Ship todo RPCs",
                "priority": "high"
            }),
        )
        .await
        .expect("todo.add first");
    let first_id = first["id"].as_str().expect("first todo id").to_string();
    assert_eq!(first["priority"], "high");
    let added_event = wait_for_todo_event(&mut harness, "todo.added", &thread_id).await;
    assert_eq!(added_event["params"]["todo"]["id"], first_id);

    let second = harness
        .rpc(
            "todo.add",
            json!({
                "thread_id": &thread_id,
                "content": "Polish todo CLI",
                "priority": "low"
            }),
        )
        .await
        .expect("todo.add second");
    let second_id = second["id"].as_str().expect("second todo id").to_string();
    let added_event = wait_for_todo_event(&mut harness, "todo.added", &thread_id).await;
    assert_eq!(added_event["params"]["todo"]["id"], second_id);

    let listed = harness
        .rpc(
            "todo.list",
            json!({ "thread_id": &thread_id, "filter": "all" }),
        )
        .await
        .expect("todo.list all");
    let listed = listed.as_array().expect("todo list array");
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0]["id"], first_id);
    assert_eq!(listed[1]["id"], second_id);

    let updated = harness
        .rpc(
            "todo.update",
            json!({
                "thread_id": &thread_id,
                "todo_id": &first_id,
                "content": "Ship polished todo RPCs",
                "priority": "medium",
                "completed": true
            }),
        )
        .await
        .expect("todo.update");
    assert_eq!(updated["content"], "Ship polished todo RPCs");
    assert_eq!(updated["priority"], "medium");
    assert_eq!(updated["completed"], true);
    assert!(updated["completed_at"].is_string());
    let updated_event = wait_for_todo_event(&mut harness, "todo.updated", &thread_id).await;
    assert_eq!(updated_event["params"]["todo"]["id"], first_id);

    let active = harness
        .rpc(
            "todo.list",
            json!({ "thread_id": &thread_id, "filter": "active" }),
        )
        .await
        .expect("todo.list active");
    let active = active.as_array().expect("active todo list array");
    assert_eq!(active.len(), 1);
    assert_eq!(active[0]["id"], second_id);

    let completed = harness
        .rpc(
            "todo.list",
            json!({ "thread_id": &thread_id, "filter": "completed" }),
        )
        .await
        .expect("todo.list completed");
    let completed = completed.as_array().expect("completed todo list array");
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0]["id"], first_id);

    let reordered = harness
        .rpc(
            "todo.reorder",
            json!({ "thread_id": &thread_id, "todo_ids": [&second_id, &first_id] }),
        )
        .await
        .expect("todo.reorder");
    assert_eq!(reordered["success"], true);
    let reordered_event = wait_for_todo_event(&mut harness, "todo.reordered", &thread_id).await;
    assert_eq!(
        reordered_event["params"]["todo_ids"],
        json!([second_id, first_id])
    );

    let listed = harness
        .rpc(
            "todo.list",
            json!({ "thread_id": &thread_id, "filter": "all" }),
        )
        .await
        .expect("todo.list all after reorder");
    let listed = listed.as_array().expect("todo list array after reorder");
    assert_eq!(listed[0]["id"], second_id);
    assert_eq!(listed[1]["id"], first_id);

    let removed = harness
        .rpc(
            "todo.remove",
            json!({ "thread_id": &thread_id, "todo_id": &second_id }),
        )
        .await
        .expect("todo.remove");
    assert_eq!(removed["success"], true);
    let removed_event = wait_for_todo_event(&mut harness, "todo.removed", &thread_id).await;
    assert_eq!(removed_event["params"]["todo_id"], second_id);

    let listed = harness
        .rpc(
            "todo.list",
            json!({ "thread_id": &thread_id, "filter": "all" }),
        )
        .await
        .expect("todo.list final");
    let listed = listed.as_array().expect("final todo list array");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], first_id);

    cleanup_thread_project(&mut harness, &thread_id, &project_id).await;
}
