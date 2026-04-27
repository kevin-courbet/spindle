#![allow(dead_code)]
use std::{
    collections::VecDeque,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, Instant},
};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::{
    net::TcpListener,
    process::Command,
    sync::{oneshot, Mutex, MutexGuard},
    task::JoinHandle,
};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const TEST_PROTOCOL_VERSION: &str = "2026-03-17";
const TEST_CAPABILITIES: &[&str] = &[
    "state.delta.operations.v1",
    "preset.output.v1",
    "rpc.errors.structured.v1",
];

static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

fn test_mutex() -> &'static Mutex<()> {
    TEST_MUTEX.get_or_init(|| Mutex::new(()))
}

pub struct TestHarness {
    socket: Socket,
    ws_url: String,
    shutdown_tx: Option<oneshot::Sender<()>>,
    server_task: JoinHandle<()>,
    next_id: u64,
    events: VecDeque<Value>,
    binaries: VecDeque<Vec<u8>>,
    last_error_response: Option<Value>,
    cleanup_paths: Vec<PathBuf>,
    config_home: PathBuf,
    previous_config_home: Option<OsString>,
    _guard: MutexGuard<'static, ()>,
}

pub struct RpcFailure {
    pub code: i64,
    pub message: String,
    pub data: Value,
}

impl TestHarness {
    pub fn ws_url(&self) -> &str {
        &self.ws_url
    }

    pub async fn rpc(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;

        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        self.socket
            .send(Message::Text(payload.to_string().into()))
            .await
            .map_err(|err| format!("failed to send {method}: {err}"))?;

        let deadline = Instant::now() + Duration::from_secs(25);
        self.last_error_response = None;
        loop {
            let frame = self.next_frame(deadline).await?;
            match frame {
                Message::Text(text) => {
                    let value: Value = serde_json::from_str(text.as_ref())
                        .map_err(|err| format!("failed to parse json-rpc frame: {err}"))?;
                    if value.get("id") == Some(&json!(id)) {
                        if let Some(result) = value.get("result") {
                            return Ok(result.clone());
                        }
                        if let Some(error) = value.get("error") {
                            self.last_error_response = Some(value.clone());
                            let message = error
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown rpc error")
                                .to_string();
                            return Err(message);
                        }
                        return Err(format!("invalid rpc response for {method}"));
                    }

                    if value.get("id").is_none() {
                        self.events.push_back(value);
                    }
                }
                Message::Binary(data) => self.binaries.push_back(data.to_vec()),
                Message::Ping(payload) => {
                    self.socket
                        .send(Message::Pong(payload))
                        .await
                        .map_err(|err| format!("failed to send websocket pong: {err}"))?;
                }
                Message::Pong(_) | Message::Close(_) | Message::Frame(_) => {}
            }
        }
    }

    pub async fn rpc_expect_error(&mut self, method: &str, params: Value) -> String {
        self.rpc_expect_error_response(method, params).await.message
    }

    pub async fn rpc_expect_error_response(&mut self, method: &str, params: Value) -> RpcFailure {
        match self.rpc(method, params).await {
            Ok(result) => panic!("expected error from {method}, got result {result}"),
            Err(message) => {
                let code = self
                    .last_error_response
                    .as_ref()
                    .and_then(|response| response.get("error"))
                    .and_then(|error| error.get("code"))
                    .and_then(Value::as_i64)
                    .unwrap_or(-1);
                let data = self
                    .last_error_response
                    .as_ref()
                    .and_then(|response| response.get("error"))
                    .and_then(|error| error.get("data"))
                    .cloned()
                    .unwrap_or_else(|| json!({}));

                RpcFailure {
                    code,
                    message,
                    data,
                }
            }
        }
    }

    pub async fn send_binary(&mut self, channel_id: u16, payload: &[u8]) -> Result<(), String> {
        let mut frame = Vec::with_capacity(payload.len() + 2);
        frame.extend_from_slice(&channel_id.to_be_bytes());
        frame.extend_from_slice(payload);
        self.socket
            .send(Message::Binary(frame.into()))
            .await
            .map_err(|err| format!("failed to send binary frame for channel {channel_id}: {err}"))
    }

    pub async fn wait_for_event(
        &mut self,
        method: &str,
        timeout: Duration,
    ) -> Result<Value, String> {
        let deadline = Instant::now() + timeout;

        if let Some(event) = self.take_event(method) {
            return Ok(event);
        }

        loop {
            let frame = self.next_frame(deadline).await?;
            match frame {
                Message::Text(text) => {
                    let value: Value = serde_json::from_str(text.as_ref())
                        .map_err(|err| format!("failed to parse event frame: {err}"))?;
                    if value
                        .get("method")
                        .and_then(Value::as_str)
                        .map(|candidate| candidate == method)
                        .unwrap_or(false)
                    {
                        return Ok(value);
                    }

                    if value.get("id").is_none() {
                        self.events.push_back(value);
                    }
                }
                Message::Binary(data) => self.binaries.push_back(data.to_vec()),
                Message::Ping(payload) => {
                    self.socket
                        .send(Message::Pong(payload))
                        .await
                        .map_err(|err| format!("failed to send websocket pong: {err}"))?;
                }
                Message::Pong(_) | Message::Close(_) | Message::Frame(_) => {}
            }
        }
    }

    pub async fn wait_for_channel_output_contains(
        &mut self,
        channel_id: u16,
        needle: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>, String> {
        let deadline = Instant::now() + timeout;
        let mut collected = Vec::new();

        while let Some(chunk) = self.take_binary_chunk(channel_id) {
            collected.extend_from_slice(&chunk);
            if collected
                .windows(needle.len())
                .any(|window| window == needle)
            {
                return Ok(collected);
            }
        }

        loop {
            let frame = self.next_frame(deadline).await?;
            match frame {
                Message::Binary(data) => {
                    let data = data.to_vec();
                    let Some((frame_channel, payload)) = split_binary_frame(&data) else {
                        continue;
                    };

                    if frame_channel == channel_id {
                        collected.extend_from_slice(payload);
                        if collected
                            .windows(needle.len())
                            .any(|window| window == needle)
                        {
                            return Ok(collected);
                        }
                    } else {
                        self.binaries.push_back(data);
                    }
                }
                Message::Text(text) => {
                    let value: Value = serde_json::from_str(text.as_ref())
                        .map_err(|err| format!("failed to parse text frame: {err}"))?;
                    if value.get("id").is_none() {
                        self.events.push_back(value);
                    }
                }
                Message::Ping(payload) => {
                    self.socket
                        .send(Message::Pong(payload))
                        .await
                        .map_err(|err| format!("failed to send websocket pong: {err}"))?;
                }
                Message::Pong(_) | Message::Close(_) | Message::Frame(_) => {}
            }
        }
    }

    pub async fn expect_no_channel_output_contains(
        &mut self,
        channel_id: u16,
        needle: &[u8],
        timeout: Duration,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;

        while let Some(chunk) = self.take_binary_chunk(channel_id) {
            if chunk.windows(needle.len()).any(|window| window == needle) {
                return Err(format!(
                    "unexpected payload for channel {channel_id}: {:?}",
                    String::from_utf8_lossy(&chunk)
                ));
            }
        }

        loop {
            let now = Instant::now();
            if now >= deadline {
                return Ok(());
            }

            let frame = match self.next_frame(deadline).await {
                Ok(frame) => frame,
                Err(err) if err.contains("timed out") => return Ok(()),
                Err(err) => return Err(err),
            };

            match frame {
                Message::Binary(data) => {
                    let data = data.to_vec();
                    let Some((frame_channel, payload)) = split_binary_frame(&data) else {
                        continue;
                    };

                    if frame_channel == channel_id
                        && payload.windows(needle.len()).any(|window| window == needle)
                    {
                        return Err(format!(
                            "unexpected payload for channel {channel_id}: {:?}",
                            String::from_utf8_lossy(payload)
                        ));
                    }

                    self.binaries.push_back(data);
                }
                Message::Text(text) => {
                    let value: Value = serde_json::from_str(text.as_ref())
                        .map_err(|err| format!("failed to parse text frame: {err}"))?;
                    if value.get("id").is_none() {
                        self.events.push_back(value);
                    }
                }
                Message::Ping(payload) => {
                    self.socket
                        .send(Message::Pong(payload))
                        .await
                        .map_err(|err| format!("failed to send websocket pong: {err}"))?;
                }
                Message::Pong(_) | Message::Close(_) | Message::Frame(_) => {}
            }
        }
    }

    pub fn register_cleanup_path(&mut self, path: PathBuf) {
        self.cleanup_paths.push(path);
    }

    pub fn preserve_config_home(&mut self) -> PathBuf {
        self.cleanup_paths.retain(|path| path != &self.config_home);
        self.config_home.clone()
    }

    pub fn preserve_path(&mut self, path: &Path) {
        self.cleanup_paths.retain(|candidate| candidate != path);
    }

    fn take_event(&mut self, method: &str) -> Option<Value> {
        let index = self.events.iter().position(|event| {
            event
                .get("method")
                .and_then(Value::as_str)
                .map(|candidate| candidate == method)
                .unwrap_or(false)
        })?;
        self.events.remove(index)
    }

    fn take_binary_chunk(&mut self, channel_id: u16) -> Option<Vec<u8>> {
        let index = self.binaries.iter().position(|frame| {
            split_binary_frame(frame)
                .map(|(id, _)| id == channel_id)
                .unwrap_or(false)
        })?;
        let frame = self.binaries.remove(index)?;
        let (_, payload) = split_binary_frame(&frame)?;
        Some(payload.to_vec())
    }

    async fn next_frame(&mut self, deadline: Instant) -> Result<Message, String> {
        if Instant::now() >= deadline {
            return Err("timed out waiting for websocket frame".to_string());
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        let next = tokio::time::timeout(remaining, self.socket.next())
            .await
            .map_err(|_| "timed out waiting for websocket frame".to_string())?;

        let frame = next.ok_or_else(|| "websocket closed unexpectedly".to_string())?;
        frame.map_err(|err| format!("websocket read failed: {err}"))
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
        self.server_task.abort();

        for path in self.cleanup_paths.iter().rev() {
            let _ = fs::remove_dir_all(path);
        }

        restore_env_var("XDG_CONFIG_HOME", self.previous_config_home.take());
    }
}

pub async fn setup_test_server() -> TestHarness {
    let guard = test_mutex().lock().await;

    let config_home = unique_temp_path("spindle-config");
    fs::create_dir_all(config_home.join("threadmill")).expect("create test config directory");
    setup_test_server_with_config_home_inner(guard, config_home).await
}

pub async fn setup_test_server_with_config_home(config_home: PathBuf) -> TestHarness {
    let guard = test_mutex().lock().await;
    fs::create_dir_all(config_home.join("threadmill")).expect("create test config directory");
    setup_test_server_with_config_home_inner(guard, config_home).await
}

async fn setup_test_server_with_config_home_inner(
    guard: MutexGuard<'static, ()>,
    config_home: PathBuf,
) -> TestHarness {
    let previous_config_home = std::env::var_os("XDG_CONFIG_HOME");
    set_env_var("XDG_CONFIG_HOME", &config_home);

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("read listener addr");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let server_task = tokio::spawn(async move {
        spindle::serve_listener(listener, shutdown_rx).await;
    });

    let url = format!("ws://{addr}");
    let (socket, _) = connect_async(url.clone()).await.expect("connect websocket");

    let mut harness = TestHarness {
        socket,
        ws_url: url,
        shutdown_tx: Some(shutdown_tx),
        server_task,
        next_id: 1,
        events: VecDeque::new(),
        binaries: VecDeque::new(),
        last_error_response: None,
        cleanup_paths: vec![config_home],
        config_home: std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .expect("XDG_CONFIG_HOME set"),
        previous_config_home,
        _guard: guard,
    };

    let hello = harness
        .rpc(
            "session.hello",
            json!({
                "client": {
                    "name": "spindle-tests",
                    "version": "dev",
                },
                "protocol_version": TEST_PROTOCOL_VERSION,
                "capabilities": TEST_CAPABILITIES,
            }),
        )
        .await
        .expect("session.hello");
    assert!(hello["session_id"].is_string());

    harness
}

pub async fn tmux_available() -> bool {
    match Command::new("tmux").arg("-V").output().await {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}

pub struct TestProject {
    pub root_dir: PathBuf,
    pub repo_path: PathBuf,
    pub feature_branch: Option<String>,
}

pub async fn create_git_project(
    threadmill_config: Option<&str>,
    create_feature_branch: bool,
) -> Result<TestProject, String> {
    let root_dir = unique_temp_path("spindle-project");
    fs::create_dir_all(&root_dir)
        .map_err(|err| format!("failed to create {}: {err}", root_dir.display()))?;

    let origin_path = root_dir.join("origin.git");
    let repo_path = root_dir.join("repo");

    run_cmd(
        "git",
        &["init", "--bare", &origin_path.to_string_lossy()],
        None,
    )
    .await?;
    run_cmd(
        "git",
        &[
            "clone",
            &origin_path.to_string_lossy(),
            &repo_path.to_string_lossy(),
        ],
        None,
    )
    .await?;

    run_git(&repo_path, &["config", "user.name", "Spindle Test"]).await?;
    run_git(
        &repo_path,
        &["config", "user.email", "spindle-test@example.com"],
    )
    .await?;
    run_git(&repo_path, &["config", "commit.gpgsign", "false"]).await?;
    run_git(&repo_path, &["checkout", "-b", "main"]).await?;

    fs::write(repo_path.join("README.md"), "spindle integration test\n")
        .map_err(|err| format!("failed to write README.md: {err}"))?;

    if let Some(config) = threadmill_config {
        fs::write(repo_path.join(".threadmill.yml"), config)
            .map_err(|err| format!("failed to write .threadmill.yml: {err}"))?;
    }

    run_git(&repo_path, &["add", "-A"]).await?;
    run_git(&repo_path, &["commit", "-m", "initial commit"]).await?;
    run_git(&repo_path, &["push", "-u", "origin", "main"]).await?;

    let feature_branch = if create_feature_branch {
        let branch_name = "feature/integration-test";
        run_git(&repo_path, &["checkout", "-b", branch_name]).await?;
        fs::write(repo_path.join("feature.txt"), "feature branch\n")
            .map_err(|err| format!("failed to write feature.txt: {err}"))?;
        run_git(&repo_path, &["add", "feature.txt"]).await?;
        run_git(&repo_path, &["commit", "-m", "feature commit"]).await?;
        run_git(&repo_path, &["push", "-u", "origin", branch_name]).await?;
        run_git(&repo_path, &["checkout", "main"]).await?;
        Some(branch_name.to_string())
    } else {
        None
    };

    Ok(TestProject {
        root_dir,
        repo_path,
        feature_branch,
    })
}

pub async fn create_git_project_without_remote(
    threadmill_config: Option<&str>,
) -> Result<TestProject, String> {
    let root_dir = unique_temp_path("spindle-project");
    fs::create_dir_all(&root_dir)
        .map_err(|err| format!("failed to create {}: {err}", root_dir.display()))?;

    let repo_path = root_dir.join("repo");
    fs::create_dir_all(&repo_path)
        .map_err(|err| format!("failed to create {}: {err}", repo_path.display()))?;

    run_cmd("git", &["init", &repo_path.to_string_lossy()], None).await?;

    run_git(&repo_path, &["config", "user.name", "Spindle Test"]).await?;
    run_git(
        &repo_path,
        &["config", "user.email", "spindle-test@example.com"],
    )
    .await?;
    run_git(&repo_path, &["config", "commit.gpgsign", "false"]).await?;
    run_git(&repo_path, &["checkout", "-b", "main"]).await?;

    fs::write(repo_path.join("README.md"), "spindle integration test\n")
        .map_err(|err| format!("failed to write README.md: {err}"))?;

    if let Some(config) = threadmill_config {
        fs::write(repo_path.join(".threadmill.yml"), config)
            .map_err(|err| format!("failed to write .threadmill.yml: {err}"))?;
    }

    run_git(&repo_path, &["add", "-A"]).await?;
    run_git(&repo_path, &["commit", "-m", "initial commit"]).await?;

    Ok(TestProject {
        root_dir,
        repo_path,
        feature_branch: None,
    })
}

pub async fn create_unborn_git_project_without_remote(branch: &str) -> Result<TestProject, String> {
    let root_dir = unique_temp_path("spindle-project");
    fs::create_dir_all(&root_dir)
        .map_err(|err| format!("failed to create {}: {err}", root_dir.display()))?;

    let repo_path = root_dir.join("repo");
    fs::create_dir_all(&repo_path)
        .map_err(|err| format!("failed to create {}: {err}", repo_path.display()))?;

    run_cmd("git", &["init", &repo_path.to_string_lossy()], None).await?;
    run_git(&repo_path, &["checkout", "-b", branch]).await?;

    Ok(TestProject {
        root_dir,
        repo_path,
        feature_branch: None,
    })
}

pub fn unique_name(prefix: &str) -> String {
    format!("{}-{}", prefix, Uuid::new_v4().simple())
}

fn split_binary_frame(frame: &[u8]) -> Option<(u16, &[u8])> {
    if frame.len() < 2 {
        return None;
    }

    let channel_id = u16::from_be_bytes([frame[0], frame[1]]);
    Some((channel_id, &frame[2..]))
}

async fn run_git(repo_path: &Path, args: &[&str]) -> Result<String, String> {
    run_cmd("git", args, Some(repo_path)).await
}

async fn run_cmd(bin: &str, args: &[&str], cwd: Option<&Path>) -> Result<String, String> {
    let mut command = Command::new(bin);
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }

    let output = command
        .output()
        .await
        .map_err(|err| format!("failed to run {bin} {:?}: {err}", args))?;

    if !output.status.success() {
        return Err(format!(
            "command failed: {bin} {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn unique_temp_path(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        Uuid::new_v4().simple()
    ))
}

fn set_env_var(key: &str, value: &Path) {
    #[allow(unused_unsafe)]
    unsafe {
        std::env::set_var(key, value);
    }
}

fn restore_env_var(key: &str, previous: Option<OsString>) {
    match previous {
        Some(value) => {
            #[allow(unused_unsafe)]
            unsafe {
                std::env::set_var(key, value);
            }
        }
        None => {
            #[allow(unused_unsafe)]
            unsafe {
                std::env::remove_var(key);
            }
        }
    }
}
