use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::{net::TcpListener, sync::oneshot};
use tokio_tungstenite::{connect_async, tungstenite::Message};

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
