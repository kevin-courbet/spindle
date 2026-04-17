use std::{sync::Arc, time::Duration};

use tokio::process::Command;
use tokio::time::sleep;

use crate::{
    protocol,
    services::{
        project::{load_project_presets, resolve_preset_cwd},
        thread::load_threadmill_config,
    },
    state_store::{port_base_with_offset, thread_env, Thread},
    tmux, AppState,
};

pub struct PresetService;

const PRESET_MONITOR_POLL: Duration = Duration::from_secs(1);
const PRESET_CAPTURE_LINES: &str = "-200";
const PRESET_MAX_CHUNK_BYTES: usize = 8 * 1024;
const PRESET_MAX_LAST_OUTPUT: usize = 20;
const PRESET_STOP_WAIT_ATTEMPTS: usize = 20;
const PRESET_STOP_WAIT_DELAY: Duration = Duration::from_millis(25);

impl PresetService {
    pub async fn start(
        state: Arc<AppState>,
        params: protocol::PresetStartParams,
    ) -> Result<protocol::PresetStartResult, String> {
        let effective_id = params
            .session_id
            .clone()
            .unwrap_or_else(|| params.preset.clone());
        let (thread, project) = {
            let store = state.store.lock().await;
            let thread = store
                .thread_by_id(&params.thread_id)
                .ok_or_else(|| format!("thread not found: {}", params.thread_id))?
                .clone();
            let project = store
                .project_by_id(&thread.project_id)
                .ok_or_else(|| format!("project not found: {}", thread.project_id))?
                .clone();
            (thread, project)
        };

        if !tmux::session_exists(&thread.tmux_session).await? {
            // Tmux session died (beast reboot, manual kill, etc.) — recreate it.
            // Same logic as StateStore startup reconciliation.
            let config = load_threadmill_config(&thread.worktree_path, &project.path)?;
            let port_base = port_base_with_offset(config.ports.base, thread.port_offset)?;
            let env = thread_env(&project, &thread, port_base);
            tmux::create_session(&thread.tmux_session, &thread.worktree_path, &env).await?;
        }

        if tmux::window_exists(&thread.tmux_session, &effective_id).await? {
            return Ok(protocol::PresetStartResult { ok: true });
        }

        let config = load_threadmill_config(&thread.worktree_path, &project.path)?;
        let port_base = port_base_with_offset(config.ports.base, thread.port_offset)?;
        let env = thread_env(&project, &thread, port_base);
        tmux::set_session_environment(&thread.tmux_session, &env).await?;

        let project_presets = load_project_presets(&project.path)?;
        let preset_config = project_presets.iter().find(|p| p.name == params.preset);

        if let Some(preset) = preset_config {
            let cwd = resolve_preset_cwd(&thread.worktree_path, preset.cwd.as_deref())?;
            tmux::create_window(&thread.tmux_session, &effective_id, &preset.command, &cwd).await?;
        } else {
            start_legacy_preset(&thread, &params.preset, &effective_id, &project.path).await?;
        }

        emit_preset_event(
            &state,
            &params.thread_id,
            &params.preset,
            protocol::PresetProcessKind::Started,
            None,
            None,
        );
        emit_preset_output(
            &state,
            &params.thread_id,
            &params.preset,
            protocol::PresetOutputStream::Stdout,
            format!("{} started", params.preset),
        );
        spawn_preset_monitor(
            Arc::clone(&state),
            params.thread_id.clone(),
            params.preset.clone(),
            effective_id,
            thread.tmux_session.clone(),
        );

        Ok(protocol::PresetStartResult { ok: true })
    }

    pub async fn stop(
        state: Arc<AppState>,
        params: protocol::PresetStopParams,
    ) -> Result<protocol::PresetStopResult, String> {
        let effective_id = params
            .session_id
            .clone()
            .unwrap_or_else(|| params.preset.clone());
        let thread = {
            let store = state.store.lock().await;
            store
                .thread_by_id(&params.thread_id)
                .ok_or_else(|| format!("thread not found: {}", params.thread_id))?
                .clone()
        };

        if !tmux::window_exists(&thread.tmux_session, &effective_id).await? {
            return Ok(protocol::PresetStopResult { ok: false });
        }

        tmux::kill_window(&thread.tmux_session, &effective_id).await?;
        for _ in 0..PRESET_STOP_WAIT_ATTEMPTS {
            match tmux::window_exists(&thread.tmux_session, &effective_id).await {
                Ok(false) => break,
                Ok(true) => sleep(PRESET_STOP_WAIT_DELAY).await,
                Err(_) => break,
            }
        }
        emit_preset_event(
            &state,
            &params.thread_id,
            &params.preset,
            protocol::PresetProcessKind::Exited,
            None,
            None,
        );

        Ok(protocol::PresetStopResult { ok: true })
    }

    pub async fn restart(
        state: Arc<AppState>,
        params: protocol::PresetRestartParams,
    ) -> Result<protocol::PresetRestartResult, String> {
        let _ = Self::stop(
            Arc::clone(&state),
            protocol::PresetStopParams {
                thread_id: params.thread_id.clone(),
                preset: params.preset.clone(),
                session_id: None,
            },
        )
        .await;

        Self::start(
            state,
            protocol::PresetStartParams {
                thread_id: params.thread_id,
                preset: params.preset,
                session_id: None,
            },
        )
        .await?;

        Ok(protocol::PresetRestartResult { ok: true })
    }
}

async fn start_legacy_preset(
    thread: &Thread,
    preset_name: &str,
    window_name: &str,
    project_path: &str,
) -> Result<(), String> {
    let config = load_threadmill_config(&thread.worktree_path, project_path)?;
    let preset = config
        .presets
        .get(preset_name)
        .ok_or_else(|| format!("preset not found: {}", preset_name))?
        .clone();

    if preset.commands.is_empty() {
        return Err(format!("preset {} has no commands", preset_name));
    }

    if preset.parallel && preset.commands.len() > 1 {
        tmux::create_window(
            &thread.tmux_session,
            window_name,
            &preset.commands[0],
            &thread.worktree_path,
        )
        .await?;

        for command in preset.commands.iter().skip(1) {
            tmux::split_window(
                &thread.tmux_session,
                window_name,
                command,
                &thread.worktree_path,
            )
            .await?;
        }

        tmux::select_layout(&thread.tmux_session, window_name, "tiled").await?;
    } else {
        let command = preset.commands.join(" && ");
        tmux::create_window(
            &thread.tmux_session,
            window_name,
            &command,
            &thread.worktree_path,
        )
        .await?;
    }

    Ok(())
}

fn spawn_preset_monitor(
    state: Arc<AppState>,
    thread_id: String,
    preset: String,
    window_name: String,
    session: String,
) {
    tokio::spawn(async move {
        let mut latest_capture = String::new();
        loop {
            sleep(PRESET_MONITOR_POLL).await;

            if let Ok(capture) = capture_preset_output(&session, &window_name).await {
                if let Some(chunk) = capture_new_chunk(&latest_capture, &capture) {
                    emit_preset_output(
                        &state,
                        &thread_id,
                        &preset,
                        protocol::PresetOutputStream::Stdout,
                        chunk,
                    );
                }
                latest_capture = capture;
            }

            match tmux::window_exists(&session, &window_name).await {
                Ok(true) => continue,
                Ok(false) => {
                    emit_preset_event(
                        &state,
                        &thread_id,
                        &preset,
                        protocol::PresetProcessKind::Exited,
                        None,
                        None,
                    );
                    break;
                }
                Err(err) => {
                    emit_preset_event(
                        &state,
                        &thread_id,
                        &preset,
                        protocol::PresetProcessKind::Crashed,
                        None,
                        Some(protocol::PresetCrashContext {
                            signal: None,
                            reason: Some(err),
                            last_output: tail_lines(&latest_capture, PRESET_MAX_LAST_OUTPUT),
                        }),
                    );
                    break;
                }
            }
        }
    });
}

fn emit_preset_event(
    state: &AppState,
    thread_id: &str,
    preset: &str,
    event: protocol::PresetProcessKind,
    exit_code: Option<i64>,
    crash_context: Option<protocol::PresetCrashContext>,
) {
    state.emit_state_delta(vec![
        protocol::StateDeltaOperationPayload::PresetProcessEvent {
            thread_id: thread_id.to_string(),
            preset: preset.to_string(),
            event,
            exit_code,
            crash_context,
        },
    ]);
}

fn emit_preset_output(
    state: &AppState,
    thread_id: &str,
    preset: &str,
    stream: protocol::PresetOutputStream,
    chunk: String,
) {
    if chunk.is_empty() {
        return;
    }

    state.emit_preset_output(protocol::PresetOutputEvent {
        thread_id: thread_id.to_string(),
        preset: preset.to_string(),
        stream: stream.clone(),
        chunk: chunk.clone(),
    });
}

async fn capture_preset_output(session: &str, preset: &str) -> Result<String, String> {
    let target = format!("{session}:{preset}");
    let output = Command::new("tmux")
        .args([
            "capture-pane",
            "-p",
            "-t",
            &target,
            "-S",
            PRESET_CAPTURE_LINES,
        ])
        .output()
        .await
        .map_err(|err| format!("failed to run tmux capture-pane: {err}"))?;

    if !output.status.success() {
        return Err(format!(
            "tmux capture-pane failed for {target}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let mut capture = String::from_utf8_lossy(&output.stdout).to_string();
    truncate_utf8_tail(&mut capture, PRESET_MAX_CHUNK_BYTES);
    Ok(capture)
}

fn truncate_utf8_tail(input: &mut String, max_bytes: usize) {
    if input.len() <= max_bytes {
        return;
    }

    let mut start = input.len().saturating_sub(max_bytes);
    while start < input.len() && !input.is_char_boundary(start) {
        start += 1;
    }

    input.drain(..start);
}

fn capture_new_chunk(previous: &str, current: &str) -> Option<String> {
    if current.is_empty() || current == previous {
        return None;
    }

    let overlap = suffix_prefix_overlap(previous, current);
    let chunk = current[overlap..].trim().to_string();
    if chunk.is_empty() {
        None
    } else {
        Some(chunk)
    }
}

fn suffix_prefix_overlap(previous: &str, current: &str) -> usize {
    let max_overlap = previous.len().min(current.len());

    for overlap in (1..=max_overlap).rev() {
        let previous_start = previous.len() - overlap;
        if !previous.is_char_boundary(previous_start) || !current.is_char_boundary(overlap) {
            continue;
        }

        if previous[previous_start..] == current[..overlap] {
            return overlap;
        }
    }

    0
}

fn tail_lines(input: &str, max_lines: usize) -> Vec<String> {
    if max_lines == 0 {
        return Vec::new();
    }

    let mut lines = input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<String>>();

    if lines.len() > max_lines {
        lines = lines.split_off(lines.len() - max_lines);
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::{capture_new_chunk, truncate_utf8_tail};

    #[test]
    fn capture_new_chunk_only_emits_new_tail_after_rollover() {
        let previous = "line1\nline2\nline3\n";
        let current = "line2\nline3\nline4\n";

        assert_eq!(
            capture_new_chunk(previous, current),
            Some("line4".to_string())
        );
    }

    #[test]
    fn truncate_utf8_tail_drops_partial_leading_scalar() {
        let mut capture = "ab🙂cd".to_string();

        truncate_utf8_tail(&mut capture, 5);

        assert_eq!(capture, "cd");
    }
}
