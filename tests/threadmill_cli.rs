use serde_json::Value;
use tokio::{net::TcpListener, process::Command, sync::oneshot};

#[tokio::test]
async fn thread_list_outputs_valid_json() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("read listener addr");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let server_task = tokio::spawn(async move {
        spindle::serve_listener(listener, shutdown_rx).await;
    });

    let output = Command::new("cargo")
        .args([
            "run",
            "--quiet",
            "--bin",
            "threadmill-cli",
            "--",
            "thread",
            "list",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("THREADMILL_CLI_WS_URL", format!("ws://{addr}"))
        .output()
        .await
        .expect("run threadmill-cli");

    assert!(
        output.status.success(),
        "threadmill-cli failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("threadmill-cli output is UTF-8");
    let parsed: Value = serde_json::from_str(stdout.trim()).expect("threadmill-cli output is JSON");
    assert!(parsed.is_array(), "thread.list output must be a JSON array");

    let _ = shutdown_tx.send(());
    server_task.await.expect("join daemon task");
}
