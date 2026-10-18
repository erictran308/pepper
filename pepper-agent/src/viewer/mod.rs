//! The viewer: `pepper-agent view`, which the human runs in a Pepper pane
//! (usually a split next to the agent). It owns the pane: it places a box
//! per source with `OSC 777;pepper`, draws a label over each, the agent's
//! message and the keys, reads the human's keys and mouse, and shows each
//! source: a pepper-play for media, a page of its own headless Chrome for
//! the web (`web.rs`, `frames.rs`). Agents reach it only through its control
//! socket (`proto.rs`), with the commands there; it never runs anything
//! they send.
//!
//! One thread runs the viewer; others read (the socket's connections, the
//! terminal, each player's IPC and stderr, the browser, Pepper's reports)
//! or do slow work (a web command, a capture) and hand what they have to it
//! (`Msg`). A request that waits (an event, a file loading, a seek, a
//! snapshot, a web command) is kept with its connection and answered when
//! that comes or its time is up.

mod compare;
mod frames;
mod input;
mod layout;
mod live;
mod pages;
mod player;
mod pointing;
mod record;
mod screen;
mod seekbar;
mod serve;
mod sharing;
mod watch;
mod web;

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::json::Value;
use crate::text::{self, Time};
use crate::{edit, files, pepper, proto, sys};
use input::{Input, Key};
use layout::{Cells, Mode};
use player::Player;
use screen::{Row, Screen};

pub use live::is_stream;
pub use player::find_play;
pub use web::is_local;

/// A viewer shows at most this many sources: Pepper's limit of boxes in one
/// pane.
const MAX_SOURCES: usize = 4;
/// How long `open` waits for the first frame, a seek for its frame, a
/// snapshot for its file.
const OPEN_WAIT: Duration = Duration::from_secs(30);
const SEEK_WAIT: Duration = Duration::from_secs(10);
const SHOT_WAIT: Duration = Duration::from_secs(10);
/// The longest `events --wait`, in seconds.
pub const MAX_WAIT: f64 = 3600.0;
const MAX_EVENTS_KEPT: usize = 10_000;
const MAX_EVENTS_ANSWERED: usize = 200;
const MAX_SAY: usize = 300;
const MAX_NOTE: usize = 500;
/// How long a short message on the keys row stays.
const FLASH: Duration = Duration::from_millis(2500);
/// Lines of a player's stderr kept, for errors.
const LOG_KEPT: usize = 20;
/// The most often the screen is drawn.
const DRAW_EVERY: Duration = Duration::from_millis(40);
const LOOK_EVERY: Duration = Duration::from_millis(250);
/// How long a source must have shown in focus before y or x takes it.
const SEEN_FOR: Duration = Duration::from_millis(500);

/// Event types, as `events --type` takes them.
pub const EVENT_TYPES: &[&str] = &[
    "mark", "note", "done", "point", "region", "range", "approve", "reject", "delete",
];

/// What the reading and working threads hand the viewer.
pub enum Msg {
    /// Bytes from the terminal, and when they were read.
    Input(Vec<u8>, Instant),
    TtyClosed,
    Request(Request),
    Player(u64, player::Event),
    /// A web page's title, address or state changed.
    Web(u64),
    /// A web page is open, or couldn't be.
    WebOpened(u64, Result<Arc<web::Page>>),
    /// Slow work done: a web command, a capture.
    Job(u64, Result<Value>),
    /// What Pepper says of a box the viewer draws into itself.
    BoxSize(String, pepper::BoxSize),
    /// Pepper didn't take a box's surfaces: they're offered again later.
    BoxRefused(String),
    /// Pepper answered: the viewer's own client, for its pages' boxes.
    Pepper(Arc<pepper::Client>),
    /// Pepper didn't answer (yet): said in the log, once.
    PepperNot(String),
    /// A file changed under a page's --watch folder.
    Watch(u64),
}

/// A command from an agent, with the connection to answer on and who
/// holds it.
pub struct Request {
    stream: UnixStream,
    cmd: String,
    args: Value,
    peer: sys::Peer,
}

impl Request {
    /// Answers, on a thread of its own: an agent that doesn't read can't
    /// hold up the viewer.
    fn answer(self, result: Result<Value>) {
        let mut line = match result {
            Ok(value) => proto::answer_ok(value),
            Err(error) => proto::answer_err(&error),
        };
        // Never more than a command reads (proto::MAX_ANSWER), which would
        // take an answer cut short for no viewer at all.
        if line.len() > proto::MAX_ANSWER {
            line = proto::answer_err(&Error::new(
                "TOO_LONG",
                format!(
                    "the answer would be {} MB: {} MB at most",
                    line.len() >> 20,
                    proto::MAX_ANSWER >> 20
                ),
            ));
        }
        let mut stream = self.stream;
        std::thread::spawn(move || {
            let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
            let _ = stream.write_all(line.as_bytes());
        });
    }
}

pub struct Options {
    pub project: Option<PathBuf>,
    pub debug: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum State {
    Loading,
    Ready,
    Failed(String),
    /// pepper-play ended.
    Gone(String),
}

struct Source {
    sid: u64,
    name: String,
    /// "media" (pepper-play) or "web" (a page).
    kind: &'static str,
    /// An absolute path or an http(s) URL, as loaded.
    target: String,
    token: String,
    player: Option<Player>,
    state: State,
    time_pos: Option<f64>,
    duration: Option<f64>,
    paused: bool,
    eof: bool,
    buffering: bool,
    size: (Option<f64>, Option<f64>),
    hwdec: Option<String>,
    /// `review`: pause once playback reaches this.
    review_to: Option<f64>,
    loop_ab: Option<(f64, f64)>,
    placed: Option<Cells>,
    log: Vec<String>,
    web: Option<pages::WebSource>,
    /// The file's frame rate, for its frame numbers.
    fps: Option<f64>,
    /// The human's seeks with the mouse, one at a time.
    scrub: seekbar::Scrub,
    /// An edit's preview: where each clip lands, to say where a time of
    /// it is in the footage.
    edit: Option<edit::Timeline>,
    /// The session that opened it (only it drives it), and its owner's
    /// name for the label.
    session: Option<String>,
    owner: Option<String>,
    /// A watched file, a pipe or a stream: when it loads again.
    live: Option<live::Live>,
}

/// A source to add: what `add_source` needs besides the request.
struct New {
    kind: &'static str,
    target: String,
    play: bool,
    session: Option<String>,
    owner: Option<String>,
    edit: Option<edit::Timeline>,
    live: Option<live::Live>,
    /// Whether what the target names is followed: a remote address's, or
    /// the viewer's own EDL of `edit` ([`Player::load`]).
    references: bool,
}

impl Source {
    /// Whether something shows it: its player, or its page.
    fn alive(&self) -> bool {
        self.player.is_some() || self.web.is_some()
    }

    fn status(&self) -> &'static str {
        if let Some(status) = self.live.as_ref().and_then(live::Live::status) {
            return status;
        }
        match self.state {
            State::Loading => "loading",
            State::Failed(_) | State::Gone(_) => "error",
            State::Ready if self.web.is_some() => "ready",
            State::Ready if self.eof => "ended",
            State::Ready if self.buffering => "buffering",
            State::Ready if self.paused => "paused",
            State::Ready => "playing",
        }
    }

    fn aspect(&self) -> Option<f64> {
        match self.size {
            (Some(w), Some(h)) if w > 0.0 && h > 0.0 => Some(w / h),
            _ => None,
        }
    }

    /// What outputs and the record say of where it's from: a URL without
    /// any user and password in it.
    fn shown_target(&self) -> String {
        text::redact_url(&self.target)
    }

    fn error(&self) -> Option<&str> {
        match &self.state {
            State::Failed(e) | State::Gone(e) => Some(e),
            _ => None,
        }
    }

    /// The number of the frame it shows (from 0): its time and its file's
    /// rate. Not for an edit's preview, whose clips' rates may differ: its
    /// clip and the time in that file say where it is.
    fn frame(&self) -> Option<u64> {
        match (self.time_pos, self.fps) {
            (Some(t), Some(fps)) if self.edit.is_none() => Some((t.max(0.0) * fps).round() as u64),
            _ => None,
        }
    }

    /// What it shows, in an event or an answer: a page's `url`, an edit's
    /// `project`, or a `file`.
    fn origin(&self, into: &mut Value) {
        match &self.edit {
            _ if self.web.is_some() => into.set("url", self.shown_target()),
            Some(edit) => {
                into.set("project", edit.project.as_str());
                // A render opened with its cut: the file too (a preview's
                // EDL is pepper-agent's own, of no use to the agent).
                if !self.target.ends_with(".edl") {
                    into.set("file", self.shown_target());
                }
            }
            None => into.set("file", self.shown_target()),
        }
    }

    /// A time of it, in an event or an answer: `time`, and for an edit's
    /// preview the clip, its file and the time in that file.
    fn at_time(&self, t: f64, into: &mut Value) {
        into.set("time", round3(t));
        into.set("time_text", text::clock(t));
        if let Some(edit) = &self.edit {
            edit.describe(t, into);
        }
    }

    fn to_json(&self, focused: bool, visible: bool) -> Value {
        if let Some(web) = &self.web {
            return web.to_json(self, focused, visible);
        }
        let mut v = Value::obj()
            .with("name", self.name.as_str())
            .with("kind", self.kind)
            .with("target", self.shown_target())
            .with("status", self.status())
            .with("playable", true)
            .with("time_pos", self.time_pos.map(round3))
            .with("duration", self.duration.map(round3))
            .with("paused", self.paused)
            .with("width", self.size.0)
            .with("height", self.size.1)
            .with("hwdec", self.hwdec.as_deref())
            .with("focused", focused)
            .with("owner", self.owner.as_deref())
            .with("visible", visible);
        v.set("frame", self.frame());
        v.set(
            "box",
            self.placed
                .map(|c| vec![c.col as u32, c.row as u32, c.cols as u32, c.rows as u32]),
        );
        if let Some((a, b)) = self.loop_ab {
            v.set("loop", vec![round3(a), round3(b)]);
        }
        if let Some(edit) = &self.edit {
            v.set("project", edit.project.as_str());
            if let Some((clip, _)) = self.time_pos.and_then(|t| edit.locate(t)) {
                v.set("clip", clip);
            }
        }
        if let Some(live) = &self.live {
            live.to_json(&mut v);
        }
        if let Some(error) = self.error() {
            v.set("error", error);
        }
        v
    }
}

fn round3(t: f64) -> f64 {
    (t * 1000.0).round() / 1000.0
}

enum WaitFor {
    Events {
        since: u64,
        types: Vec<String>,
        source: Option<String>,
        owner: Option<String>,
        until: Vec<String>,
    },
    /// `open`: the first frame.
    Loaded { sid: u64 },
    /// A seek: its frame; then, for `review`, the time to pause at.
    Restart {
        sid: u64,
        result: Value,
        review_to: Option<f64>,
    },
    Shot {
        sid: u64,
        request: u64,
        result: Value,
    },
    /// A command of the player's: `result`, once the player has done it.
    Reply {
        sid: u64,
        request: u64,
        result: Value,
    },
    /// Slow work on another thread: its result is the answer.
    Job { job: u64, sid: u64 },
}

struct Wait {
    req: Request,
    deadline: Instant,
    what: WaitFor,
}

/// The note the human is typing.
struct Prompt {
    text: String,
    sid: Option<u64>,
    time: Option<f64>,
    /// The source was playing, and plays again after.
    resume: bool,
    /// The note is for a point or region the human just made.
    point: Option<u64>,
    /// The note is for a range (`i`, `o`): its event, sent with the note.
    range: Option<Value>,
}

/// The terminal's mode before the viewer, to put back even after a panic.
static SAVED_MODE: OnceLock<(RawFd, sys::Termios)> = OnceLock::new();

/// Runs the viewer in this pane until the human quits it.
pub fn run(options: Options) -> Result<()> {
    let port = std::env::var("PEPPER_PORT")
        .ok()
        .filter(|p| !p.is_empty())
        .ok_or_else(|| {
            Error::new(
                "NOT_IN_PEPPER",
                "PEPPER_PORT isn't set: this isn't a Pepper tab",
            )
        })?;
    if std::env::var_os("TMUX").is_some() {
        return Err(Error::new(
            "NOT_IN_PEPPER",
            "this is inside tmux, which keeps Pepper's escape sequences from the terminal",
        ));
    }
    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|e| {
            Error::new(
                "NOT_IN_PEPPER",
                format!("no terminal to show video in: {e}"),
            )
        })?;
    let play = find_play().ok_or_else(|| {
        Error::new("MISSING_DEP", "pepper-play wasn't found").hint(
            "pepper-play comes with Pepper, which tells every shell where it is (PEPPER_PLAY): \
             run the viewer in a Pepper tab.",
        )
    })?;
    let dir = proto::run_dir()?;
    proto::viewers(&dir);
    let id = std::process::id().to_string();
    let socket = proto::socket_path(&dir, &id);
    let _ = std::fs::remove_file(&socket);
    // Under umask 177 (no thread makes files yet): the socket is the
    // user's only (0600) from the start, whatever their umask.
    let listener = sys::with_umask(0o177, || UnixListener::bind(&socket)).map_err(|e| {
        Error::new(
            "FAILED",
            format!("cannot listen at {}: {e}", socket.display()),
        )
    })?;

    let project = match options.project {
        Some(p) => std::path::absolute(p),
        None => std::env::current_dir(),
    }
    .map_err(|e| Error::new("FAILED", format!("the project folder: {e}")))?;
    // The project's .pepper, held open: the human's events and points'
    // pictures are written through it, never by its path again (it could
    // become a link), into files this viewer makes (never one a project
    // came with, which git may track).
    let pepper = files::pepper_dir(&project, &[]).and_then(|top| files::Dir::open(&top));
    let (events_file, events_out) =
        match pepper.as_ref().map(|top| files::new_events_file(top, &id)) {
            Ok(Ok((path, file))) => (path, Some(file)),
            _ => (dir.join(format!("{id}.events.jsonl")), None),
        };
    // What it writes (snapshots, recordings, a difference, points'
    // pictures without a project) goes in a folder of its own outside the
    // temp folder, which a sandboxed agent's commands may be let write
    // (proto.rs): no link planted there leads mpv or ffmpeg elsewhere. The
    // FIFOs of `pipe new`, which the agent's programs write, stay in it.
    let outputs = proto::outputs_path()
        .ok_or_else(|| Error::new("FAILED", "this user has no home folder"))?;
    files::own_dir(&outputs).map_err(|e| Error::new("FAILED", e.to_string()))?;
    let shots = fresh_dir(&proto::shots_dir(&outputs, &id))?;
    let pipes = fresh_dir(&dir.join(format!("{id}-pipes")))?;
    let log = options.debug.then(|| dir.join(format!("{id}.log")));
    let points = match pepper.and_then(|top| top.sub("points")) {
        Ok(points) => points,
        Err(_) => {
            let fallback = outputs.join(format!("{id}-points"));
            files::own_dir(&fallback)
                .and_then(|_| files::Dir::open(&fallback))
                .map_err(|e| Error::new("FAILED", format!("{}: {e}", fallback.display())))?
        }
    };

    sys::catch_signals();
    let fd = tty.as_raw_fd();
    let mode =
        sys::raw_mode(fd).map_err(|e| Error::new("FAILED", format!("the terminal's mode: {e}")))?;
    let _ = SAVED_MODE.set((fd, mode));
    let raw =
        sys::mode(fd).map_err(|e| Error::new("FAILED", format!("the terminal's mode: {e}")))?;
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        give_terminal_back();
        default_hook(info);
    }));

    let (tx, rx) = mpsc::channel();
    {
        let tx = tx.clone();
        std::thread::spawn(move || listen(listener, tx));
    }
    {
        let tx = tx.clone();
        let reader = tty
            .try_clone()
            .map_err(|e| Error::new("FAILED", e.to_string()))?;
        std::thread::spawn(move || read_keys(reader, tx));
    }
    // The viewer's own Mach client, for the boxes it draws into itself (web
    // pages): Pepper's reports of their sizes come to it. Asked for on a
    // thread of its own, again each second until Pepper answers (it may be
    // busy as the viewer starts), so the viewer never waits on it.
    {
        let tx = tx.clone();
        let port = port.clone();
        std::thread::spawn(move || {
            let mut told = false;
            let client = loop {
                match pepper::Client::connect(&port) {
                    Ok(client) => break Arc::new(client),
                    Err(e) => {
                        if !std::mem::replace(&mut told, true)
                            && tx.send(Msg::PepperNot(e)).is_err()
                        {
                            return;
                        }
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            };
            let reports = client.reports();
            if tx.send(Msg::Pepper(client)).is_err() {
                return;
            }
            while let Some(report) = reports.next_report() {
                let msg = match report {
                    pepper::Report::Size(token, size) => Msg::BoxSize(token, size),
                    pepper::Report::Refused(token) => Msg::BoxRefused(token),
                };
                if tx.send(msg).is_err() {
                    return;
                }
            }
        });
    }
    let pepper = None;
    let screen = Screen::enter(
        tty.try_clone()
            .map_err(|e| Error::new("FAILED", e.to_string()))?,
    );
    let started = sys::now();
    let mut viewer = Viewer {
        id: id.clone(),
        started,
        dir: dir.clone(),
        project,
        events_file,
        events_out,
        shots: shots.clone(),
        pipes: pipes.clone(),
        pepper_port: port,
        play,
        log,
        tx,
        tty,
        screen,
        size: sys::WinSize::default(),
        input: Input::default(),
        raw,
        focus_seen: None,
        sources: Vec::new(),
        next_sid: 1,
        focus: None,
        mode: Mode::Auto,
        zoom: false,
        tiles: Vec::new(),
        events: Vec::new(),
        next_event: 1,
        waits: Vec::new(),
        say: None,
        prompt: None,
        confirm_quit: false,
        confirm_delete: None,
        deleted: HashSet::new(),
        flash: None,
        shot_count: 0,
        pepper,
        host: Arc::new(Mutex::new(None)),
        next_job: 1,
        point_jobs: HashMap::new(),
        points: Vec::new(),
        next_point: 1,
        drag: None,
        scrubbing: None,
        range_from: None,
        sessions: Vec::new(),
        recordings: Vec::new(),
        finished: Vec::new(),
        auto_stops: Vec::new(),
        compare: None,
        points_dir: Arc::new(points),
        quit: false,
        dirty: true,
        record_dirty: true,
        last_draw: Instant::now(),
    };
    viewer.note(&format!(
        "viewer {id} started, pepper-play {}, Pepper's port {}",
        viewer.play.display(),
        if viewer.pepper.is_some() {
            "reached"
        } else {
            "not reached"
        }
    ));
    viewer.run(rx);
    viewer.end();
    give_terminal_back();
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(proto::record_path(&dir, &id));
    let _ = std::fs::remove_dir_all(&shots);
    let _ = std::fs::remove_dir_all(&pipes);
    Ok(())
}

/// A new, empty folder of the viewer's own at `path`: whatever had the
/// name (left by a viewer that had the same pid, or put there) goes first.
fn fresh_dir(path: &Path) -> Result<PathBuf> {
    let fail = |e: std::io::Error| Error::new("FAILED", format!("{}: {e}", path.display()));
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path).map_err(fail)?,
        Ok(_) => std::fs::remove_file(path).map_err(fail)?,
        Err(_) => {}
    }
    // Not made if something has the name again: refused, not shared.
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(fail)?;
    Ok(path.to_owned())
}

fn give_terminal_back() {
    if let Some((fd, mode)) = SAVED_MODE.get() {
        let _ = sys::set_mode(*fd, mode);
        // The screen too, or the shell gets the mouse's reports as text.
        if let Ok(mut tty) = OpenOptions::new().write(true).open("/dev/tty") {
            let _ = tty.write_all(screen::LEAVE.as_bytes());
        }
    }
}

/// Takes connections from this user only, reads each one's request, and
/// hands the allowed ones to the viewer, with who sent it: whether a
/// sandbox confines it decides what it may ask (Viewer::sandboxed).
fn listen(listener: UnixListener, tx: Sender<Msg>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let Some(peer) = sys::peer(stream.as_raw_fd()).filter(|p| p.uid == sys::uid()) else {
            continue;
        };
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let line = proto::read_line(&mut BufReader::new(&stream), proto::MAX_REQUEST);
            let parsed = match line {
                Ok(Some(line)) => proto::parse_request(&line),
                Ok(None) => return,
                Err(_) => Err(Error::new(
                    "BAD_REQUEST",
                    "the request is too long, or not UTF-8 text",
                )),
            };
            match parsed {
                Ok((cmd, args)) => {
                    let _ = tx.send(Msg::Request(Request {
                        stream,
                        cmd,
                        args,
                        peer,
                    }));
                }
                Err(error) => {
                    let mut stream = stream;
                    let _ = stream.write_all(proto::answer_err(&error).as_bytes());
                }
            }
        });
    }
}

fn read_keys(mut tty: File, tx: Sender<Msg>) {
    use std::io::Read;
    let mut buf = [0u8; 4096];
    loop {
        match tty.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if tx
                    .send(Msg::Input(buf[..n].to_vec(), Instant::now()))
                    .is_err()
                {
                    return;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let _ = tx.send(Msg::TtyClosed);
}

struct Viewer {
    id: String,
    started: f64,
    dir: PathBuf,
    project: PathBuf,
    /// Where the human's events go, and the file this viewer made there
    /// (in its run folder, by path, when there's none).
    events_file: PathBuf,
    events_out: Option<File>,
    shots: PathBuf,
    /// The FIFOs of `pipe new`.
    pipes: PathBuf,
    pepper_port: String,
    play: PathBuf,
    log: Option<PathBuf>,
    tx: Sender<Msg>,
    tty: File,
    screen: Screen,
    size: sys::WinSize,
    input: Input,
    /// The terminal's raw mode, kept (keep_raw).
    raw: sys::Termios,
    /// The focused source as last drawn, its target, and since when: what
    /// the human has seen, which y and x act on.
    focus_seen: Option<(u64, String, Instant)>,
    sources: Vec<Source>,
    next_sid: u64,
    focus: Option<usize>,
    mode: Mode,
    zoom: bool,
    /// What shows: a source's index, its tile and its box.
    tiles: Vec<(usize, Cells, Cells)>,
    events: Vec<Value>,
    next_event: u64,
    waits: Vec<Wait>,
    say: Option<String>,
    prompt: Option<Prompt>,
    confirm_quit: bool,
    /// A mark the human asked to delete (`d`), until they say again.
    confirm_delete: Option<u64>,
    /// The events of the marks the human deleted.
    deleted: HashSet<u64>,
    flash: Option<(String, Instant)>,
    shot_count: u64,
    /// Pepper's port, for what the viewer draws itself.
    pepper: Option<Arc<pepper::Client>>,
    /// The browser for web sources, while any is open.
    host: Arc<Mutex<Option<Arc<web::Host>>>>,
    next_job: u64,
    /// Jobs that capture for a point: the point's id.
    point_jobs: HashMap<u64, u64>,
    /// Points and regions waiting for their note or their pictures.
    points: Vec<pointing::Pending>,
    next_point: u64,
    drag: Option<pointing::Drag>,
    /// The source whose seek bar the human holds, and the time there.
    scrubbing: Option<(u64, f64)>,
    /// A range the human started (`i`): its source and time.
    range_from: Option<(u64, f64)>,
    /// The sessions this viewer gave: token and owner's name.
    sessions: Vec<(String, String)>,
    /// Recordings under way; those that stopped by themselves (their
    /// --max), for `record stop`; and the jobs finishing those.
    recordings: Vec<record::Recording>,
    finished: Vec<(u64, Result<Value>)>,
    auto_stops: Vec<(u64, u64)>,
    /// Two sources side by side, before and after.
    compare: Option<compare::Compare>,
    /// Where points' pictures go: `.pepper/points/` in the project, held
    /// open.
    points_dir: Arc<files::Dir>,
    quit: bool,
    dirty: bool,
    record_dirty: bool,
    last_draw: Instant,
}

impl Viewer {
    /// Under --debug, a line in the viewer's log.
    fn note(&self, line: &str) {
        if let Some(log) = &self.log {
            let line = text::redact_urls(line);
            let _ = files::append_line(log, &format!("{} {line}", sys::iso_time(sys::now())));
        }
    }

    fn run(&mut self, rx: Receiver<Msg>) {
        let mut looked = Instant::now();
        self.look_at_size();
        loop {
            if sys::STOP.load(Ordering::Relaxed) || self.quit {
                return;
            }
            let timeout = if self.dirty {
                DRAW_EVERY.saturating_sub(self.last_draw.elapsed())
            } else {
                LOOK_EVERY
            };
            // A lone ESC waits only a little to be the Esc key.
            let timeout = if self.input.waiting() {
                timeout.min(input::ESC_WAIT)
            } else {
                timeout
            };
            match rx.recv_timeout(timeout.max(Duration::from_millis(1))) {
                Ok(msg) => self.on(msg),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            while !self.quit
                && let Ok(msg) = rx.try_recv()
            {
                self.on(msg);
            }
            if let Some(key) = self.input.flush(Instant::now()) {
                self.on_key(key);
            }
            if sys::RESIZED.swap(false, Ordering::Relaxed) || looked.elapsed() >= LOOK_EVERY {
                looked = Instant::now();
                self.look_at_size();
                self.keep_raw();
            }
            if self.screen.stale() {
                self.dirty = true;
            }
            self.expire_waits();
            self.expire_points();
            self.pump_scrubs();
            self.end_long_recordings();
            self.sync_drift();
            self.reload_watched();
            self.pump_live();
            if let Some((_, at)) = &self.flash
                && at.elapsed() >= FLASH
            {
                self.flash = None;
                self.dirty = true;
            }
            if self.dirty && self.last_draw.elapsed() >= DRAW_EVERY {
                self.draw();
            }
            if self.record_dirty {
                self.write_record();
            }
        }
    }

    fn on(&mut self, msg: Msg) {
        match msg {
            Msg::Input(bytes, at) => {
                if let Some(key) = self.input.flush(at) {
                    self.on_key(key);
                }
                let keys = self.input.feed_at(&bytes, at);
                // Keys that came together, as no one types them: a
                // terminal's reply, tmux's commands, not the human.
                let burst = input::burst(&keys);
                for key in keys {
                    if burst && !matches!(key, Key::Mouse(_) | Key::Paste(_)) {
                        self.on_burst_key(key);
                    } else {
                        self.on_key(key);
                    }
                }
            }
            Msg::TtyClosed => self.quit = true,
            Msg::Request(req) => self.on_request(req),
            Msg::Player(sid, event) => self.on_player(sid, event),
            Msg::Web(sid) => self.on_web(sid),
            Msg::WebOpened(sid, page) => self.on_web_opened(sid, page),
            Msg::Job(job, result) => self.on_job(job, result),
            Msg::BoxSize(token, size) => self.on_box_size(&token, size),
            Msg::BoxRefused(token) => self.on_box_refused(&token),
            Msg::Pepper(client) => self.on_pepper(client),
            Msg::PepperNot(why) => self.note(&format!("Pepper: {why}; asked again each second")),
            Msg::Watch(sid) => {
                if !self.on_watch_live(sid) {
                    self.on_watch(sid);
                }
            }
        }
    }

    // The pane.

    /// Raw mode again if something changed the terminal's mode (anything
    /// that can open the pane's terminal can): keys one at a time, ^C a
    /// key, as the viewer reads them.
    fn keep_raw(&mut self) {
        let fd = self.tty.as_raw_fd();
        if sys::mode(fd).is_ok_and(|mode| mode != self.raw) {
            let _ = sys::set_mode(fd, &self.raw);
            self.note("the terminal's mode was changed: raw again");
        }
    }

    fn look_at_size(&mut self) {
        if let Some(size) = sys::win_size(self.tty.as_raw_fd())
            && size != self.size
        {
            self.size = size;
            self.relayout();
        }
    }

    /// A cell's size in pixels.
    fn cell(&self) -> (f64, f64) {
        let s = self.size;
        if s.width > 0 && s.height > 0 && s.cols > 0 && s.rows > 0 {
            (
                s.width as f64 / s.cols as f64,
                s.height as f64 / s.rows as f64,
            )
        } else {
            (8.0, 16.0)
        }
    }

    /// Works out what shows where, and places, moves or takes away boxes.
    fn relayout(&mut self) {
        let area = Cells {
            col: 0,
            row: 0,
            cols: self.size.cols,
            rows: self.size.rows.saturating_sub(2),
        };
        // Comparing: the two, side by side (z still zooms one).
        let (mode, shown) = match (self.compare_pair(), self.zoom) {
            (Some((a, b)), false) => (Mode::SplitH, vec![a, b]),
            _ => {
                let mode = if self.zoom { Mode::Single } else { self.mode };
                (mode, layout::visible(mode, self.sources.len(), self.focus))
            }
        };
        let tiles = layout::tiles(mode, shown.len(), area, self.cell());
        let cell = self.cell();
        self.tiles = shown
            .iter()
            .zip(tiles)
            .map(|(&i, tile)| {
                // A video's seek bar takes the row under its box.
                let bar = u16::from(self.sources[i].web.is_none());
                let room = Cells {
                    col: tile.col,
                    row: tile.row + 1,
                    cols: tile.cols,
                    rows: tile.rows.saturating_sub(1 + bar),
                };
                (i, tile, layout::fit(room, cell, self.sources[i].aspect()))
            })
            .collect();
        let mut out = String::new();
        let mut moved = Vec::new();
        for (i, source) in self.sources.iter_mut().enumerate() {
            let want = self
                .tiles
                .iter()
                .find(|t| t.0 == i)
                .map(|t| t.2)
                .filter(|b| b.rows > 0 && b.cols > 0 && source.alive());
            if want != source.placed {
                match want {
                    Some(at) => out.push_str(&screen::place(&source.token, at)),
                    None => out.push_str(&screen::remove(&source.token)),
                }
                source.placed = want;
                if source.web.is_some() {
                    moved.push(source.sid);
                }
            }
        }
        for sid in moved {
            self.web_box_changed(sid);
        }
        if !out.is_empty() {
            self.screen.write(&out);
            let placed: Vec<String> = self
                .sources
                .iter()
                .map(|s| format!("{}={:?}", s.name, s.placed))
                .collect();
            self.note(&format!("placed {}", placed.join(" ")));
        }
        self.dirty = true;
    }

    fn draw(&mut self) {
        let (cols, rows) = (self.size.cols, self.size.rows);
        if cols == 0 || rows == 0 {
            return;
        }
        let mut out: Vec<Row> = (0..rows).map(|_| Row::default()).collect();
        if self.sources.is_empty() && rows >= 4 {
            let mid = (rows - 2) / 2;
            let lines = [
                (
                    format!("pepper-agent viewer {}: waiting for an agent", self.id),
                    screen::BOLD,
                ),
                (
                    "An agent finds it with: pepper-agent status".to_owned(),
                    screen::DIM,
                ),
            ];
            for (k, (line, style)) in lines.iter().enumerate() {
                let len = line.chars().count() as u16;
                out[(mid as usize + k).min(rows as usize - 3)].put(
                    cols.saturating_sub(len) / 2,
                    cols,
                    line,
                    style,
                );
            }
        }
        for (n, &(i, tile, at)) in self.tiles.iter().enumerate() {
            let source = &self.sources[i];
            // Right over the picture, which may be narrower or lower than
            // its tile.
            let (row, col) = (at.row.saturating_sub(1).max(tile.row), at.col.max(tile.col));
            let width = tile.cols.saturating_sub(col - tile.col);
            let focused = self.focus == Some(i);
            let owner = source
                .owner
                .as_deref()
                .map(|o| format!(" ({o})"))
                .unwrap_or_default();
            let role = self
                .compare_role(source.sid)
                .map(|r| format!(" [{r}]"))
                .unwrap_or_default();
            let mut label = format!(
                " {} {}{owner}{role}  {}",
                n + 1,
                source.name,
                source.status()
            );
            if let Some(secs) = self.recording(source.sid) {
                label += &format!("  ● REC {}", text::short_clock(secs));
            }
            if let Some(web) = &source.web {
                label += &web.label();
            } else if let Some(t) = self.held_time(source.sid).or(source.time_pos) {
                // Paused, or while the human seeks: to the frame.
                let held = self.held_time(source.sid).is_some();
                let exact = source.paused || held;
                label += &format!(
                    "  {}",
                    if exact {
                        text::clock(t)
                    } else {
                        text::short_clock(t)
                    }
                );
                if let Some(d) = source.duration {
                    label += &format!(" / {}", text::short_clock(d));
                }
                if exact
                    && !held
                    && let Some(f) = source.frame()
                {
                    label += &format!("  frame {f}");
                }
                if let Some(edit) = &source.edit {
                    match edit.locate(t) {
                        _ if exact => label += &format!("  {}", edit.say(t).unwrap_or_default()),
                        Some((k, _)) => {
                            label += &format!("  clip {} of {}", k + 1, edit.cuts.len())
                        }
                        None => {}
                    }
                }
            }
            if let Some(error) = source.error() {
                label += &format!(": {error}");
            }
            label.push(' ');
            let style = if focused {
                screen::REVERSE
            } else {
                screen::BOLD
            };
            out[row as usize].put(col, width, &label, style);
        }
        if rows >= 2 {
            let say_row = &mut out[rows as usize - 2];
            match &self.say {
                Some(say) => say_row.put(0, cols, &format!(" {say}"), screen::AGENT),
                None if self.sources.is_empty() => {}
                None => say_row.put(0, cols, " (the agent hasn't said anything)", screen::DIM),
            }
        }
        self.draw_bars(&mut out);
        let drag = self
            .scrub_text()
            .or_else(|| self.drag_text())
            .or_else(|| self.range_text());
        let on_mark = self.on_mark_text();
        let deleting = self.delete_text();
        let keys_row = &mut out[rows as usize - 1];
        if let Some(prompt) = &self.prompt {
            let range = prompt.range.as_ref().map(|r| {
                format!(
                    "the range {}–{}",
                    r.get_str("time_text").unwrap_or(""),
                    r.get_str("to_text").unwrap_or("")
                )
            });
            let point = prompt
                .point
                .and_then(|id| self.points.iter().find(|p| p.id == id))
                .map(|p| p.what().to_owned());
            let (what, hint) = match range.or(point) {
                Some(point) => (
                    format!(" note on {point}"),
                    "  (Enter sends, Esc sends it without a note)",
                ),
                None => (
                    format!(
                        " note{}",
                        prompt
                            .time
                            .map(|t| format!(" at {}", text::clock(t)))
                            .unwrap_or_default()
                    ),
                    "  (Enter sends, Esc drops)",
                ),
            };
            let text = format!("{what}: {}_", prompt.text);
            // The end of the text, so the cursor stays in sight.
            let room = (cols as usize).saturating_sub(hint.len());
            let skip = text.chars().count().saturating_sub(room);
            let shown: String = text.chars().skip(skip).collect();
            keys_row.put(0, cols, &format!("{shown}{hint}"), screen::BOLD);
        } else if self.confirm_quit {
            let n = self.sources.len();
            keys_row.put(
                0,
                cols,
                &format!(
                    " Quit the viewer and close {n} source{}? q quits, any other key stays",
                    if n == 1 { "" } else { "s" }
                ),
                screen::BOLD,
            );
        } else if let Some(deleting) = deleting {
            keys_row.put(0, cols, &format!(" {deleting}"), screen::BOLD);
        } else if let Some(drag) = drag {
            keys_row.put(0, cols, &format!(" {drag}"), screen::BOLD);
        } else if let Some((flash, _)) = &self.flash {
            keys_row.put(0, cols, &format!(" {flash}"), screen::BOLD);
        } else if let Some(here) = on_mark {
            keys_row.put(0, cols, &format!(" {here}"), screen::BOLD);
        } else if self.sources.is_empty() {
            keys_row.put(0, cols, " q quits", screen::DIM);
        } else if self
            .focused()
            .is_some_and(|i| self.sources[i].web.is_some())
        {
            keys_row.put(
                0,
                cols,
                " click to point · drag a region · n note · y/x approve/reject · Enter done · wheel scrolls · r reload · Tab focus · z zoom · q quit",
                screen::DIM,
            );
        } else {
            keys_row.put(
                0,
                cols,
                " m mark · n note · i o range · Enter done · space pause · , . or wheel: a frame · < > marks · the bar seeks · ←→ 5 s · click or drag to point · y/x approve/reject · Tab focus · z zoom · q quit",
                screen::DIM,
            );
        }
        // What the human sees in focus, and since when (verdict).
        let seen = self.focus_seen.as_ref().map(|(s, t, _)| (*s, t.as_str()));
        let shown = self
            .focused()
            .map(|i| (self.sources[i].sid, self.sources[i].target.as_str()));
        if seen != shown {
            self.focus_seen = shown.map(|(s, t)| (s, t.to_owned(), Instant::now()));
        }
        self.screen.draw(out, cols);
        self.dirty = false;
        self.last_draw = Instant::now();
    }

    fn flash(&mut self, text: impl Into<String>) {
        self.flash = Some((text.into(), Instant::now()));
        self.dirty = true;
    }

    // The human's keys.

    fn focused(&self) -> Option<usize> {
        self.focus.filter(|&i| i < self.sources.len())
    }

    /// A key that came in a burst (`input::burst`): it may add to a note
    /// being typed, but it never acts (sends the note, approves, quits).
    fn on_burst_key(&mut self, key: Key) {
        if self.prompt.is_some() && matches!(key, Key::Char(_)) {
            self.on_prompt_key(key);
        }
    }

    fn on_key(&mut self, key: Key) {
        self.dirty = true;
        if let Key::Mouse(mouse) = key {
            self.on_mouse(mouse);
            return;
        }
        if self.prompt.is_some() {
            self.on_prompt_key(key);
            return;
        }
        if let Some(id) = self.confirm_delete.take() {
            // A second d deletes; any other key keeps the mark.
            if key == Key::Char('d') {
                self.delete_mark(id);
            }
            return;
        }
        if self.confirm_quit {
            self.confirm_quit = false;
            if matches!(key, Key::Char('q' | 'Q' | 'y') | Key::Ctrl('c')) {
                self.quit = true;
            }
            return;
        }
        match key {
            Key::Char('q') | Key::Ctrl('c') => {
                if self.sources.is_empty() {
                    self.quit = true;
                } else {
                    self.confirm_quit = true;
                }
            }
            Key::Tab | Key::BackTab if !self.sources.is_empty() => {
                let n = self.sources.len();
                let at = self.focused().unwrap_or(0);
                let next = if key == Key::Tab {
                    (at + 1) % n
                } else {
                    (at + n - 1) % n
                };
                self.focus = Some(next);
                self.relayout();
            }
            Key::Char(c @ '1'..='4') => {
                let k = c as usize - '1' as usize;
                if let Some(&(i, _, _)) = self.tiles.get(k) {
                    self.focus = Some(i);
                    self.relayout();
                }
            }
            Key::Char('z') => {
                if self.sources.len() > 1 || self.zoom {
                    self.zoom = !self.zoom;
                    self.relayout();
                }
            }
            Key::Char('m') if self.focused_web().is_some() => {
                self.flash("A page has no timeline: click it to point at something instead.");
            }
            Key::Char('m') => self.mark(),
            Key::Char('n') => self.start_note(),
            Key::Char('y') => self.verdict(true),
            Key::Char('x') => self.verdict(false),
            Key::Char('d') if self.comparing_pictures() => self.toggle_difference(),
            Key::Char('d') => self.ask_delete(),
            Key::Char('<') => self.jump_mark(false),
            Key::Char('>') => self.jump_mark(true),
            Key::Char('i') => self.range_in(),
            Key::Char('o') => self.range_out(),
            Key::Esc if self.range_from.is_some() => {
                self.range_from = None;
                self.flash("Range dropped.");
            }
            Key::Char('r') if self.focused_web().is_some() => self.web_key_reload(),
            Key::Up | Key::Down | Key::PageUp | Key::PageDown if self.focused_web().is_some() => {
                let by = match key {
                    Key::Up => -80.0,
                    Key::Down => 80.0,
                    Key::PageUp => -600.0,
                    _ => 600.0,
                };
                self.web_key_scroll(by, None);
            }
            Key::Enter => {
                self.add_event(Value::obj().with("type", "done"));
                self.flash("Sent: done. The agent has your marks and notes.");
            }
            Key::Paste(_) => {}
            key => {
                if let (Some(i), Some(name)) = (self.focused(), mpv_key(&key))
                    && self.sources[i].web.is_none()
                {
                    let source = &mut self.sources[i];
                    source.review_to = None;
                    if let Some(player) = &mut source.player {
                        player.command(&["keypress", name]);
                    }
                }
            }
        }
    }

    fn mark(&mut self) {
        let Some(i) = self.focused() else {
            self.flash("Nothing to mark: no source is open.");
            return;
        };
        let source = &self.sources[i];
        let Some(t) = source.time_pos else {
            self.flash("Nothing to mark yet: it hasn't started.");
            return;
        };
        let name = source.name.clone();
        let mut event = Value::obj()
            .with("type", "mark")
            .with("source", name.as_str())
            .with("kind", source.kind);
        source.origin(&mut event);
        source.at_time(t, &mut event);
        let clip = source
            .edit
            .as_ref()
            .and_then(|e| e.say(t))
            .map(|s| format!(", {s}"))
            .unwrap_or_default();
        self.add_event(event);
        let n = self
            .events
            .iter()
            .filter(|e| e.get_str("type") == Some("mark"))
            .count();
        self.flash(format!(
            "Marked {name} at {} (mark {n}{clip}). Enter when you're done.",
            text::clock(t)
        ));
    }

    /// `y` or `x`: the human approves or rejects what the focused source
    /// shows.
    fn verdict(&mut self, approve: bool) {
        let Some(i) = self.focused() else {
            self.flash("Nothing to approve: no source is open.");
            return;
        };
        // Only what the human has had in focus a moment: an agent's open
        // or focus just before the key doesn't take their verdict.
        let source = &self.sources[i];
        let seen = self
            .focus_seen
            .as_ref()
            .is_some_and(|(sid, target, since)| {
                *sid == source.sid && *target == source.target && since.elapsed() >= SEEN_FOR
            });
        if !seen {
            let (key, does) = if approve {
                ("y", "approves")
            } else {
                ("x", "rejects")
            };
            self.flash(format!(
                "{} has just come into focus: {key} again {does} it.",
                source.name
            ));
            return;
        }
        let name = source.name.clone();
        let mut event = Value::obj()
            .with("type", if approve { "approve" } else { "reject" })
            .with("source", name.as_str())
            .with("kind", source.kind);
        source.origin(&mut event);
        if let Some(pair) = self.compare_json(source.sid) {
            event.set("compare", pair);
        }
        if let Some(t) = source.time_pos.filter(|_| source.web.is_none()) {
            source.at_time(t, &mut event);
        }
        self.add_event(event);
        self.flash(if approve {
            format!("Approved {name}. Enter when you're done.")
        } else {
            format!("Rejected {name}: n says why. Enter when you're done.")
        });
    }

    fn start_note(&mut self) {
        let mut prompt = Prompt {
            text: String::new(),
            sid: None,
            time: None,
            resume: false,
            point: None,
            range: None,
        };
        if let Some(i) = self.focused() {
            let source = &mut self.sources[i];
            prompt.sid = Some(source.sid);
            prompt.time = source.time_pos;
            prompt.resume = pause_for_note(source);
        }
        self.prompt = Some(prompt);
    }

    fn on_prompt_key(&mut self, key: Key) {
        let Some(prompt) = &mut self.prompt else {
            return;
        };
        match key {
            Key::Char(c) => {
                if prompt.text.chars().count() < MAX_NOTE {
                    prompt.text.push_str(&text::typed(&c.to_string(), 1));
                }
            }
            Key::Paste(pasted) => {
                let room = MAX_NOTE.saturating_sub(prompt.text.chars().count());
                let pasted = text::typed(&pasted.replace(['\n', '\r'], " "), room);
                prompt.text.push_str(&pasted);
            }
            Key::Backspace => {
                prompt.text.pop();
            }
            Key::Ctrl('u') => prompt.text.clear(),
            Key::Enter | Key::Esc | Key::Ctrl('c') if prompt.range.is_some() => {
                let prompt = self.prompt.take().unwrap_or_else(|| unreachable!());
                let text = prompt.text.trim().to_owned();
                if let Some(mut event) = prompt.range {
                    if key == Key::Enter && !text.is_empty() {
                        event.set("note", text);
                    }
                    self.add_event(event);
                    self.flash("Sent the range. More, or Enter when you're done.");
                }
                self.end_note(prompt.sid, prompt.resume);
            }
            Key::Enter | Key::Esc | Key::Ctrl('c') if prompt.point.is_some() => {
                let prompt = self.prompt.take().unwrap_or_else(|| unreachable!());
                let text = prompt.text.trim().to_owned();
                let note = (key == Key::Enter && !text.is_empty()).then_some(text);
                if let Some(id) = prompt.point {
                    self.noted(id, note);
                }
                self.end_note(prompt.sid, prompt.resume);
            }
            Key::Enter => {
                let prompt = self.prompt.take().unwrap_or_else(|| unreachable!());
                let text = prompt.text.trim().to_owned();
                if !text.is_empty() {
                    let mut event = Value::obj().with("type", "note").with("text", text);
                    if let Some(source) = prompt.sid.and_then(|sid| self.source(sid)) {
                        event.set("source", source.name.as_str());
                        event.set("kind", source.kind);
                        source.origin(&mut event);
                        if let Some(t) = prompt.time {
                            source.at_time(t, &mut event);
                        }
                    }
                    self.add_event(event);
                    self.flash("Note sent. Enter when you're done.");
                }
                self.end_note(prompt.sid, prompt.resume);
            }
            Key::Esc | Key::Ctrl('c') => {
                let prompt = self.prompt.take().unwrap_or_else(|| unreachable!());
                self.end_note(prompt.sid, prompt.resume);
            }
            _ => {}
        }
    }

    fn end_note(&mut self, sid: Option<u64>, resume: bool) {
        if resume
            && let Some(source) = sid.and_then(|sid| self.source_mut(sid))
            && let Some(player) = &mut source.player
        {
            player.set("pause", false);
        }
    }

    fn source(&self, sid: u64) -> Option<&Source> {
        self.sources.iter().find(|s| s.sid == sid)
    }

    fn source_mut(&mut self, sid: u64) -> Option<&mut Source> {
        self.sources.iter_mut().find(|s| s.sid == sid)
    }

    // Events.

    fn add_event(&mut self, event: Value) {
        let id = self.next_event;
        self.next_event += 1;
        let mut full = Value::obj().with("id", id);
        // Whose source it's about, when it's someone's.
        let owner = event
            .get_str("source")
            .and_then(|name| self.sources.iter().find(|s| s.name == name))
            .and_then(|s| s.owner.clone());
        if let Value::Obj(members) = event {
            for (key, value) in members {
                if key == "type" {
                    full.set("type", value);
                    full.set("at", sys::iso_time(sys::now()));
                } else {
                    full.set(&key, value);
                    if key == "source"
                        && let Some(owner) = &owner
                    {
                        full.set("owner", owner.as_str());
                    }
                }
            }
        }
        if let Err(e) = self.write_event(&full.to_line()) {
            self.note(&format!("events file: {e}"));
        }
        self.note(&format!("event {}", full.to_line()));
        self.events.push(full);
        if self.events.len() > MAX_EVENTS_KEPT {
            self.events.remove(0);
        }
        self.answer_event_waits(false);
    }

    /// Adds an event's line to the events file this viewer made. Once that
    /// file is gone or has another name (its folder replaced, a hard link
    /// made to it), they go on in the viewer's own folder.
    fn write_event(&mut self, line: &str) -> std::io::Result<()> {
        if let Some(file) = &mut self.events_out {
            if files::alone(file).is_ok() {
                return file.write_all(format!("{line}\n").as_bytes());
            }
            self.events_out = None;
            self.events_file = self.dir.join(format!("{}.events.jsonl", self.id));
            self.note("events file: gone or linked; on in the viewer's folder");
        }
        files::append_line(&self.events_file, line)
    }

    /// The events a wait asks for (`matching`), of this viewer's.
    fn matching(&self, what: &WaitFor) -> (Vec<Value>, bool) {
        matching(&self.events, &self.deleted, what)
    }

    fn events_answer(&self, events: Vec<Value>, since: u64, until: Option<bool>) -> Value {
        let last = events
            .last()
            .and_then(|e| e.get_num("id"))
            .map(|id| id as u64)
            .unwrap_or(since);
        let mut out = Value::obj()
            .with("events", events)
            .with("last_id", last)
            .with("events_file", self.events_file.display().to_string());
        if let Some(reached) = until {
            out.set("until_reached", reached);
        }
        out
    }

    /// Answers the `events` waits that can be: on a new event, or (`expired`)
    /// those whose time is up.
    fn answer_event_waits(&mut self, expired: bool) {
        let now = Instant::now();
        let waits = std::mem::take(&mut self.waits);
        for wait in waits {
            let WaitFor::Events { since, until, .. } = &wait.what else {
                self.waits.push(wait);
                continue;
            };
            let (events, reached) = self.matching(&wait.what);
            let late = expired && now >= wait.deadline;
            let since = *since;
            if until.is_empty() {
                if !events.is_empty() {
                    let answer = self.events_answer(events, since, None);
                    wait.req.answer(Ok(answer));
                } else if late {
                    wait.req
                        .answer(Err(Error::new("TIMEOUT", "no events came in time")));
                } else {
                    self.waits.push(wait);
                }
            } else if reached || (late && !events.is_empty()) {
                let answer = self.events_answer(events, since, Some(reached));
                wait.req.answer(Ok(answer));
            } else if late {
                wait.req
                    .answer(Err(Error::new("TIMEOUT", "no events came in time")));
            } else {
                self.waits.push(wait);
            }
        }
    }

    fn expire_waits(&mut self) {
        let now = Instant::now();
        if !self.waits.iter().any(|w| now >= w.deadline) {
            return;
        }
        self.answer_event_waits(true);
        let waits = std::mem::take(&mut self.waits);
        for wait in waits {
            if now < wait.deadline {
                self.waits.push(wait);
                continue;
            }
            match wait.what {
                WaitFor::Loaded { sid } => {
                    // Not an error: a slow web address. Its status says so.
                    let answer = self.source_answer(sid);
                    wait.req.answer(answer);
                }
                WaitFor::Restart { result, .. } => wait.req.answer(Ok(result)),
                WaitFor::Shot { .. } => wait.req.answer(Err(Error::new(
                    "TIMEOUT",
                    "the player didn't take the snapshot in time",
                ))),
                WaitFor::Job { .. } => wait
                    .req
                    .answer(Err(Error::new("TIMEOUT", "the page didn't finish in time"))),
                WaitFor::Reply { .. } => wait.req.answer(Err(Error::new(
                    "TIMEOUT",
                    "the player didn't answer in time",
                ))),
                WaitFor::Events { .. } => {}
            }
        }
    }

    fn source_answer(&self, sid: u64) -> Result<Value> {
        let i = self
            .sources
            .iter()
            .position(|s| s.sid == sid)
            .ok_or_else(|| Error::new("NO_SOURCE", "the source was closed"))?;
        let source = &self.sources[i];
        let visible = self.tiles.iter().any(|t| t.0 == i);
        let mut answer =
            Value::obj().with("source", source.to_json(self.focus == Some(i), visible));
        if source.state == State::Loading {
            answer.set(
                "note",
                "still loading: `pepper-agent where` says when it's ready",
            );
        }
        Ok(answer)
    }

    // Players.

    fn on_player(&mut self, sid: u64, event: player::Event) {
        let Some(i) = self.sources.iter().position(|s| s.sid == sid) else {
            return;
        };
        match event {
            player::Event::Log(line) => {
                self.note(&format!("{}: {line}", self.sources[i].name));
                let log = &mut self.sources[i].log;
                log.push(line);
                if log.len() > LOG_KEPT {
                    log.remove(0);
                }
            }
            player::Event::Gone => {
                let source = &mut self.sources[i];
                let why = source
                    .log
                    .iter()
                    .rev()
                    .find(|l| l.starts_with("pepper-play:"))
                    .cloned()
                    .unwrap_or_else(|| "the player ended".into());
                source.state = State::Gone(why.clone());
                if let Some(mut player) = source.player.take() {
                    player.stop();
                    std::thread::spawn(move || {
                        player.reap(Instant::now() + Duration::from_secs(2))
                    });
                }
                self.fail_waits(sid, &Error::new("LOAD_FAILED", why).hint(
                    "The player ended. Check `pepper-agent status` (mpv must be installed: brew install mpv), then open it again.",
                ));
                self.record_dirty = true;
                self.relayout();
            }
            player::Event::Line(line) => self.on_line(i, line),
        }
    }

    fn on_line(&mut self, i: usize, line: Value) {
        let sid = self.sources[i].sid;
        match line.get_str("event") {
            Some("property-change") => {
                let name = line.get_str("name").unwrap_or("");
                let data = line.get("data").unwrap_or(&Value::Null);
                self.on_property(i, name, data);
            }
            Some("playback-restart") => {
                if self.sources[i].state == State::Loading {
                    self.sources[i].state = State::Ready;
                    self.record_dirty = true;
                    self.dirty = true;
                }
                self.live_started(i);
                self.on_restart(sid);
                self.seek_done(i);
                self.sync_seek(sid);
            }
            Some("end-file") if line.get_str("reason") == Some("error") => {
                let why = text::inert(
                    line.get_str("file_error")
                        .unwrap_or("it couldn't be played"),
                    200,
                );
                if self.live_failed(i, &why) {
                    return;
                }
                let source = &mut self.sources[i];
                let detail = source
                    .log
                    .iter()
                    .rev()
                    .take(3)
                    .rev()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" | ");
                source.state = State::Failed(why.clone());
                self.record_dirty = true;
                self.dirty = true;
                let message = if detail.is_empty() {
                    why
                } else {
                    format!("{why} ({detail})")
                };
                self.fail_waits(sid, &Error::new("LOAD_FAILED", message));
            }
            None => {
                if let Some(request) = line.get_num("request_id") {
                    let ok = line.get_str("error") == Some("success");
                    let error = text::inert(line.get_str("error").unwrap_or("no answer"), 200);
                    self.on_reply(sid, request as u64, ok, error);
                }
            }
            _ => {}
        }
    }

    fn on_property(&mut self, i: usize, name: &str, data: &Value) {
        let source = &mut self.sources[i];
        match name {
            "time-pos" => {
                source.time_pos = data.num();
                if let (Some(t), Some(to)) = (source.time_pos, source.review_to)
                    && t >= to
                {
                    source.review_to = None;
                    if let Some(player) = &mut source.player {
                        player.set("pause", true);
                    }
                }
                self.track_played(i);
            }
            "duration" => source.duration = data.num(),
            "pause" => {
                source.paused = data.bool().unwrap_or(false);
                self.record_dirty = true;
            }
            "eof-reached" => {
                source.eof = data.bool().unwrap_or(false);
                self.record_dirty = true;
                if self.sources[i].eof && self.sources[i].live.is_some() {
                    self.live_ended(i);
                }
            }
            "paused-for-cache" => source.buffering = data.bool().unwrap_or(false),
            "dwidth" | "dheight" => {
                let value = data.num().filter(|v| *v > 0.0);
                if name == "dwidth" {
                    source.size.0 = value;
                } else {
                    source.size.1 = value;
                }
                self.relayout();
            }
            "hwdec-current" => source.hwdec = data.str().map(|s| text::inert(s, 40)),
            "container-fps" => source.fps = data.num().filter(|f| f.is_finite() && *f > 0.0),
            _ => {}
        }
        self.dirty = true;
        if name == "pause" {
            let sid = self.sources[i].sid;
            self.sync_pause(sid);
        }
    }

    fn on_restart(&mut self, sid: u64) {
        let waits = std::mem::take(&mut self.waits);
        for wait in waits {
            match wait.what {
                WaitFor::Loaded { sid: s } if s == sid => {
                    let answer = self.source_answer(sid);
                    wait.req.answer(answer);
                }
                WaitFor::Restart {
                    sid: s,
                    result,
                    review_to,
                } if s == sid => {
                    if let Some(source) = self.source_mut(sid) {
                        source.review_to = review_to;
                    }
                    wait.req.answer(Ok(result));
                }
                what => self.waits.push(Wait { what, ..wait }),
            }
        }
    }

    fn on_reply(&mut self, sid: u64, request: u64, ok: bool, error: String) {
        if self.point_shot_taken(sid, request, ok, &error) {
            return;
        }
        let waits = std::mem::take(&mut self.waits);
        for wait in waits {
            match wait.what {
                WaitFor::Shot {
                    sid: s,
                    request: r,
                    result,
                } if s == sid && r == request => {
                    if ok {
                        wait.req.answer(Ok(result));
                    } else {
                        wait.req.answer(Err(Error::new(
                            "FAILED",
                            format!("the player couldn't take the snapshot: {error}"),
                        )
                        .hint("A source with sound only has no picture to take.")));
                    }
                }
                WaitFor::Reply {
                    sid: s,
                    request: r,
                    result,
                } if s == sid && r == request => {
                    if ok {
                        wait.req.answer(Ok(result));
                    } else {
                        wait.req.answer(Err(Error::new(
                            "FAILED",
                            format!("the player couldn't: {error}"),
                        )));
                    }
                }
                what => self.waits.push(Wait { what, ..wait }),
            }
        }
    }

    /// Answers every wait on a source with `error`.
    fn fail_waits(&mut self, sid: u64, error: &Error) {
        let waits = std::mem::take(&mut self.waits);
        for wait in waits {
            let on_it = match &wait.what {
                WaitFor::Loaded { sid: s }
                | WaitFor::Restart { sid: s, .. }
                | WaitFor::Shot { sid: s, .. }
                | WaitFor::Reply { sid: s, .. }
                | WaitFor::Job { sid: s, .. } => *s == sid,
                WaitFor::Events { .. } => false,
            };
            if on_it {
                wait.req.answer(Err(error.clone()));
            } else {
                self.waits.push(wait);
            }
        }
    }

    fn wait(&mut self, req: Request, after: Duration, what: WaitFor) {
        self.waits.push(Wait {
            req,
            deadline: Instant::now() + after,
            what,
        });
    }

    // Requests.

    /// A request from an agent whose commands run in a sandbox, while the
    /// viewer runs outside any: nothing its sandbox keeps from it is
    /// reached through the viewer (`sandboxed_reach`). The files it may
    /// read are named by their real paths from then on. A page it looks at
    /// or drives (or snapshots, or records) mustn't show a file it may not
    /// read: one from a file can go to another (a link, a script).
    fn sandboxed(&self, req: &mut Request) -> Result<()> {
        let peer = req.peer;
        let may_read = |path: &Path| peer.may_read(path);
        let mut remote_page = false;
        for i in self.pages_asked(&req.cmd, &req.args) {
            let Some(web) = &self.sources[i].web else {
                continue;
            };
            remote_page |= web.allow_remote;
            // A page from a file is served with its folders (serve.rs): all
            // it may load, whatever its address says now (pushState).
            let roots = web
                .page
                .as_ref()
                .map(|p| p.served_roots())
                .unwrap_or_default();
            let refused =
                |what: &str| Error::new("SANDBOXED", format!("{} {what}", self.sources[i].name));
            for root in &roots {
                if !may_read(root) {
                    return Err(refused("shows a folder your sandbox keeps from you"));
                }
                readable_dir(root, &may_read)?;
            }
            if !roots.is_empty() && req.cmd == "record.start" {
                return Err(refused(
                    "is a page from a file, which may go to another file while it's recorded: serve it (localhost) to record it",
                ));
            }
        }
        // A video another agent opened, of a file this one may not read.
        if matches!(req.cmd.as_str(), "snapshot" | "record.start")
            && let Ok(i) = self.pick(&req.args)
        {
            let s = &self.sources[i];
            if s.web.is_none() && s.target.starts_with('/') && !may_read(Path::new(&s.target)) {
                return Err(Error::new(
                    "SANDBOXED",
                    format!("{} shows a file your sandbox keeps from you", s.name),
                ));
            }
        }
        sandboxed_reach(&req.cmd, &mut req.args, &may_read, remote_page)
    }

    /// The sources a request may act on as pages (pick_web's choice, or
    /// all it may be): for a web command, a snapshot or a recording.
    fn pages_asked(&self, cmd: &str, args: &Value) -> Vec<usize> {
        let pages = |i: &usize| self.sources[*i].web.is_some();
        if matches!(cmd, "snapshot" | "record.start") {
            return self.pick(args).into_iter().filter(pages).collect();
        }
        if !cmd.starts_with("web.") || cmd == "web.open" {
            return Vec::new();
        }
        if let Some(i) = self.own_source(args, |i| self.sources[i].web.is_some()) {
            return vec![i];
        }
        if let Some(name) = args.get_str("source") {
            return (0..self.sources.len())
                .filter(|&i| self.sources[i].name == name)
                .collect();
        }
        match self.focused_web() {
            Some(i) => vec![i],
            None => (0..self.sources.len()).filter(pages).collect(),
        }
    }

    fn on_request(&mut self, mut req: Request) {
        // Not in the log: what's typed into a page (a password, maybe), nor
        // a session's token.
        let mut logged = req.args.clone();
        if logged.get("session").is_some() {
            logged.set("session", "***");
        }
        if req.cmd == "web.type" {
            logged.set("text", "***");
        }
        self.note(&format!("request {} {}", req.cmd, logged.to_line()));
        if req.peer.sandboxed
            && let Err(e) = self.sandboxed(&mut req)
        {
            return req.answer(Err(e));
        }
        let args = req.args.clone();
        // A session this viewer never gave (or gave before it started
        // again); a new one may still be asked for.
        if let Some(token) = args.get_str("session")
            && self.session_owner(token).is_none()
            && !matches!(req.cmd.as_str(), "session.new" | "status")
        {
            return req.answer(Err(sharing::no_session()));
        }
        let result = match req.cmd.as_str() {
            "status" => Ok(self.status()),
            "sources" => {
                let owner = args.get_str("owner");
                let list: Vec<Value> = self
                    .sources_json()
                    .into_iter()
                    .filter(|s| owner.is_none_or(|o| s.get_str("owner") == Some(o)))
                    .collect();
                Ok(Value::obj().with("sources", list))
            }
            "session.new" => self.new_session(&args),
            "record.start" => self.record_start(&args),
            "record.stop" => return self.record_stop(req),
            "compare" => self.set_compare(&args),
            "open" | "web.open" => return self.open(req),
            "watch" => return self.watch(req),
            "pipe.new" => self.pipe_new(&args),
            "close" => self.close(&args),
            "layout" => self.set_layout(&args),
            "focus" => self.set_focus(&args),
            "snapshot" => return self.snapshot(req),
            "say" => self.set_say(&args),
            "events" => return self.events_request(req),
            "play" | "pause" | "toggle" => {
                let cmd = req.cmd.clone();
                self.pause(&cmd, &args)
            }
            "seek" => return self.seek(req),
            "loop" => return self.set_loop(req),
            "where" => self.where_is(&args),
            "review" => return self.review(req),
            cmd if cmd.starts_with("web.") => return self.web_request(req),
            _ => Err(Error::new("UNSUPPORTED", "unknown command")),
        };
        req.answer(result);
    }

    fn sources_json(&self) -> Vec<Value> {
        self.sources
            .iter()
            .enumerate()
            .map(|(i, s)| s.to_json(self.focus == Some(i), self.tiles.iter().any(|t| t.0 == i)))
            .collect()
    }

    fn status(&self) -> Value {
        Value::obj()
            .with("id", self.id.as_str())
            .with("pid", std::process::id())
            .with("started", sys::iso_time(self.started))
            .with("project", self.project.display().to_string())
            .with("events_file", self.events_file.display().to_string())
            .with(
                "layout",
                if self.zoom {
                    "single (zoomed)"
                } else {
                    self.mode.name()
                },
            )
            .with(
                "focus",
                self.focused().map(|i| self.sources[i].name.as_str()),
            )
            .with("say", self.say.as_deref())
            .with(
                "browser",
                self.host
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|h| h.version()),
            )
            .with("last_event_id", self.next_event - 1)
            .with("waiting_for_note", self.prompt.is_some())
            .with("sources", self.sources_json())
    }

    fn write_record(&mut self) {
        self.record_dirty = false;
        let sources: Vec<Value> = self
            .sources
            .iter()
            .map(|s| {
                Value::obj()
                    .with("name", s.name.as_str())
                    .with("kind", s.kind)
                    .with("target", s.shown_target())
                    .with("status", s.status())
            })
            .collect();
        let record = Value::obj()
            .with("v", proto::VERSION)
            .with("id", self.id.as_str())
            .with("pid", std::process::id())
            .with("started", sys::iso_time(self.started))
            .with("started_unix", self.started)
            .with("project", self.project.display().to_string())
            .with("pepper_port", self.pepper_port.as_str())
            .with("sources", sources);
        let path = proto::record_path(&self.dir, &self.id);
        if let Err(e) = files::write_private(&path, record.to_pretty().as_bytes()) {
            self.note(&format!("record: {e}"));
        }
    }

    /// The source a request names (`source`), or the focused one, or the
    /// only one.
    fn pick(&self, args: &Value) -> Result<usize> {
        if let Some(name) = args.get_str("source") {
            return self
                .sources
                .iter()
                .position(|s| s.name == name)
                .ok_or_else(|| {
                    Error::new(
                        "NO_SOURCE",
                        format!("no source is called {:?}", text::inert(name, 64)),
                    )
                });
        }
        if let Some(i) = self.own_source(args, |_| true) {
            return Ok(i);
        }
        self.focused()
            .or((self.sources.len() == 1).then_some(0))
            .ok_or_else(|| Error::new("NO_SOURCE", "no source is open"))
    }

    /// The source a request names, ready to play.
    fn pick_ready(&self, args: &Value) -> Result<usize> {
        let i = self.pick(args)?;
        let source = &self.sources[i];
        match &source.state {
            State::Ready => Ok(i),
            State::Loading => Err(Error::new("TIMEOUT", format!("{} is still loading", source.name))
                .hint("Run `pepper-agent where` until its status isn't loading, then again.")),
            State::Failed(e) | State::Gone(e) => Err(Error::new(
                "LOAD_FAILED",
                format!("{} couldn't be played: {e}", source.name),
            )
            .hint("Close it (`pepper-agent close <name>`) and open it again, or check it with `pepper-agent probe`.")),
        }
    }

    /// The source a request names, ready, and with a timeline; the request's
    /// to drive.
    fn pick_playable(&self, args: &Value) -> Result<usize> {
        let i = self.pick(args)?;
        if self.sources[i].web.is_some() {
            return Err(Error::new(
                "NOT_PLAYABLE",
                format!("{} is a web page: it has no timeline", self.sources[i].name),
            )
            .hint("Drive a page with `pepper-agent web …` (click, scroll, goto); see `pepper-agent guide`."));
        }
        self.check_owner(i, args)?;
        self.pick_ready(args)
    }

    fn open(&mut self, req: Request) {
        let result = self.start_open(&req.args);
        match result {
            Ok(sid) => self.wait(req, OPEN_WAIT, WaitFor::Loaded { sid }),
            Err(e) => req.answer(Err(e)),
        }
    }

    fn watch(&mut self, req: Request) {
        match self.start_watch(&req.args) {
            Ok(sid) => self.wait(req, OPEN_WAIT, WaitFor::Loaded { sid }),
            Err(e) => req.answer(Err(e)),
        }
    }

    fn start_open(&mut self, args: &Value) -> Result<u64> {
        let target = args
            .get_str("target")
            .ok_or_else(|| Error::new("BAD_ARGS", "open: no target"))?;
        let kind = args.get_str("kind").unwrap_or("media");
        if kind == "web" {
            return self.start_web(args);
        }
        // A recording `play` shows is a video like any other, so labelled;
        // a stream's address is a stream, however asked for.
        let kind: &'static str = match kind {
            "media" | "stream" if live::is_stream(target) => "stream",
            "media" => "media",
            "recording" => "recording",
            other => {
                return Err(Error::new(
                    "UNSUPPORTED",
                    format!(
                        "this viewer shows media only, not {:?}",
                        text::inert(other, 20)
                    ),
                ));
            }
        };
        let mut target = check_target(target, &self.project)?;
        let edit = args
            .get("edit")
            .map(edit::Timeline::from_json)
            .transpose()?;
        // An edit's preview: the one file whose references are followed, an
        // EDL the viewer writes itself.
        let own_edl = args.get_bool("edl") == Some(true);
        if own_edl {
            target = self.own_edl(edit.as_ref())?;
        }
        let (session, owner) = self.claim(args)?;
        let stream = kind == "stream";
        self.add_source(
            args,
            New {
                kind,
                references: own_edl || player::remote(&target),
                target,
                // A stream is live: it plays.
                play: stream || args.get_bool("play").unwrap_or(false),
                session,
                owner,
                edit,
                live: stream.then(|| live::Live::Stream(live::Streamed::new())),
            },
        )
    }

    /// An edit preview's EDL (`edl` in an open request) is the viewer's own,
    /// written in its folder from the timeline sent with it, each clip at
    /// its real path: a file [`check_target`] takes, that reads as a sound
    /// or a video by itself (probed: not a playlist, a list or an EDL by
    /// another name, whose own references mpv would follow too). So what
    /// mpv opens through it is the clips the answers name, never what a file
    /// names, and the EDL the command wrote is never read.
    fn own_edl(&mut self, edit: Option<&edit::Timeline>) -> Result<String> {
        const O_NOFOLLOW: i32 = 0x0100;
        let refuse = |why: String| Error::new("BAD_ARGS", format!("an edit preview's EDL: {why}"));
        let mut own = edit
            .ok_or_else(|| refuse("no timeline with it".into()))?
            .clone();
        for src in &mut own.srcs {
            if !src.starts_with('/') {
                return Err(refuse("a clip that isn't a file".into()));
            }
            let real = check_target(src, &self.project)?;
            // A picture or a sound (media::probe's `video`, `audio`).
            let plain = crate::media::probe(&self.shots, Path::new(&real)).is_ok_and(|p| {
                p.json.get("video").is_some_and(|v| !v.is_null())
                    || p.json
                        .get("audio")
                        .and_then(Value::arr)
                        .is_some_and(|a| !a.is_empty())
            });
            if !plain {
                return Err(refuse(format!(
                    "the clip {} isn't a sound or a video by itself",
                    text::inert(src, 300)
                )));
            }
            *src = real;
        }
        let path = self.output("edit", "edl")?;
        OpenOptions::new()
            .write(true)
            .truncate(true)
            .custom_flags(O_NOFOLLOW)
            .open(&path)
            .and_then(|mut f| f.write_all(own.edl().as_bytes()))
            .map_err(|e| Error::new("FAILED", format!("{}: {e}", path.display())))?;
        Ok(path.display().to_string())
    }

    /// A source with a player, named as the request says: in the box of
    /// the source of that name if there's one (the same kind of plain
    /// video), or a new one.
    fn add_source(&mut self, args: &Value, new: New) -> Result<u64> {
        let name = args.get_str("name");
        if let Some(name) = name
            && !valid_name(name)
        {
            return Err(Error::new(
                "BAD_ARGS",
                format!("{:?} isn't a source name", text::inert(name, 70)),
            )
            .hint("A name is 1 to 64 of A-Z a-z 0-9 . _ / -, starting with a letter or digit."));
        }
        let same = name.and_then(|n| self.sources.iter().position(|s| s.name == n));
        if let Some(i) = same
            && self.sources[i].player.is_some()
            && self.sources[i].live.is_none()
            && new.live.is_none()
            && self.check_owner(i, args).map(|_| true)?
        {
            // The same name again: the new target, in the same box.
            let source = &mut self.sources[i];
            source.target = new.target.clone();
            source.kind = new.kind;
            source.state = State::Loading;
            source.time_pos = None;
            source.duration = None;
            source.eof = false;
            source.review_to = None;
            source.loop_ab = None;
            source.log.clear();
            source.edit = new.edit;
            source.scrub = seekbar::Scrub::default();
            if let Some(player) = &mut source.player {
                player.set("ab-loop-a", "no");
                player.set("ab-loop-b", "no");
                player.set("pause", !new.play);
                player.load(&new.target, new.references, None);
            }
            let sid = source.sid;
            self.focus = Some(i);
            self.record_dirty = true;
            self.relayout();
            return Ok(sid);
        }
        if let Some(i) = same {
            // Another kind, or its player ended: it goes, and a new one comes.
            self.check_owner(i, args)?;
            self.remove_source(i);
        }
        if self.sources.len() >= MAX_SOURCES {
            return Err(Error::new(
                "LIMIT_REACHED",
                format!("this viewer already shows {MAX_SOURCES} sources"),
            ));
        }
        let name = match name {
            Some(n) => n.to_owned(),
            None => self.unique_name(&new.target),
        };
        let token = sys::random_hex().map_err(|e| Error::new("FAILED", format!("a token: {e}")))?;
        let sid = self.next_sid;
        self.next_sid += 1;
        let mut player = Player::start(&self.play, sid, &token, self.tx.clone(), self.log.is_some())
            .map_err(|e| {
                Error::new("MISSING_DEP", format!("cannot run {}: {e}", self.play.display()))
                    .hint("pepper-play comes with Pepper; mpv's library too must be installed: brew install mpv")
            })?;
        let load = match &new.live {
            Some(live) => {
                live.prepare(&mut player, &new.target);
                live.load_target(&new.target)
            }
            None => new.target.clone(),
        };
        if new.play {
            player.set("pause", false);
        }
        player.load(&load, new.references, None);
        self.sources.push(Source {
            sid,
            name,
            kind: new.kind,
            target: new.target,
            token,
            player: Some(player),
            state: State::Loading,
            time_pos: None,
            duration: None,
            paused: !new.play,
            eof: false,
            buffering: false,
            size: (None, None),
            hwdec: None,
            review_to: None,
            loop_ab: None,
            placed: None,
            log: Vec::new(),
            web: None,
            edit: new.edit,
            session: new.session,
            owner: new.owner,
            live: new.live,
            fps: None,
            scrub: seekbar::Scrub::default(),
        });
        self.focus = Some(self.sources.len() - 1);
        self.record_dirty = true;
        self.relayout();
        Ok(sid)
    }

    /// A name from the file's or the address's last part, not taken yet.
    fn unique_name(&self, target: &str) -> String {
        // Never a part of a user and password.
        let target = text::redact_url(target);
        let last = target
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("");
        let last = last.split(['?', '#']).next().unwrap_or("");
        let stem = match last.rsplit_once('.') {
            Some((stem, _)) if !stem.is_empty() => stem,
            _ => last,
        };
        let mut base: String = stem
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "._-".contains(c) {
                    c
                } else {
                    '-'
                }
            })
            .take(32)
            .collect();
        base = base
            .trim_matches(|c: char| !c.is_ascii_alphanumeric())
            .to_owned();
        if base.is_empty() {
            base = "media".into();
        }
        let taken = |n: &str| self.sources.iter().any(|s| s.name == n);
        if !taken(&base) {
            return base;
        }
        (2..)
            .map(|k| format!("{base}-{k}"))
            .find(|n| !taken(n))
            .unwrap_or(base)
    }

    fn remove_source(&mut self, i: usize) {
        let mut source = self.sources.remove(i);
        if source.placed.is_some() {
            self.screen.write(&screen::remove(&source.token));
        }
        if let Some(web) = source.web.take() {
            web.close();
            self.drop_points(source.sid);
            self.close_browser_if_unused();
        }
        if let Some(mut player) = source.player.take() {
            player.stop();
            std::thread::spawn(move || player.reap(Instant::now() + Duration::from_secs(2)));
        }
        self.fail_waits(
            source.sid,
            &Error::new("NO_SOURCE", format!("{} was closed", source.name)),
        );
        self.focus = match self.focus {
            Some(f) if f == i => (!self.sources.is_empty()).then(|| i.min(self.sources.len() - 1)),
            Some(f) if f > i => Some(f - 1),
            f => f,
        };
        if self
            .compare
            .as_ref()
            .is_some_and(|c| c.a == source.sid || c.b == source.sid)
        {
            self.compare = None;
        }
        self.recordings.retain(|r| r.sid != source.sid);
        if self.sources.len() < 2 {
            self.zoom = false;
        }
        self.record_dirty = true;
        self.relayout();
    }

    fn close(&mut self, args: &Value) -> Result<Value> {
        let name = args
            .get_str("name")
            .ok_or_else(|| Error::new("BAD_ARGS", "close: which source?"))?;
        let i = self
            .sources
            .iter()
            .position(|s| s.name == name)
            .ok_or_else(|| {
                Error::new(
                    "NO_SOURCE",
                    format!("no source is called {:?}", text::inert(name, 64)),
                )
            })?;
        self.check_owner(i, args)?;
        self.remove_source(i);
        Ok(Value::obj()
            .with("closed", name)
            .with("sources", self.sources_json()))
    }

    fn set_layout(&mut self, args: &Value) -> Result<Value> {
        let mode = args.get_str("mode").and_then(Mode::parse).ok_or_else(|| {
            Error::new(
                "BAD_ARGS",
                "layout: one of auto, single, split-h, split-v, grid",
            )
        })?;
        self.end_compare();
        self.mode = mode;
        self.zoom = false;
        self.relayout();
        let visible: Vec<Value> = self
            .tiles
            .iter()
            .map(|t| Value::from(self.sources[t.0].name.as_str()))
            .collect();
        Ok(Value::obj()
            .with("layout", mode.name())
            .with("visible", Value::Arr(visible)))
    }

    fn set_focus(&mut self, args: &Value) -> Result<Value> {
        let i = self.pick(args)?;
        self.focus = Some(i);
        self.relayout();
        Ok(Value::obj().with("focus", self.sources[i].name.as_str()))
    }

    fn set_say(&mut self, args: &Value) -> Result<Value> {
        let said = args
            .get_str("text")
            .unwrap_or("")
            .replace(['\n', '\r', '\t'], " ");
        let said = text::inert(said.trim(), MAX_SAY);
        // Whose line it is: the session's owner, when there's one.
        let who = args
            .get_str("session")
            .and_then(|s| self.session_owner(s))
            .unwrap_or("agent");
        self.say = (!said.is_empty()).then(|| format!("{who}: {said}"));
        self.dirty = true;
        Ok(Value::obj().with("say", self.say.as_deref()))
    }

    /// A new file for a program to write into by name (mpv's snapshot,
    /// ffmpeg's recording or difference), in the viewer's folder for them:
    /// `<what>-<n>-<random>.<ext>`, a name no one can guess, made here new
    /// and empty (never through a link, never over a file) beforehand.
    fn output(&mut self, what: &str, ext: &str) -> Result<PathBuf> {
        const O_NOFOLLOW: i32 = 0x0100;
        self.shot_count += 1;
        let hex = sys::random_hex().map_err(|e| Error::new("FAILED", format!("a name: {e}")))?;
        let path = self
            .shots
            .join(format!("{what}-{}-{}.{ext}", self.shot_count, &hex[..12]));
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW)
            .open(&path)
            .map_err(|e| Error::new("FAILED", format!("{}: {e}", path.display())))?;
        Ok(path)
    }

    fn snapshot(&mut self, req: Request) {
        if let Ok(i) = self.pick(&req.args)
            && self.sources[i].web.is_some()
        {
            return self.web_snapshot(req, i);
        }
        let started = (|| {
            let i = self.pick_ready(&req.args)?;
            if self.sources[i].aspect().is_none() {
                return Err(Error::new(
                    "FAILED",
                    format!("{} has no picture (sound only)", self.sources[i].name),
                ));
            }
            let sid = self.sources[i].sid;
            let path = self.output(&sid.to_string(), "png")?;
            let source = &mut self.sources[i];
            let mut result = Value::obj()
                .with("raw", path.display().to_string())
                .with("source", source.name.as_str())
                .with("kind", source.kind);
            source.origin(&mut result);
            result.set("time_pos", source.time_pos.map(round3));
            result.set("time_text", source.time_pos.map(text::clock));
            result.set("duration", source.duration.map(round3));
            result.set("paused", source.paused);
            if let (Some(edit), Some(t)) = (&source.edit, source.time_pos) {
                edit.describe(t, &mut result);
            }
            let path = path.display().to_string();
            let request = source
                .player
                .as_mut()
                .map(|p| p.command(&["screenshot-to-file", &path, "video"]))
                .unwrap_or(0);
            Ok((source.sid, request, result))
        })();
        match started {
            Ok((sid, request, result)) => self.wait(
                req,
                SHOT_WAIT,
                WaitFor::Shot {
                    sid,
                    request,
                    result,
                },
            ),
            Err(e) => req.answer(Err(e)),
        }
    }

    fn events_request(&mut self, req: Request) {
        let parsed = (|| {
            let args = &req.args;
            let since = args.get_num("since").unwrap_or(0.0);
            let wait = args.get_num("wait").unwrap_or(0.0);
            if !(since >= 0.0 && since.is_finite()) {
                return Err(Error::new("BAD_ARGS", "--since: an event id, 0 or more"));
            }
            if !(0.0..=MAX_WAIT).contains(&wait) {
                return Err(Error::new("BAD_ARGS", "--wait: 0 to 3600 seconds"));
            }
            let list = |key: &str| -> Result<Vec<String>> {
                let mut out = Vec::new();
                for item in args.get(key).and_then(Value::arr).unwrap_or(&[]) {
                    let kind = item.str().unwrap_or("");
                    if !EVENT_TYPES.contains(&kind) {
                        return Err(Error::new(
                            "BAD_ARGS",
                            format!(
                                "{:?} isn't an event type ({})",
                                text::inert(kind, 20),
                                EVENT_TYPES.join(", ")
                            ),
                        ));
                    }
                    out.push(kind.to_owned());
                }
                Ok(out)
            };
            let what = WaitFor::Events {
                since: since as u64,
                types: list("types")?,
                source: args.get_str("source").map(str::to_owned),
                until: list("until")?,
                owner: args.get_str("owner").map(str::to_owned),
            };
            Ok((what, wait))
        })();
        let (what, wait) = match parsed {
            Ok(p) => p,
            Err(e) => return req.answer(Err(e)),
        };
        let (events, reached) = self.matching(&what);
        let WaitFor::Events { since, until, .. } = &what else {
            return;
        };
        let since = *since;
        let until_asked = !until.is_empty();
        let done = if until_asked {
            reached
        } else {
            !events.is_empty()
        };
        if done || wait <= 0.0 {
            let answer = self.events_answer(events, since, until_asked.then_some(reached));
            req.answer(Ok(answer));
        } else {
            self.wait(req, Duration::from_secs_f64(wait), what);
        }
    }

    fn pause(&mut self, cmd: &str, args: &Value) -> Result<Value> {
        let i = self.pick_playable(args)?;
        let source = &mut self.sources[i];
        let Some(player) = &mut source.player else {
            return Err(Error::new("NO_SOURCE", "its player ended"));
        };
        let paused = match cmd {
            "play" => false,
            "pause" => true,
            _ => !source.paused,
        };
        if !paused && source.eof {
            // At the end: from the start again.
            player.send(vec!["seek".into(), 0.0.into(), "absolute+exact".into()]);
        }
        if paused {
            source.review_to = None;
        }
        player.set("pause", paused);
        // As asked, so a `where` right after says so; mpv's own word follows.
        source.paused = paused;
        if !paused {
            source.eof = false;
        }
        let answer = Value::obj()
            .with("source", source.name.as_str())
            .with("paused", paused);
        self.dirty = true;
        self.record_dirty = true;
        Ok(answer)
    }

    fn seek(&mut self, req: Request) {
        let started = (|| {
            let i = self.pick_playable(&req.args)?;
            let to = req
                .args
                .get_str("to")
                .ok_or_else(|| Error::new("BAD_ARGS", "seek: to where?"))?;
            let source = &mut self.sources[i];
            let target = match text::parse_time(to).map_err(|e| Error::new("BAD_TIME", e))? {
                Time::At(t) => t,
                Time::By(d) => source.time_pos.unwrap_or(0.0) + d,
            };
            let target = clamp_time(target, source.duration);
            source.review_to = None;
            if let Some(player) = &mut source.player {
                player.send(vec!["seek".into(), target.into(), "absolute+exact".into()]);
            }
            let result = Value::obj()
                .with("source", source.name.as_str())
                .with("time_pos", round3(target))
                .with("time_text", text::clock(target))
                .with("paused", source.paused);
            Ok((source.sid, result))
        })();
        match started {
            Ok((sid, result)) => self.wait(
                req,
                SEEK_WAIT,
                WaitFor::Restart {
                    sid,
                    result,
                    review_to: None,
                },
            ),
            Err(e) => req.answer(Err(e)),
        }
    }

    fn set_loop(&mut self, req: Request) {
        let started = (|| {
            let i = self.pick_playable(&req.args)?;
            let source = &mut self.sources[i];
            let Some(player) = &mut source.player else {
                return Err(Error::new("NO_SOURCE", "its player ended"));
            };
            if req.args.get_bool("off") == Some(true) {
                player.set("ab-loop-a", "no");
                player.set("ab-loop-b", "no");
                source.loop_ab = None;
                return Ok(None);
            }
            let time = |key: &str| -> Result<f64> {
                let text = req
                    .args
                    .get_str(key)
                    .ok_or_else(|| Error::new("BAD_ARGS", "loop: a start and an end, or off"))?;
                text::parse_at(text).map_err(|e| Error::new("BAD_TIME", e))
            };
            let (a, b) = (time("from")?, time("to")?);
            let (a, b) = (
                clamp_time(a, source.duration),
                clamp_time(b, source.duration),
            );
            if b <= a {
                return Err(Error::new(
                    "BAD_TIME",
                    "the loop's end must come after its start",
                ));
            }
            source.review_to = None;
            source.loop_ab = Some((a, b));
            player.set("ab-loop-a", a);
            player.set("ab-loop-b", b);
            player.send(vec!["seek".into(), a.into(), "absolute+exact".into()]);
            player.set("pause", false);
            let result = Value::obj()
                .with("source", source.name.as_str())
                .with("loop", vec![round3(a), round3(b)]);
            Ok(Some((source.sid, result)))
        })();
        match started {
            Ok(Some((sid, result))) => self.wait(
                req,
                SEEK_WAIT,
                WaitFor::Restart {
                    sid,
                    result,
                    review_to: None,
                },
            ),
            Ok(None) => {
                let name = self
                    .pick(&req.args)
                    .map(|i| self.sources[i].name.clone())
                    .unwrap_or_default();
                req.answer(Ok(Value::obj()
                    .with("source", name)
                    .with("loop", Value::Null)));
            }
            Err(e) => req.answer(Err(e)),
        }
    }

    fn where_is(&self, args: &Value) -> Result<Value> {
        let i = self.pick(args)?;
        let s = &self.sources[i];
        if s.web.is_some() {
            return Ok(s.to_json(self.focus == Some(i), self.tiles.iter().any(|t| t.0 == i)));
        }
        let mut v = Value::obj()
            .with("source", s.name.as_str())
            .with("kind", s.kind);
        s.origin(&mut v);
        v.set("status", s.status());
        v.set("time_pos", s.time_pos.map(round3));
        v.set("time_text", s.time_pos.map(text::clock));
        v.set("duration", s.duration.map(round3));
        v.set("paused", s.paused);
        v.set("frame", s.frame());
        v.set("width", s.size.0);
        v.set("height", s.size.1);
        if let (Some(edit), Some(t)) = (&s.edit, s.time_pos) {
            edit.describe(t, &mut v);
        }
        if let Some(live) = &s.live {
            live.to_json(&mut v);
        }
        if let Some((a, b)) = s.loop_ab {
            v.set("loop", vec![round3(a), round3(b)]);
        }
        if let Some(to) = s.review_to {
            v.set("reviewing_to", round3(to));
        }
        if let Some(error) = s.error() {
            v.set("error", error);
        }
        Ok(v)
    }

    fn review(&mut self, req: Request) {
        let started = (|| {
            let i = self.pick_playable(&req.args)?;
            let source = &mut self.sources[i];
            let from = req
                .args
                .get_str("from")
                .ok_or_else(|| Error::new("BAD_ARGS", "review: from when?"))?;
            let from = clamp_time(
                text::parse_at(from).map_err(|e| Error::new("BAD_TIME", e))?,
                source.duration,
            );
            let to = match req.args.get_str("to") {
                Some(to) => text::parse_at(to).map_err(|e| Error::new("BAD_TIME", e))?,
                None => from + 5.0,
            };
            let to = clamp_time(to, source.duration);
            if to <= from {
                return Err(Error::new("BAD_TIME", "the end must come after the start"));
            }
            let Some(player) = &mut source.player else {
                return Err(Error::new("NO_SOURCE", "its player ended"));
            };
            source.loop_ab = None;
            source.review_to = None;
            player.set("ab-loop-a", "no");
            player.set("ab-loop-b", "no");
            player.send(vec!["seek".into(), from.into(), "absolute+exact".into()]);
            player.set("pause", false);
            let result = Value::obj()
                .with("source", source.name.as_str())
                .with("from", round3(from))
                .with("to", round3(to))
                .with("from_text", text::clock(from))
                .with("to_text", text::clock(to));
            Ok((source.sid, result, to))
        })();
        match started {
            Ok((sid, result, to)) => self.wait(
                req,
                SEEK_WAIT,
                WaitFor::Restart {
                    sid,
                    result,
                    review_to: Some(to),
                },
            ),
            Err(e) => req.answer(Err(e)),
        }
    }

    /// Takes the boxes away, ends the players, answers what waits.
    fn end(&mut self) {
        for wait in std::mem::take(&mut self.waits) {
            wait.req
                .answer(Err(Error::new("NO_VIEWER", "the viewer was closed")));
        }
        let mut out = String::new();
        let mut players = Vec::new();
        let mut pages = Vec::new();
        for source in &mut self.sources {
            if source.placed.take().is_some() {
                out.push_str(&screen::remove(&source.token));
            }
            if let Some(mut player) = source.player.take() {
                player.stop();
                players.push(player);
            }
            // A pipe's player may be waiting for a writer: let it go.
            source.live = None;
            if let Some(web) = source.web.take() {
                pages.push(web);
            }
        }
        for web in pages {
            web.close();
        }
        if let Some(host) = self.host.lock().unwrap_or_else(|e| e.into_inner()).take() {
            host.close();
        }
        self.screen.write(&out);
        self.screen.leave();
        let deadline = Instant::now() + Duration::from_secs(2);
        let reaping: Vec<_> = players
            .into_iter()
            .map(|p| std::thread::spawn(move || p.reap(deadline)))
            .collect();
        for r in reaping {
            let _ = r.join();
        }
        self.note("viewer ended");
    }
}

/// mpv's name for a key the human pressed on a playing source, for those
/// passed on: pause, seeks, frame steps, speed, volume, subtitle and audio
/// tracks, mpv's own progress and stats in the box. Never mpv's console,
/// screenshots (files in the folder), or quit.
fn mpv_key(key: &Key) -> Option<&'static str> {
    Some(match key {
        Key::Char(' ') => "SPACE",
        Key::Left => "LEFT",
        Key::Right => "RIGHT",
        Key::Up => "UP",
        Key::Down => "DOWN",
        Key::ShiftLeft => "Shift+LEFT",
        Key::ShiftRight => "Shift+RIGHT",
        Key::ShiftUp => "Shift+UP",
        Key::ShiftDown => "Shift+DOWN",
        Key::Backspace => "BS",
        Key::Char(',') => ",",
        Key::Char('.') => ".",
        Key::Char('[') => "[",
        Key::Char(']') => "]",
        Key::Char('{') => "{",
        Key::Char('}') => "}",
        Key::Char('9') => "9",
        Key::Char('0') => "0",
        Key::Char('/') => "/",
        Key::Char('*') => "*",
        Key::Char('j') => "j",
        Key::Char('J') => "J",
        Key::Char('v') => "v",
        Key::Char('#') => "SHARP",
        _ => return None,
    })
}

/// Pauses a playing source while the human writes; whether it was playing.
fn pause_for_note(source: &mut Source) -> bool {
    if source.state == State::Ready
        && !source.paused
        && !source.eof
        && let Some(player) = &mut source.player
    {
        player.set("pause", true);
        return true;
    }
    false
}

/// A time inside the media: from 0 to its length, if known.
fn clamp_time(t: f64, duration: Option<f64>) -> f64 {
    let t = t.max(0.0);
    match duration {
        Some(d) if d.is_finite() && d > 0.0 => t.min(d),
        _ => t,
    }
}

/// What may be loaded, and what to load: an absolute path to a file (the
/// file itself, links resolved; one in `project` that leads out of it is
/// refused, as `media::real_file` says), an http(s) address, or a
/// stream's (rtsp, rtmp, srt, udp). Nothing else reaches mpv's `loadfile`
/// (which would take its own protocols, `av://`, `edl://` and the rest).
fn check_target(target: &str, project: &Path) -> Result<String> {
    if target.len() > 4096 {
        return Err(Error::new("BAD_ARGS", "the target is too long"));
    }
    // A file's name may hold anything but NUL; it's never shown raw.
    if target.starts_with('/') {
        if target.contains('\0') {
            return Err(Error::new(
                "NOT_FOUND",
                format!("no file at {}", text::inert(target, 300)),
            ));
        }
        let real = crate::media::real_file(project, Path::new(target))?;
        return Ok(real.to_string_lossy().into_owned());
    }
    if target.starts_with("https://") || target.starts_with("http://") || live::is_stream(target) {
        if target.chars().any(|c| c.is_control() || c == ' ') {
            return Err(Error::new(
                "BAD_ARGS",
                "an address holds no spaces or control characters",
            ));
        }
        return Ok(target.to_owned());
    }
    Err(Error::new(
        "BAD_ARGS",
        "the target must be an absolute path, an http(s) address or a stream's (rtsp, rtmp, srt, udp)",
    ))
}

/// The web commands that only look at a page; the others drive it.
const LOOKING: &[&str] = &[
    "web.shot",
    "web.console",
    "web.errors",
    "web.dom",
    "web.a11y",
];

/// What a sandboxed agent may ask of the viewer, which runs outside any
/// sandbox (mpv and the viewer's Chrome fetch and read for it). A file
/// only if `may_read` (its sandbox's word) says it may read it as named
/// and as it really is, links followed, and then by that real path. No
/// address off this machine, no stream, and no page let reach the network
/// (`--allow-remote`), opened or driven (`remote_page`: one the request
/// may drive is).
fn sandboxed_reach(
    cmd: &str,
    args: &mut Value,
    may_read: &dyn Fn(&Path) -> bool,
    remote_page: bool,
) -> Result<()> {
    let refused = |what: String| Error::new("SANDBOXED", what);
    let file = |given: &Path| -> Result<PathBuf> {
        let real = std::fs::canonicalize(given).unwrap_or_else(|_| given.to_owned());
        if may_read(given) && may_read(&real) {
            return Ok(real);
        }
        Err(refused(format!(
            "{} is a file your sandbox keeps from you",
            text::inert(&given.display().to_string(), 300)
        )))
    };
    if cmd.starts_with("web.") && cmd != "web.open" && !LOOKING.contains(&cmd) && remote_page {
        return Err(refused(
            "that page may reach the network (--allow-remote): only an agent outside a sandbox drives it"
                .into(),
        ));
    }
    // An edit's preview: every clip of its timeline plays through its EDL.
    if cmd == "open" && args.get_bool("edl") == Some(true) {
        let srcs = args
            .get("edit")
            .and_then(|e| e.get("srcs"))
            .and_then(Value::arr)
            .unwrap_or(&[]);
        for src in srcs.iter().filter_map(Value::str) {
            file(Path::new(src))?;
        }
    }
    let key = match cmd {
        "open" | "watch" | "web.open" => "target",
        "web.goto" => "url",
        _ => return Ok(()),
    };
    let page = matches!(cmd, "web.open" | "web.goto") || args.get_str("kind") == Some("web");
    // A page, by `web open` or `open` (its kind web).
    if matches!(cmd, "open" | "web.open") {
        if args.get_bool("allow_remote") == Some(true) {
            return Err(refused(
                "--allow-remote would let the page reach the network".into(),
            ));
        }
        if let Some(dir) = args.get_str("watch") {
            let real = file(Path::new(dir))?;
            args.set("watch", real.display().to_string());
        }
    }
    let Some(target) = args.get_str(key).map(str::to_owned) else {
        return Ok(());
    };
    if target.starts_with('/') {
        let real = file(Path::new(&target))?;
        if page || is_page(&real) {
            readable_folder(&real, may_read)?;
        }
        args.set(key, real.display().to_string());
    } else if let Some((path, after)) = file_url(&target) {
        let real = file(&path)?;
        if page || is_page(&real) {
            readable_folder(&real, may_read)?;
        }
        args.set(key, format!("file://{}{after}", url_path(&real)));
    } else if live::is_stream(&target) {
        return Err(refused("a stream comes from the network".into()));
    } else if target != "about:blank" && !on_this_machine(&target) {
        return Err(refused(format!(
            "{} isn't on this machine",
            text::inert(&text::redact_url(&target), 200)
        )));
    }
    Ok(())
}

/// Whether a file opens as a page (main.rs's `open_target`).
fn is_page(file: &Path) -> bool {
    file.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| ["html", "htm", "xhtml", "svg"].contains(&e.to_ascii_lowercase().as_str()))
}

/// A page from a file is served with its whole folder (serve.rs): a
/// sandboxed agent may have one shown only if its sandbox lets it read
/// every file there. Dot-files aren't served, nor links out of the folder
/// (a link in it is served as the file it leads to, which is checked).
fn readable_folder(page: &Path, may_read: &dyn Fn(&Path) -> bool) -> Result<()> {
    match page.parent() {
        Some(root) => readable_dir(root, may_read),
        None => Ok(()),
    }
}

/// Every file below `root` readable (`readable_folder`), within MOST
/// entries, folders counted too.
fn readable_dir(root: &Path, may_read: &dyn Fn(&Path) -> bool) -> Result<()> {
    const MOST: usize = 4096;
    let refused = |what: String| Error::new("SANDBOXED", what);
    let mut left = vec![root.to_owned()];
    let mut seen = 0;
    while let Some(dir) = left.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.file_name().as_bytes().starts_with(b".") {
                continue;
            }
            let (path, Ok(kind)) = (entry.path(), entry.file_type()) else {
                continue;
            };
            seen += 1;
            if seen > MOST {
                return Err(refused(format!(
                    "the page's folder has over {MOST} files to check: serve it yourself (localhost) to show it"
                )));
            }
            if kind.is_dir() {
                left.push(path);
            } else if kind.is_file() && !may_read(&path) {
                return Err(refused(format!(
                    "the page's folder holds {}, a file your sandbox keeps from you",
                    text::inert(&path.display().to_string(), 300)
                )));
            }
        }
    }
    Ok(())
}

/// A `file://` address of this machine's: its path, read as a browser
/// reads it (percent-decoded, `\` as `/`, `.` and `..` taken out), and what
/// follows the path (`?…`, `#…`).
fn file_url(url: &str) -> Option<(PathBuf, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("file") {
        return None;
    }
    let slash = rest.find('/')?;
    let host = &rest[..slash];
    if !(host.is_empty() || host.eq_ignore_ascii_case("localhost")) {
        return None;
    }
    let rest = &rest[slash..];
    let end = rest.find(['?', '#']).unwrap_or(rest.len());
    let (path, after) = rest.split_at(end);
    let mut bytes = Vec::with_capacity(path.len());
    let raw = path.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (
            raw[i],
            raw.get(i + 1).copied().and_then(hex),
            raw.get(i + 2).copied().and_then(hex),
        ) {
            (b'%', Some(hi), Some(lo)) => {
                bytes.push((hi * 16 + lo) as u8);
                i += 3;
            }
            (b'\\', _, _) => {
                bytes.push(b'/');
                i += 1;
            }
            (b, _, _) => {
                bytes.push(b);
                i += 1;
            }
        }
    }
    let mut parts: Vec<&[u8]> = Vec::new();
    for part in bytes.split(|&b| b == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    let mut out = PathBuf::from("/");
    for part in parts {
        out.push(std::ffi::OsStr::from_bytes(part));
    }
    Some((out, after))
}

/// A path as a `file://` address's, every byte but letters, digits and
/// `/ - . _ ~` percent-encoded.
pub(crate) fn url_path(path: &Path) -> String {
    path.as_os_str()
        .as_bytes()
        .iter()
        .map(|&b| {
            if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Whether an http(s) address is of this machine, both as Chrome reads its
/// host (web::is_local) and as ffmpeg does (up to `/ ? #`, so no `\`).
fn on_this_machine(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // `*.localhost` is this machine to Chrome, but mpv and ffmpeg ask DNS.
    let host = authority.rsplit('@').next().unwrap_or("");
    let name = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host.split(':').next().unwrap_or(""),
    };
    matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https")
        && !authority.contains('\\')
        && !name.to_ascii_lowercase().ends_with(".localhost")
        && web::is_local(url)
}

/// Events after `since` (none the human deleted), of the types, source and
/// owner asked for; with `until`, up to the first of those types (which is
/// in, whatever `types`), and whether one came. What ends a wait is of the
/// source and owner asked for too, when it's about a source: another
/// agent's approval doesn't end this one's wait. One about none (`done`,
/// from Enter) is everyone's.
fn matching(events: &[Value], deleted: &HashSet<u64>, what: &WaitFor) -> (Vec<Value>, bool) {
    let WaitFor::Events {
        since,
        types,
        source,
        until,
        owner,
    } = what
    else {
        return (Vec::new(), false);
    };
    let mut out = Vec::new();
    let mut reached = false;
    for event in events {
        let id = event.get_num("id").unwrap_or(0.0) as u64;
        // A mark the human deleted: the delete event says so.
        if id <= *since || deleted.contains(&id) {
            continue;
        }
        let kind = event.get_str("type").unwrap_or("");
        let theirs = source
            .as_deref()
            .is_none_or(|s| event.get_str("source") == Some(s))
            && owner
                .as_deref()
                .is_none_or(|o| event.get_str("owner") == Some(o));
        let is_until = until.iter().any(|u| u == kind) && (event.get("source").is_none() || theirs);
        let wanted = is_until || ((types.is_empty() || types.iter().any(|t| t == kind)) && theirs);
        if wanted && out.len() < MAX_EVENTS_ANSWERED {
            out.push(event.clone());
        }
        if is_until {
            reached = true;
            break;
        }
    }
    (out, reached)
}

/// A source's name: 1 to 64 of A-Z a-z 0-9 . _ / -, from a letter or digit.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._/-".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(valid_name("clip"));
        assert!(valid_name("agent-2/web"));
        assert!(!valid_name("-x"));
        assert!(!valid_name("a b"));
        assert!(!valid_name(""));
        assert!(!valid_name(&"a".repeat(65)));
        assert!(!valid_name("a\x1b"));
    }

    #[test]
    fn targets() {
        let project = Path::new(env!("CARGO_MANIFEST_DIR"));
        let check = |target: &str| check_target(target, project);
        assert_eq!(
            check("https://example.com/v").unwrap(),
            "https://example.com/v"
        );
        assert_eq!(check("/no/such/file").unwrap_err().code, "NOT_FOUND");
        for bad in [
            "av://lavfi:testsrc",
            "edl://x",
            "file.mp4",
            "fd://3",
            "https://a\nb",
        ] {
            assert_eq!(check(bad).unwrap_err().code, "BAD_ARGS", "{bad}");
        }
        assert!(check(env!("CARGO_MANIFEST_DIR")).is_err());
        // The file itself is what's loaded.
        let manifest = project.join("Cargo.toml").canonicalize().unwrap();
        let roundabout = concat!(env!("CARGO_MANIFEST_DIR"), "/src/../Cargo.toml");
        assert_eq!(check(roundabout).unwrap(), manifest.display().to_string());

        // A project's link that leads out of it is refused.
        let dir =
            std::env::temp_dir().join(format!("pepper-viewer-targets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("project")).unwrap();
        std::fs::write(dir.join("secret.mp4"), "x").unwrap();
        std::os::unix::fs::symlink(dir.join("secret.mp4"), dir.join("project/demo.mp4")).unwrap();
        let linked = dir.join("project/demo.mp4").display().to_string();
        let e = check_target(&linked, &dir.join("project")).unwrap_err();
        assert_eq!(e.code, "ORIGIN_BLOCKED");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn times_stay_inside() {
        assert_eq!(clamp_time(-3.0, Some(10.0)), 0.0);
        assert_eq!(clamp_time(12.0, Some(10.0)), 10.0);
        assert_eq!(clamp_time(12.0, None), 12.0);
    }

    #[test]
    fn an_answer_too_long_to_read_is_an_error() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        theirs
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let req = Request {
            stream: ours,
            cmd: "web.dom".into(),
            args: Value::obj(),
            peer: sys::Peer::of(sys::uid(), std::process::id(), false),
        };
        req.answer(Ok(Value::obj().with("html", "x".repeat(proto::MAX_ANSWER))));
        let line = proto::read_line(&mut BufReader::new(&theirs), proto::MAX_ANSWER)
            .unwrap()
            .unwrap();
        let answer = crate::json::parse(&line).unwrap();
        let error = Error::from_json(answer.get("error").unwrap());
        assert_eq!(error.code, "TOO_LONG", "{}", &line[..line.len().min(200)]);
    }

    #[test]
    fn a_wait_ends_on_its_own_agents_events_only() {
        let event = |id: u64, kind: &str, source: Option<(&str, &str)>| {
            let mut e = Value::obj().with("id", id).with("type", kind);
            if let Some((name, owner)) = source {
                e.set("source", name);
                e.set("owner", owner);
            }
            e
        };
        let events = vec![
            event(1, "approve", Some(("a-app", "alice"))),
            event(2, "mark", Some(("b", "agent-2"))),
            event(3, "approve", Some(("b-rec", "agent-2"))),
            event(4, "done", None),
        ];
        let ids = |(got, reached): (Vec<Value>, bool)| {
            let ids: Vec<u64> = got
                .iter()
                .filter_map(|e| e.get_num("id"))
                .map(|n| n as u64)
                .collect();
            (ids, reached)
        };
        let wait = |since: u64, until: &[&str], source: Option<&str>, owner: Option<&str>| {
            WaitFor::Events {
                since,
                types: Vec::new(),
                source: source.map(str::to_owned),
                until: until.iter().map(|u| (*u).to_owned()).collect(),
                owner: owner.map(str::to_owned),
            }
        };
        let none = HashSet::new();
        let at = |what: WaitFor| ids(matching(&events, &none, &what));
        // Alice's approval doesn't end agent-2's wait; agent-2's does.
        assert_eq!(
            at(wait(0, &["approve", "reject"], None, Some("agent-2"))),
            (vec![2, 3], true)
        );
        assert_eq!(
            at(wait(3, &["approve"], None, Some("agent-2"))),
            (vec![], false)
        );
        assert_eq!(
            at(wait(0, &["approve"], Some("b-rec"), None)),
            (vec![3], true)
        );
        // Enter's done, about no source, ends anyone's.
        assert_eq!(
            at(wait(0, &["done"], None, Some("agent-2"))),
            (vec![2, 3, 4], true)
        );
        assert_eq!(at(wait(0, &["approve"], None, None)), (vec![1], true));
        // With --type, what ends the wait is in too.
        let typed = WaitFor::Events {
            since: 0,
            types: vec!["mark".into()],
            source: None,
            until: vec!["done".into()],
            owner: None,
        };
        assert_eq!(at(typed), (vec![2, 4], true));
        let deleted = HashSet::from([2]);
        let what = wait(0, &["done"], None, Some("agent-2"));
        assert_eq!(ids(matching(&events, &deleted, &what)), (vec![3, 4], true));
    }

    #[test]
    fn keys_passed_on() {
        assert_eq!(mpv_key(&Key::Char(' ')), Some("SPACE"));
        for kept in ['q', 'Q', '`', 's', 'S', ':', 'm', 'n', 'z', 'l', 'L'] {
            assert_eq!(mpv_key(&Key::Char(kept)), None, "{kept}");
        }
        assert_eq!(mpv_key(&Key::Ctrl('s')), None);
    }

    #[test]
    fn a_sandboxed_agent_reaches_no_further_through_the_viewer() {
        let dir = std::env::temp_dir().join(format!("pepper-sandboxed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (ok, secret) = (dir.join("ok"), dir.join("secret"));
        std::fs::create_dir_all(&ok).unwrap();
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::write(ok.join("a.mp4"), "x").unwrap();
        std::fs::write(ok.join("p.html"), "x").unwrap();
        std::fs::write(secret.join("s.mp4"), "x").unwrap();
        std::os::unix::fs::symlink(secret.join("s.mp4"), ok.join("link.mp4")).unwrap();
        let ok = ok.canonicalize().unwrap();
        let secret = secret.canonicalize().unwrap();
        // Its sandbox keeps `secret` from it.
        let may_read = |p: &Path| !p.starts_with(&secret) && !p.ends_with("hidden.txt");
        let ask = |cmd: &str, args: Value, remote_page: bool| {
            let mut args = args;
            sandboxed_reach(cmd, &mut args, &may_read, remote_page).map(|()| args)
        };
        let target = |t: &str| Value::obj().with("target", t);
        let refused = |r: Result<Value>| r.is_err_and(|e| e.code == "SANDBOXED");

        // Files: by their real paths, links followed.
        let a = ok.join("a.mp4").display().to_string();
        assert_eq!(
            ask("open", target(&a), false).unwrap().get_str("target"),
            Some(a.as_str())
        );
        assert!(refused(ask(
            "open",
            target(&secret.join("s.mp4").display().to_string()),
            false
        )));
        assert!(refused(ask(
            "open",
            target(&ok.join("link.mp4").display().to_string()),
            false
        )));
        assert!(refused(ask(
            "watch",
            target(&ok.join("link.mp4").display().to_string()),
            false
        )));
        let page = format!("file://{}", ok.join("p.html").display());
        let opened = ask("web.open", target(&page), false).unwrap();
        assert_eq!(opened.get_str("target"), Some(page.as_str()));
        let sneaky = format!("file://{}/%2E%2E\\secret/s.mp4#x", ok.display());
        assert!(refused(ask("web.open", target(&sneaky), false)));
        let watching = target(&page).with("watch", secret.display().to_string());
        assert!(refused(ask("web.open", watching, false)));
        // A page's whole folder is served: all of it must be readable.
        let mixed = dir.join("mixed");
        std::fs::create_dir_all(mixed.join("sub")).unwrap();
        std::fs::write(mixed.join("p.html"), "x").unwrap();
        std::fs::write(mixed.join("sub").join("hidden.txt"), "x").unwrap();
        let mixed_page = mixed.canonicalize().unwrap().join("p.html");
        let mixed_page = mixed_page.display().to_string();
        assert!(refused(ask("web.open", target(&mixed_page), false)));
        assert!(refused(ask("open", target(&mixed_page), false)));
        assert!(ask("open", target(&a), false).is_ok());
        // An edit's preview: its clips are files it names.
        let edl = |srcs: Vec<String>| {
            target(&a)
                .with("edl", true)
                .with("edit", Value::obj().with("srcs", srcs))
        };
        let s = secret.join("s.mp4").display().to_string();
        assert!(refused(ask("open", edl(vec![a.clone(), s]), false)));
        assert!(ask("open", edl(vec![a.clone()]), false).is_ok());
        // Nothing off this machine.
        for far in [
            "https://example.com/v.mp4",
            "http://127.0.0.1\\@example.com/v.mp4",
            "http://localhost.example.com/",
            "rtsp://127.0.0.1:8554/cam",
            "http://127.0.0.1:8000/live.m3u8",
            "file://server/share/a.mp4",
        ] {
            assert!(refused(ask("open", target(far), false)), "{far}");
        }
        for near in [
            "http://127.0.0.1:8000/v.mp4",
            "http://localhost:5173/",
            "about:blank",
        ] {
            assert!(ask("open", target(near), false).is_ok(), "{near}");
        }
        let remote = target("http://localhost:5173/").with("allow_remote", true);
        assert!(refused(ask("web.open", remote.clone(), false)));
        assert!(refused(ask("open", remote.with("kind", "web"), false)));
        let watching = target(&page)
            .with("kind", "web")
            .with("watch", secret.display().to_string());
        assert!(refused(ask("open", watching, false)));
        let goto = Value::obj().with("url", "https://example.com/");
        assert!(refused(ask("web.goto", goto, false)));
        // A page let reach the network: looked at, not driven.
        assert!(refused(ask(
            "web.click",
            Value::obj().with("target", "#b"),
            true
        )));
        assert!(refused(ask(
            "web.goto",
            Value::obj().with("url", "about:blank"),
            true
        )));
        assert!(ask("web.dom", Value::obj(), true).is_ok());
        assert!(ask("web.click", Value::obj().with("target", "#b"), false).is_ok());
        assert!(ask("snapshot", Value::obj(), true).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
