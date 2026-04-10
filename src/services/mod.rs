pub mod agent_registry;
pub mod chat;
pub mod checkpoint;
pub mod file;
pub mod git;
pub mod opencode;
pub mod preset;
pub mod project;
pub mod terminal;
pub mod thread;
pub mod todo;

pub fn sanitize_name(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut previous_dash = false;

    for ch in value.chars() {
        let mapped = if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };

        if mapped == '-' {
            if previous_dash {
                continue;
            }
            previous_dash = true;
            out.push('-');
        } else {
            previous_dash = false;
            out.push(mapped);
        }
    }

    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "thread".to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn short_id(value: &str) -> String {
    value.chars().take(8).collect()
}
pub mod system;
