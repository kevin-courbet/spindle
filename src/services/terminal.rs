use std::{
    collections::HashMap,
    sync::Arc,
    time::Duration,
};

use serde_json::{json, Value};
use tokio::{
    fs::OpenOptions,
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{mpsc, oneshot, Mutex},
    task::JoinHandle,
    time::sleep,
};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{protocol, AppState};

const INPUT_CHANNEL_CAPACITY: usize = 256;
const ATTACH_RETRY_DELAY: Duration = Duration::from_millis(15);

#[derive(Default)]
pub struct TerminalConnectionState {
    by_channel: HashMap<u16, Attachment>,
    by_target: HashMap<String, u16>,
    attaching_targets: HashMap<String, u16>,
}

struct Attachment {
    target: String,
    pane_target: String,
    fifo_path: String,
    input_tx: Option<mpsc::Sender<Vec<u8>>>,
    output_shutdown_tx: Option<oneshot::Sender<()>>,
    input_task: JoinHandle<()>,
    output_task: JoinHandle<()>,
}

pub async fn handle_binary_frame(
    data: Vec<u8>,
    connection_state: Arc<Mutex<TerminalConnectionState>>,
) -> Result<(), String> {
    if data.len() < 2 {
        return Err("binary frame too short".to_string());
    }

    let channel_id = u16::from_be_bytes([data[0], data[1]]);
    let payload = data[2..].to_vec();

    let input_tx = {
        let guard = connection_state.lock().await;
        let attachment = guard
            .by_channel
            .get(&channel_id)
            .ok_or_else(|| format!("unknown channel {channel_id}"))?;
        attachment
            .input_tx
            .as_ref()
            .ok_or_else(|| format!("channel {channel_id} is closed"))?
            .clone()
    };

    input_tx
        .try_send(payload)
        .map_err(|err| format!("failed to queue input for channel {channel_id}: {err}"))
}

pub async fn attach(
    params: protocol::TerminalAttachParams,
    state: Arc<AppState>,
    connection_state: Arc<Mutex<TerminalConnectionState>>,
    outbound_tx: mpsc::UnboundedSender<Message>,
) -> Result<Value, String> {
    let thread = {
        let store = state.store.lock().await;
        store
            .thread_by_id(&params.thread_id)
            .ok_or_else(|| format!("thread not found: {}", params.thread_id))?
            .clone()
    };

    let target_key = target_key(&params.thread_id, &params.preset);
    let channel_id = loop {
        let maybe_channel = {
            let mut guard = connection_state.lock().await;
            if let Some(channel_id) = guard.by_target.get(&target_key) {
                return Ok(json!({ "channel_id": channel_id }));
            }

            if guard.attaching_targets.contains_key(&target_key) {
                None
            } else {
                let reserved_channel_id = state.alloc_channel_id_with(|candidate| {
                    guard.by_channel.contains_key(&candidate)
                        || guard.attaching_targets.values().any(|existing| *existing == candidate)
                });
                guard
                    .attaching_targets
                    .insert(target_key.clone(), reserved_channel_id);
                Some(reserved_channel_id)
            }
        };

        if let Some(channel_id) = maybe_channel {
            break channel_id;
        }
        sleep(ATTACH_RETRY_DELAY).await;
    };

    let attach_result = async {
        let pane_target = resolve_pane_target(&thread.tmux_session, &params.preset).await?;
        let pane_tty = resolve_pane_tty(&pane_target).await?;
        let fifo_path = format!("/tmp/threadmill-pipe-{channel_id}-{}", Uuid::new_v4());

        match std::fs::remove_file(&fifo_path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(format!("failed to clear stale fifo {fifo_path}: {err}")),
        }

        let mkfifo_output = Command::new("mkfifo")
            .arg(&fifo_path)
            .output()
            .await
            .map_err(|err| format!("failed to run mkfifo for {fifo_path}: {err}"))?;
        if !mkfifo_output.status.success() {
            return Err(format!(
                "mkfifo failed for {fifo_path}: {}",
                String::from_utf8_lossy(&mkfifo_output.stderr).trim()
            ));
        }

        let clear_pipe_output = Command::new("tmux")
            .args(["pipe-pane", "-t", &pane_target])
            .output()
            .await
            .map_err(|err| format!("failed to clear existing tmux pipe-pane: {err}"))?;
        if !clear_pipe_output.status.success() {
            let _ = std::fs::remove_file(&fifo_path);
            return Err(format!(
                "tmux pipe-pane clear failed for {target_key}: {}",
                String::from_utf8_lossy(&clear_pipe_output.stderr).trim()
            ));
        }

        let pipe_command = format!("cat > {fifo_path}");
        let pipe_output = Command::new("tmux")
            .args(["pipe-pane", "-t", &pane_target, "-O", &pipe_command])
            .output()
            .await
            .map_err(|err| format!("failed to run tmux pipe-pane: {err}"))?;
        if !pipe_output.status.success() {
            let _ = std::fs::remove_file(&fifo_path);
            return Err(format!(
                "tmux pipe-pane failed for {target_key}: {}",
                String::from_utf8_lossy(&pipe_output.stderr).trim()
            ));
        }

        let (output_shutdown_tx, mut output_shutdown_rx) = oneshot::channel();
        let output_tx = outbound_tx.clone();
        let fifo_path_for_task = fifo_path.clone();
        let output_task = tokio::spawn(async move {
            let mut reader = match OpenOptions::new().read(true).open(&fifo_path_for_task).await {
                Ok(reader) => reader,
                Err(err) => {
                    warn!(fifo = %fifo_path_for_task, error = %err, "failed to open tmux fifo");
                    return;
                }
            };

            let mut buf = [0_u8; 8192];
            loop {
                tokio::select! {
                    _ = &mut output_shutdown_rx => break,
                    read_result = reader.read(&mut buf) => {
                        match read_result {
                            Ok(0) => break,
                            Ok(read_len) => {
                                let mut payload = Vec::with_capacity(read_len + 2);
                                payload.extend_from_slice(&channel_id.to_be_bytes());
                                payload.extend_from_slice(&buf[..read_len]);
                                if output_tx.send(Message::Binary(payload.into())).is_err() {
                                    break;
                                }
                            }
                            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(err) => {
                                warn!(fifo = %fifo_path_for_task, error = %err, "failed to read tmux fifo");
                                break;
                            }
                        }
                    }
                }
            }
        });

        let (input_tx, mut input_rx) = mpsc::channel::<Vec<u8>>(INPUT_CHANNEL_CAPACITY);
        let input_task = tokio::spawn(async move {
            let mut writer = match OpenOptions::new().write(true).open(&pane_tty).await {
                Ok(writer) => writer,
                Err(err) => {
                    warn!(tty = %pane_tty, error = %err, "failed to open pane tty for input");
                    return;
                }
            };

            while let Some(mut batch) = input_rx.recv().await {
                while let Ok(more) = input_rx.try_recv() {
                    batch.extend_from_slice(&more);
                }

                if let Err(err) = writer.write_all(&batch).await {
                    warn!(tty = %pane_tty, error = %err, "failed to write pane input");
                    break;
                }

                if let Err(err) = writer.flush().await {
                    warn!(tty = %pane_tty, error = %err, "failed to flush pane input");
                    break;
                }
            }
        });

        Ok::<Attachment, String>(Attachment {
            target: target_key.clone(),
            pane_target,
            fifo_path,
            input_tx: Some(input_tx),
            output_shutdown_tx: Some(output_shutdown_tx),
            input_task,
            output_task,
        })
    }
    .await;

    let attachment = match attach_result {
        Ok(attachment) => attachment,
        Err(err) => {
            let mut guard = connection_state.lock().await;
            guard.attaching_targets.remove(&target_key);
            return Err(err);
        }
    };

    {
        let mut guard = connection_state.lock().await;
        guard.attaching_targets.remove(&target_key);
        guard.by_target.insert(target_key, channel_id);
        guard.by_channel.insert(channel_id, attachment);
    }

    Ok(json!({ "channel_id": channel_id }))
}

pub async fn detach(
    params: protocol::TerminalDetachParams,
    connection_state: Arc<Mutex<TerminalConnectionState>>,
) -> Result<Value, String> {
    let target = target_key(&params.thread_id, &params.preset);
    let detached = detach_by_target(&target, connection_state).await?;
    Ok(json!({ "detached": detached }))
}

pub async fn resize(
    params: protocol::TerminalResizeParams,
    connection_state: Arc<Mutex<TerminalConnectionState>>,
) -> Result<Value, String> {
    let target = target_key(&params.thread_id, &params.preset);
    let pane_target = {
        let guard = connection_state.lock().await;
        let channel_id = *guard
            .by_target
            .get(&target)
            .ok_or_else(|| format!("target {target} is not attached"))?;
        guard
            .by_channel
            .get(&channel_id)
            .ok_or_else(|| format!("channel {channel_id} not found"))?
            .pane_target
            .clone()
    };

    let output = Command::new("tmux")
        .args([
            "resize-pane",
            "-t",
            &pane_target,
            "-x",
            &params.cols.to_string(),
            "-y",
            &params.rows.to_string(),
        ])
        .output()
        .await
        .map_err(|err| format!("failed to run tmux resize-pane: {err}"))?;

    if !output.status.success() {
        return Err(format!(
            "tmux resize-pane failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(json!({ "resized": true }))
}

pub async fn cleanup_connection(connection_state: Arc<Mutex<TerminalConnectionState>>) {
    let attachments = {
        let mut guard = connection_state.lock().await;
        guard.by_target.clear();
        guard.attaching_targets.clear();
        std::mem::take(&mut guard.by_channel)
    };

    for (_, mut attachment) in attachments {
        cleanup_attachment(&mut attachment).await;
    }
}

async fn detach_by_target(
    target: &str,
    connection_state: Arc<Mutex<TerminalConnectionState>>,
) -> Result<bool, String> {
    let maybe_attachment = {
        let mut guard = connection_state.lock().await;
        let Some(channel_id) = guard.by_target.remove(target) else {
            return Ok(false);
        };
        guard.by_channel.remove(&channel_id)
    };

    if let Some(mut attachment) = maybe_attachment {
        cleanup_attachment(&mut attachment).await;
    }

    Ok(true)
}

async fn cleanup_attachment(attachment: &mut Attachment) {
    debug!(target = %attachment.target, "cleaning attachment");
    if let Some(input_tx) = attachment.input_tx.take() {
        drop(input_tx);
    }
    if let Some(output_shutdown_tx) = attachment.output_shutdown_tx.take() {
        let _ = output_shutdown_tx.send(());
    }

    let stop_pipe_output = Command::new("tmux")
        .args(["pipe-pane", "-t", &attachment.pane_target])
        .output()
        .await;
    match stop_pipe_output {
        Ok(output) if !output.status.success() => {
            warn!(
                target = %attachment.pane_target,
                error = %String::from_utf8_lossy(&output.stderr).trim(),
                "tmux pipe-pane stop failed"
            );
        }
        Err(err) => {
            warn!(target = %attachment.pane_target, error = %err, "failed to stop tmux pipe-pane");
        }
        Ok(_) => {}
    }

    attachment.input_task.abort();
    let _ = (&mut attachment.input_task).await;

    match tokio::time::timeout(Duration::from_millis(250), &mut attachment.output_task).await {
        Ok(_) => {}
        Err(_) => {
            attachment.output_task.abort();
            let _ = (&mut attachment.output_task).await;
        }
    }

    if let Err(err) = std::fs::remove_file(&attachment.fifo_path) {
        if err.kind() != std::io::ErrorKind::NotFound {
            warn!(fifo = %attachment.fifo_path, error = %err, "failed to remove tmux fifo");
        }
    }
}

async fn resolve_pane_target(session: &str, preset: &str) -> Result<String, String> {
    let target = format!("{session}:{preset}");
    let output = Command::new("tmux")
        .args(["list-panes", "-t", &target, "-F", "#{pane_id}"])
        .output()
        .await
        .map_err(|err| format!("failed to run tmux list-panes: {err}"))?;

    if !output.status.success() {
        return Err(format!(
            "tmux list-panes failed for {target}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let pane = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or_else(|| format!("no panes available for {target}"))?
        .to_string();

    Ok(pane)
}

async fn resolve_pane_tty(pane_target: &str) -> Result<String, String> {
    let output = Command::new("tmux")
        .args(["display-message", "-p", "-t", pane_target, "#{pane_tty}"])
        .output()
        .await
        .map_err(|err| format!("failed to run tmux display-message: {err}"))?;

    if !output.status.success() {
        return Err(format!(
            "tmux display-message failed for {pane_target}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let pane_tty = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if pane_tty.is_empty() {
        return Err(format!("tmux returned empty pane tty for {pane_target}"));
    }

    Ok(pane_tty)
}

fn target_key(thread_id: &str, preset: &str) -> String {
    format!("{thread_id}:{preset}")
}
