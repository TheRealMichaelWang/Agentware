//! The local backend: an OpenAI-compatible chat-completions endpoint,
//! streamed.
//!
//! This is a sibling of `claude`, not a refactor of it, because the
//! [`Backend`](crate::backend::Backend) contract is one streamed exchange and
//! nothing else. What differs is only the shape of the wire, and the one
//! structural difference is worth naming: the Anthropic API carries tool
//! results as content blocks inside a user message, and this one carries each
//! result as a message of its own with role `tool`. So a single
//! [`ModelMessage`] can become several messages here, which is why the
//! conversation is expanded rather than mapped.
//!
//! It speaks plain HTTP. The endpoint is a model server on the other side of
//! the same machine, reached over a loopback the packets never leave, where
//! TLS would buy nothing and would need a certificate nobody can issue for an
//! address slirp invented.
//!
//! Nothing here is Agentware-shaped: no telemetry, no intents, no rejections.
//! The harness owns all of that and cannot tell this backend from the other.

use std::io::Read;

use serde_json::{Value, json};

use crate::backend::{
    Assistant, Backend, BackendError, Block, Delta, ModelMessage, Role, Stop, ToolDef, Usage,
};
use crate::http::{self, SseReader};

const PATH: &str = "/v1/chat/completions";

/// Room for the turn's reasoning and its answer together.
const MAX_TOKENS: u32 = 8_192;

/// Connection troubles are retried this many times in all. Fewer than the
/// hosted backend gets: a server on the same machine is either there or it is
/// not, and waiting eight seconds to say so helps nobody.
const ATTEMPTS: u32 = 2;
const BACKOFF: std::time::Duration = std::time::Duration::from_millis(500);

pub struct OpenAi {
    model: String,
    client: http::Client,
    /// Whether to ask the model to think before answering.
    ///
    /// Off by default, and the measurements are why: thinking is where the
    /// tokens go, and tokens are the whole cost of a local exchange. It is
    /// also the part a draft model predicts worst, so it loses twice.
    think: bool,
}

impl OpenAi {
    pub fn new(host: &str, port: u16, model: String, think: bool) -> OpenAi {
        OpenAi { model, client: http::Client::http(host, port), think }
    }
}

impl Backend for OpenAi {
    fn respond(
        &mut self,
        system: &str,
        messages: &[ModelMessage],
        tools: &[ToolDef],
        on: &mut dyn FnMut(Delta),
    ) -> Result<Assistant, BackendError> {
        let body = request_body(&self.model, system, messages, tools, self.think).to_string();
        let headers = [("content-type", "application/json"), ("accept", "text/event-stream")];

        let mut delay = BACKOFF;
        for attempt in 1..=ATTEMPTS {
            let transient = match self.client.post(PATH, &headers, body.as_bytes()) {
                Ok(mut response) if response.status == 200 => {
                    let result = consume_stream(&mut response, on);
                    if result.is_ok() {
                        response.drain();
                        self.client.recycle(response);
                    }
                    return result;
                }
                Ok(mut response) => {
                    let status = response.status;
                    let message = api_error(&mut response);
                    self.client.recycle(response);
                    match status {
                        // A model server answers 503 while it is still loading
                        // a model, which is exactly the case worth waiting out.
                        503 => format!("the model server is not ready yet: {message}"),
                        500..=599 => format!("the model server answered {status}: {message}"),
                        _ => {
                            return Err(BackendError::new(format!(
                                "the model server refused the request ({status}): {message}"
                            )));
                        }
                    }
                }
                Err(err) => format!("could not reach the model server at {}: {err}", self.client.address()),
            };
            if attempt == ATTEMPTS {
                return Err(BackendError::new(format!(
                    "{transient}. Check that a model is loaded and serving"
                )));
            }
            std::thread::sleep(delay);
            delay *= 2;
        }
        unreachable!("the attempt loop returns before running out");
    }
}

/// The request, as an OpenAI-compatible endpoint takes it.
fn request_body(
    model: &str,
    system: &str,
    messages: &[ModelMessage],
    tools: &[ToolDef],
    think: bool,
) -> Value {
    let tools: Vec<Value> = tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.schema,
                },
            })
        })
        .collect();

    let mut conversation = vec![json!({"role": "system", "content": system})];
    for message in messages {
        expand(message, &mut conversation);
    }

    let mut body = json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "stream": true,
        // Without this the usage block never arrives on a streamed response,
        // and the harness's accounting prints zeroes that look like a cache
        // that is working perfectly.
        "stream_options": {"include_usage": true},
        "messages": conversation,
        // The server keeps the conversation's KV in a slot and reuses the
        // common prefix, which is the local equivalent of a cache breakpoint
        // and is what makes the second exchange of a turn cheap. Every
        // exchange here is a strict prefix extension of the last, so this is
        // the whole of what the caching needs from this end.
        "cache_prompt": true,
    });
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    if !think {
        // Two knobs exist and only this one works on the templates measured:
        // `reasoning_effort: "none"` is a template error on Qwen's, and the
        // server's own `reasoning_budget` is ignored by it. A server whose
        // template does not know this argument ignores it, which is the right
        // failure: thinking on is slower, not broken.
        body["chat_template_kwargs"] = json!({"enable_thinking": false});
    }
    body
}

/// One message of the harness's conversation, as the one or more messages
/// this API wants.
///
/// The expansion is the whole difference between the two wire shapes. A user
/// message carrying three tool results and a line of text is four messages
/// here: three with role `tool`, then one with role `user`.
fn expand(message: &ModelMessage, out: &mut Vec<Value>) {
    match message.role {
        Role::Assistant => {
            let mut text = String::new();
            let mut reasoning = String::new();
            let mut calls = Vec::new();
            for block in &message.content {
                match block {
                    Block::Text(piece) => text.push_str(piece),
                    Block::Thinking { text: piece, .. } => reasoning.push_str(piece),
                    Block::RedactedThinking { .. } => {}
                    Block::ToolUse { id, name, input } => calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": input.to_string()},
                    })),
                    // An assistant message never carries one. If one appears,
                    // dropping it is better than sending a tool result with
                    // the wrong role and having the server refuse the turn.
                    Block::ToolResult { .. } => {}
                }
            }
            let mut assistant = json!({"role": "assistant"});
            // Content must be present even when there is none to give, or
            // some servers reject the message; null is the documented way to
            // say a message is only tool calls.
            assistant["content"] = if text.is_empty() { Value::Null } else { json!(text) };
            if !reasoning.is_empty() {
                assistant["reasoning_content"] = json!(reasoning);
            }
            if !calls.is_empty() {
                assistant["tool_calls"] = json!(calls);
            }
            out.push(assistant);
        }
        Role::User => {
            // Tool results first, in order, then whatever else was said. The
            // order matters: a result must follow the call it answers and
            // precede anything that comments on it.
            let mut text = String::new();
            for block in &message.content {
                match block {
                    Block::ToolResult { id, content, is_error } => {
                        let body = if *is_error { format!("error: {content}") } else { content.clone() };
                        out.push(json!({"role": "tool", "tool_call_id": id, "content": body}));
                    }
                    Block::Text(piece) => {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(piece);
                    }
                    _ => {}
                }
            }
            if !text.is_empty() {
                out.push(json!({"role": "user", "content": text}));
            }
        }
    }
}

/// The message out of an error body, or the raw body if it is not the shape
/// the API documents.
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

/// A tool call as it accumulates: the name arrives once and the arguments in
/// fragments of JSON text.
#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
    /// Whether the harness has been told this call started.
    announced: bool,
}

/// Drink the event stream down to the completed assistant turn, handing
/// deltas out as they arrive.
///
/// Generic over the reader so a test can feed it a stream from memory.
fn consume_stream(
    reader: &mut dyn Read,
    on: &mut dyn FnMut(Delta),
) -> Result<Assistant, BackendError> {
    let mut events = SseReader::new(reader);
    let mut text = String::new();
    let mut reasoning = String::new();
    // Indexed, because the API identifies a call by its position in the
    // message and the fragments of two calls can interleave.
    let mut calls: Vec<PartialCall> = Vec::new();
    let mut stop: Option<Stop> = None;
    let mut usage = Usage::default();

    loop {
        let event = match events.next_event() {
            Ok(Some(event)) => event,
            Ok(None) => break,
            Err(err) => return Err(BackendError::new(format!("the stream broke: {err}"))),
        };
        // The sentinel that ends an OpenAI stream is not JSON.
        if event.data.trim() == "[DONE]" {
            break;
        }
        let data: Value = serde_json::from_str(&event.data).map_err(|err| {
            BackendError::new(format!("the stream sent something unreadable: {err}"))
        })?;
        if let Some(message) = data["error"]["message"].as_str() {
            return Err(BackendError::new(format!(
                "the model server reported an error mid-stream: {message}"
            )));
        }

        // The usage block rides on its own chunk at the end, with no choices.
        if data["usage"].is_object() {
            let reported = &data["usage"];
            let count = |value: &Value| value.as_u64().unwrap_or(0) as u32;
            usage.input = count(&reported["prompt_tokens"]);
            usage.output = count(&reported["completion_tokens"]);
            // What the server served out of the slot it already held, which
            // is the local equivalent of a cache read.
            usage.cache_read = count(&reported["prompt_tokens_details"]["cached_tokens"]);
        }

        let choice = &data["choices"][0];
        if let Some(reason) = choice["finish_reason"].as_str() {
            stop = Some(match reason {
                "stop" => Stop::EndTurn,
                "tool_calls" | "function_call" => Stop::ToolUse,
                "length" => Stop::MaxTokens,
                "content_filter" => Stop::Refusal,
                other => Stop::Other(other.to_owned()),
            });
        }

        let delta = &choice["delta"];
        if let Some(piece) = delta["content"].as_str().filter(|piece| !piece.is_empty()) {
            text.push_str(piece);
            on(Delta::Text(piece.to_owned()));
        }
        // llama.cpp and several others split reasoning out under this name
        // rather than wrapping it in tags inside the content.
        if let Some(piece) = delta["reasoning_content"]
            .as_str()
            .or_else(|| delta["reasoning"].as_str())
            .filter(|piece| !piece.is_empty())
        {
            reasoning.push_str(piece);
            on(Delta::Thinking(piece.to_owned()));
        }
        if let Some(fragments) = delta["tool_calls"].as_array() {
            for fragment in fragments {
                let at = fragment["index"].as_u64().unwrap_or(0) as usize;
                if calls.len() <= at {
                    calls.resize_with(at + 1, PartialCall::default);
                }
                let call = &mut calls[at];
                if let Some(id) = fragment["id"].as_str() {
                    call.id.push_str(id);
                }
                if let Some(name) = fragment["function"]["name"].as_str() {
                    call.name.push_str(name);
                }
                if let Some(arguments) = fragment["function"]["arguments"].as_str() {
                    call.arguments.push_str(arguments);
                }
                if !call.announced && !call.name.is_empty() {
                    call.announced = true;
                    on(Delta::ToolCallStarted(call.name.clone()));
                }
            }
        }
    }

    let mut content = Vec::new();
    if !reasoning.is_empty() {
        // No signature: this API does not sign reasoning and does not ask for
        // one back. The field stays because the harness's block is shared.
        content.push(Block::Thinking { text: reasoning, signature: String::new() });
    }
    if !text.is_empty() {
        content.push(Block::Text(text));
    }
    let had_calls = !calls.is_empty();
    for (at, call) in calls.into_iter().enumerate() {
        if call.name.is_empty() {
            continue;
        }
        let input = if call.arguments.trim().is_empty() {
            Value::Object(Default::default())
        } else {
            serde_json::from_str(&call.arguments).unwrap_or(Value::Null)
        };
        // A server that streams a call without an id still needs one, because
        // the result has to name the call it answers.
        let id = if call.id.is_empty() { format!("call_{at}") } else { call.id };
        content.push(Block::ToolUse { id, name: call.name, input });
    }

    // A finish_reason is not guaranteed to arrive: some servers end the
    // stream with [DONE] and nothing else. Tool calls in hand say what it
    // was, and no tool calls means the answer is the answer.
    let stop = stop.unwrap_or(if had_calls { Stop::ToolUse } else { Stop::EndTurn });
    Ok(Assistant { content, stop, usage })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_results_become_their_own_messages() {
        // The one structural difference between this wire and the other.
        let messages = vec![
            ModelMessage::user_text("add 12 and 34"),
            ModelMessage {
                role: Role::Assistant,
                content: vec![
                    Block::Thinking { text: "punch it in".into(), signature: String::new() },
                    Block::ToolUse {
                        id: "call_1".into(),
                        name: "act".into(),
                        input: json!({"app": "awcalc"}),
                    },
                    Block::ToolUse {
                        id: "call_2".into(),
                        name: "act".into(),
                        input: json!({"app": "awcalc"}),
                    },
                ],
            },
            ModelMessage {
                role: Role::User,
                content: vec![
                    Block::ToolResult { id: "call_1".into(), content: "done".into(), is_error: false },
                    Block::ToolResult { id: "call_2".into(), content: "nope".into(), is_error: true },
                    Block::Text("The workspace changed".into()),
                ],
            },
        ];
        let body = request_body("local", "be helpful", &messages, &[], false);
        let out = body["messages"].as_array().unwrap();

        assert_eq!(out[0]["role"], "system");
        assert_eq!(out[1]["role"], "user");
        assert_eq!(out[2]["role"], "assistant");
        assert_eq!(out[2]["content"], Value::Null);
        assert_eq!(out[2]["reasoning_content"], "punch it in");
        assert_eq!(out[2]["tool_calls"][0]["function"]["name"], "act");
        // Arguments travel as a JSON string here, not as an object.
        assert_eq!(out[2]["tool_calls"][0]["function"]["arguments"], r#"{"app":"awcalc"}"#);
        // One tool message per result, in order, then the text.
        assert_eq!(out[3]["role"], "tool");
        assert_eq!(out[3]["tool_call_id"], "call_1");
        assert_eq!(out[3]["content"], "done");
        assert_eq!(out[4]["tool_call_id"], "call_2");
        assert_eq!(out[4]["content"], "error: nope");
        assert_eq!(out[5]["role"], "user");
        assert_eq!(out[5]["content"], "The workspace changed");
        assert_eq!(out.len(), 6);
    }

    #[test]
    fn thinking_is_off_unless_asked_for() {
        let quiet = request_body("local", "s", &[], &[], false);
        assert_eq!(quiet["chat_template_kwargs"]["enable_thinking"], false);
        let thoughtful = request_body("local", "s", &[], &[], true);
        assert!(thoughtful.get("chat_template_kwargs").is_none());
        // Usage has to be asked for or a streamed answer never reports any.
        assert_eq!(quiet["stream_options"]["include_usage"], true);
        assert_eq!(quiet["cache_prompt"], true);
    }

    #[test]
    fn streams_accumulate() {
        // Two calls whose argument fragments interleave, which is what an
        // index in the delta exists to make possible.
        let wire = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"press one\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Adding now.\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\
             \"function\":{\"name\":\"act\",\"arguments\":\"{\\\"app\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"c2\",\
             \"function\":{\"name\":\"act\",\"arguments\":\"{\\\"app\\\":\\\"b\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\
             \"function\":{\"arguments\":\"\\\"a\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1302,\"completion_tokens\":50,\
             \"prompt_tokens_details\":{\"cached_tokens\":1298}}}\n\n",
            "data: [DONE]\n\n",
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
            Block::Thinking { text: "press one".into(), signature: String::new() }
        );
        assert_eq!(assistant.content[1], Block::Text("Adding now.".into()));
        assert_eq!(
            assistant.content[2],
            Block::ToolUse { id: "c1".into(), name: "act".into(), input: json!({"app": "a"}) }
        );
        assert_eq!(
            assistant.content[3],
            Block::ToolUse { id: "c2".into(), name: "act".into(), input: json!({"app": "b"}) }
        );
        assert_eq!(
            assistant.usage,
            Usage { input: 1302, output: 50, cache_read: 1298, cache_write: 0 }
        );
        assert_eq!(deltas, ["think:press one", "text:Adding now.", "tool:act", "tool:act"]);
    }

    #[test]
    fn a_stream_that_never_says_why_it_stopped_is_read_from_what_it_sent() {
        let wire = "data: {\"choices\":[{\"delta\":{\"content\":\"All done.\"}}]}\n\ndata: [DONE]\n\n";
        let assistant = consume_stream(&mut wire.as_bytes(), &mut |_| {}).unwrap();
        assert_eq!(assistant.stop, Stop::EndTurn);
        assert_eq!(assistant.text(), "All done.");
    }

    #[test]
    fn an_error_mid_stream_is_an_error() {
        let wire = "data: {\"error\":{\"message\":\"context shift is disabled\"}}\n\n";
        let err = consume_stream(&mut wire.as_bytes(), &mut |_| {}).unwrap_err();
        assert!(err.message.contains("context shift"));
    }
}
