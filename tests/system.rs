mod common;

use std::time::Duration;

use serde_json::json;
use tokio::process::Command;

#[tokio::test]
async fn system_stats_ignores_non_opencode_processes_and_returns_metrics() {
    let mut harness = common::setup_test_server().await;

    let baseline = harness
        .rpc("system.stats", json!({}))
        .await
        .expect("read baseline system stats");

    let mut decoy = Command::new("python3")
        .arg("-c")
        .arg("import time; time.sleep(30)")
        .arg("opencode-shadow")
        .spawn()
        .expect("spawn decoy process");

    tokio::time::sleep(Duration::from_millis(200)).await;

    let result = harness
        .rpc("system.stats", json!({}))
        .await
        .expect("read system stats with decoy process");

    let _ = decoy.start_kill();
    let _ = decoy.wait().await;

    assert!(
        result["load_avg_1m"].as_f64().is_some(),
        "load_avg_1m should be a number"
    );
    assert!(
        result["memory_total_mb"].as_u64().is_some(),
        "memory_total_mb should be a number"
    );
    assert!(
        result["memory_used_mb"].as_u64().is_some(),
        "memory_used_mb should be a number"
    );

    let baseline_instances = baseline["opencode_instances"]
        .as_u64()
        .expect("baseline opencode_instances should be u64");
    let result_instances = result["opencode_instances"]
        .as_u64()
        .expect("result opencode_instances should be u64");

    assert_eq!(
        result_instances, baseline_instances,
        "non-opencode process should not change opencode_instances"
    );
}
