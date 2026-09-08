//! The contract between the harness and a model backend.
//!
//! The boundary is deliberately one model exchange. The harness owns the
//! agentic loop and both wires (the compositor link and the turn channel); a
//! backend is only ever asked: given the conversation so far and the tool
//! definitions, what does the model say next, streamed as it arrives. That
//! keeps every backend from reimplementing the loop, and keeps everything
//! Agentware-shaped (telemetry, intents, rejections) out of the code that
//! speaks an API.
//!
//! A local backend later is one more implementation of [`Backend`] behind
//! `backends::from_settings`, and the harness will not know the difference.

use std::fmt;

/// Who said a message. The two roles a Messages-shaped API accepts; the
/// system prompt travels separately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

/// One piece of a message's content.
///
/// A conversation with tools is not plain text: an assistant turn may carry
/// the model's reasoning and the calls it wants made, and the user turn that
/// answers it carries the results. Thinking blocks are kept and sent back
/// verbatim on later exchanges, signature included, because the API requires
/// them unmodified within a turn's loop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    Text(String),
    Thinking { text: String, signature: String },
    /// Reasoning the API returns only in encrypted form. Carried back as-is.
    RedactedThinking { data: String },
    ToolUse { id: String, name: String, input: serde_json::Value },
    ToolResult { id: String, content: String, is_error: bool },
}

/// One message of the conversation as a backend sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelMessage {
    pub role: Role,
    pub content: Vec<Block>,
}

impl ModelMessage {
    pub fn user_text(text: &str) -> ModelMessage {
        ModelMessage { role: Role::User, content: vec![Block::Text(text.to_owned())] }
    }

    pub fn assistant_text(text: &str) -> ModelMessage {
        ModelMessage { role: Role::Assistant, content: vec![Block::Text(text.to_owned())] }
    }
}

/// One tool the harness offers the model. The schema is JSON Schema, passed
/// through to the API untouched.
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: serde_json::Value,
}

/// A piece of the response, delivered as it streams. The harness turns these
/// into telemetry so the human watches the model work rather than a spinner.
pub enum Delta {
    /// A piece of the model's (summarized) reasoning.
    Thinking(String),
    /// A piece of the model's text. The harness deliberately does not speak
    /// these as they stream, because whether the text is narration or the
    /// reply is only known once the exchange completes; the payload is part
    /// of the contract for a harness that decides otherwise.
    Text(#[allow(dead_code)] String),
    /// The model started composing a tool call, named. Its input follows in
    /// the completed [`Assistant`]; this exists so a harness can say so
    /// early, while the input is still streaming. This one flushes the
    /// thinking buffer and leaves the name for the execution line that
    /// follows moments later.
    ToolCallStarted(#[allow(dead_code)] String),
}

/// Why the model stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// Finished answering. The text is the reply.
    EndTurn,
    /// Wants its tool calls executed and the results back.
    ToolUse,
    /// Ran out of output room mid-answer.
    MaxTokens,
    /// Declined to answer.
    Refusal,
    /// A stop this code does not know. Treated like an end of turn, reported
    /// as what it was.
    Other(String),
}

/// What one exchange cost, as the backend reports it.
///
/// Every field is what the endpoint said, not what this code worked out, and
/// a backend that reports nothing leaves it zero. That distinction matters:
/// the instrumentation prints these, and a zero that means "not reported" and
/// a zero that means "nothing cached" look the same in a log, so the harness
/// says which backend produced the line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// Prompt tokens that were prefilled.
    pub input: u32,
    /// Tokens the model produced, thinking included.
    pub output: u32,
    /// Prompt tokens served from cache rather than prefilled. The number the
    /// whole caching effort is judged by.
    pub cache_read: u32,
    /// Prompt tokens written into the cache by this exchange.
    pub cache_write: u32,
}

/// The completed assistant turn: everything the model said, why it stopped
/// saying it, and what it cost.
#[derive(Debug)]
pub struct Assistant {
    pub content: Vec<Block>,
    pub stop: Stop,
    pub usage: Usage,
}

impl Assistant {
    /// The turn's tool calls, in order.
    pub fn tool_uses(&self) -> impl Iterator<Item = (&str, &str, &serde_json::Value)> {
        self.content.iter().filter_map(|block| match block {
            Block::ToolUse { id, name, input } => Some((id.as_str(), name.as_str(), input)),
            _ => None,
        })
    }

    /// The turn's text, joined. For a final turn this is the reply.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for block in &self.content {
            if let Block::Text(text) = block {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
        out
    }
}

/// Why an exchange did not complete.
#[derive(Debug)]
pub enum BackendError {
    /// The watch fired: the workspace changed while the model was answering,
    /// and the answer was abandoned mid-stream. Not a failure. The harness
    /// starts the exchange again with the change attached, and nothing here
    /// is retried, backed off or reported to the human as an error.
    Interrupted,
    /// What went wrong, in words meant for the pane: the human reads this, so
    /// "no API key is set: enter one in Settings, on the Agent page" beats an
    /// errno.
    Failed(String),
}

impl BackendError {
    pub fn new(message: impl Into<String>) -> BackendError {
        BackendError::Failed(message.into())
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::Interrupted => f.write_str("the workspace changed mid-answer"),
            BackendError::Failed(message) => f.write_str(message),
        }
    }
}

/// One streamed model exchange. The whole of what a backend is.
pub trait Backend {
    /// Ask the model what comes next. Deltas stream through `on` as they
    /// arrive; the returned [`Assistant`] is the completed turn.
    fn respond(
        &mut self,
        system: &str,
        messages: &[ModelMessage],
        tools: &[ToolDef],
        on: &mut dyn FnMut(Delta),
    ) -> Result<Assistant, BackendError>;

    /// Set, or clear, the descriptor whose readability ends an exchange
    /// early with [`BackendError::Interrupted`].
    fn watch(&mut self, watch: Option<crate::interrupt::Watch>);
}
