mod common;

use serde_json::json;

#[tokio::test]
async fn opencode_status_and_ensure_report_shared_server() {
    let mut harness = common::setup_test_server().await;

    let status = harness
        .rpc("opencode.status", json!({}))
        .await
        .expect("read opencode status");

    assert_eq!(status["port"], 4101);
    assert_eq!(status["url"], "http://127.0.0.1:4101");

    let running = status["running"]
        .as_bool()
        .expect("opencode.status running is bool");

    if !running {
        eprintln!("skipping opencode.ensure assertion: shared opencode server unavailable");
        return;
    }

    let ensured = harness
        .rpc("opencode.ensure", json!({}))
        .await
        .expect("ensure opencode server");

    assert_eq!(ensured, "http://127.0.0.1:4101");
}
