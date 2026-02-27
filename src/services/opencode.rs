use std::process::Stdio;

use tokio::{
    net::TcpStream,
    process::Command,
    time::{sleep, Duration, Instant},
};

use crate::protocol;

const OPENCODE_HOST: &str = "127.0.0.1";
const OPENCODE_PORT: u16 = 4101;
const OPENCODE_URL: &str = "http://127.0.0.1:4101";

pub struct OpencodeService;

impl OpencodeService {
    pub async fn status(
        _params: protocol::OpencodeStatusParams,
    ) -> Result<protocol::OpencodeStatusResult, String> {
        Ok(protocol::OpencodeStatusResult {
            running: is_running().await,
            port: OPENCODE_PORT,
            url: OPENCODE_URL.to_string(),
        })
    }

    pub async fn ensure(
        _params: protocol::OpencodeEnsureParams,
    ) -> Result<protocol::OpencodeEnsureResult, String> {
        if is_running().await {
            return Ok(OPENCODE_URL.to_string());
        }

        Command::new("opencode")
            .arg("serve")
            .arg("--hostname")
            .arg(OPENCODE_HOST)
            .arg("--port")
            .arg(OPENCODE_PORT.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| format!("failed to start opencode serve: {err}"))?;

        wait_for_startup().await?;
        Ok(OPENCODE_URL.to_string())
    }
}

async fn is_running() -> bool {
    TcpStream::connect((OPENCODE_HOST, OPENCODE_PORT))
        .await
        .is_ok()
}

async fn wait_for_startup() -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if is_running().await {
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "opencode serve did not become reachable on {}:{}",
                OPENCODE_HOST, OPENCODE_PORT
            ));
        }

        sleep(Duration::from_millis(150)).await;
    }
}
