use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader},
    path::Path,
};

use serde_json::Value;
use tracing::warn;

use super::{extract_tool_call_id, extract_tool_status};

pub(crate) fn build_conversation_context(
    history_path: &Path,
    cursor: Option<u64>,
) -> Option<String> {
    if !history_path.exists() {
        return None;
    }

    let file = fs::File::open(history_path).ok()?;
    let reader = BufReader::new(file);
    let max_lines = cursor.unwrap_or(u64::MAX).min(usize::MAX as u64) as usize;
    let mut entries = Vec::new();
    let mut message_indices = HashMap::new();
    let mut tool_indices = HashMap::new();

    for (line_index, line) in reader.lines().take(max_lines).enumerate() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                warn!(
                    path = %history_path.display(),
                    line_index,
                    error = %error,
                    "failed to read chat history line while building conversation context"
                );
                continue;
            }
        };

        let value = match serde_json::from_str::<Value>(&line) {
            Ok(value) => value,
            Err(error) => {
                warn!(
                    path = %history_path.display(),
                    line_index,
                    error = %error,
                    "failed to parse chat history line while building conversation context"
                );
                continue;
            }
        };

        let Some(update) = value.get("update") else {
            continue;
        };

        match session_update_kind(update) {
            Some("user_message_chunk") => append_message_entry(
                &mut entries,
                &mut message_indices,
                "[User]",
                extract_message_id(update),
                extract_inline_text(update.get("content")),
            ),
            Some("agent_message_chunk") => append_message_entry(
                &mut entries,
                &mut message_indices,
                "[Assistant]",
                extract_message_id(update),
                extract_inline_text(update.get("content")),
            ),
            Some("agent_thought_chunk") => append_message_entry(
                &mut entries,
                &mut message_indices,
                "[Assistant - Thinking]",
                extract_message_id(update),
                extract_inline_text(update.get("content")),
            ),
            Some("tool_call") => upsert_tool_call_entry(&mut entries, &mut tool_indices, update),
            Some("tool_call_update") => {
                apply_tool_call_update_entry(&mut entries, &mut tool_indices, update)
            }
            Some("plan") => {
                if let Some(plan) = format_plan_update(update) {
                    entries.push(ConversationContextEntry::Plan(plan));
                }
            }
            Some("usage_update") | Some("config_option_update") => {}
            _ => {}
        }
    }

    let transcript = entries
        .into_iter()
        .filter_map(|entry| entry.render())
        .collect::<Vec<_>>();

    if transcript.is_empty() {
        return None;
    }

    Some(format!(
        "<conversation-history>\nThe following is a conversation you were having with the user. All tool calls\nhave already been executed and their results are reflected in the current working\ndirectory state. Continue this conversation naturally — the user's message\nfollows after this context block.\n\n{}\n</conversation-history>",
        transcript.join("\n\n")
    ))
}

enum ConversationContextEntry {
    Message {
        label: &'static str,
        content: String,
    },
    ToolCall {
        title: String,
        status: ConversationToolStatus,
        input: Option<String>,
        output: Option<String>,
        error: Option<String>,
    },
    Plan(String),
}

impl ConversationContextEntry {
    fn render(self) -> Option<String> {
        match self {
            Self::Message { label, content } => {
                if content.trim().is_empty() {
                    None
                } else {
                    Some(format!("{label}\n{content}"))
                }
            }
            Self::ToolCall {
                title,
                status,
                input,
                output,
                error,
            } => {
                let mut lines = vec![format!("[Tool Call: {title} ({})]", status.as_str())];
                if let Some(input) = input.filter(|input| !input.trim().is_empty()) {
                    lines.push(format!("Input: {input}"));
                }
                match status {
                    ConversationToolStatus::Pending => {}
                    ConversationToolStatus::Completed => {
                        if let Some(output) = output.filter(|output| !output.trim().is_empty()) {
                            lines.push(format!("Output: {output}"));
                        }
                    }
                    ConversationToolStatus::Cancelled => {}
                    ConversationToolStatus::Failed => {
                        if let Some(error) = error.filter(|error| !error.trim().is_empty()) {
                            lines.push(format!("Error: {error}"));
                        }
                    }
                }
                Some(lines.join("\n"))
            }
            Self::Plan(plan) => {
                if plan.trim().is_empty() {
                    None
                } else {
                    Some(format!("[Plan Update]\n{plan}"))
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum ConversationToolStatus {
    Pending,
    Completed,
    Cancelled,
    Failed,
}

impl ConversationToolStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

fn session_update_kind(update: &Value) -> Option<&str> {
    update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .or_else(|| update.get("kind").and_then(Value::as_str))
}

fn extract_message_id(update: &Value) -> Option<&str> {
    update
        .get("messageId")
        .and_then(Value::as_str)
        .or_else(|| update.get("message_id").and_then(Value::as_str))
        .filter(|message_id| !message_id.is_empty())
}

fn append_message_entry(
    entries: &mut Vec<ConversationContextEntry>,
    message_indices: &mut HashMap<String, usize>,
    label: &'static str,
    message_id: Option<&str>,
    content: Option<String>,
) {
    let Some(content) = content.filter(|content| !content.trim().is_empty()) else {
        return;
    };

    if let Some(message_id) = message_id {
        let key = format!("{label}:{message_id}");
        if let Some(index) = message_indices.get(&key).copied() {
            if let Some(ConversationContextEntry::Message {
                content: existing, ..
            }) = entries.get_mut(index)
            {
                existing.push_str(&content);
                return;
            }
        }

        message_indices.insert(key, entries.len());
    }

    entries.push(ConversationContextEntry::Message { label, content });
}

fn upsert_tool_call_entry(
    entries: &mut Vec<ConversationContextEntry>,
    tool_indices: &mut HashMap<String, usize>,
    update: &Value,
) {
    let title = extract_tool_title(update);
    let input = extract_raw_input(update);
    if let Some(tool_call_id) = extract_tool_call_id(update) {
        if let Some(index) = tool_indices.get(&tool_call_id).copied() {
            if let Some(ConversationContextEntry::ToolCall {
                title: existing_title,
                status,
                input: existing_input,
                ..
            }) = entries.get_mut(index)
            {
                *existing_title = title;
                *status = ConversationToolStatus::Pending;
                if input.is_some() {
                    *existing_input = input;
                }
                return;
            }
        }

        tool_indices.insert(tool_call_id, entries.len());
    }

    entries.push(ConversationContextEntry::ToolCall {
        title,
        status: ConversationToolStatus::Pending,
        input,
        output: None,
        error: None,
    });
}

fn apply_tool_call_update_entry(
    entries: &mut Vec<ConversationContextEntry>,
    tool_indices: &mut HashMap<String, usize>,
    update: &Value,
) {
    let title = extract_tool_title(update);
    let status = extract_tool_context_status(update);
    let input = extract_raw_input(update);
    let output = extract_tool_output(update);
    let error = extract_tool_error(update);

    if let Some(tool_call_id) = extract_tool_call_id(update) {
        if let Some(index) = tool_indices.get(&tool_call_id).copied() {
            if let Some(ConversationContextEntry::ToolCall {
                title: existing_title,
                status: existing_status,
                input: existing_input,
                output: existing_output,
                error: existing_error,
            }) = entries.get_mut(index)
            {
                *existing_title = title;
                *existing_status = status;
                if input.is_some() {
                    *existing_input = input;
                }
                if output.is_some() {
                    *existing_output = output;
                }
                if error.is_some() {
                    *existing_error = error;
                }
                return;
            }
        }

        tool_indices.insert(tool_call_id, entries.len());
    }

    entries.push(ConversationContextEntry::ToolCall {
        title,
        status,
        input,
        output,
        error,
    });
}

fn extract_tool_context_status(update: &Value) -> ConversationToolStatus {
    match extract_tool_status(update).unwrap_or_default() {
        "completed" => ConversationToolStatus::Completed,
        "cancelled" => ConversationToolStatus::Cancelled,
        "failed" | "error" => ConversationToolStatus::Failed,
        _ => ConversationToolStatus::Pending,
    }
}

fn extract_tool_title(update: &Value) -> String {
    update
        .get("title")
        .and_then(Value::as_str)
        .or_else(|| {
            update
                .get("toolCall")
                .and_then(|tool_call| tool_call.get("state"))
                .and_then(|state| state.get("title"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            update
                .get("toolCall")
                .and_then(|tool_call| tool_call.get("title"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            update
                .get("toolCall")
                .and_then(|tool_call| tool_call.get("name"))
                .and_then(Value::as_str)
        })
        .or_else(|| update.get("name").and_then(Value::as_str))
        .unwrap_or("Tool")
        .to_string()
}

fn extract_raw_input(update: &Value) -> Option<String> {
    update
        .get("rawInput")
        .and_then(render_scalar_or_json)
        .or_else(|| update.get("input").and_then(render_scalar_or_json))
        .or_else(|| {
            update
                .get("toolCall")
                .and_then(|tool_call| tool_call.get("rawInput"))
                .and_then(render_scalar_or_json)
        })
}

fn extract_tool_output(update: &Value) -> Option<String> {
    extract_block_text(update.get("content"))
        .or_else(|| extract_block_text(update.get("output")))
        .or_else(|| {
            update
                .get("toolCall")
                .and_then(|tool_call| tool_call.get("content"))
                .and_then(|content| extract_block_text(Some(content)))
        })
}

fn extract_tool_error(update: &Value) -> Option<String> {
    extract_block_text(update.get("error"))
        .or_else(|| update.get("error").and_then(render_scalar_or_json))
}

fn format_plan_update(update: &Value) -> Option<String> {
    let entries = update
        .get("entries")
        .and_then(Value::as_array)
        .or_else(|| {
            update
                .get("plan")
                .and_then(|plan| plan.get("entries"))
                .and_then(Value::as_array)
        })?;

    let mut lines = Vec::new();
    for entry in entries {
        let Some(text) = entry
            .get("content")
            .and_then(Value::as_str)
            .or_else(|| entry.get("description").and_then(Value::as_str))
            .or_else(|| entry.get("text").and_then(Value::as_str))
            .or_else(|| entry.get("title").and_then(Value::as_str))
            .map(str::trim)
            .filter(|text| !text.is_empty())
        else {
            continue;
        };
        let marker = match entry
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending")
        {
            "completed" => "[x]",
            "in_progress" | "inProgress" => "[-]",
            "cancelled" => "[/]",
            "failed" | "error" => "[!]",
            _ => "[ ]",
        };
        lines.push(format!("{marker} {text}"));
    }

    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

fn extract_inline_text(value: Option<&Value>) -> Option<String> {
    extract_text_fragments(value).map(|fragments| fragments.join(""))
}

fn extract_block_text(value: Option<&Value>) -> Option<String> {
    extract_text_fragments(value).map(|fragments| fragments.join("\n"))
}

fn extract_text_fragments(value: Option<&Value>) -> Option<Vec<String>> {
    let value = value?;
    let mut fragments = Vec::new();
    collect_text_fragments(value, &mut fragments);
    if fragments.is_empty() {
        None
    } else {
        Some(fragments)
    }
}

fn collect_text_fragments(value: &Value, fragments: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            if !text.is_empty() {
                fragments.push(text.clone());
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_text_fragments(item, fragments);
            }
        }
        Value::Object(map) => {
            if let Some(text) = map.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    fragments.push(text.to_string());
                }
                return;
            }
            if let Some(message) = map.get("message").and_then(Value::as_str) {
                if !message.is_empty() {
                    fragments.push(message.to_string());
                }
                return;
            }
            for key in ["content", "parts", "value", "error"] {
                if let Some(nested) = map.get(key) {
                    collect_text_fragments(nested, fragments);
                    if !fragments.is_empty() {
                        return;
                    }
                }
            }
        }
        _ => {}
    }
}

fn render_scalar_or_json(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        other => serde_json::to_string(other).ok(),
    }
}
