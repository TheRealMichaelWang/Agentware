//! The backends, and how a turn's configuration becomes one.
//!
//! The agentdesk names a configuration from `awproto::turn::BACKENDS` on the
//! turn channel; this resolves that id against the same table and builds the
//! implementation it names. Several configurations share one backend: the
//! Claude models are one implementation asked for three different things. A
//! local model later is one new module here and one new match arm below;
//! nothing in the harness changes.

pub mod claude;

use awproto::settings::Settings;
use awproto::turn;

use crate::backend::Backend;

/// The backend a turn should run with, or a sentence for the pane saying why
/// there is none: the human reads this, so it says where to fix the problem.
pub fn from_config(id: &str, settings: &Settings) -> Result<Box<dyn Backend>, String> {
    let Some(config) = turn::backend_config(id) else {
        return Err(format!(
            "this agentdesk asked for a configuration this agent does not know ({id})"
        ));
    };
    match config.backend {
        "claude" => {
            let key = settings.anthropic_key.trim();
            if key.is_empty() {
                return Err(
                    "no Anthropic API key is set. Open Settings, choose the Agent page, \
                     enter a key, and send the message again"
                        .to_owned(),
                );
            }
            Ok(Box::new(claude::Claude::new(key.to_owned(), config.model.to_owned())))
        }
        other => Err(format!("the {other} backend is not built yet")),
    }
}
