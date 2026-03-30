//! Automatic message complexity scoring for model routing.
//!
//! Analyzes conversation history to determine whether a request is simple
//! (suitable for a cheap/fast model), moderate (default model), or complex
//! (needs a reasoning model). Uses pure heuristics — no ML required.

use super::traits::ChatMessage;

/// Complexity tier for a conversation, used to select a model route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageComplexity {
    /// Short, single-turn, no code — route to a cheap/fast model.
    Simple,
    /// Moderate conversation depth or content — use the default model.
    Moderate,
    /// Long context, many tool calls, code-heavy — use a reasoning model.
    Complex,
}

impl MessageComplexity {
    /// Map complexity to the route hint the [`RouterProvider`] should use.
    pub fn route_hint(self) -> &'static str {
        match self {
            Self::Simple => "hint:cheap",
            Self::Moderate => "hint:default",
            Self::Complex => "hint:reasoning",
        }
    }
}

/// Score the complexity of a conversation based on simple heuristics.
///
/// Scoring dimensions (each contributes 0–3 points):
///
/// | Signal                        | 0        | 1          | 2           | 3            |
/// |-------------------------------|----------|------------|-------------|--------------|
/// | Last-user-message length      | < 100    | 100–500    | 500–2000    | > 2000       |
/// | Conversation turns            | 1        | 2–4        | 5–10        | > 10         |
/// | Tool-call indicators          | 0        | 1–2        | 3–5         | > 5          |
/// | Code-block indicators         | 0        | 1          | 2–3         | > 3          |
///
/// Total 0–3 → Simple, 4–7 → Moderate, 8–12 → Complex.
pub fn score_complexity(messages: &[ChatMessage]) -> MessageComplexity {
    let score = compute_score(messages);
    match score {
        0..=3 => MessageComplexity::Simple,
        4..=7 => MessageComplexity::Moderate,
        _ => MessageComplexity::Complex,
    }
}

/// Compute the raw numeric score (visible for testing).
fn compute_score(messages: &[ChatMessage]) -> u32 {
    let mut score: u32 = 0;

    // --- Last user message length ---
    let last_user_len = messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.len())
        .unwrap_or(0);

    score += match last_user_len {
        0..=99 => 0,
        100..=499 => 1,
        500..=1999 => 2,
        _ => 3,
    };

    // --- Conversation turns (count user messages) ---
    let user_turns = messages.iter().filter(|m| m.role == "user").count();
    score += match user_turns {
        0..=1 => 0,
        2..=4 => 1,
        5..=10 => 2,
        _ => 3,
    };

    // --- Tool-call indicators ---
    // Look for JSON tool_call patterns or "tool" role messages in history.
    let tool_indicators: usize = messages
        .iter()
        .filter(|m| m.role == "tool" || m.content.contains("\"tool_calls\""))
        .count();

    score += match tool_indicators {
        0 => 0,
        1..=2 => 1,
        3..=5 => 2,
        _ => 3,
    };

    // --- Code-block indicators ---
    let code_blocks: usize = messages
        .iter()
        .map(|m| m.content.matches("```").count() / 2) // opening+closing = 1 block
        .sum();

    score += match code_blocks {
        0 => 0,
        1 => 1,
        2..=3 => 2,
        _ => 3,
    };

    score
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(content: &str) -> ChatMessage {
        ChatMessage::user(content)
    }

    fn assistant(content: &str) -> ChatMessage {
        ChatMessage::assistant(content)
    }

    fn tool(content: &str) -> ChatMessage {
        ChatMessage::tool(content)
    }

    #[test]
    fn empty_messages_is_simple() {
        assert_eq!(score_complexity(&[]), MessageComplexity::Simple);
    }

    #[test]
    fn short_single_turn_is_simple() {
        let msgs = [user("hello")];
        assert_eq!(score_complexity(&msgs), MessageComplexity::Simple);
    }

    #[test]
    fn long_message_increases_complexity() {
        let long_msg = "x".repeat(2500);
        let msgs = [user(&long_msg)];
        // Length alone scores 3, still Simple boundary
        assert_eq!(score_complexity(&msgs), MessageComplexity::Simple);
    }

    #[test]
    fn many_turns_increases_complexity() {
        let mut msgs = Vec::new();
        for i in 0..12 {
            msgs.push(user(&format!("question {i}")));
            msgs.push(assistant(&format!("answer {i}")));
        }
        // 12 user turns = 3 points, short messages = 0 points → Moderate is possible
        // but with 12 short messages the last user msg is short (0) and no code/tools (0)
        // so total = 3 → still Simple
        assert_eq!(score_complexity(&msgs), MessageComplexity::Simple);
    }

    #[test]
    fn moderate_with_some_tools_and_turns() {
        let mut msgs = Vec::new();
        for i in 0..6 {
            msgs.push(user(&format!("question about topic {i} with some detail")));
            msgs.push(assistant(&format!("here is the answer for {i}")));
        }
        // Add a couple of tool messages
        msgs.push(tool("tool result 1"));
        msgs.push(tool("tool result 2"));
        msgs.push(user("now summarize everything we discussed above"));
        // user turns = 7 → 2 pts; last msg ~47 chars → 0 pts; tool indicators = 2 → 1 pt; code = 0 → 0 pts
        // total = 3 → Simple
        // Let's make the last message longer
        msgs.pop();
        msgs.push(user(&"a]".repeat(200))); // 400 chars → 1 pt
        // total = 2 + 1 + 1 = 4 → Moderate
        assert_eq!(score_complexity(&msgs), MessageComplexity::Moderate);
    }

    #[test]
    fn complex_with_code_tools_and_long_message() {
        let code_msg = format!(
            "Here is my code:\n```rust\nfn main() {{}}\n```\n\
             And another:\n```python\nprint('hi')\n```\n\
             And more:\n```js\nconsole.log('x')\n```\n\
             And even more:\n```go\nfmt.Println(\"y\")\n```"
        );
        let mut msgs = Vec::new();
        for i in 0..8 {
            msgs.push(user(&format!("step {i}: {}", "detailed ".repeat(20))));
            msgs.push(assistant(&code_msg));
        }
        // Add many tool calls
        for _ in 0..6 {
            msgs.push(tool("tool output data"));
        }
        msgs.push(user(&"analyze all of the above in detail ".repeat(60)));
        // user turns = 9 → 2; last msg ~2100 chars → 3; tool indicators = 6 → 3; code blocks = 4*8=32 → 3
        // total = 2+3+3+3 = 11 → Complex
        assert_eq!(score_complexity(&msgs), MessageComplexity::Complex);
    }

    #[test]
    fn route_hint_mapping() {
        assert_eq!(MessageComplexity::Simple.route_hint(), "hint:cheap");
        assert_eq!(MessageComplexity::Moderate.route_hint(), "hint:default");
        assert_eq!(MessageComplexity::Complex.route_hint(), "hint:reasoning");
    }

    #[test]
    fn tool_calls_json_pattern_detected() {
        let msgs = [
            user("do something"),
            assistant(r#"{"tool_calls": [{"name": "search"}]}"#),
            assistant(r#"{"tool_calls": [{"name": "read"}]}"#),
            assistant(r#"{"tool_calls": [{"name": "edit"}]}"#),
        ];
        // 1 user turn = 0; short msg = 0; tool_calls patterns = 3 → 2; no code = 0
        // total = 2 → Simple
        assert_eq!(score_complexity(&msgs), MessageComplexity::Simple);
    }

    #[test]
    fn code_blocks_counted_correctly() {
        let msg_with_blocks = "```rust\nfn foo() {}\n```\n```python\npass\n```";
        let msgs = [user("help with code"), assistant(msg_with_blocks)];
        // 1 turn = 0; short msg = 0; no tools = 0; 2 code blocks → 2 pts
        // total = 2 → Simple
        assert_eq!(score_complexity(&msgs), MessageComplexity::Simple);
    }
}
