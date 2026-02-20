use tokio::sync::oneshot;
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), spindle::DaemonError> {
    spindle::init_tracing();

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = shutdown_tx.send(());
        }
    });

    info!(address = spindle::DEFAULT_ADDR, "starting daemon");
    spindle::serve(spindle::DEFAULT_ADDR, shutdown_rx).await
}
