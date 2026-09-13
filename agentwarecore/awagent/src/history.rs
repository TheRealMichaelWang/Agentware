//! An agent's earlier turn, as the model should see it.
//!
//! The agentdesk keeps a turn as the reply the human read, followed by the
//! tool calls the turn made (`awproto::turn::HISTORY_CALLS`, one JSON call
//! per line). Here that becomes what the model would have seen had it been
//! there: an assistant message making the calls, a user message answering
//! each with a placeholder, and the assistant's reply. The results are not
//! kept (they are the tokens, tens of thousands in a long turn), so the
//! placeholder says only that the call was made.
//!
//! Why the calls have to be real tool-use blocks and not a list in the
//! reply's text: measured 12 September 2026, a history of forty turns whose
//! entries were reply-only showed the model no tool call anywhere, and it
//! answered new requests with a report of work not done, 0 of 15 short
//! tasks; the same history with the calls written as text above each reply
//! had it writing that text itself, still calling nothing. Text describing
//! calls is text the model can produce. Tool-use blocks are not.

use serde_json::Value;

use crate::backend::{Block, ModelMessage, Role};

/// The messages one history entry stands for. `next_id` numbers the
/// synthetic tool-use ids uniquely across the conversation.
pub fn expand(text: &str, next_id: &mut usize) -> Vec<ModelMessage> {
    let marker = format!("\n{}\n", awproto::turn::HISTORY_CALLS);
    let Some((reply, trailer)) = text.split_once(&marker) else {
        return vec![ModelMessage::assistant_text(text)];
    };
    let calls: Vec<(String, Value)> = trailer
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter_map(|call| Some((call.get("name")?.as_str()?.to_owned(), call.get("input")?.clone())))
        .collect();
    if calls.is_empty() {
        return vec![ModelMessage::assistant_text(reply)];
    }
    let mut uses = Vec::new();
    let mut results = Vec::new();
    for (name, input) in calls {
        let id = format!("earlier-{}", *next_id);
        *next_id += 1;
        uses.push(Block::ToolUse { id: id.clone(), name, input });
        results.push(Block::ToolResult {
            id,
            content: "done (the result is not kept in the conversation's history)".to_owned(),
            is_error: false,
        });
    }
    vec![
        ModelMessage { role: Role::Assistant, content: uses },
        ModelMessage { role: Role::User, content: results },
        ModelMessage::assistant_text(reply),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_alone_is_one_message() {
        let mut n = 0;
        let out = expand("12 + 34 = 46.", &mut n);
        assert_eq!(out.len(), 1);
        assert_eq!(n, 0);
    }

    #[test]
    fn calls_become_tool_use_then_results_then_the_reply() {
        let text = format!(
            "27 + 45 = 72.\n{}\n{{\"name\":\"open_app\",\"input\":{{\"name\":\"awcalc\"}}}}\n\
             {{\"name\":\"act\",\"input\":{{\"instance\":\"awcalc#1\",\"action\":\"click\",\"target\":\"two\"}}}}\n",
            awproto::turn::HISTORY_CALLS
        );
        let mut n = 5;
        let out = expand(&text, &mut n);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].role, Role::Assistant);
        assert_eq!(out[0].content.len(), 2);
        match &out[0].content[1] {
            Block::ToolUse { id, name, input } => {
                assert_eq!(id, "earlier-6");
                assert_eq!(name, "act");
                assert_eq!(input["target"], "two");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(out[1].role, Role::User);
        assert!(matches!(&out[1].content[0], Block::ToolResult { id, .. } if id == "earlier-5"));
        assert_eq!(out[2], ModelMessage::assistant_text("27 + 45 = 72."));
        assert_eq!(n, 7);
    }

    #[test]
    fn a_trailer_with_nothing_parseable_is_just_the_reply() {
        let text = format!("Done.\n{}\nnot json\n", awproto::turn::HISTORY_CALLS);
        let mut n = 0;
        assert_eq!(expand(&text, &mut n), vec![ModelMessage::assistant_text("Done.")]);
    }
}
