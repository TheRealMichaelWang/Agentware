//! The application toolkit.
//!
//! Applications are a model and a `render`, whole tree every time, and this
//! crate is written the same way: pieces of model that render to AWML and
//! accept events, for an application to embed in its own. Three dialogs so
//! far: files and folders, a yes-or-no, and a line of text. Nothing here talks
//! to the compositor or the supervisor. An application that uses a piece of
//! this toolkit puts its markup in its tree and hands it the events that name
//! its ids; the compositor sees one document, and so does the agent.

pub mod confirm;
pub mod filedialog;
pub mod prompt;

pub use confirm::{Confirm, Decision};
pub use filedialog::{Answer, FileDialog, Purpose};
pub use prompt::{Reply, TextPrompt};
