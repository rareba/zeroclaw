use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use crate::hooks::traits::HookHandler;

/// Tracks per-session start times and message counts.
struct SessionState {
    started_at: Instant,
    message_count: u64,
}

/// Logs session lifecycle events (start, end) with duration and message count.
pub struct SessionLoggerHook {
    sessions: Mutex<HashMap<String, SessionState>>,
}

impl SessionLoggerHook {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl HookHandler for SessionLoggerHook {
    fn name(&self) -> &str {
        "session-logger"
    }

    fn priority(&self) -> i32 {
        -50
    }

    async fn on_session_start(&self, session_id: &str, channel: &str) {
        tracing::info!(
            hook = "session-logger",
            session_id,
            channel,
            "Session started"
        );
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                session_id.to_string(),
                SessionState {
                    started_at: Instant::now(),
                    message_count: 0,
                },
            );
    }

    async fn on_session_end(&self, session_id: &str, channel: &str) {
        let state = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);

        match state {
            Some(s) => {
                let duration = s.started_at.elapsed();
                tracing::info!(
                    hook = "session-logger",
                    session_id,
                    channel,
                    duration_secs = duration.as_secs(),
                    message_count = s.message_count,
                    "Session ended"
                );
            }
            None => {
                tracing::info!(
                    hook = "session-logger",
                    session_id,
                    channel,
                    "Session ended (no start record)"
                );
            }
        }
    }

    async fn on_message_sent(&self, _channel: &str, _recipient: &str, _content: &str) {
        // Increment message count for all active sessions.
        // In practice there is typically one active session per channel,
        // but we increment all to stay simple and correct.
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        for state in sessions.values_mut() {
            state.message_count += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn logs_session_lifecycle() {
        let hook = SessionLoggerHook::new();

        hook.on_session_start("sess-1", "telegram").await;

        // Simulate some messages
        hook.on_message_sent("telegram", "user", "hello").await;
        hook.on_message_sent("telegram", "user", "world").await;

        {
            let sessions = hook.sessions.lock().unwrap();
            let state = sessions.get("sess-1").unwrap();
            assert_eq!(state.message_count, 2);
        }

        hook.on_session_end("sess-1", "telegram").await;

        // Session should be removed after end
        let sessions = hook.sessions.lock().unwrap();
        assert!(!sessions.contains_key("sess-1"));
    }

    #[tokio::test]
    async fn session_end_without_start_does_not_panic() {
        let hook = SessionLoggerHook::new();
        // Should not panic
        hook.on_session_end("unknown-sess", "cli").await;
    }
}
