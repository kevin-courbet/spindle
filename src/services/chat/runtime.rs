use std::{
    collections::{HashSet, VecDeque},
    path::PathBuf,
    sync::Arc,
};

use chrono::Utc;
use serde_json::Value;
use tokio::{
    sync::{mpsc, oneshot, Notify},
    task::JoinHandle,
};

use crate::protocol;

pub(super) struct ChatSessionRuntime {
    pub(super) summary: protocol::ChatSessionSummary,
    pub(super) thread_id: String,
    pub(super) display_name: Option<String>,
    pub(super) parent_session_id: Option<String>,
    pub(super) agent_command: Option<String>,
    pub(super) system_prompt: Option<String>,
    pub(super) initial_prompt: Option<String>,
    pub(super) conversation_context: Option<String>,
    pub(super) acp_session_id: Option<String>,
    pub(super) attached_channels: HashSet<u16>,
    pub(super) input_tx: Option<mpsc::Sender<Vec<u8>>>,
    pub(super) stop_tx: Option<oneshot::Sender<()>>,
    pub(super) status_notify: Arc<Notify>,
    pub(super) ended_emitted: bool,
    pub(super) history_path: PathBuf,
    pub(super) input_buffer: Vec<u8>,
    pub(super) output_buffer: Vec<u8>,
    pub(super) active_tools: HashSet<String>,
    pub(super) total_tool_count: usize,
    pub(super) latest_tool_name: Option<String>,
    pub(super) latest_tool_title: Option<String>,
    pub(super) started_at: Option<chrono::DateTime<Utc>>,
    pub(super) pending_prompt_ids: HashSet<String>,
    pub(super) checkpoint_seq: u64,
    pub(super) last_update_time: Option<chrono::DateTime<Utc>>,
    pub(super) stall_generation: u64,
    pub(super) stall_task: Option<JoinHandle<()>>,
    pub(super) modes: Option<Value>,
    pub(super) models: Option<Value>,
    pub(super) config_options: Option<Value>,
    /// Tracks the injection prompt request ID so we can detect when the injection turn completes.
    /// Set when injection is sent, cleared when the response arrives in apply_outbound_status_updates.
    pub(super) injection_prompt_id: Option<String>,
    /// Number of user-originated prompts routed through this session.
    pub(super) user_prompt_count: u32,
    /// Text of the first user prompt, captured for title generation.
    pub(super) first_prompt_text: Option<String>,
    /// True if session was created with conversation_context (revert/fork). Suppresses title gen.
    pub(super) had_conversation_context: bool,
    /// User prompts persisted by Spindle; matching agent echoes are suppressed to avoid duplicate JSONL.
    pub(super) pending_user_echoes: VecDeque<String>,
}

impl ChatSessionRuntime {
    pub(super) fn new(
        summary: protocol::ChatSessionSummary,
        thread_id: String,
        history_path: PathBuf,
    ) -> Self {
        Self {
            summary,
            thread_id,
            display_name: None,
            parent_session_id: None,
            agent_command: None,
            system_prompt: None,
            initial_prompt: None,
            conversation_context: None,
            acp_session_id: None,
            attached_channels: HashSet::new(),
            input_tx: None,
            stop_tx: None,
            status_notify: Arc::new(Notify::new()),
            ended_emitted: false,
            history_path,
            input_buffer: Vec::new(),
            output_buffer: Vec::new(),
            active_tools: HashSet::new(),
            total_tool_count: 0,
            latest_tool_name: None,
            latest_tool_title: None,
            started_at: None,
            pending_prompt_ids: HashSet::new(),
            checkpoint_seq: 0,
            last_update_time: None,
            stall_generation: 0,
            stall_task: None,
            modes: None,
            models: None,
            config_options: None,
            injection_prompt_id: None,
            user_prompt_count: 0,
            first_prompt_text: None,
            had_conversation_context: false,
            pending_user_echoes: VecDeque::new(),
        }
    }
}
