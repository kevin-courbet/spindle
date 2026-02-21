use std::sync::Arc;

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
            return Ok(protocol::PresetStartResult { started: Some(true) });
        }

        let config = load_threadmill_config(&thread.worktree_path, &project_path)?;
        let preset = config
            .presets
            .get(&params.preset)
            .ok_or_else(|| format!("preset not found: {}", params.preset))?
            .clone();

        if preset.commands.is_empty() {
            return Err(format!("preset '{}' has no commands", params.preset));
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

        Ok(protocol::PresetStartResult {
            started: Some(true),
        })
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
            return Ok(protocol::PresetStopResult {
                stopped: Some(false),
            });
        }

        tmux::kill_window(&thread.tmux_session, &params.preset).await?;

        Ok(protocol::PresetStopResult {
            stopped: Some(true),
        })
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

        Ok(protocol::PresetRestartResult {
            restarted: Some(true),
        })
    }
}
