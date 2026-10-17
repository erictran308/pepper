//! A source's player: pepper-play, run as an app's player (README "From an
//! app"), with mpv's JSON IPC over a socketpair as its file descriptor 3.
//! The viewer gives it the box's token over the IPC and places the box
//! itself; pepper-play never touches the terminal.
//!
//! What pepper-play says (IPC lines, its stderr) is read on threads of
//! its own and handed to the viewer's loop. It decodes what it's given, so
//! its lines are taken as untrusted: capped in length, parsed, and only
//! known fields used.

use std::io::{BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use super::Msg;
use crate::json::{self, Value};
use crate::proto::read_line;
use crate::sys;

/// The longest IPC line taken from pepper-play.
const MAX_LINE: usize = 1024 * 1024;
/// The longest stderr line kept.
const MAX_LOG_LINE: usize = 400;

/// The properties a source follows, by observe id.
pub const OBSERVED: &[&str] = &[
    "time-pos",
    "duration",
    "pause",
    "eof-reached",
    "paused-for-cache",
    "dwidth",
    "dheight",
    "hwdec-current",
    "idle-active",
    "container-fps",
];

/// What a player's threads tell the viewer.
pub enum Event {
    /// A line of mpv's IPC.
    Line(Value),
    /// A line of pepper-play's stderr, made inert.
    Log(String),
    /// The IPC closed: pepper-play ended.
    Gone,
}

pub struct Player {
    child: Child,
    ipc: UnixStream,
    next_id: u64,
}

/// pepper-play's path: `PEPPER_PLAY`, which Pepper gives every shell; or
/// the one next to this program (both are in Pepper.app's Contents/MacOS);
/// or one on the PATH.
pub fn find_play() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PEPPER_PLAY").map(PathBuf::from)
        && path.is_absolute()
        && path.is_file()
    {
        return Some(path);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(next) = exe.parent().map(|dir| dir.join("pepper-play"))
        && next.is_file()
    {
        return Some(next);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("pepper-play"))
            .find(|path| path.is_absolute() && path.is_file())
    })
}

impl Player {
    /// Starts pepper-play, idle and paused, and gives it the box's token.
    pub fn start(
        play: &Path,
        sid: u64,
        token: &str,
        tx: Sender<Msg>,
        debug: bool,
    ) -> std::io::Result<Player> {
        let (ours, theirs) = UnixStream::pair()?;
        let theirs_fd = theirs.as_raw_fd();
        let mut command = Command::new(play);
        command
            .args([
                "--input-ipc-client=fd://3",
                "--idle=yes",
                // At the end the last frame stays, paused, to look at.
                "--keep-open=yes",
                "--pause=yes",
                // A picture stays up until closed.
                "--image-display-duration=inf",
                // Seeks land on the frame asked for, not a keyframe.
                "--hr-seek=yes",
            ])
            .env_remove("PEPPER_TOKEN")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if debug {
            command.env("PEPPER_PLAY_DEBUG", "1");
        } else {
            command.env_remove("PEPPER_PLAY_DEBUG");
        }
        // SAFETY: as_fd3 makes only async-signal-safe calls.
        unsafe { command.pre_exec(move || sys::as_fd3(theirs_fd)) };
        let mut child = command.spawn()?;
        drop(theirs);

        if let Some(stderr) = child.stderr.take() {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stderr);
                loop {
                    match read_line(&mut reader, MAX_LINE) {
                        Ok(Some(line)) => {
                            let line = crate::text::redact_urls(&crate::text::inert(
                                line.trim_end(),
                                MAX_LOG_LINE,
                            ));
                            if tx.send(Msg::Player(sid, Event::Log(line))).is_err() {
                                return;
                            }
                        }
                        Ok(None) => return,
                        // Too long or not UTF-8: skipped.
                        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {}
                        Err(_) => return,
                    }
                }
            });
        }
        let reader = ours.try_clone()?;
        std::thread::spawn(move || {
            let mut reader = BufReader::new(reader);
            loop {
                match read_line(&mut reader, MAX_LINE) {
                    Ok(Some(line)) => {
                        if let Ok(value) = json::parse(&line)
                            && tx.send(Msg::Player(sid, Event::Line(value))).is_err()
                        {
                            return;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {}
                    Ok(None) | Err(_) => {
                        let _ = tx.send(Msg::Player(sid, Event::Gone));
                        return;
                    }
                }
            }
        });

        ours.set_write_timeout(Some(Duration::from_secs(2)))?;
        let mut player = Player {
            child,
            ipc: ours,
            next_id: 1,
        };
        player.command(&["script-message-to", "main", "pepper-token", token]);
        for (i, name) in OBSERVED.iter().enumerate() {
            player.send(vec![
                Value::from("observe_property"),
                Value::from(i as u32 + 1),
                Value::from(*name),
            ]);
        }
        Ok(player)
    }

    /// Sends a command of mpv's; returns its request id, which its reply
    /// carries.
    pub fn send(&mut self, args: Vec<Value>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let line = Value::obj()
            .with("command", Value::Arr(args))
            .with("request_id", id)
            .to_line()
            + "\n";
        let _ = self.ipc.write_all(line.as_bytes());
        id
    }

    pub fn command(&mut self, args: &[&str]) -> u64 {
        self.send(args.iter().map(|a| Value::from(*a)).collect())
    }

    pub fn set(&mut self, name: &str, value: impl Into<Value>) -> u64 {
        self.send(vec!["set_property".into(), name.into(), value.into()])
    }

    /// Asks pepper-play to quit and closes the IPC (which also ends it).
    pub fn stop(&mut self) {
        self.command(&["quit"]);
        let _ = self.ipc.shutdown(std::net::Shutdown::Both);
    }

    /// After [`Player::stop`]: waits for it to end until `deadline`, then
    /// makes it.
    pub fn reap(mut self, deadline: Instant) {
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
