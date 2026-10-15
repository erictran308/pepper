//! How commands reach a viewer: its control socket and the record next to
//! it, in a folder only the user can open.
//!
//! `<user temp>/pepper-agent-<uid>/` (mode 0700) holds, per viewer,
//! `<id>.sock` (0600) and `<id>.json` (0600: its pid, start, project, the
//! Pepper it runs in, its sources). A command connects, checks the viewer is
//! the same user, sends one request and reads one answer, each a line of
//! JSON:
//!
//! ```text
//! → {"v": 1, "cmd": "seek", "args": {"to": "1:23.5"}}
//! ← {"v": 1, "ok": true, "result": {…}}
//! ← {"v": 1, "ok": false, "error": {"code": "…", "message": "…", "hint": "…"}}
//! ```
//!
//! The viewer takes only the commands in [`COMMANDS`], a request of at most
//! [`MAX_REQUEST`] bytes, and version [`VERSION`]; nothing is passed through
//! to mpv.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::json::{self, Value};
use crate::sys;

pub const VERSION: f64 = 1.0;
/// A request's most bytes, its newline included.
pub const MAX_REQUEST: usize = 64 * 1024;
/// An answer's most bytes.
pub const MAX_ANSWER: usize = 16 * 1024 * 1024;

/// What a viewer does on request; anything else is refused.
pub const COMMANDS: &[&str] = &[
    "status",
    "sources",
    "open",
    "close",
    "layout",
    "focus",
    "snapshot",
    "say",
    "events",
    "play",
    "pause",
    "toggle",
    "seek",
    "loop",
    "where",
    "review",
    "web.open",
    "web.goto",
    "web.reload",
    "web.back",
    "web.click",
    "web.type",
    "web.press",
    "web.scroll",
    "web.hover",
    "web.viewport",
    "web.wait-for",
    "web.shot",
    "web.console",
    "web.errors",
    "web.dom",
    "web.a11y",
    "web.eval",
];

/// The folder the viewers' sockets and records are in, made if need be:
/// a folder of this user's, not a link, that no one else can open.
pub fn run_dir() -> Result<PathBuf> {
    let dir = sys::user_temp_dir().join(format!("pepper-agent-{}", sys::uid()));
    let fail = |what: String| Error::new("FAILED", what);
    match std::fs::symlink_metadata(&dir) {
        Ok(meta) => {
            if !meta.is_dir() || meta.file_type().is_symlink() {
                return Err(fail(format!("{} isn't a folder", dir.display())));
            }
            if meta.uid() != sys::uid() {
                return Err(fail(format!("{} isn't yours", dir.display())));
            }
            if meta.mode() & 0o077 != 0 {
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                    .map_err(|e| fail(format!("{}: {e}", dir.display())))?;
            }
        }
        Err(_) => {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&dir)
                .map_err(|e| fail(format!("cannot make {}: {e}", dir.display())))?;
        }
    }
    Ok(dir)
}

pub fn socket_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.sock"))
}

pub fn record_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

/// A viewer, as its record says.
#[derive(Clone, Debug)]
pub struct Viewer {
    pub id: String,
    pub pid: u32,
    pub started: f64,
    pub pepper_port: Option<String>,
    pub record: Value,
}

/// The viewers that run, most recent first. Records of viewers that ended
/// without taking them away are taken away.
pub fn viewers(dir: &Path) -> Vec<Viewer> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
            continue;
        };
        if !valid_id(id) {
            continue;
        }
        let record = std::fs::read_to_string(entry.path())
            .ok()
            .and_then(|text| json::parse(&text).ok());
        let pid = record
            .as_ref()
            .and_then(|r| r.get_num("pid"))
            .map(|p| p as u32)
            .unwrap_or(0);
        if !sys::alive(pid) {
            let _ = std::fs::remove_file(entry.path());
            let _ = std::fs::remove_file(socket_path(dir, id));
            continue;
        }
        let Some(record) = record else { continue };
        found.push(Viewer {
            id: id.to_owned(),
            pid,
            started: record.get_num("started_unix").unwrap_or(0.0),
            pepper_port: record.get_str("pepper_port").map(str::to_owned),
            record,
        });
    }
    found.sort_by(|a, b| b.started.total_cmp(&a.started));
    found
}

/// A viewer's id: its pid, in digits.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 10 && id.bytes().all(|b| b.is_ascii_digit())
}

/// The viewer to talk to: the one named, or the most recent, one in the same
/// Pepper as this shell first.
pub fn pick(dir: &Path, named: Option<&str>) -> Result<Viewer> {
    let all = viewers(dir);
    if let Some(id) = named {
        return all.into_iter().find(|v| v.id == id).ok_or_else(|| {
            Error::new(
                "NO_VIEWER",
                format!("no viewer {} is running", crate::text::inert(id, 20)),
            )
            .hint("Run `pepper-agent status` for the viewers that run.")
        });
    }
    let port = std::env::var("PEPPER_PORT").ok();
    let same = all
        .iter()
        .position(|v| port.is_some() && v.pepper_port == port)
        .unwrap_or(0);
    all.into_iter()
        .nth(same)
        .ok_or_else(|| Error::new("NO_VIEWER", "no viewer is running"))
}

/// Sends a request and waits up to `wait` for the answer.
pub fn request(
    dir: &Path,
    viewer: &Viewer,
    cmd: &str,
    args: Value,
    wait: Duration,
) -> Result<Value> {
    let path = socket_path(dir, &viewer.id);
    let gone = |e: std::io::Error| {
        Error::new(
            "NO_VIEWER",
            format!("viewer {} doesn't answer: {e}", viewer.id),
        )
    };
    let mut stream = UnixStream::connect(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            Error::new("FAILED", format!("not allowed to connect to {}: {e}", path.display())).hint(format!(
                "A sandbox around your commands blocks the viewer's socket: allow Unix sockets in {}.",
                dir.display()
            ))
        } else {
            gone(e)
        }
    })?;
    if sys::peer_uid(stream.as_raw_fd()) != Some(sys::uid()) {
        return Err(Error::new(
            "FAILED",
            "the viewer's socket isn't this user's",
        ));
    }
    let line = Value::obj()
        .with("v", VERSION)
        .with("cmd", cmd)
        .with("args", args)
        .to_line()
        + "\n";
    if line.len() > MAX_REQUEST {
        return Err(Error::new(
            "BAD_ARGS",
            "the request is too long (64 KB at most)",
        ));
    }
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .and_then(|()| stream.write_all(line.as_bytes()))
        .and_then(|()| stream.set_read_timeout(Some(wait)))
        .map_err(gone)?;
    let answer = match read_line(&mut BufReader::new(&stream), MAX_ANSWER) {
        Ok(Some(answer)) => answer,
        Ok(None) => return Err(Error::new("FAILED", "the viewer closed the connection")),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            return Err(Error::new(
                "TIMEOUT",
                format!("the viewer didn't answer in {} s", wait.as_secs()),
            ));
        }
        Err(e) => return Err(gone(e)),
    };
    let answer = json::parse(&answer)
        .map_err(|e| Error::new("FAILED", format!("the viewer's answer: {e}")))?;
    if answer.get_bool("ok") == Some(true) {
        Ok(answer.get("result").cloned().unwrap_or_else(Value::obj))
    } else {
        Err(Error::from_json(
            answer.get("error").unwrap_or(&Value::Null),
        ))
    }
}

/// One line, without its newline, of at most `max` bytes; None at the end.
pub fn read_line(reader: &mut impl BufRead, max: usize) -> std::io::Result<Option<String>> {
    let mut buf = Vec::new();
    let n = reader.take(max as u64 + 1).read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
    } else if buf.len() > max {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "line too long",
        ));
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "not UTF-8"))
}

/// A request as the viewer reads it: its command, if allowed, and its
/// arguments.
pub fn parse_request(line: &str) -> Result<(String, Value)> {
    let bad = |m: String| Error::new("BAD_REQUEST", m);
    let request = json::parse(line).map_err(bad)?;
    if request.get_num("v") != Some(VERSION) {
        return Err(Error::new(
            "UNSUPPORTED",
            "this viewer speaks version 1 of the protocol",
        )
        .hint("Use the pepper-agent that came with this Pepper."));
    }
    let cmd = request.get_str("cmd").ok_or_else(|| bad("no cmd".into()))?;
    if !COMMANDS.contains(&cmd) {
        return Err(Error::new(
            "UNSUPPORTED",
            format!(
                "the viewer has no command {:?}",
                crate::text::inert(cmd, 40)
            ),
        ));
    }
    let args = request.get("args").cloned().unwrap_or_else(Value::obj);
    if !matches!(args, Value::Obj(_)) {
        return Err(bad("args must be an object".into()));
    }
    Ok((cmd.to_owned(), args))
}

pub fn answer_ok(result: Value) -> String {
    Value::obj()
        .with("v", VERSION)
        .with("ok", true)
        .with("result", result)
        .to_line()
        + "\n"
}

pub fn answer_err(error: &Error) -> String {
    let mut answer = error.to_json();
    answer.set("v", VERSION);
    answer.set("ok", false);
    answer.to_line() + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_checked() {
        let ok = parse_request(r#"{"v":1,"cmd":"seek","args":{"to":"1:00"}}"#).unwrap();
        assert_eq!(ok.0, "seek");
        assert_eq!(ok.1.get_str("to"), Some("1:00"));
        assert_eq!(
            parse_request(r#"{"v":1,"cmd":"status"}"#).unwrap().0,
            "status"
        );
        let code = |line: &str| parse_request(line).unwrap_err().code;
        assert_eq!(code(r#"{"v":1,"cmd":"run","args":{}}"#), "UNSUPPORTED");
        assert_eq!(code(r#"{"v":1,"cmd":"loadfile"}"#), "UNSUPPORTED");
        assert_eq!(code(r#"{"v":2,"cmd":"status"}"#), "UNSUPPORTED");
        assert_eq!(code(r#"{"cmd":"status"}"#), "UNSUPPORTED");
        assert_eq!(code(r#"{"v":1,"cmd":"seek","args":[1]}"#), "BAD_REQUEST");
        assert_eq!(code("not json"), "BAD_REQUEST");
    }

    #[test]
    fn lines_are_capped() {
        let mut r = BufReader::new(&b"abc\ndef"[..]);
        assert_eq!(read_line(&mut r, 10).unwrap().as_deref(), Some("abc"));
        assert_eq!(read_line(&mut r, 10).unwrap().as_deref(), Some("def"));
        assert_eq!(read_line(&mut r, 10).unwrap(), None);
        let mut r = BufReader::new(&b"0123456789abc\n"[..]);
        assert!(read_line(&mut r, 10).is_err());
        let mut r = BufReader::new(&b"0123456789\n"[..]);
        assert_eq!(
            read_line(&mut r, 10).unwrap().as_deref(),
            Some("0123456789")
        );
    }

    #[test]
    fn ids_are_digits() {
        assert!(valid_id("12345"));
        assert!(!valid_id("../x"));
        assert!(!valid_id(""));
        assert!(!valid_id("12345678901"));
    }
}
