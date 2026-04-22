use tokio::process::Command;

pub async fn tmux_run(args: &[&str]) -> Result<String, String> {
    let output = Command::new("tmux")
        .args(args)
        .output()
        .await
        .map_err(|err| format!("failed to run tmux {}: {err}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let message = if stderr.is_empty() { stdout } else { stderr };
        return Err(format!("tmux {} failed: {message}", args.join(" ")));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub async fn tmux_run_owned(args: Vec<String>) -> Result<String, String> {
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    tmux_run(&arg_refs).await
}

pub async fn set_session_environment(
    session: &str,
    env: &[(String, String)],
) -> Result<(), String> {
    for (key, value) in env {
        tmux_run_owned(vec![
            "set-environment".to_string(),
            "-t".to_string(),
            session.to_string(),
            key.clone(),
            value.clone(),
        ])
        .await?;
    }

    Ok(())
}

pub async fn create_session(name: &str, cwd: &str, env: &[(String, String)]) -> Result<(), String> {
    tmux_run(&["new-session", "-d", "-s", name, "-n", "terminal", "-c", cwd]).await?;
    set_session_environment(name, env).await
}

pub async fn create_window(
    session: &str,
    name: &str,
    command: &str,
    cwd: &str,
) -> Result<(), String> {
    tmux_run_owned(build_create_window_args(session, name, command, cwd))
        .await
        .map(|_| ())
}

pub async fn split_window(
    session: &str,
    name: &str,
    command: &str,
    cwd: &str,
) -> Result<(), String> {
    tmux_run_owned(build_split_window_args(session, name, command, cwd))
        .await
        .map(|_| ())
}

/// Build the argv for `tmux new-window`. Preset commands are wrapped in
/// `/bin/sh -c …` so that shell metacharacters (`$VAR`, `&&`, pipes,
/// quoting) work as users expect when they declare `commands: ["$SHELL"]`
/// or `npm run dev` in `.threadmill.yml`. Without the wrapper tmux passes
/// the command straight to `execvp`, which treats `$SHELL` as a literal
/// path and exits the pane immediately.
pub(crate) fn build_create_window_args(
    session: &str,
    name: &str,
    command: &str,
    cwd: &str,
) -> Vec<String> {
    vec![
        "new-window".to_string(),
        "-d".to_string(),
        "-t".to_string(),
        session.to_string(),
        "-n".to_string(),
        name.to_string(),
        "-c".to_string(),
        cwd.to_string(),
        "/bin/sh".to_string(),
        "-c".to_string(),
        command.to_string(),
    ]
}

pub(crate) fn build_split_window_args(
    session: &str,
    name: &str,
    command: &str,
    cwd: &str,
) -> Vec<String> {
    let target = format!("{session}:{name}");
    vec![
        "split-window".to_string(),
        "-d".to_string(),
        "-t".to_string(),
        target,
        "-c".to_string(),
        cwd.to_string(),
        "/bin/sh".to_string(),
        "-c".to_string(),
        command.to_string(),
    ]
}

pub async fn select_layout(session: &str, name: &str, layout: &str) -> Result<(), String> {
    let target = format!("{session}:{name}");
    tmux_run_owned(vec![
        "select-layout".to_string(),
        "-t".to_string(),
        target,
        layout.to_string(),
    ])
    .await
    .map(|_| ())
}

pub async fn kill_window(session: &str, name: &str) -> Result<(), String> {
    let target = format!("{session}:{name}");
    tmux_run_owned(vec!["kill-window".to_string(), "-t".to_string(), target])
        .await
        .map(|_| ())
}

pub async fn window_exists(session: &str, name: &str) -> Result<bool, String> {
    let windows = tmux_run_owned(vec![
        "list-windows".to_string(),
        "-t".to_string(),
        session.to_string(),
        "-F".to_string(),
        "#{window_name}".to_string(),
    ])
    .await;

    match windows {
        Ok(raw) => Ok(raw.lines().any(|entry| entry.trim() == name)),
        Err(err)
            if err.contains("can't find session")
                || err.contains("no server running")
                || err.contains("no such file or directory") =>
        {
            Ok(false)
        }
        Err(err) => Err(err),
    }
}

pub async fn kill_session(name: &str) -> Result<(), String> {
    tmux_run(&["kill-session", "-t", name]).await.map(|_| ())
}

pub async fn list_sessions() -> Result<Vec<String>, String> {
    match tmux_run(&["list-sessions", "-F", "#S"]).await {
        Ok(raw) => Ok(raw
            .lines()
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(ToOwned::to_owned)
            .collect()),
        Err(err)
            if err.contains("no server running") || err.contains("no such file or directory") =>
        {
            Ok(Vec::new())
        }
        Err(err) => Err(err),
    }
}

pub async fn session_exists(name: &str) -> Result<bool, String> {
    let output = Command::new("tmux")
        .args(["has-session", "-t", name])
        .output()
        .await
        .map_err(|err| format!("failed to run tmux has-session: {err}"))?;

    if output.status.success() {
        return Ok(true);
    }

    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    if stderr.contains("can't find session")
        || stderr.contains("no server running")
        || stderr.contains("no such file or directory")
    {
        Ok(false)
    } else {
        Err(format!("tmux has-session failed: {}", stderr.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_window_args_invoke_command_via_shell_so_env_vars_expand() {
        let args = build_create_window_args("ses1", "aux", "$SHELL", "/tmp/work");
        assert_eq!(
            args,
            vec![
                "new-window".to_string(),
                "-d".to_string(),
                "-t".to_string(),
                "ses1".to_string(),
                "-n".to_string(),
                "aux".to_string(),
                "-c".to_string(),
                "/tmp/work".to_string(),
                "/bin/sh".to_string(),
                "-c".to_string(),
                "$SHELL".to_string(),
            ]
        );
    }

    #[test]
    fn create_window_args_preserve_compound_shell_commands_intact() {
        let args =
            build_create_window_args("ses1", "dev", "npm install && npm run dev", "/tmp/work");
        assert_eq!(args.last(), Some(&"npm install && npm run dev".to_string()));
        let sh_index = args
            .iter()
            .position(|arg| arg == "/bin/sh")
            .expect("argv must include /bin/sh");
        assert_eq!(args.get(sh_index + 1), Some(&"-c".to_string()));
    }

    #[test]
    fn split_window_args_target_named_window_and_wrap_in_shell() {
        let args = build_split_window_args("ses1", "dev", "tail -f log", "/tmp/work");
        assert_eq!(args[0], "split-window");
        assert!(
            args.contains(&"ses1:dev".to_string()),
            "split-window must target {{session}}:{{name}}, got argv={args:?}"
        );
        let sh_index = args
            .iter()
            .position(|arg| arg == "/bin/sh")
            .expect("argv must include /bin/sh");
        assert_eq!(args.get(sh_index + 1), Some(&"-c".to_string()));
        assert_eq!(args.last(), Some(&"tail -f log".to_string()));
    }
}
