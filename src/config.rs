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
