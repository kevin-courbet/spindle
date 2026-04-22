//! Runtime configuration — OS-aware defaults for workspace layout.
//!
//! Values are resolved at call time, not cached. Environment variables take
//! precedence over compile-time platform defaults so a single binary can be
//! redeployed across hosts with different workspace layouts.

use std::path::PathBuf;

/// Root under which threadmill places cloned projects and the `.threadmill/`
/// worktree tree for non-main-checkout threads.
///
/// Resolution order: `SPINDLE_WORKSPACE_ROOT` env var, then the platform default.
pub fn workspace_root() -> PathBuf {
    if let Ok(value) = std::env::var("SPINDLE_WORKSPACE_ROOT") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    default_workspace_root()
}

#[cfg(target_os = "linux")]
fn default_workspace_root() -> PathBuf {
    PathBuf::from("/home/wsl/dev")
}

#[cfg(target_os = "macos")]
fn default_workspace_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/Users"))
        .join("dev")
}

/// User config directory — where `threads.json`, chat history, and workflow
/// state live.
///
/// Resolution order: `XDG_CONFIG_HOME` env var (honoured on every platform,
/// not just Linux), then `dirs::config_dir()` (which falls back to
/// `~/.config` on Linux and `~/Library/Application Support` on macOS).
///
/// We honour XDG even on macOS because:
///   - Spindle dev runs locally on macOS but deploys to Linux. Tests need a
///     single env var that redirects state on both, otherwise the same test
///     code reads from `~/Library/...` on the dev machine and `~/.config/...`
///     on CI / beast.
///   - Cross-platform CLI tools (ripgrep, fd, helix, …) follow the same
///     XDG-first convention on macOS.
pub fn config_dir() -> Option<PathBuf> {
    if let Ok(value) = std::env::var("XDG_CONFIG_HOME") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    dirs::config_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Env vars are process-global; serialize tests that mutate them.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn config_dir_prefers_xdg_config_home_when_set_even_on_macos() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: serialized via ENV_LOCK; restored below.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", "/tmp/explicit-xdg");
        }
        let result = config_dir();
        // SAFETY: restore previous value.
        unsafe {
            match previous {
                Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
        assert_eq!(result, Some(PathBuf::from("/tmp/explicit-xdg")));
    }

    #[test]
    fn config_dir_ignores_blank_xdg_value_and_falls_back_to_platform_default() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: serialized via ENV_LOCK; restored below.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", "   ");
        }
        let result = config_dir();
        // SAFETY: restore previous value.
        unsafe {
            match previous {
                Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
        // Whatever the platform default is, it must not be the blank value.
        assert_ne!(result, Some(PathBuf::from("   ")));
    }
}
