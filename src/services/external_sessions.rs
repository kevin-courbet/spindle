use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::Utc;
use rusqlite::{types::ValueRef, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{sync::MutexGuard, time::Duration};
use tracing::warn;
use uuid::Uuid;

use crate::{protocol, AppState};

const OPENCODE_DB_RELATIVE_PATH: &[&str] = &["opencode", "opencode.db"];
const CLAUDE_PROJECTS_RELATIVE_PATH: &[&str] = &["projects"];
const IMPORTED_FROM_OPENCODE: &str = "Imported from OpenCode session";
const IMPORTED_FROM_CLAUDE: &str = "Imported from Claude Code session";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExternalProvider {
    Opencode,
    Claude,
}

impl ExternalProvider {
    fn registry_name(self) -> &'static str {
        match self {
            Self::Opencode => "opencode",
            Self::Claude => "claude",
        }
    }

    fn from_registry_name(value: &str) -> Option<Self> {
        match value {
            "opencode" => Some(Self::Opencode),
            "claude" => Some(Self::Claude),
            _ => None,
        }
    }

    fn agent_type(self) -> &'static str {
        self.registry_name()
    }

    fn import_marker(self) -> &'static str {
        match self {
            Self::Opencode => IMPORTED_FROM_OPENCODE,
            Self::Claude => IMPORTED_FROM_CLAUDE,
        }
    }
}

#[derive(Debug, Clone)]
struct KnownThread {
    thread_id: String,
    worktree_path: String,
}

#[derive(Debug, Clone)]
struct ExternalSessionCandidate {
    provider: ExternalProvider,
    provider_session_id: String,
    thread_id: String,
    history_updates: Vec<Value>,
    created_at: Option<String>,
    title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegistryEntry {
    thread_id: String,
    session_id: String,
    adopted_at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AdoptionRegistry {
    #[serde(flatten)]
    entries: HashMap<String, RegistryEntry>,
}

pub(crate) struct ExternalSessionScanner;

impl ExternalSessionScanner {
    pub(crate) fn start(state: Arc<AppState>) {
        tokio::spawn(async move {
            run_scan_cycle(Arc::clone(&state)).await;
            loop {
                let delay = if state.active_connection_count() > 0 {
                    Duration::from_secs(15)
                } else {
                    Duration::from_secs(60)
                };
                tokio::time::sleep(delay).await;
                run_scan_cycle(Arc::clone(&state)).await;
            }
        });
    }

    pub(crate) fn trigger(state: Arc<AppState>) {
        tokio::spawn(async move {
            run_scan_cycle(state).await;
        });
    }

    pub(crate) async fn scan_now(state: Arc<AppState>) -> Result<usize, String> {
        scan_external_sessions(state).await
    }
}

async fn run_scan_cycle(state: Arc<AppState>) {
    if let Err(error) = scan_external_sessions(state).await {
        warn!(error = %error, "external session scan failed");
    }
}

async fn scan_external_sessions(state: Arc<AppState>) -> Result<usize, String> {
    let lock = Arc::clone(&state.external_session_scan_lock);
    let _guard = lock.lock().await;
    scan_external_sessions_locked(state, _guard).await
}

async fn scan_external_sessions_locked(
    state: Arc<AppState>,
    _guard: MutexGuard<'_, ()>,
) -> Result<usize, String> {
    let (state_dir, known_threads) = {
        let store = state.store.lock().await;
        let state_dir = store
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let known_threads = known_thread_checkouts(&store.data.threads);
        (state_dir, known_threads)
    };

    if known_threads.is_empty() {
        return Ok(0);
    }

    let registry_path = state_dir.join("external-sessions.json");
    let history_root = {
        let chat = state.chat.lock().await;
        chat.history_root_path().to_path_buf()
    };
    let mut registry = load_registry(&registry_path)?;
    restore_registered_external_sessions(&state, &history_root, &known_threads, &registry).await?;
    let mut candidates = Vec::new();

    match discover_opencode_sessions(&known_threads) {
        Ok(mut discovered) => candidates.append(&mut discovered),
        Err(error) => warn!(error = %error, "opencode external session discovery failed"),
    }
    match discover_claude_sessions(&known_threads) {
        Ok(mut discovered) => candidates.append(&mut discovered),
        Err(error) => warn!(error = %error, "claude external session discovery failed"),
    }

    let mut adopted = 0;
    for candidate in candidates {
        let key = registry_key(candidate.provider, &candidate.provider_session_id);
        if registry.entries.contains_key(&key) {
            continue;
        }
        let session_id = adopt_external_session(&state, &history_root, &candidate).await?;
        registry.entries.insert(
            key,
            RegistryEntry {
                thread_id: candidate.thread_id.clone(),
                session_id,
                adopted_at: Utc::now().to_rfc3339(),
            },
        );
        save_registry(&registry_path, &registry)?;
        adopted += 1;
    }

    Ok(adopted)
}

async fn restore_registered_external_sessions(
    state: &Arc<AppState>,
    history_root: &Path,
    known_threads: &[KnownThread],
    registry: &AdoptionRegistry,
) -> Result<(), String> {
    let known_thread_ids = known_threads
        .iter()
        .map(|thread| thread.thread_id.as_str())
        .collect::<HashSet<_>>();
    for (key, entry) in &registry.entries {
        if !known_thread_ids.contains(entry.thread_id.as_str()) {
            continue;
        }
        let Some((provider_name, provider_session_id)) = key.split_once(':') else {
            continue;
        };
        let Some(provider) = ExternalProvider::from_registry_name(provider_name) else {
            continue;
        };
        let history_path = crate::services::chat::history_path_for_session(
            history_root,
            &entry.thread_id,
            &entry.session_id,
        );
        if !history_path.is_file() {
            continue;
        }
        let summary = protocol::ChatSessionSummary {
            session_id: entry.session_id.clone(),
            agent_type: provider.agent_type().to_string(),
            status: protocol::ChatSessionStatus::Ended,
            agent_status: protocol::AgentStatus::Idle,
            worker_count: 0,
            title: None,
            model_id: None,
            created_at: entry.adopted_at.clone(),
            display_name: None,
            parent_session_id: None,
            pending_blocked_requests: Vec::new(),
        };
        crate::services::chat::ChatService::register_imported_session(
            Arc::clone(state),
            entry.thread_id.clone(),
            entry.session_id.clone(),
            summary,
            Some(provider_session_id.to_string()),
            history_path,
        )
        .await;
    }
    Ok(())
}

fn known_thread_checkouts(threads: &[crate::state_store::Thread]) -> Vec<KnownThread> {
    threads
        .iter()
        .filter_map(|thread| {
            let worktree_path = thread.worktree_path.clone()?;
            Path::new(&worktree_path).is_dir().then(|| KnownThread {
                thread_id: thread.id.clone(),
                worktree_path,
            })
        })
        .collect()
}

async fn adopt_external_session(
    state: &Arc<AppState>,
    history_root: &Path,
    candidate: &ExternalSessionCandidate,
) -> Result<String, String> {
    let session_id = Uuid::new_v4().to_string();
    let history_path = crate::services::chat::history_path_for_session(
        history_root,
        &candidate.thread_id,
        &session_id,
    );
    write_history(&history_path, &candidate.history_updates)?;
    crate::services::chat::persist_session_metadata(
        history_root,
        &candidate.thread_id,
        &session_id,
        Some(&candidate.provider_session_id),
    )?;

    let summary = protocol::ChatSessionSummary {
        session_id: session_id.clone(),
        agent_type: candidate.provider.agent_type().to_string(),
        status: protocol::ChatSessionStatus::Ended,
        agent_status: protocol::AgentStatus::Idle,
        worker_count: 0,
        title: candidate.title.clone(),
        model_id: None,
        created_at: candidate
            .created_at
            .clone()
            .unwrap_or_else(|| Utc::now().to_rfc3339()),
        display_name: None,
        parent_session_id: None,
        pending_blocked_requests: Vec::new(),
    };

    crate::services::chat::ChatService::register_imported_session(
        Arc::clone(state),
        candidate.thread_id.clone(),
        session_id.clone(),
        summary,
        Some(candidate.provider_session_id.clone()),
        history_path,
    )
    .await;

    Ok(session_id)
}

fn discover_opencode_sessions(
    known_threads: &[KnownThread],
) -> Result<Vec<ExternalSessionCandidate>, String> {
    let Some(data_dir) = dirs::data_dir() else {
        return Ok(Vec::new());
    };
    let db_path = OPENCODE_DB_RELATIVE_PATH
        .iter()
        .fold(data_dir, |path, component| path.join(component));
    if !db_path.exists() {
        return Ok(Vec::new());
    }

    let conn = Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|err| format!("failed to open {} read-only: {err}", db_path.display()))?;

    let session_columns = table_columns(&conn, "session")?;
    let Some(id_col) = choose_column(&session_columns, &["id", "sessionID", "session_id"]) else {
        return Ok(Vec::new());
    };
    let Some(directory_col) = choose_column(&session_columns, &["directory", "cwd", "path"]) else {
        return Ok(Vec::new());
    };
    let title_col = choose_column(&session_columns, &["title", "name"]);
    let created_col = choose_column(
        &session_columns,
        &[
            "timeCreated",
            "createdAt",
            "created_at",
            "time",
            "timestamp",
        ],
    );
    let mut select_cols = vec![
        format!("{} AS id", quote_ident(&id_col)),
        format!("{} AS directory", quote_ident(&directory_col)),
    ];
    if let Some(title_col) = &title_col {
        select_cols.push(format!("{} AS title", quote_ident(title_col)));
    }
    if let Some(created_col) = &created_col {
        select_cols.push(format!("{} AS created_at", quote_ident(created_col)));
    }
    let order = created_col
        .as_ref()
        .map(|column| format!(" ORDER BY {}", quote_ident(column)))
        .unwrap_or_default();
    let sql = format!("SELECT {} FROM session{}", select_cols.join(", "), order);

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|err| format!("failed to prepare OpenCode session query: {err}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                cell_to_string(row.get_ref(0)?),
                cell_to_string(row.get_ref(1)?),
                if title_col.is_some() {
                    cell_to_string(row.get_ref(2)?)
                } else {
                    None
                },
                if created_col.is_some() {
                    let idx = if title_col.is_some() { 3 } else { 2 };
                    cell_to_string(row.get_ref(idx)?)
                } else {
                    None
                },
            ))
        })
        .map_err(|err| format!("failed to query OpenCode sessions: {err}"))?;

    let worktree_to_thread = known_threads
        .iter()
        .map(|thread| (thread.worktree_path.as_str(), thread.thread_id.as_str()))
        .collect::<HashMap<_, _>>();
    let mut candidates = Vec::new();
    for row in rows {
        let (Some(session_id), Some(directory), title, created_at) =
            row.map_err(|err| format!("failed to read OpenCode session row: {err}"))?
        else {
            continue;
        };
        let Some(thread_id) = worktree_to_thread.get(directory.as_str()) else {
            continue;
        };
        let mut history = vec![import_marker_update(
            &session_id,
            ExternalProvider::Opencode,
        )];
        history.extend(opencode_history_updates(&conn, &session_id)?);
        candidates.push(ExternalSessionCandidate {
            provider: ExternalProvider::Opencode,
            provider_session_id: session_id,
            thread_id: (*thread_id).to_string(),
            history_updates: history,
            created_at,
            title,
        });
    }
    Ok(candidates)
}

fn opencode_history_updates(conn: &Connection, session_id: &str) -> Result<Vec<Value>, String> {
    if !table_exists(conn, "message")? {
        return Ok(Vec::new());
    }
    let message_columns = table_columns(conn, "message")?;
    let Some(message_id_col) = choose_column(&message_columns, &["id", "messageID", "message_id"])
    else {
        return Ok(Vec::new());
    };
    let Some(session_col) =
        choose_column(&message_columns, &["sessionID", "session_id", "session"])
    else {
        return Ok(Vec::new());
    };
    let role_col = choose_column(&message_columns, &["role", "author"]);
    let message_text_col = choose_column(&message_columns, &["content", "text", "summary"]);
    let created_col = choose_column(
        &message_columns,
        &[
            "timeCreated",
            "createdAt",
            "created_at",
            "time",
            "timestamp",
        ],
    );
    let order = created_col
        .as_ref()
        .map(|column| format!(" ORDER BY {}", quote_ident(column)))
        .unwrap_or_default();
    let sql = format!(
        "SELECT {}, {}, {}, {} FROM message WHERE {} = ?{}",
        quote_ident(&message_id_col),
        role_col
            .as_ref()
            .map(|column| quote_ident(column))
            .unwrap_or_else(|| "NULL".to_string()),
        message_text_col
            .as_ref()
            .map(|column| quote_ident(column))
            .unwrap_or_else(|| "NULL".to_string()),
        created_col
            .as_ref()
            .map(|column| quote_ident(column))
            .unwrap_or_else(|| "NULL".to_string()),
        quote_ident(&session_col),
        order
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|err| format!("failed to prepare OpenCode message query: {err}"))?;
    let rows = stmt
        .query_map([session_id], |row| {
            Ok((
                cell_to_string(row.get_ref(0)?),
                cell_to_string(row.get_ref(1)?),
                cell_to_string(row.get_ref(2)?),
                cell_to_string(row.get_ref(3)?),
            ))
        })
        .map_err(|err| format!("failed to query OpenCode messages: {err}"))?;

    let mut updates = Vec::new();
    for row in rows {
        let (Some(message_id), role, message_text, _) =
            row.map_err(|err| format!("failed to read OpenCode message row: {err}"))?
        else {
            continue;
        };
        let before = updates.len();
        updates.extend(opencode_part_updates(
            conn,
            session_id,
            &message_id,
            role.as_deref(),
        )?);
        if updates.len() == before {
            if let Some(text) = message_text.filter(|text| !text.trim().is_empty()) {
                updates.push(message_text_update(
                    session_id,
                    role_to_message_kind(role.as_deref()),
                    &text,
                ));
            }
        }
    }
    Ok(updates)
}

fn opencode_part_updates(
    conn: &Connection,
    session_id: &str,
    message_id: &str,
    role: Option<&str>,
) -> Result<Vec<Value>, String> {
    if !table_exists(conn, "part")? {
        return Ok(Vec::new());
    }
    let columns = table_columns(conn, "part")?;
    let Some(message_col) = choose_column(&columns, &["messageID", "message_id", "message"]) else {
        return Ok(Vec::new());
    };
    let type_col = choose_column(&columns, &["type", "kind"]);
    let text_col = choose_column(&columns, &["text", "content", "summary"]);
    let tool_col = choose_column(&columns, &["tool", "name"]);
    let call_id_col = choose_column(
        &columns,
        &["callID", "call_id", "toolCallId", "tool_call_id", "id"],
    );
    let input_col = choose_column(&columns, &["input", "rawInput", "raw_input"]);
    let output_col = choose_column(&columns, &["output", "content", "text"]);
    let status_col = choose_column(&columns, &["status", "state"]);
    let created_col = choose_column(
        &columns,
        &[
            "timeCreated",
            "createdAt",
            "created_at",
            "time",
            "timestamp",
        ],
    );
    let order = created_col
        .as_ref()
        .map(|column| format!(" ORDER BY {}", quote_ident(column)))
        .unwrap_or_default();
    let sql = format!(
        "SELECT {}, {}, {}, {}, {}, {}, {} FROM part WHERE {} = ?{}",
        type_col
            .as_ref()
            .map(|column| quote_ident(column))
            .unwrap_or_else(|| "NULL".to_string()),
        text_col
            .as_ref()
            .map(|column| quote_ident(column))
            .unwrap_or_else(|| "NULL".to_string()),
        tool_col
            .as_ref()
            .map(|column| quote_ident(column))
            .unwrap_or_else(|| "NULL".to_string()),
        call_id_col
            .as_ref()
            .map(|column| quote_ident(column))
            .unwrap_or_else(|| "NULL".to_string()),
        input_col
            .as_ref()
            .map(|column| quote_ident(column))
            .unwrap_or_else(|| "NULL".to_string()),
        output_col
            .as_ref()
            .map(|column| quote_ident(column))
            .unwrap_or_else(|| "NULL".to_string()),
        status_col
            .as_ref()
            .map(|column| quote_ident(column))
            .unwrap_or_else(|| "NULL".to_string()),
        quote_ident(&message_col),
        order,
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|err| format!("failed to prepare OpenCode part query: {err}"))?;
    let rows = stmt
        .query_map([message_id], |row| {
            Ok((
                cell_to_string(row.get_ref(0)?),
                cell_to_string(row.get_ref(1)?),
                cell_to_string(row.get_ref(2)?),
                cell_to_string(row.get_ref(3)?),
                cell_to_json(row.get_ref(4)?),
                cell_to_string(row.get_ref(5)?),
                cell_to_string(row.get_ref(6)?),
            ))
        })
        .map_err(|err| format!("failed to query OpenCode parts: {err}"))?;
    let mut updates = Vec::new();
    for row in rows {
        let (kind, text, tool_name, tool_call_id, raw_input, output, status) =
            row.map_err(|err| format!("failed to read OpenCode part row: {err}"))?;
        let kind_lower = kind.as_deref().unwrap_or_default().to_ascii_lowercase();
        if kind_lower.contains("reason") || kind_lower.contains("thought") {
            if let Some(text) = text.filter(|text| !text.trim().is_empty()) {
                updates.push(message_text_update(
                    session_id,
                    "agent_thought_chunk",
                    &text,
                ));
            }
            continue;
        }
        if kind_lower.contains("tool") {
            let call_id = tool_call_id.unwrap_or_else(|| format!("tool-{message_id}"));
            let name = tool_name.unwrap_or_else(|| "tool".to_string());
            let raw_input = raw_input.unwrap_or(Value::Null);
            updates.push(tool_call_update(session_id, &call_id, &name, raw_input));
            if output
                .as_ref()
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false)
                || status
                    .as_deref()
                    .map(is_terminal_tool_status)
                    .unwrap_or(false)
            {
                updates.push(tool_result_update(
                    session_id,
                    &call_id,
                    &name,
                    status.as_deref().unwrap_or("completed"),
                    output.as_deref(),
                ));
            }
            continue;
        }
        if let Some(text) = text.filter(|text| !text.trim().is_empty()) {
            updates.push(message_text_update(
                session_id,
                role_to_message_kind(role),
                &text,
            ));
        }
    }
    Ok(updates)
}

fn discover_claude_sessions(
    known_threads: &[KnownThread],
) -> Result<Vec<ExternalSessionCandidate>, String> {
    let Some(home_dir) = dirs::home_dir() else {
        return Ok(Vec::new());
    };
    let projects_root = CLAUDE_PROJECTS_RELATIVE_PATH
        .iter()
        .fold(home_dir.join(".claude"), |path, component| {
            path.join(component)
        });
    if !projects_root.exists() {
        return Ok(Vec::new());
    }
    let mut candidates = Vec::new();
    for thread in known_threads {
        let project_dir = projects_root.join(claude_encoded_cwd(&thread.worktree_path));
        if !project_dir.exists() {
            continue;
        }
        let entries = fs::read_dir(&project_dir)
            .map_err(|err| format!("failed to read {}: {err}", project_dir.display()))?;
        for entry in entries {
            let entry =
                entry.map_err(|err| format!("failed to read Claude project entry: {err}"))?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(session_id) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            let mut history = vec![import_marker_update(&session_id, ExternalProvider::Claude)];
            let (mut converted, created_at) =
                claude_history_updates(&path, &thread.worktree_path, &session_id)?;
            history.append(&mut converted);
            candidates.push(ExternalSessionCandidate {
                provider: ExternalProvider::Claude,
                provider_session_id: session_id,
                thread_id: thread.thread_id.clone(),
                history_updates: history,
                created_at,
                title: None,
            });
        }
    }
    Ok(candidates)
}

fn claude_history_updates(
    path: &Path,
    expected_cwd: &str,
    fallback_session_id: &str,
) -> Result<(Vec<Value>, Option<String>), String> {
    let file =
        fs::File::open(path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    let reader = BufReader::new(file);
    let mut updates = Vec::new();
    let mut malformed = 0_usize;
    let mut created_at = None;
    for line in reader.lines() {
        let line = line.map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            malformed += 1;
            continue;
        };
        if !claude_record_visible_for_cwd(&value, expected_cwd) {
            continue;
        }
        if created_at.is_none() {
            created_at = value
                .get("timestamp")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
        }
        let session_id = value
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or(fallback_session_id);
        updates.extend(convert_claude_record(session_id, &value));
    }
    if malformed > 0 {
        warn!(path = %path.display(), malformed, "skipped malformed Claude JSONL records");
    }
    Ok((updates, created_at))
}

fn claude_record_visible_for_cwd(value: &Value, expected_cwd: &str) -> bool {
    if value
        .get("isMeta")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return false;
    }
    match value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "user" | "assistant" => {}
        _ => return false,
    }
    value
        .get("cwd")
        .and_then(Value::as_str)
        .map(|cwd| cwd == expected_cwd)
        .unwrap_or(true)
}

fn convert_claude_record(session_id: &str, value: &Value) -> Vec<Value> {
    let role = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let Some(message) = value.get("message") else {
        return Vec::new();
    };
    let content = message.get("content").unwrap_or(message);
    let mut updates = Vec::new();
    match content {
        Value::String(text) if role == "user" => {
            updates.push(message_text_update(session_id, "user_message_chunk", text));
        }
        Value::String(text) if role == "assistant" => {
            updates.push(message_text_update(session_id, "agent_message_chunk", text));
        }
        Value::Array(blocks) => {
            for block in blocks {
                let block_type = block
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match (role, block_type) {
                    ("user", "text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            updates.push(message_text_update(
                                session_id,
                                "user_message_chunk",
                                text,
                            ));
                        }
                    }
                    ("user", "tool_result") => {
                        let call_id = block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or("tool-result");
                        let text = extract_text_from_value(block.get("content"));
                        updates.push(tool_result_update(
                            session_id,
                            call_id,
                            "tool",
                            if block
                                .get("is_error")
                                .and_then(Value::as_bool)
                                .unwrap_or(false)
                            {
                                "failed"
                            } else {
                                "completed"
                            },
                            text.as_deref(),
                        ));
                    }
                    ("assistant", "text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            updates.push(message_text_update(
                                session_id,
                                "agent_message_chunk",
                                text,
                            ));
                        }
                    }
                    ("assistant", "thinking") => {
                        if let Some(text) = block
                            .get("thinking")
                            .and_then(Value::as_str)
                            .or_else(|| block.get("text").and_then(Value::as_str))
                        {
                            updates.push(message_text_update(
                                session_id,
                                "agent_thought_chunk",
                                text,
                            ));
                        }
                    }
                    ("assistant", "tool_use") => {
                        let call_id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("tool-call");
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let input = block.get("input").cloned().unwrap_or(Value::Null);
                        updates.push(tool_call_update(session_id, call_id, name, input));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    updates
}

pub(crate) fn claude_encoded_cwd(path: &str) -> String {
    path.chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

fn import_marker_update(session_id: &str, provider: ExternalProvider) -> Value {
    message_text_update(session_id, "user_message_chunk", provider.import_marker())
}

fn message_text_update(session_id: &str, kind: &str, text: &str) -> Value {
    let mut update = json!({
        "content": { "type": "text", "text": text },
    });
    if kind == "user_message_chunk" {
        update["sessionUpdate"] = Value::String(kind.to_string());
    } else {
        update["kind"] = Value::String(kind.to_string());
    }
    json!({ "sessionId": session_id, "update": update })
}

fn tool_call_update(session_id: &str, tool_call_id: &str, name: &str, input: Value) -> Value {
    json!({
        "sessionId": session_id,
        "update": {
            "kind": "tool_call",
            "toolCallId": tool_call_id,
            "status": "pending",
            "title": name,
            "rawInput": input,
            "toolCall": {
                "id": tool_call_id,
                "name": name,
                "status": "pending",
                "state": { "title": name }
            }
        }
    })
}

fn tool_result_update(
    session_id: &str,
    tool_call_id: &str,
    name: &str,
    status: &str,
    output: Option<&str>,
) -> Value {
    json!({
        "sessionId": session_id,
        "update": {
            "kind": "tool_call_update",
            "toolCallId": tool_call_id,
            "status": normalize_tool_status(status),
            "title": name,
            "content": output.map(|text| json!([{ "type": "text", "text": text }])).unwrap_or(Value::Array(Vec::new()))
        }
    })
}

fn role_to_message_kind(role: Option<&str>) -> &'static str {
    match role.unwrap_or_default().to_ascii_lowercase().as_str() {
        "user" => "user_message_chunk",
        _ => "agent_message_chunk",
    }
}

fn normalize_tool_status(status: &str) -> &'static str {
    match status {
        "error" | "failed" => "failed",
        "cancelled" | "canceled" => "cancelled",
        _ => "completed",
    }
}

fn is_terminal_tool_status(status: &str) -> bool {
    matches!(
        normalize_tool_status(status),
        "completed" | "failed" | "cancelled"
    )
}

fn extract_text_from_value(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => {
            let texts = items
                .iter()
                .filter_map(|item| {
                    item.get("text")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                        .or_else(|| item.as_str().map(ToOwned::to_owned))
                })
                .collect::<Vec<_>>();
            if texts.is_empty() {
                None
            } else {
                Some(texts.join("\n"))
            }
        }
        other => serde_json::to_string(other).ok(),
    }
}

fn registry_key(provider: ExternalProvider, provider_session_id: &str) -> String {
    format!("{}:{}", provider.registry_name(), provider_session_id)
}

fn load_registry(path: &Path) -> Result<AdoptionRegistry, String> {
    if !path.exists() {
        return Ok(AdoptionRegistry::default());
    }
    let raw = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(AdoptionRegistry::default());
    }
    serde_json::from_str(&raw).map_err(|err| format!("failed to parse {}: {err}", path.display()))
}

fn save_registry(path: &Path, registry: &AdoptionRegistry) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("invalid registry path: {}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec_pretty(registry)
        .map_err(|err| format!("failed to encode registry: {err}"))?;
    fs::write(&tmp, data).map_err(|err| format!("failed to write {}: {err}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|err| {
        format!(
            "failed to move {} to {}: {err}",
            tmp.display(),
            path.display()
        )
    })
}

fn write_history(path: &Path, updates: &[Value]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("invalid chat history path: {}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|err| format!("failed to create {}: {err}", path.display()))?;
    for update in updates {
        serde_json::to_writer(&mut file, update)
            .map_err(|err| format!("failed to encode history update: {err}"))?;
        file.write_all(b"\n")
            .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    }
    file.sync_all()
        .map_err(|err| format!("failed to fsync {}: {err}", path.display()))
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool, String> {
    let mut stmt = conn
        .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ? LIMIT 1")
        .map_err(|err| format!("failed to prepare table existence query: {err}"))?;
    let mut rows = stmt
        .query([table])
        .map_err(|err| format!("failed to query sqlite_master: {err}"))?;
    rows.next()
        .map(|row| row.is_some())
        .map_err(|err| format!("failed to read sqlite_master row: {err}"))
}

fn table_columns(conn: &Connection, table: &str) -> Result<HashSet<String>, String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({})", quote_ident(table)))
        .map_err(|err| format!("failed to prepare table_info({table}): {err}"))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|err| format!("failed to query table_info({table}): {err}"))?;
    let mut columns = HashSet::new();
    for row in rows {
        columns
            .insert(row.map_err(|err| format!("failed to read table_info({table}) row: {err}"))?);
    }
    Ok(columns)
}

fn choose_column(columns: &HashSet<String>, choices: &[&str]) -> Option<String> {
    choices
        .iter()
        .find(|choice| columns.contains(**choice))
        .map(|choice| (*choice).to_string())
}

fn quote_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn cell_to_string(value: ValueRef<'_>) -> Option<String> {
    match value {
        ValueRef::Null => None,
        ValueRef::Integer(value) => Some(value.to_string()),
        ValueRef::Real(value) => Some(value.to_string()),
        ValueRef::Text(value) => Some(String::from_utf8_lossy(value).to_string()),
        ValueRef::Blob(value) => Some(String::from_utf8_lossy(value).to_string()),
    }
}

fn cell_to_json(value: ValueRef<'_>) -> Option<Value> {
    let text = cell_to_string(value)?;
    serde_json::from_str(&text)
        .ok()
        .or(Some(Value::String(text)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_store::{AppData, Project, StateStore, Thread};

    #[test]
    fn claude_cwd_encoder_matches_project_directory_shape() {
        assert_eq!(
            claude_encoded_cwd("/home/wsl/dev/.threadmill/repo/feature-x"),
            "-home-wsl-dev--threadmill-repo-feature-x"
        );
    }

    #[test]
    fn known_thread_checkouts_skip_deleted_worktrees() {
        let root = std::env::temp_dir().join(format!(
            "spindle-known-checkouts-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let existing = root.join("existing");
        fs::create_dir_all(&existing).expect("create existing checkout");
        let threads = vec![
            Thread::new(
                "thread-existing".to_string(),
                "project".to_string(),
                "existing".to_string(),
                "main".to_string(),
                Some(existing.to_string_lossy().to_string()),
                protocol::ThreadStatus::Active,
                protocol::SourceType::ExistingBranch,
                Utc::now(),
                "tm_existing".to_string(),
                0,
            ),
            Thread::new(
                "thread-deleted".to_string(),
                "project".to_string(),
                "deleted".to_string(),
                "main".to_string(),
                Some(root.join("deleted").to_string_lossy().to_string()),
                protocol::ThreadStatus::Closed,
                protocol::SourceType::ExistingBranch,
                Utc::now(),
                "tm_deleted".to_string(),
                0,
            ),
        ];

        let known = known_thread_checkouts(&threads);

        assert_eq!(known.len(), 1);
        assert_eq!(known[0].thread_id, "thread-existing");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn claude_import_preserves_visible_messages_and_tools() {
        let root = std::env::temp_dir().join(format!(
            "spindle-claude-import-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&root).expect("create temp dir");
        let path = root.join("claude-session.jsonl");
        fs::write(
            &path,
            concat!(
                r#"{"type":"system","cwd":"/repo"}"#,
                "\n",
                r#"{"type":"user","cwd":"/repo","sessionId":"claude-1","timestamp":"2026-04-01T00:00:00Z","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}"#,
                "\n",
                r#"{"type":"assistant","cwd":"/repo","sessionId":"claude-1","message":{"role":"assistant","content":[{"type":"thinking","thinking":"plan"},{"type":"text","text":"hi"},{"type":"tool_use","id":"tool-1","name":"Read","input":{"file":"a"}}]}}"#,
                "\n",
                r#"{"type":"user","cwd":"/repo","sessionId":"claude-1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool-1","content":"contents"}]}}"#,
                "\n",
                "not json\n"
            ),
        )
        .expect("write fixture");

        let (updates, created_at) =
            claude_history_updates(&path, "/repo", "claude-1").expect("convert claude history");

        assert_eq!(created_at.as_deref(), Some("2026-04-01T00:00:00Z"));
        assert_eq!(updates.len(), 5);
        assert_eq!(updates[0]["update"]["sessionUpdate"], "user_message_chunk");
        assert_eq!(updates[1]["update"]["kind"], "agent_thought_chunk");
        assert_eq!(updates[2]["update"]["kind"], "agent_message_chunk");
        assert_eq!(updates[3]["update"]["kind"], "tool_call");
        assert_eq!(updates[4]["update"]["kind"], "tool_call_update");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn opencode_import_reads_matching_directory_and_visible_parts() {
        let root = std::env::temp_dir().join(format!(
            "spindle-opencode-import-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&root).expect("create temp dir");
        let db_path = root.join("opencode.db");
        let conn = Connection::open(&db_path).expect("open fixture db");
        conn.execute_batch(
            r#"
            CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT, timeCreated TEXT);
            CREATE TABLE message (id TEXT PRIMARY KEY, sessionID TEXT, role TEXT, timeCreated TEXT);
            CREATE TABLE part (id TEXT PRIMARY KEY, messageID TEXT, type TEXT, text TEXT, tool TEXT, input TEXT, output TEXT, status TEXT, timeCreated TEXT);
            INSERT INTO session VALUES ('ses-1', '/repo', 'Imported chat', '2026-04-01T00:00:00Z');
            INSERT INTO session VALUES ('ses-2', '/elsewhere', 'Ignored chat', '2026-04-01T00:00:00Z');
            INSERT INTO message VALUES ('msg-1', 'ses-1', 'user', '1');
            INSERT INTO part VALUES ('part-1', 'msg-1', 'text', 'hello', NULL, NULL, NULL, NULL, '1');
            INSERT INTO message VALUES ('msg-2', 'ses-1', 'assistant', '2');
            INSERT INTO part VALUES ('part-2', 'msg-2', 'reasoning', 'thinking', NULL, NULL, NULL, NULL, '2');
            INSERT INTO part VALUES ('part-3', 'msg-2', 'text', 'hi', NULL, NULL, NULL, NULL, '3');
            INSERT INTO part VALUES ('part-4', 'msg-2', 'tool', NULL, 'Read', '{"file":"a"}', 'contents', 'completed', '4');
            "#,
        )
        .expect("seed fixture db");
        let updates = opencode_history_updates(&conn, "ses-1").expect("convert opencode history");

        assert_eq!(updates.len(), 5);
        assert_eq!(updates[0]["update"]["sessionUpdate"], "user_message_chunk");
        assert_eq!(updates[1]["update"]["kind"], "agent_thought_chunk");
        assert_eq!(updates[2]["update"]["kind"], "agent_message_chunk");
        assert_eq!(updates[3]["update"]["kind"], "tool_call");
        assert_eq!(updates[4]["update"]["kind"], "tool_call_update");

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn adoption_registers_ended_session_and_deduplicates_registry_key() {
        let temp_root = std::env::temp_dir().join(format!(
            "spindle-external-adoption-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let state_dir = temp_root.join("threadmill");
        fs::create_dir_all(&state_dir).expect("create state dir");
        let thread_id = "thread-1".to_string();
        let state = Arc::new(AppState::new(StateStore {
            path: state_dir.join("threads.json"),
            data: AppData {
                projects: vec![Project {
                    id: "project-1".to_string(),
                    name: "project".to_string(),
                    path: "/repo".to_string(),
                    default_branch: "main".to_string(),
                }],
                threads: vec![Thread {
                    id: thread_id.clone(),
                    project_id: "project-1".to_string(),
                    name: "thread".to_string(),
                    branch: "main".to_string(),
                    worktree_path: Some("/repo".to_string()),
                    status: protocol::ThreadStatus::Active,
                    source_type: protocol::SourceType::ExistingBranch,
                    created_at: Utc::now(),
                    tmux_session: "tm_test".to_string(),
                    port_offset: 0,
                    display_name: None,
                }],
            },
        }));
        let history_root = state_dir.join("chat");
        let candidate = ExternalSessionCandidate {
            provider: ExternalProvider::Opencode,
            provider_session_id: "ses-1".to_string(),
            thread_id: thread_id.clone(),
            history_updates: vec![import_marker_update("ses-1", ExternalProvider::Opencode)],
            created_at: Some("2026-04-01T00:00:00Z".to_string()),
            title: Some("Imported chat".to_string()),
        };

        let session_id = adopt_external_session(&state, &history_root, &candidate)
            .await
            .expect("adopt external session");

        let sessions = crate::services::chat::ChatService::list(
            Arc::clone(&state),
            protocol::ChatListParams { thread_id },
        )
        .await
        .expect("list chat sessions");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, session_id);
        assert_eq!(sessions[0].status, protocol::ChatSessionStatus::Ended);
        assert_eq!(sessions[0].agent_type, "opencode");
        assert_eq!(
            registry_key(ExternalProvider::Opencode, "ses-1"),
            "opencode:ses-1"
        );

        let _ = fs::remove_dir_all(temp_root);
    }

    #[tokio::test]
    async fn scanner_restores_registered_adoptions_after_daemon_restart() {
        let temp_root = std::env::temp_dir().join(format!(
            "spindle-external-restore-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let state_dir = temp_root.join("threadmill");
        let worktree = temp_root.join("repo");
        fs::create_dir_all(&state_dir).expect("create state dir");
        fs::create_dir_all(&worktree).expect("create worktree");
        let thread_id = "thread-restore".to_string();
        let session_id = "session-restore".to_string();
        let state = Arc::new(AppState::new(StateStore {
            path: state_dir.join("threads.json"),
            data: AppData {
                projects: vec![Project {
                    id: "project-1".to_string(),
                    name: "project".to_string(),
                    path: worktree.to_string_lossy().to_string(),
                    default_branch: "main".to_string(),
                }],
                threads: vec![Thread {
                    id: thread_id.clone(),
                    project_id: "project-1".to_string(),
                    name: "thread".to_string(),
                    branch: "main".to_string(),
                    worktree_path: Some(worktree.to_string_lossy().to_string()),
                    status: protocol::ThreadStatus::Active,
                    source_type: protocol::SourceType::ExistingBranch,
                    created_at: Utc::now(),
                    tmux_session: "tm_restore".to_string(),
                    port_offset: 0,
                    display_name: None,
                }],
            },
        }));
        let history_root = state_dir.join("chat");
        let history_path =
            crate::services::chat::history_path_for_session(&history_root, &thread_id, &session_id);
        write_history(
            &history_path,
            &[import_marker_update(
                "provider-session",
                ExternalProvider::Opencode,
            )],
        )
        .expect("write imported history");
        save_registry(
            &state_dir.join("external-sessions.json"),
            &AdoptionRegistry {
                entries: HashMap::from([(
                    registry_key(ExternalProvider::Opencode, "provider-session"),
                    RegistryEntry {
                        thread_id: thread_id.clone(),
                        session_id: session_id.clone(),
                        adopted_at: "2026-04-28T00:00:00Z".to_string(),
                    },
                )]),
            },
        )
        .expect("write registry");

        crate::services::chat::ChatService::recover_persisted_sessions(Arc::clone(&state))
            .await
            .expect("recover persisted sessions");
        let recovered = crate::services::chat::ChatService::list(
            Arc::clone(&state),
            protocol::ChatListParams {
                thread_id: thread_id.clone(),
            },
        )
        .await
        .expect("list recovered sessions");
        assert_eq!(recovered[0].agent_type, "unknown");

        scan_external_sessions(Arc::clone(&state))
            .await
            .expect("scan external sessions");

        let sessions = crate::services::chat::ChatService::list(
            Arc::clone(&state),
            protocol::ChatListParams { thread_id },
        )
        .await
        .expect("list restored sessions");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, session_id);
        assert_eq!(sessions[0].agent_type, "opencode");
        assert_eq!(sessions[0].status, protocol::ChatSessionStatus::Ended);

        let _ = fs::remove_dir_all(temp_root);
    }
}
