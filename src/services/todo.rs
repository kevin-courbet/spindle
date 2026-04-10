use std::{collections::HashSet, fs, path::PathBuf, sync::Arc};

use chrono::Utc;
use tokio::task;
use uuid::Uuid;

use crate::{protocol, AppState};

pub struct TodoService;

impl TodoService {
    pub async fn list(
        state: Arc<AppState>,
        params: protocol::TodoListParams,
    ) -> Result<protocol::TodoListResult, String> {
        ensure_thread_exists(&state, &params.thread_id).await?;
        let path = todo_path(&state, &params.thread_id).await?;
        let filter = params.filter;
        run_blocking("todo.list", move || {
            let todos = load_todos(&path)?;
            Ok(todos
                .into_iter()
                .filter(|todo| match filter {
                    protocol::TodoFilter::All => true,
                    protocol::TodoFilter::Active => !todo.completed,
                    protocol::TodoFilter::Completed => todo.completed,
                })
                .collect())
        })
        .await
    }

    pub async fn add(
        state: Arc<AppState>,
        params: protocol::TodoAddParams,
    ) -> Result<protocol::TodoAddResult, String> {
        ensure_thread_exists(&state, &params.thread_id).await?;
        let path = todo_path(&state, &params.thread_id).await?;
        let thread_id = params.thread_id.clone();
        let content = validate_content(&params.content)?;
        let priority = params.priority;
        let added = run_blocking("todo.add", move || {
            let mut todos = load_todos(&path)?;
            let todo = protocol::TodoItem {
                id: Uuid::new_v4().to_string(),
                thread_id,
                content,
                completed: false,
                priority,
                created_at: Utc::now().to_rfc3339(),
                completed_at: None,
            };
            todos.push(todo.clone());
            save_todos(&path, &todos)?;
            Ok(todo)
        })
        .await?;

        state.emit_event(
            "todo.added",
            protocol::TodoAddedEvent {
                thread_id: params.thread_id,
                todo: added.clone(),
            },
        );

        Ok(added)
    }

    pub async fn update(
        state: Arc<AppState>,
        params: protocol::TodoUpdateParams,
    ) -> Result<protocol::TodoUpdateResult, String> {
        ensure_thread_exists(&state, &params.thread_id).await?;
        let path = todo_path(&state, &params.thread_id).await?;
        let thread_id = params.thread_id.clone();
        let updated = run_blocking("todo.update", move || {
            let mut todos = load_todos(&path)?;
            let todo = todos
                .iter_mut()
                .find(|todo| todo.id == params.todo_id)
                .ok_or_else(|| format!("todo not found: {}", params.todo_id))?;

            if let Some(content) = params.content.as_deref() {
                todo.content = validate_content(content)?;
            }
            if let Some(priority) = params.priority {
                todo.priority = Some(priority);
            }
            if let Some(completed) = params.completed {
                if completed {
                    if !todo.completed {
                        todo.completed_at = Some(Utc::now().to_rfc3339());
                    }
                } else {
                    todo.completed_at = None;
                }
                todo.completed = completed;
            }

            let updated = todo.clone();
            save_todos(&path, &todos)?;
            Ok(updated)
        })
        .await?;

        state.emit_event(
            "todo.updated",
            protocol::TodoUpdatedEvent {
                thread_id,
                todo: updated.clone(),
            },
        );

        Ok(updated)
    }

    pub async fn toggle(
        state: Arc<AppState>,
        params: protocol::TodoToggleParams,
    ) -> Result<protocol::TodoUpdateResult, String> {
        ensure_thread_exists(&state, &params.thread_id).await?;
        let path = todo_path(&state, &params.thread_id).await?;
        let thread_id = params.thread_id.clone();
        let updated = run_blocking("todo.toggle", move || {
            let mut todos = load_todos(&path)?;
            let todo = todos
                .iter_mut()
                .find(|todo| todo.id == params.todo_id)
                .ok_or_else(|| format!("todo not found: {}", params.todo_id))?;

            let completed = !todo.completed;
            if completed {
                todo.completed_at = Some(Utc::now().to_rfc3339());
            } else {
                todo.completed_at = None;
            }
            todo.completed = completed;

            let updated = todo.clone();
            save_todos(&path, &todos)?;
            Ok(updated)
        })
        .await?;

        state.emit_event(
            "todo.updated",
            protocol::TodoUpdatedEvent {
                thread_id,
                todo: updated.clone(),
            },
        );

        Ok(updated)
    }

    pub async fn remove(
        state: Arc<AppState>,
        params: protocol::TodoRemoveParams,
    ) -> Result<protocol::TodoRemoveResult, String> {
        ensure_thread_exists(&state, &params.thread_id).await?;
        let path = todo_path(&state, &params.thread_id).await?;
        let thread_id = params.thread_id.clone();
        let todo_id = params.todo_id.clone();
        let result = run_blocking("todo.remove", move || {
            let mut todos = load_todos(&path)?;
            let before = todos.len();
            todos.retain(|todo| todo.id != todo_id);
            if todos.len() == before {
                return Err(format!("todo not found: {todo_id}"));
            }
            save_todos(&path, &todos)?;
            Ok(protocol::TodoRemoveResult { success: true })
        })
        .await?;

        state.emit_event(
            "todo.removed",
            protocol::TodoRemovedEvent {
                thread_id,
                todo_id: params.todo_id,
            },
        );

        Ok(result)
    }

    pub async fn reorder(
        state: Arc<AppState>,
        params: protocol::TodoReorderParams,
    ) -> Result<protocol::TodoReorderResult, String> {
        ensure_thread_exists(&state, &params.thread_id).await?;
        let path = todo_path(&state, &params.thread_id).await?;
        let thread_id = params.thread_id.clone();
        let todo_ids = params.todo_ids.clone();
        let result = run_blocking("todo.reorder", move || {
            let todos = load_todos(&path)?;
            if todos.len() != params.todo_ids.len() {
                return Err("todo_ids must include every todo exactly once".to_string());
            }

            let mut seen = HashSet::with_capacity(params.todo_ids.len());
            for todo_id in &params.todo_ids {
                if !seen.insert(todo_id.as_str()) {
                    return Err(format!("duplicate todo id in reorder: {todo_id}"));
                }
            }

            let mut by_id = todos
                .into_iter()
                .map(|todo| (todo.id.clone(), todo))
                .collect::<std::collections::HashMap<_, _>>();
            let mut reordered = Vec::with_capacity(params.todo_ids.len());
            for todo_id in params.todo_ids {
                let todo = by_id
                    .remove(&todo_id)
                    .ok_or_else(|| format!("todo not found: {todo_id}"))?;
                reordered.push(todo);
            }
            if !by_id.is_empty() {
                return Err("todo_ids must include every todo exactly once".to_string());
            }

            save_todos(&path, &reordered)?;
            Ok(protocol::TodoReorderResult { success: true })
        })
        .await?;

        state.emit_event(
            "todo.reordered",
            protocol::TodoReorderedEvent {
                thread_id,
                todo_ids,
            },
        );

        Ok(result)
    }

    pub async fn cleanup(state: Arc<AppState>, thread_id: &str) -> Result<(), String> {
        let path = todo_path(&state, thread_id).await?;
        run_blocking("todo.cleanup", move || {
            if path.exists() {
                fs::remove_file(&path)
                    .map_err(|err| format!("failed to remove {}: {err}", path.display()))?;
            }
            Ok(())
        })
        .await
    }

    pub async fn cleanup_thread(state: Arc<AppState>, thread_id: &str) -> Result<(), String> {
        Self::cleanup(state, thread_id).await
    }
}

async fn ensure_thread_exists(state: &Arc<AppState>, thread_id: &str) -> Result<(), String> {
    let store = state.store.lock().await;
    store
        .thread_by_id(thread_id)
        .ok_or_else(|| format!("thread not found: {thread_id}"))?;
    Ok(())
}

async fn todo_path(state: &Arc<AppState>, thread_id: &str) -> Result<PathBuf, String> {
    let store = state.store.lock().await;
    let thread = store
        .thread_by_id(thread_id)
        .ok_or_else(|| format!("thread not found: {thread_id}"))?;
    Ok(PathBuf::from(&thread.worktree_path)
        .join(".threadmill")
        .join("todos.json"))
}

fn load_todos(path: &PathBuf) -> Result<Vec<protocol::TodoItem>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let raw = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }

    serde_json::from_str(&raw).map_err(|err| format!("failed to parse {}: {err}", path.display()))
}

fn save_todos(path: &PathBuf, todos: &[protocol::TodoItem]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("invalid todo path {}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;

    let serialized = serde_json::to_vec_pretty(todos)
        .map_err(|err| format!("failed to serialize todos: {err}"))?;
    fs::write(path, serialized).map_err(|err| format!("failed to write {}: {err}", path.display()))
}

fn validate_content(content: &str) -> Result<String, String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err("todo content must not be empty".to_string());
    }
    Ok(trimmed.to_string())
}

async fn run_blocking<T, F>(operation: &'static str, blocking_fn: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    task::spawn_blocking(blocking_fn)
        .await
        .map_err(|err| format!("{operation} task failed: {err}"))?
}
