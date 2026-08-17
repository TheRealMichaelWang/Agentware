//! A yes-or-no dialog.
//!
//! For the actions that cannot be undone: a heading, a sentence saying what is
//! about to happen, and two buttons. The same shape as every dialog here: a
//! `dialog` in the application's tree, modal by the compositor, and to the
//! agent two buttons nested in `<dialog>` with descriptions that say what each
//! does. An agent that reads them knows exactly what confirming means, which
//! is the whole reason to ask.

use awproto::display::{self, Event, escape};

const ID_YES: &str = "confirm-dialog-yes";
const ID_CANCEL: &str = "confirm-dialog-cancel";

/// What an event came to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Decision {
    /// Not the dialog's event.
    Ignored,
    Confirmed,
    Cancelled,
}

pub struct Confirm {
    label: String,
    message: String,
    /// The confirming button's word: "Delete", "Replace".
    verb: String,
    /// Whether confirming destroys something, which colours the button.
    danger: bool,
}

impl Confirm {
    pub fn new(label: &str, message: &str, verb: &str) -> Confirm {
        Confirm { label: label.into(), message: message.into(), verb: verb.into(), danger: false }
    }

    /// Mark the confirming action as destructive.
    pub fn danger(mut self) -> Confirm {
        self.danger = true;
        self
    }

    pub fn accept(&mut self, event: &Event) -> Decision {
        match (event.target.as_str(), event.action.as_str()) {
            (ID_YES, display::ACTION_CLICK) => Decision::Confirmed,
            (ID_CANCEL, display::ACTION_CLICK) => Decision::Cancelled,
            _ => Decision::Ignored,
        }
    }

    pub fn render(&self) -> String {
        format!(
            "  <dialog label=\"{label}\">\n    <vstack gap=\"md\">\n      <text>{message}</text>\n      \
             <hstack gap=\"sm\">\n        <text grow=\"true\"/>\n        \
             <button id=\"{ID_CANCEL}\" label=\"Cancel\" description=\"Closes this dialog without doing anything\"/>\n        \
             <button id=\"{ID_YES}\" label=\"{verb}\" emphasis=\"{emphasis}\" description=\"{description}\"/>\n      \
             </hstack>\n    </vstack>\n  </dialog>\n",
            label = escape(&self.label),
            message = escape(&self.message),
            verb = escape(&self.verb),
            emphasis = if self.danger { "danger" } else { "primary" },
            description = escape(&format!("{}: {}", self.verb, self.message)),
        )
    }
}
