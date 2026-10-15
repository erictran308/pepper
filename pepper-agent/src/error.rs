//! Errors as an agent reads them: a code to act on, what happened, and what
//! to do next. Written to stderr as `{"error": {"code", "message", "hint"}}`.

use crate::json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    pub code: &'static str,
    pub message: String,
    pub hint: String,
}

pub const NO_VIEWER_HINT: &str =
    "Ask the user to open a split in Pepper and run: pepper-agent view";

impl Error {
    /// An error with the code's usual hint.
    pub fn new(code: &'static str, message: impl Into<String>) -> Error {
        Error {
            code,
            message: message.into(),
            hint: hint(code).to_owned(),
        }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Error {
        self.hint = hint.into();
        self
    }

    pub fn to_json(&self) -> Value {
        Value::obj().with(
            "error",
            Value::obj()
                .with("code", self.code)
                .with("message", self.message.as_str())
                .with("hint", self.hint.as_str()),
        )
    }

    /// From the `error` member a viewer answered with.
    pub fn from_json(error: &Value) -> Error {
        let code = error.get_str("code").unwrap_or("FAILED");
        // Only codes this program knows; anything else is FAILED.
        let code = CODES
            .iter()
            .find(|c| **c == code)
            .copied()
            .unwrap_or("FAILED");
        Error {
            code,
            message: error.get_str("message").unwrap_or("").to_owned(),
            hint: error
                .get_str("hint")
                .unwrap_or_else(|| hint(code))
                .to_owned(),
        }
    }
}

pub const CODES: &[&str] = &[
    "NO_VIEWER",
    "NO_SOURCE",
    "NOT_PLAYABLE",
    "LIMIT_REACHED",
    "MISSING_DEP",
    "UNSUPPORTED",
    "PERMISSION_NEEDED",
    "ORIGIN_BLOCKED",
    "NOT_OWNER",
    "INVALID_PROJECT",
    "TIMEOUT",
    "BAD_TIME",
    "BAD_ARGS",
    "BAD_REQUEST",
    "NOT_FOUND",
    "NOT_IN_PEPPER",
    "LOAD_FAILED",
    "FAILED",
];

fn hint(code: &str) -> &'static str {
    match code {
        "NO_VIEWER" => NO_VIEWER_HINT,
        "NO_SOURCE" => "Run `pepper-agent sources` for the names, or `pepper-agent open <file>`.",
        "NOT_PLAYABLE" => "Playback commands work on media sources (files and video URLs).",
        "LIMIT_REACHED" => {
            "A viewer shows at most 4 sources: close one (`pepper-agent close <name>`), or ask \
             the user to run another viewer (`pepper-agent view`) and pass --viewer <id>."
        }
        "MISSING_DEP" => "Install it (the message says how), then run the command again.",
        "UNSUPPORTED" => "Run `pepper-agent guide` for what this version does.",
        "TIMEOUT" => {
            "Nothing yet: run it again to keep waiting, or ask the user with `pepper-agent say`."
        }
        "BAD_TIME" => "Times look like 83.5, 1:23.5 or 00:01:23.500; seek also takes +5 and -2.",
        "BAD_ARGS" => "Run `pepper-agent <command> --help` for its arguments and an example.",
        "BAD_REQUEST" => {
            "The viewer didn't understand the request: are pepper-agent and the viewer the same version?"
        }
        "NOT_FOUND" => "Check the path; a relative one is from this shell's directory.",
        "NOT_IN_PEPPER" => {
            "The viewer runs only in a Pepper tab, not in tmux: open a split in Pepper and run \
             `pepper-agent view` there."
        }
        "LOAD_FAILED" => {
            "Check the file with `pepper-agent probe <file>`; for a web address, that yt-dlp is installed."
        }
        _ => "Run `pepper-agent status` to see what's there.",
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_as_json() {
        let e = Error::new("NO_VIEWER", "no viewer is running");
        assert_eq!(
            e.to_json().to_line(),
            format!(
                r#"{{"error":{{"code":"NO_VIEWER","message":"no viewer is running","hint":"{NO_VIEWER_HINT}"}}}}"#
            )
        );
        let back = Error::from_json(e.to_json().get("error").unwrap());
        assert_eq!(back, e);
        let odd = Error::from_json(&Value::obj().with("code", "\x1b[2J").with("message", "x"));
        assert_eq!(odd.code, "FAILED");
    }
}
