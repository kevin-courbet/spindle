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
async fn project_add_invalid_path_returns_error() {
    let mut harness = setup_test_server().await;
    let invalid_path = format!("/tmp/{}", common::unique_name("missing-project"));

    let error = harness
        .rpc_expect_error("project.add", json!({ "path": invalid_path }))
        .await;

    assert!(error.contains("project path does not exist"), "{error}");
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
        .rpc("project.branches", json!({ "project_id": project_id.clone() }))
        .await
        .expect("list project branches");
    let branches = branches.as_array().expect("project.branches returns array");

    assert!(branches.iter().any(|branch| branch.as_str() == Some("main")));
    let expected_feature = project
        .feature_branch
        .expect("feature branch exists in test repository");
    assert!(
        branches
            .iter()
            .any(|branch| branch.as_str() == Some(expected_feature.as_str()))
    );

    harness
        .rpc("project.remove", json!({ "project_id": project_id }))
        .await
        .expect("remove project");
}
