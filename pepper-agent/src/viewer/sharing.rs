//! Several agents in one viewer (spec 4.6). An agent takes a session
//! (`session new`, with an owner's name for the labels) and passes its token
//! with each command; the sources it opens are then its own: only it can
//! drive or close them, unless a command says `force` (the user asked).
//! Without `--source`, a command goes to the session's own source if it has
//! just one of the kind. Sources opened without a session are anyone's.
//!
//! A token is a capability between the user's own programs, as the socket
//! is: it keeps agents from stepping on each other by mistake, not out.

use super::Viewer;
use crate::error::{Error, Result};
use crate::json::Value;
use crate::{sys, text};

/// The most sessions a viewer keeps.
const MAX_SESSIONS: usize = 64;

impl Viewer {
    /// `session new`: a token for an agent, and the name its sources show.
    pub(super) fn new_session(&mut self, args: &Value) -> Result<Value> {
        let owner = match args.get_str("owner") {
            Some(owner) if super::valid_name(owner) && owner.len() <= 32 => owner.to_owned(),
            Some(owner) => {
                return Err(Error::new(
                    "BAD_ARGS",
                    format!("{:?} isn't an owner's name", text::inert(owner, 40)),
                )
                .hint("1 to 32 of A-Z a-z 0-9 . _ / -, from a letter or digit: agent-2."));
            }
            None => format!("agent-{}", self.sessions.len() + 1),
        };
        if self.sessions.len() >= MAX_SESSIONS {
            return Err(Error::new(
                "LIMIT_REACHED",
                format!("this viewer has {MAX_SESSIONS} sessions already"),
            ));
        }
        // The viewer's id first: the session's commands go to it, whichever
        // viewer is newest.
        let token = sys::random_hex()
            .map(|hex| format!("{}-{hex}", std::process::id()))
            .map_err(|e| Error::new("FAILED", format!("a token: {e}")))?;
        self.sessions.push((token.clone(), owner.clone()));
        Ok(Value::obj()
            .with("session", token)
            .with("owner", owner)
            .with(
                "next",
                "Pass --session <it> with every command (or set PEPPER_AGENT_SESSION), and give your sources names of your own (--name agent-2/app).",
            ))
    }

    /// The owner's name of a session token, if the viewer gave it.
    pub(super) fn session_owner(&self, token: &str) -> Option<&str> {
        self.sessions
            .iter()
            .find(|(t, _)| t == token)
            .map(|(_, o)| o.as_str())
    }

    /// Whom a new source belongs to, from a request: its session (which
    /// must be one of this viewer's) and the owner's name (`--owner`, or
    /// the session's).
    pub(super) fn claim(&self, args: &Value) -> Result<(Option<String>, Option<String>)> {
        let session = match args.get_str("session") {
            Some(token) if self.session_owner(token).is_some() => Some(token.to_owned()),
            Some(_) => return Err(no_session()),
            None => None,
        };
        let owner = match args.get_str("owner") {
            Some(owner) if super::valid_name(owner) && owner.len() <= 32 => Some(owner.to_owned()),
            Some(owner) => {
                return Err(Error::new(
                    "BAD_ARGS",
                    format!("{:?} isn't an owner's name", text::inert(owner, 40)),
                ));
            }
            None => session
                .as_deref()
                .and_then(|s| self.session_owner(s))
                .map(str::to_owned),
        };
        Ok((session, owner))
    }

    /// Whether a request may drive or close source `i`: one of no session's,
    /// or of the request's, or `force`.
    pub(super) fn check_owner(&self, i: usize, args: &Value) -> Result<()> {
        let s = &self.sources[i];
        let Some(theirs) = &s.session else {
            return Ok(());
        };
        if args.get_str("session") == Some(theirs.as_str()) || args.get_bool("force") == Some(true)
        {
            return Ok(());
        }
        if let Some(token) = args.get_str("session")
            && self.session_owner(token).is_none()
        {
            return Err(no_session());
        }
        Err(Error::new(
            "NOT_OWNER",
            format!(
                "{} is {}'s",
                s.name,
                s.owner.as_deref().unwrap_or("another agent")
            ),
        ))
    }

    /// The request's session's only source that `fits`, when it names none.
    pub(super) fn own_source(&self, args: &Value, fits: impl Fn(usize) -> bool) -> Option<usize> {
        if args.get_str("source").is_some() {
            return None;
        }
        let token = args.get_str("session")?;
        let mine: Vec<usize> = (0..self.sources.len())
            .filter(|&i| self.sources[i].session.as_deref() == Some(token) && fits(i))
            .collect();
        match mine.as_slice() {
            [only] => Some(*only),
            _ => None,
        }
    }
}

pub(super) fn no_session() -> Error {
    Error::new("BAD_ARGS", "this viewer gave no such session").hint(
        "Take one from this viewer: pepper-agent session new --owner <your name> (a viewer started again forgets them).",
    )
}
