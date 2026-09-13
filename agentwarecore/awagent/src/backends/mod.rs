//! The backends, and how a turn's configuration becomes one.
//!
//! The agentdesk names a configuration from `awproto::turn::BACKENDS` on the
//! turn channel; this resolves that id against the same table and builds the
//! implementation it names. Several configurations share one backend: the
//! Claude models are one implementation asked for three different things. A
//! local model later is one new module here and one new match arm below;
//! nothing in the harness changes.

pub mod claude;
pub mod openai;

/// Where the local model server listens.
///
/// The host across the guest's slirp NIC, which is where llama-server runs
/// while the inference service is still being built and measured. It becomes
/// a service in PID 1's table with a socket handed over at fork, at which
/// point this constant goes; until then it is a development address and says
/// so rather than pretending to be configuration.
const LOCAL_HOST: &str = "10.0.2.2";
const LOCAL_PORT: u16 = 8080;

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
            let claude = claude::Claude::new(
                key.clone(),
                config.model.to_owned(),
                settings.anthropic_workspace.clone(),
            )
            .map_err(|err| err.to_string())?;
            Ok(Box::new(claude))
        }
        "openai" => Ok(Box::new(openai::OpenAi::new(
            LOCAL_HOST,
            LOCAL_PORT,
            config.model.to_owned(),
            // Off by default and a setting rather than a constant. It is
            // where the tokens go, and on a local model tokens are the whole
            // cost of an exchange; it is also what a draft model predicts
            // worst, so it loses at both ends. Measured on this machine:
            // acceptance 0.97 on a tool call and 0.52 on reasoning. What it
            // buys in correctness is the task suite's question, and the
            // setting is what lets the suite ask it.
            settings.local_thinking,
        ))),
        other => Err(format!("the {other} backend is not built yet")),
    }
}
