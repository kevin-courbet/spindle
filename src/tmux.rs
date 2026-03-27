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
    tmux_run_owned(vec![
        "new-window".to_string(),
        "-d".to_string(),
        "-t".to_string(),
        session.to_string(),
        "-n".to_string(),
        name.to_string(),
        "-c".to_string(),
        cwd.to_string(),
        command.to_string(),
    ])
    .await
    .map(|_| ())
}

pub async fn split_window(
    session: &str,
    name: &str,
    command: &str,
    cwd: &str,
) -> Result<(), String> {
    let target = format!("{session}:{name}");
    tmux_run_owned(vec![
        "split-window".to_string(),
        "-d".to_string(),
        "-t".to_string(),
        target,
        "-c".to_string(),
        cwd.to_string(),
        command.to_string(),
    ])
    .await
    .map(|_| ())
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
