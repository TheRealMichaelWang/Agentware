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
            // The type carries the guarantee: a `Some` key is never empty.
            let Some(key) = &settings.anthropic_key else {
                return Err(
                    "no Anthropic API key is set. Open Settings, choose the Agent page, \
                     enter a key, and send the message again"
                        .to_owned(),
                );
            };
            // The workspace goes with the key, not with the model: it says
            // where an identity-linked key acts, and is `None` for an
            // ordinary one, which already says so itself.
            Ok(Box::new(claude::Claude::new(
                key.clone(),
                config.model.to_owned(),
                settings.anthropic_workspace.clone(),
            )))
        }
        other => Err(format!("the {other} backend is not built yet")),
    }
}
