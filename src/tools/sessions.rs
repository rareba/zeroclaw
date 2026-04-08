//! Session-to-session messaging tools for inter-agent communication.
//!
//! Provides four tools:
//! - `sessions_list` — list active sessions with metadata
//! - `sessions_history` — read message history from a specific session
//! - `sessions_send` — send a message to a specific session
//! - `sessions_await` — wait for one or more sessions to complete (fan-in)

use super::traits::{Tool, ToolResult};
use crate::channels::session_backend::SessionBackend;
use crate::security::SecurityPolicy;
use crate::security::policy::ToolOperation;
use async_trait::async_trait;
use serde_json::json;
use std::fmt::Write;
use std::sync::Arc;
use tokio::time::{Duration, Instant};

/// Validate that a session ID is non-empty and contains at least one
/// alphanumeric character (prevents blank keys after sanitization).
fn validate_session_id(session_id: &str) -> Result<(), ToolResult> {
    let trimmed = session_id.trim();
    if trimmed.is_empty() || !trimmed.chars().any(|c| c.is_alphanumeric()) {
        return Err(ToolResult {
            success: false,
            output: String::new(),
            error: Some(
                "Invalid 'session_id': must be non-empty and contain at least one alphanumeric character.".into(),
            ),
        });
    }
    Ok(())
}

// ── SessionsListTool ────────────────────────────────────────────────

/// Lists active sessions with their channel, last activity time, and message count.
pub struct SessionsListTool {
    backend: Arc<dyn SessionBackend>,
}

impl SessionsListTool {
    pub fn new(backend: Arc<dyn SessionBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Tool for SessionsListTool {
    fn name(&self) -> &str {
        "sessions_list"
    }

    fn description(&self) -> &str {
        "List all active conversation sessions with their channel, last activity time, and message count."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Max sessions to return (default: 50)"
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        #[allow(clippy::cast_possible_truncation)]
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(50, |v| v as usize);

        let metadata = self.backend.list_sessions_with_metadata();

        if metadata.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "No active sessions found.".into(),
                error: None,
            });
        }

        let capped: Vec<_> = metadata.into_iter().take(limit).collect();
        let mut output = format!("Found {} session(s):\n", capped.len());
        for meta in &capped {
            // Extract channel from key (convention: channel__identifier)
            let channel = meta.key.split("__").next().unwrap_or(&meta.key);
            let _ = writeln!(
                output,
                "- {}: channel={}, messages={}, last_activity={}",
                meta.key, channel, meta.message_count, meta.last_activity
            );
        }

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}

// ── SessionsHistoryTool ─────────────────────────────────────────────

/// Reads the message history of a specific session by ID.
pub struct SessionsHistoryTool {
    backend: Arc<dyn SessionBackend>,
    security: Arc<SecurityPolicy>,
}

impl SessionsHistoryTool {
    pub fn new(backend: Arc<dyn SessionBackend>, security: Arc<SecurityPolicy>) -> Self {
        Self { backend, security }
    }
}

#[async_trait]
impl Tool for SessionsHistoryTool {
    fn name(&self) -> &str {
        "sessions_history"
    }

    fn description(&self) -> &str {
        "Read the message history of a specific session by its session ID. Returns the last N messages."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "The session ID to read history from (e.g. telegram__user123)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max messages to return, from most recent (default: 20)"
                }
            },
            "required": ["session_id"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if let Err(error) = self
            .security
            .enforce_tool_operation(ToolOperation::Read, "sessions_history")
        {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            });
        }

        let session_id = args
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'session_id' parameter"))?;

        if let Err(result) = validate_session_id(session_id) {
            return Ok(result);
        }

        #[allow(clippy::cast_possible_truncation)]
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(20, |v| v as usize);

        let messages = self.backend.load(session_id);

        if messages.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: format!("No messages found for session '{session_id}'."),
                error: None,
            });
        }

        // Take the last `limit` messages
        let start = messages.len().saturating_sub(limit);
        let tail = &messages[start..];

        let mut output = format!(
            "Session '{}': showing {}/{} messages\n",
            session_id,
            tail.len(),
            messages.len()
        );
        for msg in tail {
            let _ = writeln!(output, "[{}] {}", msg.role, msg.content);
        }

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}

// ── SessionsSendTool ────────────────────────────────────────────────

/// Sends a message to a specific session, enabling inter-agent communication.
pub struct SessionsSendTool {
    backend: Arc<dyn SessionBackend>,
    security: Arc<SecurityPolicy>,
}

impl SessionsSendTool {
    pub fn new(backend: Arc<dyn SessionBackend>, security: Arc<SecurityPolicy>) -> Self {
        Self { backend, security }
    }
}

#[async_trait]
impl Tool for SessionsSendTool {
    fn name(&self) -> &str {
        "sessions_send"
    }

    fn description(&self) -> &str {
        "Send a message to a specific session by its session ID. The message is appended to the session's conversation history as a 'user' message, enabling inter-agent communication."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "The target session ID (e.g. telegram__user123)"
                },
                "message": {
                    "type": "string",
                    "description": "The message content to send"
                }
            },
            "required": ["session_id", "message"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if let Err(error) = self
            .security
            .enforce_tool_operation(ToolOperation::Act, "sessions_send")
        {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            });
        }

        let session_id = args
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'session_id' parameter"))?;

        if let Err(result) = validate_session_id(session_id) {
            return Ok(result);
        }

        let message = args
            .get("message")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'message' parameter"))?;

        if message.trim().is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Message content must not be empty.".into()),
            });
        }

        let chat_msg = crate::providers::traits::ChatMessage::user(message);

        match self.backend.append(session_id, &chat_msg) {
            Ok(()) => Ok(ToolResult {
                success: true,
                output: format!("Message sent to session '{session_id}'."),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to send message: {e}")),
            }),
        }
    }
}

// ── SessionsAwaitTool ──────────────────────────────────────────────

/// Completion markers that signal a session has finished its work.
const COMPLETION_MARKERS: &[&str] = &["[DONE]", "[COMPLETE]"];

/// Default idle-timeout in seconds: a session with no new messages for this
/// long is considered complete.
const DEFAULT_IDLE_SECS: u64 = 30;

/// Waits for one or more sessions to complete, then returns their final outputs.
///
/// A session is considered "complete" when:
/// 1. Its last message contains a completion marker (`[DONE]` or `[COMPLETE]`), or
/// 2. It has been idle (no new messages) for `idle_timeout_secs` (default 30s).
///
/// Supports two modes:
/// - `"all"` (default): wait until every listed session is complete or the timeout expires.
/// - `"any"`: return as soon as the first session completes.
pub struct SessionsAwaitTool {
    backend: Arc<dyn SessionBackend>,
    security: Arc<SecurityPolicy>,
}

impl SessionsAwaitTool {
    pub fn new(backend: Arc<dyn SessionBackend>, security: Arc<SecurityPolicy>) -> Self {
        Self { backend, security }
    }
}

/// Per-session tracking state used during the polling loop.
struct SessionPollState {
    id: String,
    last_message_count: usize,
    last_change: Instant,
    completed: bool,
}

/// Check whether the last message in a session contains a completion marker.
fn has_completion_marker(backend: &dyn SessionBackend, session_id: &str) -> bool {
    let messages = backend.load(session_id);
    messages.last().is_some_and(|msg| {
        let content = msg.content.to_uppercase();
        COMPLETION_MARKERS
            .iter()
            .any(|marker| content.contains(marker))
    })
}

/// Build a human-readable summary of final messages for completed sessions.
fn format_completed_sessions(
    backend: &dyn SessionBackend,
    states: &[SessionPollState],
    timed_out: bool,
) -> String {
    let completed: Vec<_> = states.iter().filter(|s| s.completed).collect();
    let pending: Vec<_> = states.iter().filter(|s| !s.completed).collect();

    let mut output = String::new();

    if timed_out {
        let _ = writeln!(
            output,
            "Timeout reached. {}/{} session(s) completed.",
            completed.len(),
            states.len()
        );
    } else {
        let _ = writeln!(output, "{} session(s) completed.", completed.len());
    }

    for state in &completed {
        let messages = backend.load(&state.id);
        let _ = writeln!(output, "\n--- {} ---", state.id);
        // Show last 5 messages as final context
        let start = messages.len().saturating_sub(5);
        for msg in &messages[start..] {
            let _ = writeln!(output, "[{}] {}", msg.role, msg.content);
        }
    }

    if !pending.is_empty() {
        let _ = write!(output, "\nPending sessions: ");
        let ids: Vec<&str> = pending.iter().map(|s| s.id.as_str()).collect();
        let _ = writeln!(output, "{}", ids.join(", "));
    }

    output
}

#[async_trait]
impl Tool for SessionsAwaitTool {
    fn name(&self) -> &str {
        "sessions_await"
    }

    fn description(&self) -> &str {
        "Wait for one or more sessions to complete, then return their final outputs."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "session_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Session IDs to wait for"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Maximum wait time in seconds (default: 300)"
                },
                "mode": {
                    "type": "string",
                    "enum": ["all", "any"],
                    "description": "Wait mode: 'all' waits for every session (default), 'any' returns when the first completes"
                },
                "idle_timeout_secs": {
                    "type": "integer",
                    "description": "Seconds of inactivity before a session is considered complete (default: 30)"
                }
            },
            "required": ["session_ids"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        // Security check
        if let Err(error) = self
            .security
            .enforce_tool_operation(ToolOperation::Read, "sessions_await")
        {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            });
        }

        // Parse session_ids
        let session_ids: Vec<String> = args
            .get("session_ids")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow::anyhow!("Missing or invalid 'session_ids' parameter"))?
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();

        if session_ids.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("'session_ids' must contain at least one session ID.".into()),
            });
        }

        // Validate all session IDs
        for id in &session_ids {
            if let Err(result) = validate_session_id(id) {
                return Ok(result);
            }
        }

        let timeout_secs = args
            .get("timeout_secs")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(300);

        let idle_timeout_secs = args
            .get("idle_timeout_secs")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(DEFAULT_IDLE_SECS);

        let mode = args
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("all");

        if mode != "all" && mode != "any" {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Invalid mode '{mode}'. Must be 'all' or 'any'."
                )),
            });
        }

        let wait_all = mode == "all";
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        let idle_duration = Duration::from_secs(idle_timeout_secs);
        let poll_interval = Duration::from_secs(2);

        // Initialize per-session poll state
        let now = Instant::now();
        let mut states: Vec<SessionPollState> = session_ids
            .iter()
            .map(|id| {
                let messages = self.backend.load(id);
                SessionPollState {
                    id: id.clone(),
                    last_message_count: messages.len(),
                    last_change: now,
                    completed: false,
                }
            })
            .collect();

        // Check for already-completed sessions (completion marker present)
        for state in &mut states {
            if has_completion_marker(self.backend.as_ref(), &state.id) {
                state.completed = true;
            }
        }

        // Early return if condition already met
        let done = if wait_all {
            states.iter().all(|s| s.completed)
        } else {
            states.iter().any(|s| s.completed)
        };

        if done {
            let output = format_completed_sessions(self.backend.as_ref(), &states, false);
            return Ok(ToolResult {
                success: true,
                output,
                error: None,
            });
        }

        // Polling loop
        loop {
            tokio::time::sleep(poll_interval).await;

            if Instant::now() >= deadline {
                // Mark any idle-completed sessions before reporting timeout
                let output = format_completed_sessions(self.backend.as_ref(), &states, true);
                return Ok(ToolResult {
                    success: true,
                    output,
                    error: None,
                });
            }

            for state in &mut states {
                if state.completed {
                    continue;
                }

                // Check for completion marker first
                if has_completion_marker(self.backend.as_ref(), &state.id) {
                    state.completed = true;
                    continue;
                }

                // Check for activity changes
                let current_count = self.backend.load(&state.id).len();
                if current_count != state.last_message_count {
                    state.last_message_count = current_count;
                    state.last_change = Instant::now();
                } else if state.last_change.elapsed() >= idle_duration {
                    // Idle timeout reached — consider complete
                    state.completed = true;
                }
            }

            let done = if wait_all {
                states.iter().all(|s| s.completed)
            } else {
                states.iter().any(|s| s.completed)
            };

            if done {
                let output = format_completed_sessions(self.backend.as_ref(), &states, false);
                return Ok(ToolResult {
                    success: true,
                    output,
                    error: None,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::session_store::SessionStore;
    use crate::providers::traits::ChatMessage;
    use tempfile::TempDir;

    fn test_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy::default())
    }

    fn test_backend() -> (TempDir, Arc<dyn SessionBackend>) {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        (tmp, Arc::new(store))
    }

    fn seeded_backend() -> (TempDir, Arc<dyn SessionBackend>) {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        store
            .append("telegram__alice", &ChatMessage::user("Hello from Alice"))
            .unwrap();
        store
            .append(
                "telegram__alice",
                &ChatMessage::assistant("Hi Alice, how can I help?"),
            )
            .unwrap();
        store
            .append("discord__bob", &ChatMessage::user("Hey from Bob"))
            .unwrap();
        (tmp, Arc::new(store))
    }

    // ── SessionsListTool tests ──────────────────────────────────────

    #[tokio::test]
    async fn list_empty_sessions() {
        let (_tmp, backend) = test_backend();
        let tool = SessionsListTool::new(backend);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("No active sessions"));
    }

    #[tokio::test]
    async fn list_sessions_shows_all() {
        let (_tmp, backend) = seeded_backend();
        let tool = SessionsListTool::new(backend);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("2 session(s)"));
        assert!(result.output.contains("telegram__alice"));
        assert!(result.output.contains("discord__bob"));
    }

    #[tokio::test]
    async fn list_sessions_respects_limit() {
        let (_tmp, backend) = seeded_backend();
        let tool = SessionsListTool::new(backend);
        let result = tool.execute(json!({"limit": 1})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("1 session(s)"));
    }

    #[tokio::test]
    async fn list_sessions_extracts_channel() {
        let (_tmp, backend) = seeded_backend();
        let tool = SessionsListTool::new(backend);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.output.contains("channel=telegram"));
        assert!(result.output.contains("channel=discord"));
    }

    #[test]
    fn list_tool_name_and_schema() {
        let (_tmp, backend) = test_backend();
        let tool = SessionsListTool::new(backend);
        assert_eq!(tool.name(), "sessions_list");
        assert!(tool.parameters_schema()["properties"]["limit"].is_object());
    }

    // ── SessionsHistoryTool tests ───────────────────────────────────

    #[tokio::test]
    async fn history_empty_session() {
        let (_tmp, backend) = test_backend();
        let tool = SessionsHistoryTool::new(backend, test_security());
        let result = tool
            .execute(json!({"session_id": "nonexistent"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("No messages found"));
    }

    #[tokio::test]
    async fn history_returns_messages() {
        let (_tmp, backend) = seeded_backend();
        let tool = SessionsHistoryTool::new(backend, test_security());
        let result = tool
            .execute(json!({"session_id": "telegram__alice"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("showing 2/2 messages"));
        assert!(result.output.contains("[user] Hello from Alice"));
        assert!(result.output.contains("[assistant] Hi Alice"));
    }

    #[tokio::test]
    async fn history_respects_limit() {
        let (_tmp, backend) = seeded_backend();
        let tool = SessionsHistoryTool::new(backend, test_security());
        let result = tool
            .execute(json!({"session_id": "telegram__alice", "limit": 1}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("showing 1/2 messages"));
        // Should show only the last message
        assert!(result.output.contains("[assistant]"));
        assert!(!result.output.contains("[user] Hello from Alice"));
    }

    #[tokio::test]
    async fn history_missing_session_id() {
        let (_tmp, backend) = test_backend();
        let tool = SessionsHistoryTool::new(backend, test_security());
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("session_id"));
    }

    #[tokio::test]
    async fn history_rejects_empty_session_id() {
        let (_tmp, backend) = test_backend();
        let tool = SessionsHistoryTool::new(backend, test_security());
        let result = tool.execute(json!({"session_id": "   "})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Invalid"));
    }

    #[test]
    fn history_tool_name_and_schema() {
        let (_tmp, backend) = test_backend();
        let tool = SessionsHistoryTool::new(backend, test_security());
        assert_eq!(tool.name(), "sessions_history");
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["session_id"].is_object());
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("session_id"))
        );
    }

    // ── SessionsSendTool tests ──────────────────────────────────────

    #[tokio::test]
    async fn send_appends_message() {
        let (_tmp, backend) = test_backend();
        let tool = SessionsSendTool::new(backend.clone(), test_security());
        let result = tool
            .execute(json!({
                "session_id": "telegram__alice",
                "message": "Hello from another agent"
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("Message sent"));

        // Verify message was appended
        let messages = backend.load("telegram__alice");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "Hello from another agent");
    }

    #[tokio::test]
    async fn send_to_existing_session() {
        let (_tmp, backend) = seeded_backend();
        let tool = SessionsSendTool::new(backend.clone(), test_security());
        let result = tool
            .execute(json!({
                "session_id": "telegram__alice",
                "message": "Inter-agent message"
            }))
            .await
            .unwrap();
        assert!(result.success);

        let messages = backend.load("telegram__alice");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2].content, "Inter-agent message");
    }

    #[tokio::test]
    async fn send_rejects_empty_message() {
        let (_tmp, backend) = test_backend();
        let tool = SessionsSendTool::new(backend, test_security());
        let result = tool
            .execute(json!({
                "session_id": "telegram__alice",
                "message": "   "
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("empty"));
    }

    #[tokio::test]
    async fn send_rejects_empty_session_id() {
        let (_tmp, backend) = test_backend();
        let tool = SessionsSendTool::new(backend, test_security());
        let result = tool
            .execute(json!({
                "session_id": "",
                "message": "hello"
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Invalid"));
    }

    #[tokio::test]
    async fn send_rejects_non_alphanumeric_session_id() {
        let (_tmp, backend) = test_backend();
        let tool = SessionsSendTool::new(backend, test_security());
        let result = tool
            .execute(json!({
                "session_id": "///",
                "message": "hello"
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Invalid"));
    }

    #[tokio::test]
    async fn send_missing_session_id() {
        let (_tmp, backend) = test_backend();
        let tool = SessionsSendTool::new(backend, test_security());
        let result = tool.execute(json!({"message": "hi"})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("session_id"));
    }

    #[tokio::test]
    async fn send_missing_message() {
        let (_tmp, backend) = test_backend();
        let tool = SessionsSendTool::new(backend, test_security());
        let result = tool.execute(json!({"session_id": "telegram__alice"})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("message"));
    }

    #[test]
    fn send_tool_name_and_schema() {
        let (_tmp, backend) = test_backend();
        let tool = SessionsSendTool::new(backend, test_security());
        assert_eq!(tool.name(), "sessions_send");
        let schema = tool.parameters_schema();
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("session_id"))
        );
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("message"))
        );
    }

    // ── SessionsAwaitTool tests ────────────────────────────────────

    #[test]
    fn await_tool_name_and_schema() {
        let (_tmp, backend) = test_backend();
        let tool = SessionsAwaitTool::new(backend, test_security());
        assert_eq!(tool.name(), "sessions_await");
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["session_ids"].is_object());
        assert!(schema["properties"]["timeout_secs"].is_object());
        assert!(schema["properties"]["mode"].is_object());
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("session_ids"))
        );
    }

    #[tokio::test]
    async fn await_rejects_empty_session_ids() {
        let (_tmp, backend) = test_backend();
        let tool = SessionsAwaitTool::new(backend, test_security());
        let result = tool
            .execute(json!({"session_ids": []}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("at least one"));
    }

    #[tokio::test]
    async fn await_rejects_invalid_session_id() {
        let (_tmp, backend) = test_backend();
        let tool = SessionsAwaitTool::new(backend, test_security());
        let result = tool
            .execute(json!({"session_ids": ["///"]}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Invalid"));
    }

    #[tokio::test]
    async fn await_rejects_invalid_mode() {
        let (_tmp, backend) = test_backend();
        let tool = SessionsAwaitTool::new(backend, test_security());
        let result = tool
            .execute(json!({"session_ids": ["test1"], "mode": "first"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Invalid mode"));
    }

    #[tokio::test]
    async fn await_missing_session_ids_param() {
        let (_tmp, backend) = test_backend();
        let tool = SessionsAwaitTool::new(backend, test_security());
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("session_ids"));
    }

    #[tokio::test]
    async fn await_completes_immediately_with_done_marker() {
        let (_tmp, backend) = test_backend();
        // Seed a session with a completion marker
        backend
            .append("agent__task1", &ChatMessage::user("Start task"))
            .unwrap();
        backend
            .append(
                "agent__task1",
                &ChatMessage::assistant("Task finished. [DONE]"),
            )
            .unwrap();

        let tool = SessionsAwaitTool::new(backend, test_security());
        let result = tool
            .execute(json!({
                "session_ids": ["agent__task1"],
                "timeout_secs": 5
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("1 session(s) completed"));
        assert!(result.output.contains("agent__task1"));
        assert!(result.output.contains("[DONE]"));
    }

    #[tokio::test]
    async fn await_completes_immediately_with_complete_marker() {
        let (_tmp, backend) = test_backend();
        backend
            .append(
                "agent__task1",
                &ChatMessage::assistant("All done [COMPLETE]"),
            )
            .unwrap();

        let tool = SessionsAwaitTool::new(backend, test_security());
        let result = tool
            .execute(json!({
                "session_ids": ["agent__task1"],
                "timeout_secs": 5
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("1 session(s) completed"));
    }

    #[tokio::test]
    async fn await_any_mode_completes_when_first_done() {
        let (_tmp, backend) = test_backend();
        // Only task1 has completion marker
        backend
            .append(
                "agent__task1",
                &ChatMessage::assistant("Result [DONE]"),
            )
            .unwrap();
        backend
            .append(
                "agent__task2",
                &ChatMessage::assistant("Still working..."),
            )
            .unwrap();

        let tool = SessionsAwaitTool::new(backend, test_security());
        let result = tool
            .execute(json!({
                "session_ids": ["agent__task1", "agent__task2"],
                "mode": "any",
                "timeout_secs": 5
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("1 session(s) completed"));
        assert!(result.output.contains("agent__task1"));
        assert!(result.output.contains("Pending sessions"));
        assert!(result.output.contains("agent__task2"));
    }

    #[tokio::test]
    async fn await_idle_timeout_triggers_completion() {
        let (_tmp, backend) = test_backend();
        // Session exists but has no completion marker — idle timeout should kick in
        backend
            .append("agent__idle", &ChatMessage::user("Start"))
            .unwrap();

        let tool = SessionsAwaitTool::new(backend, test_security());
        let result = tool
            .execute(json!({
                "session_ids": ["agent__idle"],
                "timeout_secs": 10,
                "idle_timeout_secs": 1
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("1 session(s) completed"));
        assert!(result.output.contains("agent__idle"));
    }

    #[tokio::test]
    async fn await_timeout_returns_partial_results() {
        let (_tmp, backend) = test_backend();
        // task1 is done, task2 is not
        backend
            .append(
                "agent__t1",
                &ChatMessage::assistant("Finished [DONE]"),
            )
            .unwrap();
        // task2 has recent activity — use a very short overall timeout but long idle
        backend
            .append("agent__t2", &ChatMessage::user("Working"))
            .unwrap();

        let tool = SessionsAwaitTool::new(backend.clone(), test_security());

        // Spawn a task that keeps task2 active so it never idles
        let bg_backend = backend.clone();
        let keepalive = tokio::spawn(async move {
            for i in 0..5 {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let _ = bg_backend.append(
                    "agent__t2",
                    &ChatMessage::user(&format!("ping {i}")),
                );
            }
        });

        let result = tool
            .execute(json!({
                "session_ids": ["agent__t1", "agent__t2"],
                "mode": "all",
                "timeout_secs": 3,
                "idle_timeout_secs": 60
            }))
            .await
            .unwrap();

        keepalive.abort();

        assert!(result.success);
        assert!(result.output.contains("Timeout reached"));
        assert!(result.output.contains("agent__t1"));
    }

    #[test]
    fn has_completion_marker_detects_done() {
        let (_tmp, backend) = test_backend();
        backend
            .append("s1", &ChatMessage::assistant("result [DONE]"))
            .unwrap();
        assert!(has_completion_marker(backend.as_ref(), "s1"));
    }

    #[test]
    fn has_completion_marker_case_insensitive() {
        let (_tmp, backend) = test_backend();
        backend
            .append("s1", &ChatMessage::assistant("result [done]"))
            .unwrap();
        assert!(has_completion_marker(backend.as_ref(), "s1"));
    }

    #[test]
    fn has_completion_marker_returns_false_without_marker() {
        let (_tmp, backend) = test_backend();
        backend
            .append("s1", &ChatMessage::assistant("still working"))
            .unwrap();
        assert!(!has_completion_marker(backend.as_ref(), "s1"));
    }

    #[test]
    fn has_completion_marker_returns_false_for_empty_session() {
        let (_tmp, backend) = test_backend();
        assert!(!has_completion_marker(backend.as_ref(), "nonexistent"));
    }
}
