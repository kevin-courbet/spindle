mod common;

use std::time::Duration;

use serde_json::json;

async fn setup_test_server() -> common::TestHarness {
    common::setup_test_server().await
}

#[tokio::test]
async fn project_add_list_remove_lifecycle() {
    let mut harness = setup_test_server().await;
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

    let listed = harness
        .rpc("project.list", json!({}))
        .await
        .expect("list projects");
    let projects = listed.as_array().expect("project.list returns array");
    assert!(projects.iter().any(|entry| entry["id"] == project_id));

    harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await
        .expect("remove project");

    let listed_after = harness
        .rpc("project.list", json!({}))
        .await
        .expect("list projects after remove");
    assert!(
        listed_after
            .as_array()
            .expect("project.list returns array")
            .is_empty(),
        "project list should be empty after removing the only project"
    );
}

#[tokio::test]
async fn project_add_and_list_do_not_auto_create_main_checkout_thread() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping project_add_and_list_do_not_auto_create_main_checkout_thread: tmux unavailable"
        );
        return;
    }

    let mut harness = setup_test_server().await;
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
    harness
        .rpc("project.list", json!({}))
        .await
        .expect("list projects");

    let created = harness
        .wait_for_event("thread.created", Duration::from_secs(2))
        .await;
    assert!(
        created.is_err(),
        "project.add/project.list must not auto-create main checkout thread"
    );

    let listed = harness
        .rpc("thread.list", json!({ "project_id": project_id.clone() }))
        .await
        .expect("list threads");
    assert!(
        listed
            .as_array()
            .expect("thread.list returns array")
            .is_empty(),
        "project.add/project.list must not create default main thread"
    );

    harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await
        .expect("remove project");
}

#[tokio::test]
async fn project_add_nonexistent_path_returns_error() {
    let mut harness = setup_test_server().await;
    let invalid_path = format!("/tmp/{}", common::unique_name("missing-project"));

    let error = harness
        .rpc_expect_error("project.add", json!({ "path": invalid_path }))
        .await;

    assert_eq!(error, "path does not exist", "{error}");
}

#[tokio::test]
async fn project_add_non_git_directory_registers_workspace() {
    let mut harness = setup_test_server().await;
    let non_git_path = std::env::temp_dir().join(common::unique_name("non-git-project"));
    std::fs::create_dir_all(&non_git_path).expect("create non-git directory");
    harness.register_cleanup_path(non_git_path.clone());

    let added = harness
        .rpc(
            "project.add",
            json!({ "path": non_git_path.to_string_lossy() }),
        )
        .await
        .expect("add non-git workspace");

    assert_eq!(
        added["path"],
        json!(non_git_path
            .canonicalize()
            .expect("canonical path")
            .to_string_lossy()
            .to_string())
    );
    assert_eq!(added["default_branch"], json!("main"));
}

#[tokio::test]
async fn project_add_accepts_bare_repository() {
    let mut harness = setup_test_server().await;
    let project = common::create_git_project(None, true)
        .await
        .expect("create test git project");
    harness.register_cleanup_path(project.root_dir.clone());

    let bare_repo_path = project.root_dir.join("origin.git");
    let added = harness
        .rpc(
            "project.add",
            json!({ "path": bare_repo_path.to_string_lossy() }),
        )
        .await
        .expect("add bare repository as project");

    let project_id = added["id"]
        .as_str()
        .expect("project id in add response")
        .to_string();

    harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await
        .expect("remove project");
}

#[tokio::test]
async fn project_branches_returns_remote_branches() {
    let mut harness = setup_test_server().await;
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

    let branches = harness
        .rpc(
            "project.branches",
            json!({ "project_id": project_id.clone() }),
        )
        .await
        .expect("list project branches");
    let branches = branches.as_array().expect("project.branches returns array");

    assert!(branches
        .iter()
        .any(|branch| branch.as_str() == Some("main")));
    let expected_feature = project
        .feature_branch
        .expect("feature branch exists in test repository");
    assert!(branches
        .iter()
        .any(|branch| branch.as_str() == Some(expected_feature.as_str())));

    harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await
        .expect("remove project");
}

#[tokio::test]
async fn project_clone_registers_project() {
    let mut harness = setup_test_server().await;
    let clone_path = std::env::temp_dir().join(common::unique_name("clone-rust-mustache"));
    harness.register_cleanup_path(clone_path.clone());

    let cloned = harness
        .rpc(
            "project.clone",
            json!({
                "url": "https://github.com/nickel-org/rust-mustache.git",
                "path": clone_path.to_string_lossy(),
            }),
        )
        .await
        .expect("clone project");

    let project_id = cloned["id"]
        .as_str()
        .expect("project id in clone response")
        .to_string();

    let listed = harness
        .rpc("project.list", json!({}))
        .await
        .expect("list projects after clone");
    let listed = listed.as_array().expect("project.list returns array");

    assert!(
        listed.iter().any(|entry| entry["id"] == project_id),
        "project.list should include cloned project"
    );
}

#[tokio::test]
async fn project_list_returns_presets_from_threadmill_config() {
    let mut harness = setup_test_server().await;
    let config = r#"presets:
  editor:
    command: nvim
  shell:
    command: zsh
  server:
    command: npm run dev
    cwd: ./frontend
"#;
    let project = common::create_git_project(Some(config), true)
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

    let listed = harness
        .rpc("project.list", json!({}))
        .await
        .expect("list projects");
    let projects = listed.as_array().expect("project.list returns array");
    let project_row = projects
        .iter()
        .find(|project| project["id"] == project_id)
        .expect("project present in list");

    let presets = project_row["presets"]
        .as_array()
        .expect("project includes presets array");

    assert!(presets.iter().any(|preset| {
        preset["name"] == "editor" && preset["command"] == "nvim" && preset["cwd"].is_null()
    }));

    assert!(presets.iter().any(|preset| {
        preset["name"] == "shell" && preset["command"] == "zsh" && preset["cwd"].is_null()
    }));

    assert!(presets.iter().any(|preset| {
        preset["name"] == "server"
            && preset["command"] == "npm run dev"
            && preset["cwd"] == "./frontend"
    }));

    harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await
        .expect("remove project");
}

#[tokio::test]
async fn project_list_uses_shared_opencode_attach_preset_by_default() {
    let mut harness = setup_test_server().await;
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

    let listed = harness
        .rpc("project.list", json!({}))
        .await
        .expect("list projects");
    let projects = listed.as_array().expect("project.list returns array");
    let project_row = projects
        .iter()
        .find(|project| project["id"] == project_id)
        .expect("project present in list");

    let presets = project_row["presets"]
        .as_array()
        .expect("project includes presets array");

    assert!(presets.iter().any(|preset| {
        preset["name"] == "opencode"
            && preset["command"]
                == "opencode attach http://127.0.0.1:4101 --dir $THREADMILL_WORKTREE"
            && preset["cwd"].is_null()
    }));

    harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await
        .expect("remove project");
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

async fn assert_thread_status(
    harness: &mut common::TestHarness,
    project_id: &str,
    thread_id: &str,
    expected_status: &str,
) {
    for _ in 0..50 {
        let listed = harness
            .rpc("thread.list", json!({ "project_id": project_id }))
            .await
            .expect("list threads");
        if let Some(thread) = listed
            .as_array()
            .expect("thread.list returns array")
            .iter()
            .find(|thread| thread["id"] == thread_id)
        {
            if thread["status"] == expected_status {
                return;
            }
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    panic!("timed out waiting for thread {thread_id} to reach status {expected_status}");
}

async fn create_new_feature_thread(
    harness: &mut common::TestHarness,
    project_id: &str,
    name: &str,
) -> String {
    let created = harness
        .rpc(
            "thread.create",
            json!({
                "project_id": project_id,
                "name": name,
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

#[tokio::test]
async fn provisioning_flow_clone_then_thread_create() {
    if !common::tmux_available().await {
        eprintln!("skipping provisioning_flow_clone_then_thread_create: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;

    let provision_root = std::env::temp_dir().join(common::unique_name("provisioning-clone"));
    std::fs::create_dir_all(&provision_root).expect("create provisioning root");
    harness.register_cleanup_path(provision_root.clone());

    let missing_repo_path = provision_root.join("nonexistent-repo");
    let lookup_missing = harness
        .rpc(
            "project.lookup",
            json!({ "path": missing_repo_path.to_string_lossy() }),
        )
        .await
        .expect("lookup missing repo path");
    assert_eq!(lookup_missing["exists"], false);
    assert_eq!(lookup_missing["is_git_repo"], false);
    assert!(lookup_missing["project_id"].is_null());

    let source_project = common::create_git_project(None, false)
        .await
        .expect("create source git project");
    harness.register_cleanup_path(source_project.root_dir.clone());

    let clone_parent = provision_root.join("clones");
    std::fs::create_dir_all(&clone_parent).expect("create clone parent");

    let origin_repo_path = source_project.root_dir.join("origin.git");
    let cloned = harness
        .rpc(
            "project.clone",
            json!({
                "url": origin_repo_path.to_string_lossy(),
                "path": clone_parent.to_string_lossy(),
            }),
        )
        .await
        .expect("clone project into provisioning path");

    let project_id = cloned["id"]
        .as_str()
        .expect("cloned project id")
        .to_string();
    let cloned_path = cloned["path"]
        .as_str()
        .expect("cloned project path")
        .to_string();

    let lookup_cloned = harness
        .rpc("project.lookup", json!({ "path": cloned_path.clone() }))
        .await
        .expect("lookup cloned repo path");
    assert_eq!(lookup_cloned["exists"], true);
    assert_eq!(lookup_cloned["is_git_repo"], true);
    assert_eq!(lookup_cloned["project_id"], project_id);

    let thread_id = create_new_feature_thread(&mut harness, &project_id, "feature-1").await;
    wait_for_thread_ready(&mut harness, &thread_id).await;
    assert_thread_status(&mut harness, &project_id, &thread_id, "active").await;

    harness
        .rpc(
            "thread.close",
            json!({ "thread_id": thread_id, "mode": "close" }),
        )
        .await
        .expect("close provisioned thread");
    harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await
        .expect("remove cloned project");
}

#[tokio::test]
async fn provisioning_flow_existing_repo_register_then_thread() {
    if !common::tmux_available().await {
        eprintln!(
            "skipping provisioning_flow_existing_repo_register_then_thread: tmux unavailable"
        );
        return;
    }

    let mut harness = setup_test_server().await;
    let project = common::create_git_project(None, true)
        .await
        .expect("create existing git project");
    harness.register_cleanup_path(project.root_dir.clone());

    let lookup_before_add = harness
        .rpc(
            "project.lookup",
            json!({ "path": project.repo_path.to_string_lossy() }),
        )
        .await
        .expect("lookup existing unregistered repo");
    assert_eq!(lookup_before_add["exists"], true);
    assert_eq!(lookup_before_add["is_git_repo"], true);
    assert!(lookup_before_add["project_id"].is_null());

    let added = harness
        .rpc(
            "project.add",
            json!({ "path": project.repo_path.to_string_lossy() }),
        )
        .await
        .expect("register existing repo");
    let project_id = added["id"].as_str().expect("project id").to_string();

    let lookup_after_add = harness
        .rpc(
            "project.lookup",
            json!({ "path": project.repo_path.to_string_lossy() }),
        )
        .await
        .expect("lookup registered repo");
    assert_eq!(lookup_after_add["exists"], true);
    assert_eq!(lookup_after_add["is_git_repo"], true);
    assert_eq!(lookup_after_add["project_id"], project_id);

    let thread_id = create_new_feature_thread(&mut harness, &project_id, "feature-1").await;
    wait_for_thread_ready(&mut harness, &thread_id).await;
    assert_thread_status(&mut harness, &project_id, &thread_id, "active").await;

    harness
        .rpc(
            "thread.close",
            json!({ "thread_id": thread_id, "mode": "close" }),
        )
        .await
        .expect("close thread");
    harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await
        .expect("remove project");
}

#[tokio::test]
async fn provisioning_flow_already_registered() {
    if !common::tmux_available().await {
        eprintln!("skipping provisioning_flow_already_registered: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let project = common::create_git_project(None, true)
        .await
        .expect("create git project");
    harness.register_cleanup_path(project.root_dir.clone());

    let added = harness
        .rpc(
            "project.add",
            json!({ "path": project.repo_path.to_string_lossy() }),
        )
        .await
        .expect("register project");
    let project_id = added["id"].as_str().expect("project id").to_string();

    let lookup_registered = harness
        .rpc(
            "project.lookup",
            json!({ "path": project.repo_path.to_string_lossy() }),
        )
        .await
        .expect("lookup already registered repo");
    assert_eq!(lookup_registered["exists"], true);
    assert_eq!(lookup_registered["is_git_repo"], true);
    assert_eq!(lookup_registered["project_id"], project_id);

    let thread_id = create_new_feature_thread(&mut harness, &project_id, "feature-1").await;
    wait_for_thread_ready(&mut harness, &thread_id).await;
    assert_thread_status(&mut harness, &project_id, &thread_id, "active").await;

    let listed_projects = harness
        .rpc("project.list", json!({}))
        .await
        .expect("list projects");
    let listed_projects = listed_projects
        .as_array()
        .expect("project.list returns array");
    assert_eq!(listed_projects.len(), 1, "project should not be duplicated");
    assert_eq!(listed_projects[0]["id"], project_id);

    harness
        .rpc(
            "thread.close",
            json!({ "thread_id": thread_id, "mode": "close" }),
        )
        .await
        .expect("close thread");
    harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await
        .expect("remove project");
}

#[tokio::test]
async fn project_clone_and_create_thread_full_lifecycle() {
    if !common::tmux_available().await {
        eprintln!("skipping project_clone_and_create_thread_full_lifecycle: tmux unavailable");
        return;
    }

    let mut harness = setup_test_server().await;
    let source_project = common::create_git_project(None, false)
        .await
        .expect("create source git project");
    harness.register_cleanup_path(source_project.root_dir.clone());

    let clone_parent = std::env::temp_dir().join(common::unique_name("full-lifecycle-clone"));
    std::fs::create_dir_all(&clone_parent).expect("create clone parent directory");
    harness.register_cleanup_path(clone_parent.clone());

    let origin_repo_path = source_project.root_dir.join("origin.git");
    let cloned = harness
        .rpc(
            "project.clone",
            json!({
                "url": origin_repo_path.to_string_lossy(),
                "path": clone_parent.to_string_lossy(),
            }),
        )
        .await
        .expect("clone project");

    let project_id = cloned["id"]
        .as_str()
        .expect("cloned project id")
        .to_string();
    let cloned_path = cloned["path"]
        .as_str()
        .expect("cloned project path")
        .to_string();

    let thread_id = create_new_feature_thread(&mut harness, &project_id, "feature-1").await;
    wait_for_thread_ready(&mut harness, &thread_id).await;
    assert_thread_status(&mut harness, &project_id, &thread_id, "active").await;

    let hidden = harness
        .rpc("thread.hide", json!({ "thread_id": thread_id }))
        .await
        .expect("hide thread");
    assert_eq!(hidden["status"], "hidden");
    assert_thread_status(&mut harness, &project_id, &thread_id, "hidden").await;

    let reopened = harness
        .rpc("thread.reopen", json!({ "thread_id": thread_id }))
        .await
        .expect("reopen hidden thread");
    assert_eq!(reopened["status"], "active");
    assert_thread_status(&mut harness, &project_id, &thread_id, "active").await;

    let closed = harness
        .rpc(
            "thread.close",
            json!({ "thread_id": thread_id, "mode": "close" }),
        )
        .await
        .expect("close thread");
    assert_eq!(closed["status"], "closed");
    assert_thread_status(&mut harness, &project_id, &thread_id, "closed").await;

    let removed = harness
        .rpc(
            "project.remove",
            json!({ "project_id": project_id.clone() }),
        )
        .await
        .expect("remove project");
    assert_eq!(removed["removed"], true);

    let listed_projects = harness
        .rpc("project.list", json!({}))
        .await
        .expect("list projects");
    assert!(
        !listed_projects
            .as_array()
            .expect("project.list returns array")
            .iter()
            .any(|project| project["id"] == project_id),
        "project should be removed from project.list"
    );

    let lookup_after_remove = harness
        .rpc("project.lookup", json!({ "path": cloned_path }))
        .await
        .expect("lookup clone path after remove");
    assert_eq!(lookup_after_remove["exists"], true);
    assert_eq!(lookup_after_remove["is_git_repo"], true);
    assert!(lookup_after_remove["project_id"].is_null());
}

#[tokio::test]
async fn state_delta_events_for_project_lifecycle() {
    let mut harness = setup_test_server().await;
    let project = common::create_git_project(None, false)
        .await
        .expect("create test project");
    harness.register_cleanup_path(project.root_dir.clone());

    let snapshot = harness
        .rpc("state.snapshot", json!({}))
        .await
        .expect("state snapshot before lifecycle");
    let base_version = snapshot["state_version"].as_u64().expect("state_version");

    let added = harness
        .rpc(
            "project.add",
            json!({ "path": project.repo_path.to_string_lossy() }),
        )
        .await
        .expect("add project");
    let project_id = added["id"].as_str().expect("project id").to_string();

    let add_delta = loop {
        let event = harness
            .wait_for_event("state.delta", Duration::from_secs(10))
            .await
            .expect("wait for add state.delta");
        let version = event["params"]["state_version"]
            .as_u64()
            .expect("state.delta version");
        if version <= base_version {
            continue;
        }

        let operations = event["params"]["operations"]
            .as_array()
            .expect("state.delta operations array");
        if operations.iter().any(|operation| {
            operation["type"] == "project.added" && operation["project"]["id"] == project_id
        }) {
            assert!(
                operations
                    .iter()
                    .all(|operation| operation["op_id"].as_str().is_some()),
                "all operations must include op_id"
            );
            break event;
        }
    };

    let add_version = add_delta["params"]["state_version"]
        .as_u64()
        .expect("add state.delta version");
    assert!(add_version > base_version);

    harness
        .rpc(
            "project.remove",
            json!({ "project_id": project_id.clone() }),
        )
        .await
        .expect("remove project");

    let remove_delta = loop {
        let event = harness
            .wait_for_event("state.delta", Duration::from_secs(10))
            .await
            .expect("wait for remove state.delta");
        let version = event["params"]["state_version"]
            .as_u64()
            .expect("state.delta version");
        if version <= add_version {
            continue;
        }

        let operations = event["params"]["operations"]
            .as_array()
            .expect("state.delta operations array");
        if operations.iter().any(|operation| {
            operation["type"] == "project.removed" && operation["project_id"] == project_id
        }) {
            break event;
        }
    };

    let remove_version = remove_delta["params"]["state_version"]
        .as_u64()
        .expect("remove state.delta version");
    assert!(remove_version > add_version);
}
