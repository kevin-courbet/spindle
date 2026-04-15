use std::panic;
use tokio::sync::oneshot;
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), spindle::DaemonError> {
    spindle::init_tracing();

    // Global panic hook — log the panic before the thread unwinds.
    // Tokio tasks that panic get caught by JoinHandle, but this catches
    // panics on non-tokio threads and provides a tracing-formatted log line
    // visible in journalctl.
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic payload".to_string()
        };
        tracing::error!(location = %location, panic = %payload, "SPINDLE PANIC");
        default_hook(info);
    }));

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = shutdown_tx.send(());
        }
    });

    let addr =
        std::env::var("SPINDLE_ADDR").unwrap_or_else(|_| spindle::DEFAULT_ADDR.to_string());
    info!(address = %addr, "starting daemon");
    spindle::serve(&addr, shutdown_rx).await
}
