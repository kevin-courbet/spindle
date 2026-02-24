use std::{sync::Arc, time::Duration};

use tokio::time::sleep;

use crate::{
    protocol,
    services::thread::load_threadmill_config,
    tmux, AppState,
};

pub struct PresetService;

impl PresetService {
    pub async fn start(
        state: Arc<AppState>,
        params: protocol::PresetStartParams,
    ) -> Result<protocol::PresetStartResult, String> {
        let (thread, project_path) = {
            let store = state.store.lock().await;
            let thread = store
                .thread_by_id(&params.thread_id)
                .ok_or_else(|| format!("thread not found: {}", params.thread_id))?
                .clone();
            let project = store
                .project_by_id(&thread.project_id)
                .ok_or_else(|| format!("project not found: {}", thread.project_id))?
                .clone();
            (thread, project.path)
        };

        if !tmux::session_exists(&thread.tmux_session).await? {
            return Err(format!("tmux session not running: {}", thread.tmux_session));
        }

        if tmux::window_exists(&thread.tmux_session, &params.preset).await? {
            return Ok(protocol::PresetStartResult { ok: true });
        }

        let config = load_threadmill_config(&thread.worktree_path, &project_path)?;
        let preset = config
            .presets
            .get(&params.preset)
            .ok_or_else(|| format!("preset not found: {}", params.preset))?
            .clone();

        if preset.commands.is_empty() {
            return Err(format!("preset {} has no commands", params.preset));
        }

        if preset.parallel && preset.commands.len() > 1 {
            tmux::create_window(
                &thread.tmux_session,
                &params.preset,
                &preset.commands[0],
                &thread.worktree_path,
            )
            .await?;

            for command in preset.commands.iter().skip(1) {
                tmux::split_window(
                    &thread.tmux_session,
                    &params.preset,
                    command,
                    &thread.worktree_path,
                )
                .await?;
            }

            tmux::select_layout(&thread.tmux_session, &params.preset, "tiled").await?;
        } else {
            let command = preset.commands.join(" && ");
            tmux::create_window(
                &thread.tmux_session,
                &params.preset,
                &command,
                &thread.worktree_path,
            )
            .await?;
        }

        emit_preset_event(
            &state,
            &params.thread_id,
            &params.preset,
            protocol::PresetProcessKind::Started,
            None,
        );
        spawn_preset_monitor(
            Arc::clone(&state),
            params.thread_id.clone(),
            params.preset.clone(),
            thread.tmux_session.clone(),
        );

        Ok(protocol::PresetStartResult { ok: true })
    }

    pub async fn stop(
        state: Arc<AppState>,
        params: protocol::PresetStopParams,
    ) -> Result<protocol::PresetStopResult, String> {
        let thread = {
            let store = state.store.lock().await;
            store
                .thread_by_id(&params.thread_id)
                .ok_or_else(|| format!("thread not found: {}", params.thread_id))?
                .clone()
        };

        if !tmux::window_exists(&thread.tmux_session, &params.preset).await? {
            return Ok(protocol::PresetStopResult { ok: false });
        }

        tmux::kill_window(&thread.tmux_session, &params.preset).await?;
        emit_preset_event(
            &state,
            &params.thread_id,
            &params.preset,
            protocol::PresetProcessKind::Exited,
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
            },
        )
        .await;

        Self::start(
            state,
            protocol::PresetStartParams {
                thread_id: params.thread_id,
                preset: params.preset,
            },
        )
        .await?;

        Ok(protocol::PresetRestartResult { ok: true })
    }
}


fn spawn_preset_monitor(state: Arc<AppState>, thread_id: String, preset: String, session: String) {
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(2)).await;
            match tmux::window_exists(&session, &preset).await {
                Ok(true) => continue,
                Ok(false) => {
                    emit_preset_event(
                        &state,
                        &thread_id,
                        &preset,
                        protocol::PresetProcessKind::Exited,
                        None,
                    );
                    break;
                }
                Err(_) => {
                    emit_preset_event(
                        &state,
                        &thread_id,
                        &preset,
                        protocol::PresetProcessKind::Crashed,
                        None,
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
) {
    state.emit_preset_process_event(protocol::PresetProcessEvent {
        thread_id: thread_id.to_string(),
        preset: preset.to_string(),
        event: event.clone(),
        exit_code,
    });
    state.emit_state_delta(vec![protocol::StateDeltaChange::PresetProcessEvent {
        thread_id: thread_id.to_string(),
        preset: preset.to_string(),
        event,
        exit_code,
    }]);
}
