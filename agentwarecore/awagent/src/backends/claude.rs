//! The Claude backend: the Anthropic Messages API, streamed.
//!
//! One route, `POST /v1/messages`, with `stream: true`, spoken over the
//! hand-rolled HTTPS client in `http`. The request carries the system prompt,
//! the tools and the conversation; the response arrives as server-sent
//! events, which this accumulates into the completed assistant turn while
//! handing text and thinking to the harness as they stream, so the pane
//! shows the model working rather than a spinner.
//!
//! Thinking is adaptive with summarized display, so the reasoning the pane
//! shows is the summary the API chooses to give; the blocks themselves are
//! carried back verbatim on the next exchange of the same turn, signature
//! included, as the API requires. Retries cover what retrying can fix:
//! failure to connect, rate limits and server errors, with a short backoff.
//! A rejected key is not retried; it is a sentence telling the human where
//! the Settings page is.

use std::io::Read;

use serde_json::{Value, json};

use crate::backend::{Assistant, Backend, BackendError, Block, Delta, ModelMessage, Role, Stop, ToolDef};
use crate::http::{self, SseReader};

const HOST: &str = "api.anthropic.com";
const PATH: &str = "/v1/messages";
const API_VERSION: &str = "2023-06-01";

/// Room for the turn's thinking and text together. The reply in the pane is
/// short; the room is for reasoning across tool calls.
const MAX_TOKENS: u32 = 16_000;

/// Connection and server troubles are retried this many times in all, with
/// the delay doubling from [`BACKOFF`]. Bounded, because a turn that cannot
/// reach the API should say so while the human is still watching.
const ATTEMPTS: u32 = 4;
const BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);

pub struct Claude {
    key: String,
    model: String,
    /// The workspace the key acts in, for a key linked to an identity.
    ///
    /// An ordinary workspace-scoped key names its own workspace, so this is
    /// `None` and no header goes out. A key linked to an identity does not,
    /// and the API refuses it with `400 anthropic-workspace-id required when
    /// authenticated with api key linked to identity` until the request says
    /// which workspace. Absent rather than empty, so the header is either
    /// right or not sent: an empty one is a 400 of its own.
    workspace: Option<String>,
}

impl Claude {
    pub fn new(key: String, model: String, workspace: Option<String>) -> Claude {
        Claude { key, model, workspace }
    }
}

impl Backend for Claude {
    fn respond(
        &mut self,
        system: &str,
        messages: &[ModelMessage],
        tools: &[ToolDef],
        on: &mut dyn FnMut(Delta),
    ) -> Result<Assistant, BackendError> {
        let body = request_body(&self.model, system, messages, tools).to_string();
        let mut headers = vec![
            ("x-api-key", self.key.as_str()),
            ("anthropic-version", API_VERSION),
            ("content-type", "application/json"),
            ("accept", "text/event-stream"),
        ];
        if let Some(workspace) = &self.workspace {
            headers.push(("anthropic-workspace-id", workspace.as_str()));
        }

        let mut delay = BACKOFF;
        for attempt in 1..=ATTEMPTS {
            let outcome = match http::post(HOST, PATH, &headers, body.as_bytes()) {
                Ok(mut response) if response.status == 200 => {
                    // The stream is not retried: deltas already handed to the
                    // harness are already in the pane.
                    return consume_stream(&mut response, on);
                }
                Ok(mut response) => {
                    let status = response.status;
                    let message = api_error(&mut response);
                    match status {
                        401 | 403 => Err(BackendError::new(format!(
                            "the API rejected the key ({message}). Check it in Settings, on the Agent page"
                        ))),
                        429 | 500..=599 => Ok(format!("the API answered {status}: {message}")),
                        // A key linked to an identity is refused until the
                        // request names a workspace. The API's own words say
                        // which header is missing but not where a person sets
                        // it, and the answer to that is one page away.
                        400 if message.contains("workspace") => Err(BackendError::new(format!(
                            "the API refused the request (400): {message}. \
                             Set the workspace in Settings, on the Agent page"
                        ))),
                        _ => Err(BackendError::new(format!("the API refused the request ({status}): {message}"))),
                    }
                }
                Err(err) => Ok(format!("could not reach {HOST}: {err}")),
            };
            match outcome {
                Err(fatal) => return Err(fatal),
                Ok(transient) if attempt == ATTEMPTS => {
                    return Err(BackendError::new(format!("{transient} (gave up after {ATTEMPTS} attempts)")));
                }
                Ok(_) => {
                    std::thread::sleep(delay);
                    delay *= 2;
                }
            }
        }
        unreachable!("the attempt loop returns before running out");
    }
}

/// The request, as the API takes it.
///
/// A function of its inputs and nothing else, so a test can hold the whole
/// body up to the light. The system prompt is marked as a cache prefix: every
/// exchange of the loop resends it and the API charges the reread at a
/// fraction when it can serve it from cache.
fn request_body(model: &str, system: &str, messages: &[ModelMessage], tools: &[ToolDef]) -> Value {
    let tools: Vec<Value> = tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.schema,
            })
        })
        .collect();

    let mut body = json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "stream": true,
        "system": [{
            "type": "text",
            "text": system,
            "cache_control": {"type": "ephemeral"},
        }],
        "tools": tools,
        "messages": messages.iter().map(message_json).collect::<Vec<Value>>(),
    });

    // Adaptive thinking, with the summarized display that gives the pane
    // something to show. Claude 4.5-generation models (Haiku among them) take
    // a token budget instead of "adaptive" and reject this shape, so they run
    // without thinking rather than carrying a second configuration here.
    if !model.starts_with("claude-haiku") {
        body["thinking"] = json!({"type": "adaptive", "display": "summarized"});
    }
    body
}

fn message_json(message: &ModelMessage) -> Value {
    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let content: Vec<Value> = message
        .content
        .iter()
        .map(|block| match block {
            Block::Text(text) => json!({"type": "text", "text": text}),
            // Verbatim, signature included: the API rejects a thinking block
            // that came back changed.
            Block::Thinking { text, signature } => {
                json!({"type": "thinking", "thinking": text, "signature": signature})
            }
            Block::RedactedThinking { data } => {
                json!({"type": "redacted_thinking", "data": data})
            }
            Block::ToolUse { id, name, input } => {
                json!({"type": "tool_use", "id": id, "name": name, "input": input})
            }
            Block::ToolResult { id, content, is_error } => {
                let mut value = json!({"type": "tool_result", "tool_use_id": id, "content": content});
                if *is_error {
                    value["is_error"] = json!(true);
                }
                value
            }
        })
        .collect();
    json!({"role": role, "content": content})
}

/// The message out of an API error body, or the raw body if it is not the
/// shape the API documents.
fn api_error(response: &mut http::Response) -> String {
    let text = response.read_to_string().unwrap_or_default();
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|body| body["error"]["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| {
            let trimmed = text.trim();
            if trimmed.is_empty() { "no detail given".to_owned() } else { trimmed.to_owned() }
        })
}

/// One content block, as it accumulates across deltas.
enum Partial {
    Text(String),
    Thinking { text: String, signature: String },
    RedactedThinking { data: String },
    /// The input arrives as fragments of JSON text, complete only at the
    /// block's stop.
    ToolUse { id: String, name: String, json: String },
}

/// Drink the event stream down to the completed assistant turn, handing
/// deltas out as they arrive.
///
/// Generic over the reader so a test can feed it a stream from memory.
fn consume_stream(reader: &mut dyn Read, on: &mut dyn FnMut(Delta)) -> Result<Assistant, BackendError> {
    let mut events = SseReader::new(reader);
    let mut partials: Vec<Partial> = Vec::new();
    let mut stop: Option<Stop> = None;

    loop {
        let event = match events.next_event() {
            Ok(Some(event)) => event,
            Ok(None) => break,
            Err(err) => return Err(BackendError::new(format!("the stream broke: {err}"))),
        };
        let data: Value = serde_json::from_str(&event.data)
            .map_err(|err| BackendError::new(format!("the stream sent something unreadable: {err}")))?;

        match event.event.as_str() {
            "content_block_start" => {
                let block = &data["content_block"];
                let partial = match block["type"].as_str().unwrap_or("") {
                    "text" => Partial::Text(block["text"].as_str().unwrap_or("").to_owned()),
                    "thinking" => Partial::Thinking {
                        text: block["thinking"].as_str().unwrap_or("").to_owned(),
                        signature: String::new(),
                    },
                    "redacted_thinking" => Partial::RedactedThinking {
                        data: block["data"].as_str().unwrap_or("").to_owned(),
                    },
                    "tool_use" => {
                        let name = block["name"].as_str().unwrap_or("").to_owned();
                        on(Delta::ToolCallStarted(name.clone()));
                        Partial::ToolUse {
                            id: block["id"].as_str().unwrap_or("").to_owned(),
                            name,
                            json: String::new(),
                        }
                    }
                    // A block kind this code has not met. Held as text so the
                    // turn survives; the API only ever adds kinds.
                    _ => Partial::Text(String::new()),
                };
                partials.push(partial);
            }
            "content_block_delta" => {
                let Some(partial) = partials.last_mut() else { continue };
                let delta = &data["delta"];
                match delta["type"].as_str().unwrap_or("") {
                    "text_delta" => {
                        if let (Partial::Text(text), Some(piece)) = (partial, delta["text"].as_str()) {
                            text.push_str(piece);
                            on(Delta::Text(piece.to_owned()));
                        }
                    }
                    "thinking_delta" => {
                        if let (Partial::Thinking { text, .. }, Some(piece)) =
                            (partial, delta["thinking"].as_str())
                        {
                            text.push_str(piece);
                            on(Delta::Thinking(piece.to_owned()));
                        }
                    }
                    "signature_delta" => {
                        if let (Partial::Thinking { signature, .. }, Some(piece)) =
                            (partial, delta["signature"].as_str())
                        {
                            signature.push_str(piece);
                        }
                    }
                    "input_json_delta" => {
                        if let (Partial::ToolUse { json, .. }, Some(piece)) =
                            (partial, delta["partial_json"].as_str())
                        {
                            json.push_str(piece);
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(reason) = data["delta"]["stop_reason"].as_str() {
                    stop = Some(match reason {
                        "end_turn" => Stop::EndTurn,
                        "tool_use" => Stop::ToolUse,
                        "max_tokens" => Stop::MaxTokens,
                        "refusal" => Stop::Refusal,
                        other => Stop::Other(other.to_owned()),
                    });
                }
            }
            "message_stop" => break,
            "error" => {
                let message = data["error"]["message"].as_str().unwrap_or("no detail given");
                return Err(BackendError::new(format!("the API reported an error mid-stream: {message}")));
            }
            // message_start, ping, content_block_stop, and anything newer.
            _ => {}
        }
    }

    let Some(stop) = stop else {
        return Err(BackendError::new("the stream ended before the model finished"));
    };

    let content = partials
        .into_iter()
        .filter_map(|partial| match partial {
            Partial::Text(text) => (!text.is_empty()).then_some(Block::Text(text)),
            Partial::Thinking { text, signature } => Some(Block::Thinking { text, signature }),
            Partial::RedactedThinking { data } => Some(Block::RedactedThinking { data }),
            Partial::ToolUse { id, name, json } => {
                let input = if json.trim().is_empty() {
                    Value::Object(Default::default())
                } else {
                    serde_json::from_str(&json).unwrap_or(Value::Null)
                };
                Some(Block::ToolUse { id, name, input })
            }
        })
        .collect();

    Ok(Assistant { content, stop })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> ToolDef {
        ToolDef {
            name: "act",
            description: "Act on a control",
            schema: json!({"type": "object", "properties": {"app": {"type": "string"}}}),
        }
    }

    #[test]
    fn bodies_carry_the_conversation() {
        let messages = vec![
            ModelMessage::user_text("add 12 and 34"),
            ModelMessage {
                role: Role::Assistant,
                content: vec![
                    Block::Thinking { text: "punch it in".into(), signature: "sig".into() },
                    Block::ToolUse {
                        id: "toolu_1".into(),
                        name: "act".into(),
                        input: json!({"app": "awcalc"}),
                    },
                ],
            },
            ModelMessage {
                role: Role::User,
                content: vec![Block::ToolResult {
                    id: "toolu_1".into(),
                    content: "done".into(),
                    is_error: false,
                }],
            },
        ];
        let body = request_body("claude-opus-5", "be helpful", &messages, &[tool()]);

        assert_eq!(body["model"], "claude-opus-5");
        assert_eq!(body["stream"], true);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["tools"][0]["name"], "act");
        assert_eq!(body["messages"][1]["content"][0]["signature"], "sig");
        assert_eq!(body["messages"][1]["content"][1]["input"]["app"], "awcalc");
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "toolu_1");
        // is_error is only said when it is true.
        assert!(body["messages"][2]["content"][0].get("is_error").is_none());
    }

    #[test]
    fn haiku_runs_without_adaptive_thinking() {
        let body = request_body("claude-haiku-4-5", "s", &[], &[]);
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn streams_accumulate() {
        // A turn with summarized thinking, narration, and one tool call whose
        // input arrives in pieces, the way the API streams it.
        let wire = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
            "event: content_block_start\ndata: {\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"open the calc\"}}\n\n",
            "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"c2ln\"}}\n\n",
            "event: content_block_stop\ndata: {\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Adding now.\"}}\n\n",
            "event: content_block_stop\ndata: {\"index\":1}\n\n",
            "event: content_block_start\ndata: {\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_9\",\"name\":\"act\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"app\\\":\"}}\n\n",
            "event: content_block_delta\ndata: {\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"awcalc\\\"}\"}}\n\n",
            "event: content_block_stop\ndata: {\"index\":2}\n\n",
            "event: message_delta\ndata: {\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let mut deltas = Vec::new();
        let assistant = consume_stream(&mut wire.as_bytes(), &mut |delta| {
            deltas.push(match delta {
                Delta::Thinking(text) => format!("think:{text}"),
                Delta::Text(text) => format!("text:{text}"),
                Delta::ToolCallStarted(name) => format!("tool:{name}"),
            });
        })
        .unwrap();

        assert_eq!(assistant.stop, Stop::ToolUse);
        assert_eq!(
            assistant.content[0],
            Block::Thinking { text: "open the calc".into(), signature: "c2ln".into() }
        );
        assert_eq!(assistant.content[1], Block::Text("Adding now.".into()));
        assert_eq!(
            assistant.content[2],
            Block::ToolUse { id: "toolu_9".into(), name: "act".into(), input: json!({"app": "awcalc"}) }
        );
        assert_eq!(deltas, ["think:open the calc", "text:Adding now.", "tool:act"]);
    }

    #[test]
    fn a_broken_stream_is_an_error() {
        let wire = "event: content_block_start\ndata: {\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n";
        assert!(consume_stream(&mut wire.as_bytes(), &mut |_| {}).is_err());

        let wire = "event: error\ndata: {\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}\n\n";
        let err = consume_stream(&mut wire.as_bytes(), &mut |_| {}).unwrap_err();
        assert!(err.message.contains("busy"));
    }
}
