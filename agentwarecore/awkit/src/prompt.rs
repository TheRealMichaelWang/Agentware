//! A dialog that asks for one line of text: a name for a new folder, a new
//! name for a file.
//!
//! The same shape as the others: a `dialog` in the application's tree, one
//! field with the caret already in it, and two buttons. Enter in the field is
//! the same as the confirming button.

use awproto::display::{self, Event, escape};

const ID_VALUE: &str = "text-prompt-value";
const ID_OK: &str = "text-prompt-ok";
const ID_CANCEL: &str = "text-prompt-cancel";

/// What an event came to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Reply {
    /// Not the dialog's event.
    Ignored,
    /// The text changed and the dialog wants re-rendering.
    Changed,
    /// The human confirmed with this text.
    Done(String),
    Cancelled,
}

pub struct TextPrompt {
    label: String,
    /// What the field is for, shown as its placeholder and read by agents.
    hint: String,
    value: String,
    verb: String,
    error: Option<String>,
}

impl TextPrompt {
    /// `label` heads the dialog, `hint` says what to type, `initial` is what
    /// the field starts with, `verb` is the confirming button.
    pub fn new(label: &str, hint: &str, initial: &str, verb: &str) -> TextPrompt {
        TextPrompt {
            label: label.into(),
            hint: hint.into(),
            value: initial.into(),
            verb: verb.into(),
            error: None,
        }
    }

    /// The text as it stands.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Show why the last answer would not do, until the text changes.
    pub fn refuse(&mut self, why: &str) {
        self.error = Some(why.to_owned());
    }

    pub fn accept(&mut self, event: &Event) -> Reply {
        match (event.target.as_str(), event.action.as_str()) {
            (ID_VALUE, display::ACTION_TYPE_TEXT) => {
                self.value = event.value.clone();
                self.error = None;
                Reply::Changed
            }
            (ID_VALUE, display::ACTION_SUBMIT) | (ID_OK, display::ACTION_CLICK) => {
                Reply::Done(self.value.trim().to_owned())
            }
            (ID_CANCEL, display::ACTION_CLICK) => Reply::Cancelled,
            _ => Reply::Ignored,
        }
    }

    pub fn render(&self) -> String {
        let mut out = format!(
            "  <dialog label=\"{label}\">\n    <vstack gap=\"md\">\n      \
             <field id=\"{ID_VALUE}\" placeholder=\"{hint}\" value=\"{value}\" description=\"{hint}\"/>\n",
            label = escape(&self.label),
            hint = escape(&self.hint),
            value = escape(&self.value),
        );
        if let Some(error) = &self.error {
            out.push_str(&format!(
                "      <text role=\"caption\" color=\"danger\">{}</text>\n",
                escape(error)
            ));
        }
        out.push_str(&format!(
            "      <hstack gap=\"sm\">\n        <text grow=\"true\"/>\n        \
             <button id=\"{ID_CANCEL}\" label=\"Cancel\" description=\"Closes this dialog without doing anything\"/>\n        \
             <button id=\"{ID_OK}\" label=\"{verb}\" emphasis=\"primary\" description=\"{verb} with the text given\"/>\n      \
             </hstack>\n    </vstack>\n  </dialog>\n",
            verb = escape(&self.verb),
        ));
        out
    }
}
