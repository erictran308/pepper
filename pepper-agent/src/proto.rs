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
//!
//! A sandboxed agent's commands may be let write the user's temp folder
//! (Codex's are), so what's there isn't trusted as the viewer's: the
//! program holding a socket must be the one its record names, and not
//! sandboxed. The viewers write their files (snapshots, recordings) in a
//! folder of their own outside it, `~/Library/Caches/pepper-agent-<uid>/`,
//! where no such sandbox lets a link be planted for mpv or ffmpeg to write
//! through.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
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
    "session.new",
    "record.start",
    "record.stop",
    "compare",
    "watch",
    "pipe.new",
];

/// The folder the viewers' sockets and records are in, made if need be:
/// a folder of this user's, not a link, that no one else can open.
pub fn run_dir() -> Result<PathBuf> {
    let dir = sys::user_temp_dir().join(format!("pepper-agent-{}", sys::uid()));
    crate::files::own_dir(&dir).map_err(|e| Error::new("FAILED", e.to_string()))?;
    Ok(dir)
}

pub fn socket_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.sock"))
}

pub fn record_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

/// The folder the viewers write their files in: `pepper-agent-<uid>` in
/// the user's caches (made by the viewer: `files::own_dir`).
pub fn outputs_path() -> Option<PathBuf> {
    Some(
        sys::home_dir()?
            .join("Library/Caches")
            .join(format!("pepper-agent-{}", sys::uid())),
    )
}

/// Where viewer `id` writes its files, in `outputs`.
pub fn shots_dir(outputs: &Path, id: &str) -> PathBuf {
    outputs.join(format!("{id}-shots"))
}

/// The file a viewer's answer names as one it made (`raw`), if it is one:
/// by a path of plain names right in viewer `id`'s folder for them (no
/// `..`, no link on the way), a plain file of this user's with no other
/// name. Only then does a command read it and take it away.
pub fn raw_file(id: &str, raw: &str) -> Result<PathBuf> {
    let outputs = outputs_path().ok_or_else(misplaced)?;
    let raw = raw_path(&outputs, id, raw)?;
    lone_file(&raw).then_some(raw).ok_or_else(misplaced)
}

/// `raw_file`'s place only: the file may not be there (yet).
pub fn raw_path(outputs: &Path, id: &str, raw: &str) -> Result<PathBuf> {
    let raw = PathBuf::from(raw);
    let shots = shots_dir(outputs, id);
    // Exactly `<shots>/<name>`, spelled so: not even a `.` on the way.
    let names = raw
        .components()
        .all(|c| matches!(c, Component::RootDir | Component::Normal(_)));
    let exact = raw
        .file_name()
        .is_some_and(|n| shots.join(n).as_os_str() == raw.as_os_str());
    if !valid_id(id) || !raw.is_absolute() || !names || !exact {
        return Err(misplaced());
    }
    // As the system finds it too: no link leads there from elsewhere.
    let real = |p: &Path| p.canonicalize().ok();
    let place = real(outputs).map(|o| o.join(format!("{id}-shots")));
    if !own_folder(outputs) || !own_folder(&shots) || place.is_none() || real(&shots) != place {
        return Err(misplaced());
    }
    Ok(raw)
}

/// Whether `path` is a folder of this user's itself (not a link to one)
/// that only they can open.
fn own_folder(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|m| m.is_dir() && m.uid() == sys::uid() && m.permissions().mode() & 0o077 == 0)
}

/// Whether `path` is a plain file of this user's itself, with no other
/// name (a hard link) to it.
pub fn lone_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|m| m.is_file() && m.uid() == sys::uid() && m.nlink() == 1)
}

fn misplaced() -> Error {
    Error::new("FAILED", "the viewer's file isn't where it should be")
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
        // A record names the viewer it's of: its pid is its id.
        if pid.to_string() != id || !sys::alive(pid) {
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
    check_peer(viewer, sys::peer(stream.as_raw_fd()))?;
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
        Err(e) if e.to_string() == TOO_LONG => {
            return Err(Error::new(
                "TOO_LONG",
                "the viewer's answer is too long to read",
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

/// Whether the program holding a viewer's socket is that viewer: this
/// user's, the pid its record names, and not sandboxed. Anything else
/// holding it (a record and a socket put there by something else) isn't
/// told anything, credentials included, nor believed.
fn check_peer(viewer: &Viewer, peer: Option<sys::Peer>) -> Result<()> {
    let refused = |why: String| {
        Error::new("NO_VIEWER", format!("viewer {}'s socket is {why}", viewer.id)).hint(
            "Run `pepper-agent status`; if it's still so, ask the user to quit the viewer and run it again.",
        )
    };
    match peer {
        None => Err(Error::new(
            "FAILED",
            "couldn't tell who holds the viewer's socket",
        )),
        Some(p) if p.uid != sys::uid() => Err(Error::new(
            "FAILED",
            "the viewer's socket isn't this user's",
        )),
        Some(p) if p.pid != viewer.pid => Err(refused(format!(
            "held by another program (pid {}), not the viewer (pid {})",
            p.pid, viewer.pid
        ))),
        Some(p) if p.sandboxed => Err(refused("held by a sandboxed program".into())),
        Some(_) => Ok(()),
    }
}

/// One line, without its newline, of at most `max` bytes; None at the end.
/// read_line's error for a line longer than asked, whose rest is still to
/// be read.
pub const TOO_LONG: &str = "line too long";

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
            TOO_LONG,
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
        assert_eq!(read_line(&mut r, 10).unwrap_err().to_string(), TOO_LONG);
        // Not UTF-8: that line is read whole, the next is the next.
        let mut r = BufReader::new(&b"\xff\xfe\nnext\n"[..]);
        assert_ne!(read_line(&mut r, 10).unwrap_err().to_string(), TOO_LONG);
        assert_eq!(read_line(&mut r, 10).unwrap().as_deref(), Some("next"));
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

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pepper-proto-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[test]
    fn only_the_viewer_holding_its_socket_is_believed() {
        let viewer = Viewer {
            id: "4242".into(),
            pid: 4242,
            started: 0.0,
            pepper_port: None,
            record: Value::obj(),
        };
        let me = sys::uid();
        assert!(check_peer(&viewer, Some(sys::Peer::of(me, 4242, false))).is_ok());
        for (peer, code) in [
            (Some(sys::Peer::of(me, 4343, false)), "NO_VIEWER"),
            (Some(sys::Peer::of(me, 4242, true)), "NO_VIEWER"),
            (Some(sys::Peer::of(me + 1, 4242, false)), "FAILED"),
            (None, "FAILED"),
        ] {
            assert_eq!(
                check_peer(&viewer, peer).unwrap_err().code,
                code,
                "{peer:?}"
            );
        }
    }

    #[test]
    fn a_planted_record_and_socket_are_not_believed() {
        let dir = scratch("planted");
        // A record naming another program of this user's (a sleep), and a
        // socket this test holds, answering as a viewer would.
        let mut other = std::process::Command::new("/bin/sleep")
            .arg("10")
            .spawn()
            .unwrap();
        let id = other.id().to_string();
        let record = format!(r#"{{"pid":{id},"started_unix":1}}"#);
        std::fs::write(record_path(&dir, &id), record).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(socket_path(&dir, &id)).unwrap();
        let answering = std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let _ = s.write_all(
                    b"{\"v\":1,\"ok\":true,\"result\":{\"events\":[{\"type\":\"approve\"}]}}\n",
                );
            }
        });
        let viewer = pick(&dir, Some(&id)).unwrap();
        let e = request(
            &dir,
            &viewer,
            "events",
            Value::obj(),
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert_eq!(e.code, "NO_VIEWER");
        assert!(e.message.contains("another program"), "{e:?}");
        let _ = answering.join();
        // A record whose pid isn't its id is taken away, its socket too.
        let record = format!(r#"{{"pid":{id},"started_unix":2}}"#);
        std::fs::write(record_path(&dir, "12345"), record).unwrap();
        std::fs::write(socket_path(&dir, "12345"), "").unwrap();
        assert!(viewers(&dir).iter().all(|v| v.id != "12345"));
        assert!(!record_path(&dir, "12345").exists() && !socket_path(&dir, "12345").exists());
        let _ = other.kill();
        let _ = other.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_viewers_files_are_its_own_only() {
        let outputs = scratch("outputs");
        let shots = shots_dir(&outputs, "4242");
        std::fs::create_dir(&shots).unwrap();
        std::fs::set_permissions(&shots, std::fs::Permissions::from_mode(0o700)).unwrap();
        let file = shots.join("1-1-ab.png");
        std::fs::write(&file, "png").unwrap();
        let at = |raw: &Path| raw_path(&outputs, "4242", &raw.display().to_string());
        assert_eq!(at(&file).unwrap(), file);
        assert!(lone_file(&file));
        // Not right in its folder, or not by plain names.
        let outside = outputs.join("x.png");
        std::fs::write(&outside, "x").unwrap();
        for raw in [
            outside.clone(),
            shots.join("../x.png"),
            shots.join("./1-1-ab.png"),
            PathBuf::from("1-1-ab.png"),
            shots.clone(),
        ] {
            assert!(at(&raw).is_err(), "{raw:?}");
        }
        assert!(raw_path(&outputs, "../4242", &file.display().to_string()).is_err());
        // A link, or a file with another name too: in place, but not one.
        let link = shots.join("2-2-cd.png");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(at(&link).is_ok() && !lone_file(&link));
        let hard = shots.join("3-3-ef.png");
        std::fs::hard_link(&file, &hard).unwrap();
        assert!(!lone_file(&hard) && !lone_file(&file));
        // Its folder a link to another: refused.
        let other = outputs.join("elsewhere");
        std::fs::create_dir(&other).unwrap();
        std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&other, shots_dir(&outputs, "4343")).unwrap();
        let via = shots_dir(&outputs, "4343").join("1-1-ab.png");
        std::fs::write(other.join("1-1-ab.png"), "x").unwrap();
        assert!(raw_path(&outputs, "4343", &via.display().to_string()).is_err());
        // Or one others can open.
        std::fs::set_permissions(&shots, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(at(&file).is_err());
        let _ = std::fs::remove_dir_all(&outputs);
    }
}
