//! The private channel between an agentdesk and the agent it started.
//!
//! An agent has two channels and neither one is the supervisor. This is the
//! second: the socketpair PID 1 creates when the agentdesk asks for a turn,
//! handing one end to the agent at spawn (`AGENTWARE_DESK_FD`) and returning the
//! other to the agentdesk on the reply. Nothing on it ever passes through PID 1
//! or touches the filesystem, which is what lets an agent be sandboxed into its
//! own mount namespace later without losing its conversation.
//!
//! ```text
//!   backend   <id>               agentdesk -> agent, which configuration to run
//!   history   <role> <text>      agentdesk -> agent, once per earlier message
//!   prompt    <text>             agentdesk -> agent, the message that starts the turn
//!
//!   telemetry <kind> <text>      agent -> agentdesk, as the turn proceeds
//!   open-app  <name>             agent -> agentdesk, please open this application
//!   reply     <text>             agent -> agentdesk, the agent's answer; ends the turn
//! ```
//!
//! The context goes down first, in order, and `prompt` closes it: an agent reads
//! until it has the prompt and only then acts.
//!
//! `backend` names which of the configurations in [`BACKENDS`] the turn runs
//! with. The choice is the human's, made in the agentdesk's own pane, which is
//! chrome an agent cannot see or act on: an agent is told what it runs as and
//! cannot change it, the same way it is never asked which workspace it is in.
//! Per turn rather than per machine, because which model answers is a property
//! of the conversation being had, not of the machine having it; the key that
//! authenticates it is the machine's and lives in settings. Telemetry is anything the human
//! should see while the turn runs, in the words the agent chooses: a thought, an
//! action it is about to take, the result of one, an error. The reply is the
//! turn's message back to the human. After sending it the agent exits, and the
//! agentdesk learns the turn is over from the hangup rather than from a frame,
//! so an agent that dies mid-turn, or is interrupted, ends the turn the same way
//! one that finishes does.
//!
//! `open-app` is the one thing an agent may ask for rather than report. An agent
//! has no connection to the broker and no way to get one, so opening an
//! application is a request to the workspace, which is what asks PID 1, exactly
//! as it does when the human presses the launcher. The agent learns whether it
//! worked the way it learns everything: by asking the compositor what is open.
//!
//! The same framing as everything else on a socket in Agentware: a length, then
//! NUL-separated UTF-8 fields.

use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;

use crate::{DESK_FD_ENV, Decoder, encode};

pub const MSG_BACKEND: &str = "backend";
pub const MSG_HISTORY: &str = "history";
pub const MSG_PROMPT: &str = "prompt";
pub const MSG_TELEMETRY: &str = "telemetry";
pub const MSG_OPEN_APP: &str = "open-app";
pub const MSG_REPLY: &str = "reply";

/// One way the agent can be run: a backend, and the model it asks that
/// backend for. What the agentdesk's selector lists and what a turn names.
///
/// Several configurations share one backend: the three Claude models are one
/// implementation asked for three different things. The table lives here
/// because it is part of the turn contract: the agentdesk offers exactly
/// these, and the agent resolves the id it is handed against the same list,
/// so the two cannot drift.
pub struct BackendConfig {
    /// What travels on the wire and names the choice.
    pub id: &'static str,
    /// What the human sees in the selector.
    pub label: &'static str,
    /// Which implementation runs it. `claude` is the only one so far; a local
    /// model is a new name here and a new match arm in the agent.
    pub backend: &'static str,
    /// The model that backend is asked for.
    pub model: &'static str,
}

/// Every configuration an agentdesk offers, in the order shown: most capable
/// first, fastest last. The small end matters as much as the large: a Haiku
/// or a small Sonnet answers in a beat, which is what shows the interaction
/// model off rather than the model's own depth.
pub const BACKENDS: &[BackendConfig] = &[
    BackendConfig {
        id: "claude-opus-5",
        label: "Claude Opus 5",
        backend: "claude",
        model: "claude-opus-5",
    },
    BackendConfig {
        id: "claude-opus-4-8",
        label: "Claude Opus 4.8",
        backend: "claude",
        model: "claude-opus-4-8",
    },
    BackendConfig {
        id: "claude-sonnet-5",
        label: "Claude Sonnet 5",
        backend: "claude",
        model: "claude-sonnet-5",
    },
    BackendConfig {
        id: "claude-sonnet-4-6",
        label: "Claude Sonnet 4.6",
        backend: "claude",
        model: "claude-sonnet-4-6",
    },
    BackendConfig {
        id: "claude-haiku-4-5",
        label: "Claude Haiku 4.5",
        backend: "claude",
        model: "claude-haiku-4-5",
    },
];

/// What a fresh agentdesk runs until told otherwise.
pub const DEFAULT_BACKEND: &str = "claude-opus-5";

/// The configuration an id names, if it is one from the table.
pub fn backend_config(id: &str) -> Option<&'static BackendConfig> {
    BACKENDS.iter().find(|config| config.id == id)
}

/// Who said a line of the conversation.
pub const ROLE_HUMAN: &str = "human";
pub const ROLE_AGENT: &str = "agent";

// The kinds of telemetry an agentdesk knows how to show. An agent may send
// others; they are shown as plain lines rather than dropped, because the pane
// is where the human watches the turn and a kind nobody has heard of is still
// something that happened.
pub const KIND_THOUGHT: &str = "thought";
pub const KIND_ACTION: &str = "action";
pub const KIND_RESULT: &str = "result";
pub const KIND_ERROR: &str = "error";

/// Longest frame either side accepts. Conversation history can be long, so this
/// is looser than the control socket's ceiling and tighter than a document's.
pub const MAX_FRAME: usize = 256 * 1024;

/// One earlier message of the conversation, as it goes down to the agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub role: String,
    pub text: String,
}

/// Everything a turn starts with, as read off the channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Context {
    pub history: Vec<Message>,
    /// The message that starts the turn.
    pub prompt: String,
    /// The id of the configuration to run with; [`DEFAULT_BACKEND`] if the
    /// agentdesk did not say.
    pub backend: String,
}

/// Something the agent sent up while working.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Report {
    Telemetry { kind: String, text: String },
    /// Open this application into the workspace.
    OpenApp(String),
    Reply(String),
}

impl Report {
    pub fn from_fields(fields: &[String]) -> Option<Report> {
        match fields.first().map(String::as_str)? {
            MSG_TELEMETRY => Some(Report::Telemetry {
                kind: fields.get(1)?.clone(),
                text: fields.get(2).cloned().unwrap_or_default(),
            }),
            MSG_OPEN_APP => Some(Report::OpenApp(fields.get(1)?.clone())),
            MSG_REPLY => Some(Report::Reply(fields.get(1).cloned().unwrap_or_default())),
            _ => None,
        }
    }
}

/// The agent's end: read the context, then report as the turn proceeds.
pub struct Turn {
    stream: UnixStream,
    decoder: Decoder,
}

impl Turn {
    /// Adopt the descriptor named by `AGENTWARE_DESK_FD`.
    pub fn inherited() -> io::Result<Turn> {
        let raw: RawFd = std::env::var(DESK_FD_ENV)
            .map_err(|_| io::Error::other(format!("{DESK_FD_ENV} is not set")))?
            .parse()
            .map_err(|_| io::Error::other(format!("{DESK_FD_ENV} is not a descriptor number")))?;

        // SAFETY: the supervisor created this descriptor before forking us and
        // named it in our environment. Nothing else in this process owns it.
        let stream = unsafe { UnixStream::from_raw_fd(raw) };
        Ok(Turn { stream, decoder: Decoder::with_limit(MAX_FRAME) })
    }

    /// Block until the whole context has arrived: the configuration to run
    /// with, every earlier message, then the prompt that starts this turn.
    pub fn context(&mut self) -> io::Result<Context> {
        let mut history = Vec::new();
        let mut backend = DEFAULT_BACKEND.to_owned();
        loop {
            let fields = self.next_frame()?;
            match fields.first().map(String::as_str) {
                Some(MSG_BACKEND) => {
                    if let Some(id) = fields.get(1) {
                        backend = id.clone();
                    }
                }
                Some(MSG_HISTORY) => history.push(Message {
                    role: fields.get(1).cloned().unwrap_or_default(),
                    text: fields.get(2).cloned().unwrap_or_default(),
                }),
                Some(MSG_PROMPT) => {
                    return Ok(Context {
                        history,
                        prompt: fields.get(1).cloned().unwrap_or_default(),
                        backend,
                    });
                }
                // Something a future agentdesk says that this agent does not
                // know. Skipped rather than fatal, for the usual reason.
                _ => continue,
            }
        }
    }

    /// Tell the human what is happening.
    pub fn telemetry(&mut self, kind: &str, text: &str) -> io::Result<()> {
        self.stream.write_all(&encode(&[MSG_TELEMETRY, kind, text]))
    }

    /// Ask the workspace to open an application. Whether it did is learned
    /// from the compositor, by asking what is open.
    pub fn open_app(&mut self, name: &str) -> io::Result<()> {
        self.stream.write_all(&encode(&[MSG_OPEN_APP, name]))
    }

    /// The turn's answer. The agent should exit after this.
    pub fn reply(&mut self, text: &str) -> io::Result<()> {
        self.stream.write_all(&encode(&[MSG_REPLY, text]))
    }

    fn next_frame(&mut self) -> io::Result<Vec<String>> {
        loop {
            if let Some(fields) = self
                .decoder
                .next_frame()
                .map_err(|err| io::Error::other(err.to_string()))?
            {
                return Ok(fields);
            }
            let mut buf = [0u8; 8192];
            match self.stream.read(&mut buf) {
                Ok(0) => return Err(io::Error::other("the agentdesk closed the channel")),
                Ok(n) => self.decoder.feed(&buf[..n]),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        }
    }
}

/// The agentdesk's end: send the context down, then read reports as they come.
///
/// Non-blocking, because the agentdesk is also listening to the compositor and
/// to its clock. Poll [`AsFd`] and call [`Channel::pump`] when it is readable.
pub struct Channel {
    stream: UnixStream,
    decoder: Decoder,
}

impl Channel {
    /// Wrap the descriptor the supervisor returned on the `start-agent` reply.
    pub fn new(stream: UnixStream) -> io::Result<Channel> {
        stream.set_nonblocking(true)?;
        Ok(Channel { stream, decoder: Decoder::with_limit(MAX_FRAME) })
    }

    /// Send the whole context: the configuration to run with, every earlier
    /// message, then the prompt.
    ///
    /// Written in one go. The frames are small next to a socket buffer, and an
    /// agent that has not read its context yet is one that has not started
    /// doing anything, so a full buffer here would mean an agent that never
    /// started at all, which the write error reports.
    pub fn send_context(&mut self, history: &[Message], prompt: &str, backend: &str) -> io::Result<()> {
        let mut bytes = Vec::new();
        bytes.extend(encode(&[MSG_BACKEND, backend]));
        for message in history {
            bytes.extend(encode(&[MSG_HISTORY, &message.role, &message.text]));
        }
        bytes.extend(encode(&[MSG_PROMPT, prompt]));
        self.stream.set_nonblocking(false)?;
        let result = self.stream.write_all(&bytes);
        self.stream.set_nonblocking(true)?;
        result
    }

    /// Read whatever the agent has sent. Returns `false` once the agent has
    /// hung up, which is how a turn ends; reports already read are still
    /// available through [`Channel::take_report`].
    pub fn pump(&mut self) -> io::Result<bool> {
        loop {
            let mut buf = [0u8; 8192];
            match self.stream.read(&mut buf) {
                Ok(0) => return Ok(false),
                Ok(n) => self.decoder.feed(&buf[..n]),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(true),
                Err(err) => return Err(err),
            }
        }
    }

    /// The next report already read, if there is one.
    pub fn take_report(&mut self) -> io::Result<Option<Report>> {
        loop {
            let frame = self
                .decoder
                .next_frame()
                .map_err(|err| io::Error::other(err.to_string()))?;
            match frame {
                Some(fields) => match Report::from_fields(&fields) {
                    Some(report) => return Ok(Some(report)),
                    None => continue,
                },
                None => return Ok(None),
            }
        }
    }
}

impl AsFd for Channel {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.stream.as_fd()
    }
}
