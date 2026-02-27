mod common;

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
async fn project_add_nonexistent_path_returns_error() {
    let mut harness = setup_test_server().await;
    let invalid_path = format!("/tmp/{}", common::unique_name("missing-project"));

    let error = harness
        .rpc_expect_error("project.add", json!({ "path": invalid_path }))
        .await;

    assert_eq!(error, "path does not exist", "{error}");
}

#[tokio::test]
async fn project_add_non_git_directory_returns_error() {
    let mut harness = setup_test_server().await;
    let non_git_path = std::env::temp_dir().join(common::unique_name("non-git-project"));
    std::fs::create_dir_all(&non_git_path).expect("create non-git directory");
    harness.register_cleanup_path(non_git_path.clone());

    let error = harness
        .rpc_expect_error(
            "project.add",
            json!({ "path": non_git_path.to_string_lossy() }),
        )
        .await;

    assert_eq!(error, "not a git repository", "{error}");
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
