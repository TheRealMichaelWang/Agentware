//! An earlier conversation to put in front of the prompt, for measuring
//! where a model's quality falls off with context.
//!
//! The task suite makes the conversation on the host (`tools/padding.py`:
//! what is in it and why is documented there), writes it into the run's own
//! state image, and names it on the kernel command line:
//!
//! ```text
//! agentware.pad-history=/state/pad.txt
//! ```
//!
//! The file alternates `H: ` and `A: ` lines, one message each; a line
//! indented by two spaces continues the message above it, which is how an
//! agent's entry carries the actions it took before its reply, as the
//! desk's history does. This module reads it and nothing more: what a
//! padded history should look like is a question for the measurement, not
//! for the agent, and the agent carries no opinion about it. Nothing here
//! runs unless the argument is present; a machine in ordinary use never
//! carries it.

use crate::backend::ModelMessage;

/// The argument, with its `=`.
pub const ARGUMENT: &str = "agentware.pad-history=";

/// The file the command line names, if it names one.
pub fn requested() -> Option<String> {
    let cmdline = std::fs::read_to_string("/proc/cmdline").ok()?;
    cmdline
        .split_whitespace()
        .find_map(|word| word.strip_prefix(ARGUMENT))
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
}

/// The conversation in the file, as alternating human and agent messages.
pub fn read(path: &str) -> std::io::Result<Vec<ModelMessage>> {
    Ok(parse(&std::fs::read_to_string(path)?))
}

fn parse(text: &str) -> Vec<ModelMessage> {
    let mut messages: Vec<(bool, String)> = Vec::new();
    for line in text.lines() {
        if let Some(said) = line.strip_prefix("H: ") {
            messages.push((true, said.to_owned()));
        } else if let Some(said) = line.strip_prefix("A: ") {
            messages.push((false, said.to_owned()));
        } else if let (Some(more), Some((_, last))) = (line.strip_prefix("  "), messages.last_mut()) {
            last.push('\n');
            last.push_str(more);
        }
    }
    // An agent's entry may carry its calls, in the desk's own shape, and is
    // expanded the same way the desk's history is.
    let mut next_id = 0;
    let mut out = Vec::new();
    for (human, said) in &messages {
        if *human {
            out.push(ModelMessage::user_text(said));
        } else {
            out.extend(crate::history::expand(said, &mut next_id));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Block, Role};

    #[test]
    fn the_file_is_read_as_it_is_written() {
        let text = format!(
            "# a comment\nH: Add 27 and 45.\nA: 27 + 45 = 72.\n  {}\n  \
             {{\"name\":\"open_app\",\"input\":{{\"name\":\"awcalc\"}}}}\n\nH: And 5 more?\nA: 77.\n",
            awproto::turn::HISTORY_CALLS
        );
        let messages = parse(&text);
        // human, then the agent's calls, their results, its reply; human; reply.
        assert_eq!(messages.len(), 6);
        assert_eq!(messages[0].role, Role::User);
        assert!(matches!(&messages[1].content[0], Block::ToolUse { name, .. } if name == "open_app"));
        assert!(matches!(&messages[2].content[0], Block::ToolResult { .. }));
        assert_eq!(messages[3], ModelMessage::assistant_text("27 + 45 = 72."));
        assert_eq!(messages[5], ModelMessage::assistant_text("77."));
        assert!(parse("").is_empty());
    }
}
