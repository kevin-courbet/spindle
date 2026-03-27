use std::{path::Path, sync::Arc, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{mpsc, oneshot, Mutex},
    task::JoinHandle,
};
use tokio_tungstenite::tungstenite::Message;
use tracing::warn;

use crate::{protocol, services::terminal::TerminalConnectionState, AppState};

const AGENT_INPUT_CHANNEL_CAPACITY: usize = 256;
const AGENT_IO_CHUNK_SIZE: usize = 8192;

pub struct AgentAttachment {
    pub project_id: String,
    pub agent_name: String,
    pub input_tx: Option<mpsc::Sender<Vec<u8>>>,
    pub stop_tx: Option<oneshot::Sender<()>>,
    pub input_task: JoinHandle<()>,
    pub output_task: JoinHandle<()>,
}

pub struct AgentService;

impl AgentService {
    pub async fn start(
        state: Arc<AppState>,
        params: protocol::AgentStartParams,
        connection_state: Arc<Mutex<TerminalConnectionState>>,
        outbound_tx: mpsc::UnboundedSender<Message>,
    ) -> Result<protocol::AgentStartResult, String> {
        let project = {
            let store = state.store.lock().await;
            store
                .project_by_id(&params.project_id)
                .ok_or_else(|| format!("project not found: {}", params.project_id))?
                .clone()
        };

        let agents = crate::services::project::load_project_agents(&project.path)?;
        let agent = agents
            .into_iter()
            .find(|candidate| candidate.name == params.agent_name)
            .ok_or_else(|| format!("agent not found: {}", params.agent_name))?;

        let channel_id = {
            let guard = connection_state.lock().await;
            state.alloc_channel_id_with(|candidate| {
                guard.by_channel.contains_key(&candidate)
                    || guard.by_agent_channel.contains_key(&candidate)
                    || guard
                        .attaching_targets
                        .values()
                        .any(|existing| *existing == candidate)
            })
        };

        let cwd = resolve_agent_cwd(&project.path, agent.cwd.as_deref())?;
        let mut child = Command::new("bash")
            .args(["-lc", &agent.command])
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|err| format!("failed to spawn agent {}: {err}", agent.name))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("failed to capture stdout for agent {}", agent.name))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("failed to capture stdin for agent {}", agent.name))?;

        let (input_tx, mut input_rx) = mpsc::channel::<Vec<u8>>(AGENT_INPUT_CHANNEL_CAPACITY);
        let input_task = tokio::spawn(async move {
            let mut writer = stdin;
            while let Some(mut batch) = input_rx.recv().await {
                while let Ok(more) = input_rx.try_recv() {
                    batch.extend_from_slice(&more);
                }

                if let Err(err) = writer.write_all(&batch).await {
                    warn!(channel_id, error = %err, "failed to write agent stdin");
                    break;
                }

                if let Err(err) = writer.flush().await {
                    warn!(channel_id, error = %err, "failed to flush agent stdin");
                    break;
                }
            }
        });

        let output_tx = outbound_tx.clone();
        let output_task = tokio::spawn(async move {
            let mut reader = stdout;
            let mut buf = [0_u8; AGENT_IO_CHUNK_SIZE];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(read_len) => {
                        let mut payload = Vec::with_capacity(read_len + 2);
                        payload.extend_from_slice(&channel_id.to_be_bytes());
                        payload.extend_from_slice(&buf[..read_len]);
                        if output_tx.send(Message::Binary(payload)).is_err() {
                            break;
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(err) => {
                        warn!(channel_id, error = %err, "failed to read agent stdout");
                        break;
                    }
                }
            }
        });

        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        {
            let mut guard = connection_state.lock().await;
            guard.by_agent_channel.insert(
                channel_id,
                AgentAttachment {
                    project_id: params.project_id.clone(),
                    agent_name: params.agent_name.clone(),
                    input_tx: Some(input_tx),
                    stop_tx: Some(stop_tx),
                    input_task,
                    output_task,
                },
            );
        }

        state.emit_agent_status_changed(protocol::AgentStatusChanged {
            channel_id,
            project_id: params.project_id.clone(),
            agent_name: params.agent_name.clone(),
            event: protocol::AgentProcessKind::Started,
            exit_code: None,
        });

        let state_for_task = Arc::clone(&state);
        let connection_state_for_task = Arc::clone(&connection_state);
        let project_id_for_task = params.project_id.clone();
        let agent_name_for_task = params.agent_name.clone();
        tokio::spawn(async move {
            let (status, explicitly_stopped) = tokio::select! {
                status = child.wait() => (status, false),
                _ = &mut stop_rx => {
                    let _ = child.kill().await;
                    (child.wait().await, true)
                }
            };

            let (event, exit_code) = match status {
                Ok(exit_status) => {
                    let code = exit_status.code().map(|value| value as i64);
                    if explicitly_stopped || exit_status.success() {
                        (protocol::AgentProcessKind::Exited, code)
                    } else {
                        (protocol::AgentProcessKind::Crashed, code)
                    }
                }
                Err(_) => (protocol::AgentProcessKind::Crashed, None),
            };

            let maybe_attachment = {
                let mut guard = connection_state_for_task.lock().await;
                guard.by_agent_channel.remove(&channel_id)
            };

            if let Some(mut attachment) = maybe_attachment {
                cleanup_agent_attachment(&mut attachment).await;
            }

            state_for_task.emit_agent_status_changed(protocol::AgentStatusChanged {
                channel_id,
                project_id: project_id_for_task,
                agent_name: agent_name_for_task,
                event,
                exit_code,
            });
        });

        Ok(protocol::AgentStartResult { channel_id })
    }

    pub async fn stop(
        params: protocol::AgentStopParams,
        connection_state: Arc<Mutex<TerminalConnectionState>>,
    ) -> Result<protocol::AgentStopResult, String> {
        let maybe_attachment = {
            let mut guard = connection_state.lock().await;
            guard.by_agent_channel.remove(&params.channel_id)
        };

        let Some(mut attachment) = maybe_attachment else {
            return Err(format!("agent channel not found: {}", params.channel_id));
        };

        cleanup_agent_attachment(&mut attachment).await;
        Ok(protocol::AgentStopResult::default())
    }
}

pub async fn cleanup_agent_attachment(attachment: &mut AgentAttachment) {
    if let Some(input_tx) = attachment.input_tx.take() {
        drop(input_tx);
    }

    if let Some(stop_tx) = attachment.stop_tx.take() {
        let _ = stop_tx.send(());
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
}

fn resolve_agent_cwd(project_path: &str, cwd: Option<&str>) -> Result<String, String> {
    let Some(cwd) = cwd else {
        return Ok(project_path.to_string());
    };

    let cwd_path = Path::new(cwd);
    if cwd_path.is_absolute() {
        return Ok(cwd.to_string());
    }

    let project_root = std::fs::canonicalize(project_path)
        .map_err(|err| format!("failed to canonicalize project {}: {err}", project_path))?;
    let joined = project_root.join(cwd_path);
    let resolved = std::fs::canonicalize(&joined)
        .map_err(|err| format!("failed to resolve agent cwd {}: {err}", joined.display()))?;

    if !resolved.starts_with(&project_root) {
        return Err(format!("agent cwd escapes project root: {cwd}"));
    }

    resolved
        .to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("invalid utf-8 agent cwd: {}", resolved.display()))
}
