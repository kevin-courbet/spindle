use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::{net::TcpListener, sync::oneshot};
use tokio_tungstenite::{connect_async, tungstenite::Message};

const PROTOCOL_VERSION: &str = "2026-03-17";

#[tokio::test]
async fn ping_returns_pong() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("read listener addr");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let server_task = tokio::spawn(async move {
        spindle::serve_listener(listener, shutdown_rx).await;
    });

    let url = format!("ws://{addr}");
    let (mut socket, _) = connect_async(url).await.expect("connect websocket");

    socket
        .send(Message::Text(
            r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#.to_string(),
        ))
        .await
        .expect("send ping");

    let text = loop {
        let frame = socket
            .next()
            .await
            .expect("expected websocket frame")
            .expect("expected successful websocket frame");
        if let Message::Text(text) = frame {
            break text.to_string();
        }
    };

    let value: Value = serde_json::from_str(&text).expect("parse json-rpc response");
    assert_eq!(value["jsonrpc"], "2.0");
    assert_eq!(value["id"], 1);
    assert_eq!(value["result"], "pong");

    let _ = shutdown_tx.send(());
    server_task.await.expect("join daemon task");
}

#[tokio::test]
async fn session_hello_negotiates_protocol_capabilities() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("read listener addr");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let server_task = tokio::spawn(async move {
        spindle::serve_listener(listener, shutdown_rx).await;
    });

    let url = format!("ws://{addr}");
    let (mut socket, _) = connect_async(url).await.expect("connect websocket");

    let payload = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session.hello",
        "params": {
            "client": { "name": "spindle-tests", "version": "dev" },
            "protocol_version": PROTOCOL_VERSION,
            "capabilities": [
                "state.delta.operations.v1",
                "preset.output.v1",
                "unknown.capability"
            ]
        }
    });

    socket
        .send(Message::Text(payload.to_string().into()))
        .await
        .expect("send session.hello");

    let text = loop {
        let frame = socket
            .next()
            .await
            .expect("expected websocket frame")
            .expect("expected successful websocket frame");
        if let Message::Text(text) = frame {
            break text.to_string();
        }
    };

    let value: Value = serde_json::from_str(&text).expect("parse json-rpc response");
    assert_eq!(value["id"], 1);
    let result = &value["result"];
    assert!(result["session_id"].is_string());
    assert_eq!(result["protocol_version"], PROTOCOL_VERSION);
    assert_eq!(
        result["capabilities"],
        json!(["state.delta.operations.v1", "preset.output.v1"])
    );
    assert!(result["state_version"].is_u64());

    let _ = shutdown_tx.send(());
    server_task.await.expect("join daemon task");
}

#[tokio::test]
async fn request_before_session_hello_returns_structured_error() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("read listener addr");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let server_task = tokio::spawn(async move {
        spindle::serve_listener(listener, shutdown_rx).await;
    });

    let url = format!("ws://{addr}");
    let (mut socket, _) = connect_async(url).await.expect("connect websocket");

    let payload = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "project.list",
        "params": {}
    });

    socket
        .send(Message::Text(payload.to_string().into()))
        .await
        .expect("send project.list");

    let text = loop {
        let frame = socket
            .next()
            .await
            .expect("expected websocket frame")
            .expect("expected successful websocket frame");
        if let Message::Text(text) = frame {
            break text.to_string();
        }
    };

    let value: Value = serde_json::from_str(&text).expect("parse json-rpc response");
    assert_eq!(value["id"], 1);
    assert_eq!(value["error"]["code"], -32000);
    assert_eq!(value["error"]["data"]["kind"], "session.not_initialized");
    assert_eq!(value["error"]["data"]["retryable"], false);

    let _ = shutdown_tx.send(());
    server_task.await.expect("join daemon task");
}


#[tokio::test]
async fn system_cleanup_is_not_exposed_over_rpc() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("read listener addr");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let server_task = tokio::spawn(async move {
        spindle::serve_listener(listener, shutdown_rx).await;
    });

    let url = format!("ws://{addr}");
    let (mut socket, _) = connect_async(url).await.expect("connect websocket");

    let hello = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session.hello",
        "params": {
            "client": { "name": "spindle-tests", "version": "dev" },
            "protocol_version": PROTOCOL_VERSION,
            "capabilities": []
        }
    });

    socket
        .send(Message::Text(hello.to_string().into()))
        .await
        .expect("send session.hello");

    loop {
        let frame = socket
            .next()
            .await
            .expect("expected websocket frame")
            .expect("expected successful websocket frame");
        if let Message::Text(text) = frame {
            let value: Value =
                serde_json::from_str(&text).expect("parse session.hello response");
            if value["id"] == 1 {
                break;
            }
        }
    }

    let cleanup = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "system.cleanup",
        "params": {}
    });

    socket
        .send(Message::Text(cleanup.to_string().into()))
        .await
        .expect("send system.cleanup");

    let text = loop {
        let frame = socket
            .next()
            .await
            .expect("expected websocket frame")
            .expect("expected successful websocket frame");
        if let Message::Text(text) = frame {
            let value: Value = serde_json::from_str(&text).expect("parse json-rpc response");
            if value["id"] == 2 {
                break text.to_string();
            }
        }
    };

    let value: Value = serde_json::from_str(&text).expect("parse json-rpc response");
    assert_eq!(value["error"]["code"], -32601);
    assert_eq!(value["error"]["data"]["kind"], "rpc.method_not_found");

    let _ = shutdown_tx.send(());
    server_task.await.expect("join daemon task");
}


#[tokio::test]
async fn session_hello_rejects_incompatible_protocol_version() {
    let listener = TcpListener::bind(127.0.0.1:0)
        .await
        .expect(bind test listener);
    let addr = listener.local_addr().expect(read listener addr);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let server_task = tokio::spawn(async move {
        spindle::serve_listener(listener, shutdown_rx).await;
    });

    let url = format!(ws://{addr});
    let (mut socket, _) = connect_async(url).await.expect(connect websocket);

    let payload = json!({
        jsonrpc: 2.0,
        id: 1,
        method: session.hello,
        params: {
            client: { name: spindle-tests, version: dev },
            protocol_version: 1999-01-01,
            capabilities: [
                state.delta.operations.v1,
                preset.output.v1,
                rpc.errors.structured.v1
            ]
        }
    });

    socket
        .send(Message::Text(payload.to_string().into()))
        .await
        .expect(send session.hello);

    let text = loop {
        let frame = socket
            .next()
            .await
            .expect(expected websocket frame)
            .expect(expected successful websocket frame);
        if let Message::Text(text) = frame {
            break text.to_string();
        }
    };

    let value: Value = serde_json::from_str(&text).expect(parse json-rpc response);
    assert_eq!(value[id], 1);
    assert_eq!(value[error][code], -32602);

    let _ = shutdown_tx.send(());
    server_task.await.expect(join daemon task);
}

#[tokio::test]
async fn session_hello_rejects_missing_required_capabilities() {
    let listener = TcpListener::bind(127.0.0.1:0)
        .await
        .expect(bind test listener);
    let addr = listener.local_addr().expect(read listener addr);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let server_task = tokio::spawn(async move {
        spindle::serve_listener(listener, shutdown_rx).await;
    });

    let url = format!(ws://{addr});
    let (mut socket, _) = connect_async(url).await.expect(connect websocket);

    let payload = json!({
        jsonrpc: 2.0,
        id: 1,
        method: session.hello,
        params: {
            client: { name: spindle-tests, version: dev },
            protocol_version: PROTOCOL_VERSION,
            capabilities: [state.delta.operations.v1]
        }
    });

    socket
        .send(Message::Text(payload.to_string().into()))
        .await
        .expect(send session.hello);

    let text = loop {
        let frame = socket
            .next()
            .await
            .expect(expected websocket frame)
            .expect(expected successful websocket frame);
        if let Message::Text(text) = frame {
            break text.to_string();
        }
    };

    let value: Value = serde_json::from_str(&text).expect(parse json-rpc response);
    assert_eq!(value[id], 1);
    assert_eq!(value[error][code], -32602);

    let _ = shutdown_tx.send(());
    server_task.await.expect(join daemon task);
}

#[tokio::test]
async fn session_hello_rejects_repeat_initialization() {
    let listener = TcpListener::bind(127.0.0.1:0)
        .await
        .expect(bind test listener);
    let addr = listener.local_addr().expect(read listener addr);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let server_task = tokio::spawn(async move {
        spindle::serve_listener(listener, shutdown_rx).await;
    });

    let url = format!(ws://{addr});
    let (mut socket, _) = connect_async(url).await.expect(connect websocket);

    let payload = json!({
        jsonrpc: 2.0,
        id: 1,
        method: session.hello,
        params: {
            client: { name: spindle-tests, version: dev },
            protocol_version: PROTOCOL_VERSION,
            capabilities: [
                state.delta.operations.v1,
                preset.output.v1,
                rpc.errors.structured.v1
            ]
        }
    });

    socket
        .send(Message::Text(payload.to_string().into()))
        .await
        .expect(send first session.hello);

    let _ = loop {
        let frame = socket
            .next()
            .await
            .expect(expected websocket frame)
            .expect(expected successful websocket frame);
        if let Message::Text(text) = frame {
            let value: Value = serde_json::from_str(&text).expect(parse first session.hello response);
            if value[id] == 1 {
                break value;
            }
        }
    };

    let second_payload = json!({
        jsonrpc: 2.0,
        id: 2,
        method: session.hello,
        params: {
            client: { name: spindle-tests, version: dev },
            protocol_version: PROTOCOL_VERSION,
            capabilities: [
                state.delta.operations.v1,
                preset.output.v1,
                rpc.errors.structured.v1
            ]
        }
    });

    socket
        .send(Message::Text(second_payload.to_string().into()))
        .await
        .expect(send second session.hello);

    let response = loop {
        let frame = socket
            .next()
            .await
            .expect(expected websocket frame)
            .expect(expected successful websocket frame);
        if let Message::Text(text) = frame {
            let value: Value = serde_json::from_str(&text).expect(parse second session.hello response);
            if value[id] == 2 {
                break value;
            }
        }
    };

    assert_eq!(response[error][code], -32600);

    let _ = shutdown_tx.send(());
    server_task.await.expect(join daemon task);
}

#[tokio::test]
async fn uninitialized_connection_does_not_receive_events() {
    use std::{
        fs,
        process::Command,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    let listener = TcpListener::bind(127.0.0.1:0)
        .await
        .expect(bind test listener);
    let addr = listener.local_addr().expect(read listener addr);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let server_task = tokio::spawn(async move {
        spindle::serve_listener(listener, shutdown_rx).await;
    });

    let url = format!(ws://{addr});
    let (mut uninitialized_socket, _) = connect_async(&url).await.expect(connect uninitialized websocket);
    let (mut initialized_socket, _) = connect_async(&url).await.expect(connect initialized websocket);

    let hello_payload = json!({
        jsonrpc: 2.0,
        id: 1,
        method: session.hello,
        params: {
            client: { name: spindle-tests, version: dev },
            protocol_version: PROTOCOL_VERSION,
            capabilities: [
                state.delta.operations.v1,
                preset.output.v1,
                rpc.errors.structured.v1
            ]
        }
    });

    initialized_socket
        .send(Message::Text(hello_payload.to_string().into()))
        .await
        .expect(send session.hello);

    let _hello_response = loop {
        let frame = initialized_socket
            .next()
            .await
            .expect(expected websocket frame)
            .expect(expected successful websocket frame);
        if let Message::Text(text) = frame {
            let value: Value = serde_json::from_str(&text).expect(parse session.hello response);
            if value[id] == 1 {
                break value;
            }
        }
    };

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect(time should be monotonic)
        .as_nanos();
    let repo_path = std::env::temp_dir().join(format!(spindle-handshake-{nonce}));
    fs::create_dir_all(&repo_path).expect(create repo path);

    let init_status = Command::new(git)
        .arg(init)
        .current_dir(&repo_path)
        .status()
        .expect(run git init);
    assert!(init_status.success(), git init should succeed);

    let add_payload = json!({
        jsonrpc: 2.0,
        id: 2,
        method: project.add,
        params: {
            path: repo_path.to_string_lossy().to_string()
        }
    });

    initialized_socket
        .send(Message::Text(add_payload.to_string().into()))
        .await
        .expect(send project.add);

    let _project_add_response = loop {
        let frame = initialized_socket
            .next()
            .await
            .expect(expected websocket frame)
            .expect(expected successful websocket frame);
        if let Message::Text(text) = frame {
            let value: Value = serde_json::from_str(&text).expect(parse project.add response);
            if value[id] == 2 {
                break value;
            }
        }
    };

    let _event_seen_on_initialized = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let frame = initialized_socket
                .next()
                .await
                .expect(expected websocket frame)
                .expect(expected successful websocket frame);
            if let Message::Text(text) = frame {
                let value: Value = serde_json::from_str(&text).expect(parse event frame);
                if value[method].is_string() {
                    break value;
                }
            }
        }
    })
    .await
    .expect(expected event on initialized connection);

    let leaked_event = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            let frame = uninitialized_socket
                .next()
                .await
                .expect(expected websocket frame)
                .expect(expected successful websocket frame);
            if let Message::Text(text) = frame {
                break text.to_string();
            }
        }
    })
    .await;

    let _ = fs::remove_dir_all(&repo_path);

    assert!(
        leaked_event.is_err(),
        uninitialized connection should not receive server events before session.hello
    );

    let _ = shutdown_tx.send(());
    server_task.await.expect(join daemon task);
}
