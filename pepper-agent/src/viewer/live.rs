//! Live sources (spec 4.7, 4.8): a file shown again each time it's
//! rendered again (`watch`), a FIFO a program writes frames into (`pipe
//! new`), and a stream (rtsp, rtmp, srt, udp, HLS) that comes back by
//! itself when it drops. Each is a pepper-play source, as a video is; what
//! this adds is when to load it again, and how.

use std::fs::OpenOptions;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::player::{self, Player};
use super::watch::Watch;
use super::{New, Viewer, check_target};
use crate::error::{Error, Result};
use crate::json::Value;
use crate::{sys, text};

/// A render counts as done once its file hasn't changed for this long, or
/// at the latest this long after it began changing.
const SETTLED: Duration = Duration::from_millis(250);
const AT_MOST: Duration = Duration::from_secs(2);
/// The human is told a file was shown again if it hadn't been for this
/// long (seconds).
const TELL_AFTER: f64 = 10.0;
/// A stream that dropped is tried again after 1 s, then 2, 4, 8, 10.
const FIRST_RETRY: Duration = Duration::from_secs(1);
const LAST_RETRY: Duration = Duration::from_secs(10);
/// What a watch shows still; anything else it plays, looping.
const PICTURES: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "bmp", "tif", "tiff", "heic", "avif",
];
/// Addresses of streams; an http(s) one is a stream when it's HLS (.m3u8).
pub const STREAMS: &[&str] = &["rtsp", "rtsps", "rtmp", "rtmps", "srt", "udp"];
/// mpv's profile for little delay, on what's live but HLS (whose
/// segments are seconds long anyway).
const LOW_LATENCY: &str = "low-latency";

/// Whether `url` is a stream's address.
pub fn is_stream(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    let scheme = scheme.to_ascii_lowercase();
    let path = rest.split(['?', '#']).next().unwrap_or("");
    STREAMS.contains(&scheme.as_str())
        || (matches!(scheme.as_str(), "http" | "https")
            && path.to_ascii_lowercase().ends_with(".m3u8"))
}

/// What a pipe carries.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Format {
    /// YUV4MPEG2: its header says its size and rate.
    Y4m,
    /// JPEGs one after another.
    Mjpeg,
    /// Raw BGRA frames of a size given beforehand.
    Bgra { width: u32, height: u32, fps: f64 },
}

impl Format {
    pub fn from_args(args: &Value) -> Result<Format> {
        match args.get_str("format").unwrap_or("y4m") {
            "y4m" => Ok(Format::Y4m),
            "mjpeg" => Ok(Format::Mjpeg),
            "bgra" => {
                let side = |key: &str| {
                    args.get_num(key)
                        .filter(|n| n.fract() == 0.0 && (1.0..=8192.0).contains(n))
                        .map(|n| n as u32)
                };
                let (Some(width), Some(height)) = (side("width"), side("height")) else {
                    return Err(Error::new(
                        "BAD_ARGS",
                        "--format bgra needs --size WxH (each 1 to 8192)",
                    ));
                };
                let fps = args.get_num("fps").unwrap_or(30.0);
                if !(1.0..=240.0).contains(&fps) {
                    return Err(Error::new("BAD_ARGS", "--fps: 1 to 240"));
                }
                Ok(Format::Bgra { width, height, fps })
            }
            other => Err(Error::new(
                "BAD_ARGS",
                format!("--format {:?}: y4m, mjpeg or bgra", text::inert(other, 20)),
            )),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Format::Y4m => "y4m",
            Format::Mjpeg => "mjpeg",
            Format::Bgra { .. } => "bgra",
        }
    }

    /// mpv's options for reading it.
    fn options(&self) -> Vec<(&'static str, String)> {
        match *self {
            Format::Y4m => vec![("demuxer-lavf-format", "yuv4mpegpipe".into())],
            Format::Mjpeg => vec![("demuxer-lavf-format", "mjpeg".into())],
            Format::Bgra { width, height, fps } => vec![
                ("demuxer", "rawvideo".into()),
                ("demuxer-rawvideo-mp-format", "bgra".into()),
                ("demuxer-rawvideo-w", width.to_string()),
                ("demuxer-rawvideo-h", height.to_string()),
                ("demuxer-rawvideo-fps", fps.to_string()),
            ],
        }
    }

    /// A command that writes test frames into it.
    fn example(&self, fifo: &Path) -> String {
        let fifo = fifo.display();
        match self {
            Format::Y4m => format!(
                "ffmpeg -re -f lavfi -i testsrc2=size=1280x720:rate=30 -f yuv4mpegpipe -y {fifo}"
            ),
            Format::Mjpeg => format!(
                "ffmpeg -re -f lavfi -i testsrc2=size=1280x720:rate=30 -f mjpeg -q:v 3 -y {fifo}"
            ),
            Format::Bgra { width, height, fps } => format!(
                "ffmpeg -re -f lavfi -i testsrc2=size={width}x{height}:rate={fps} -f rawvideo -pix_fmt bgra -y {fifo}"
            ),
        }
    }
}

/// A file as it is on disk: a new render changes one of these.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Stamp {
    len: u64,
    modified: i64,
    modified_ns: i64,
    inode: u64,
}

/// A plain file's size, time and inode; None for a link (its name taken
/// by one since: never followed) or anything else.
fn stamp(file: &Path) -> Option<Stamp> {
    let meta = std::fs::symlink_metadata(file)
        .ok()
        .filter(|m| m.is_file())?;
    Some(Stamp {
        len: meta.len(),
        modified: meta.mtime(),
        modified_ns: meta.mtime_nsec(),
        inode: meta.ino(),
    })
}

pub enum Live {
    Watch(Watched),
    Pipe(Piped),
    Stream(Streamed),
}

pub struct Watched {
    file: PathBuf,
    still: bool,
    /// Its folder's file events (a render may write a new file and rename
    /// it over the old).
    _folder: Option<Watch>,
    /// The file as last loaded; as last looked at, and since when; since
    /// when it's been changing.
    shown: Option<Stamp>,
    seen: Option<(Stamp, Instant)>,
    changing: Option<Instant>,
    reloads: u32,
    reloaded_at: Option<f64>,
    last_error: Option<String>,
}

pub struct Piped {
    pub fifo: PathBuf,
    format: Format,
    /// Writers so far, and whether it waits for the next.
    writers: u32,
    waiting: bool,
    last_error: Option<String>,
}

impl Drop for Piped {
    fn drop(&mut self) {
        unblock(&self.fifo);
        let _ = std::fs::remove_file(&self.fifo);
    }
}

/// A player waiting in `open` for a writer can't be told to stop: a
/// writer that comes and goes lets it read the end.
fn unblock(fifo: &Path) {
    const O_NONBLOCK: i32 = 0x0004;
    let _ = OpenOptions::new()
        .write(true)
        .custom_flags(O_NONBLOCK)
        .open(fifo);
}

pub struct Streamed {
    retry_at: Option<Instant>,
    backoff: Duration,
    reconnects: u32,
    last_error: Option<String>,
}

impl Streamed {
    pub fn new() -> Streamed {
        Streamed {
            retry_at: None,
            backoff: FIRST_RETRY,
            reconnects: 0,
            last_error: None,
        }
    }
}

impl Live {
    /// What its label and answers say instead of the player's state.
    pub fn status(&self) -> Option<&'static str> {
        match self {
            Live::Pipe(p) if p.waiting => Some("waiting"),
            Live::Stream(s) if s.retry_at.is_some() => Some("reconnecting"),
            _ => None,
        }
    }

    /// What the player loads for it: a pipe through libavformat's own
    /// reading, as a stream that can't seek (mpv's file reading probes a
    /// FIFO as if it could, and stalls).
    pub fn load_target(&self, target: &str) -> String {
        match self {
            Live::Pipe(_) => format!("ffmpeg://file:{target}"),
            _ => target.to_owned(),
        }
    }

    /// Whether it's recorded as the player gets it (mpv's stream-record),
    /// not cut from a file afterwards: a stream's packets as they came.
    pub fn records_as_it_comes(&self) -> bool {
        matches!(self, Live::Stream(_))
    }

    /// Whether it can't be recorded at all: a pipe's raw frames, which
    /// mpv's recorder writes with the wrong pixel format when it starts
    /// in the middle.
    pub fn unrecordable(&self) -> bool {
        matches!(self, Live::Pipe(_))
    }

    pub fn to_json(&self, into: &mut Value) {
        let error = |e: &Option<String>| e.as_deref().map(text::redact_urls);
        match self {
            Live::Watch(w) => {
                into.set("reloads", w.reloads);
                into.set("reloaded_at", w.reloaded_at.map(sys::iso_time));
                into.set("last_error", error(&w.last_error));
            }
            Live::Pipe(p) => {
                into.set("pipe", p.fifo.display().to_string());
                into.set("format", p.format.name());
                into.set("writers", p.writers);
                into.set("last_error", error(&p.last_error));
            }
            Live::Stream(s) => {
                into.set("reconnects", s.reconnects);
                into.set(
                    "retry_in",
                    s.retry_at
                        .map(|at| at.saturating_duration_since(Instant::now()).as_secs_f64()),
                );
                into.set("last_error", error(&s.last_error));
            }
        }
    }

    /// The player's options for it, before it loads.
    pub fn prepare(&self, player: &mut Player, target: &str) {
        match self {
            Live::Watch(w) if !w.still => {
                player.set("loop-file", "inf");
            }
            Live::Watch(_) => {}
            Live::Pipe(p) => {
                player.command(&["apply-profile", LOW_LATENCY]);
                for (name, value) in p.format.options() {
                    player.set(name, value);
                }
                // Each frame as it comes, whatever its time says.
                player.set("untimed", true);
                player.set("cache", "no");
            }
            Live::Stream(_) => {
                if !target.to_ascii_lowercase().contains(".m3u8") {
                    player.command(&["apply-profile", LOW_LATENCY]);
                    // Its streams' setup read, as the profile skips: a
                    // recording's file needs it (mpv's recorder can't
                    // write a header without). Half a second at most.
                    player.set("demuxer-lavf-probe-info", "yes");
                    player.set("demuxer-lavf-analyzeduration", 0.5);
                }
                player.set("network-timeout", 10);
            }
        }
    }
}

impl Viewer {
    /// `watch`: a file shown, and shown again whenever it's rendered again.
    pub(super) fn start_watch(&mut self, args: &Value) -> Result<u64> {
        let target = args
            .get_str("target")
            .ok_or_else(|| Error::new("BAD_ARGS", "watch: no file"))?;
        // A file, checked as open's are: the file itself is watched.
        if !target.starts_with('/') {
            return Err(Error::new(
                "NOT_FOUND",
                format!("no file at {}", text::inert(target, 300)),
            ));
        }
        let target = check_target(target, &self.project)?;
        let file = PathBuf::from(&target);
        if stamp(&file).is_none() {
            return Err(Error::new(
                "NOT_FOUND",
                format!("no file at {}", text::inert(&target, 300)),
            ));
        }
        let still = file
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| PICTURES.contains(&e.to_ascii_lowercase().as_str()));
        let (session, owner) = self.claim(args)?;
        let sid = self.next_sid;
        let folder = file
            .parent()
            .and_then(|dir| dir.canonicalize().ok())
            .and_then(|dir| Watch::start(&dir, sid, self.tx.clone()));
        let live = Live::Watch(Watched {
            shown: stamp(&file),
            file,
            still,
            _folder: folder,
            seen: None,
            changing: None,
            reloads: 0,
            reloaded_at: None,
            last_error: None,
        });
        self.add_source(
            args,
            New {
                kind: "watch",
                target,
                references: false,
                play: !still && args.get_bool("play") != Some(false),
                session,
                owner,
                edit: None,
                live: Some(live),
            },
        )
    }

    /// `pipe new`: a FIFO in the viewer's own folder, its frames shown as a
    /// program writes them. Answers at once, with where to write.
    pub(super) fn pipe_new(&mut self, args: &Value) -> Result<Value> {
        let format = Format::from_args(args)?;
        let (session, owner) = self.claim(args)?;
        let fifo = self.pipes.join(format!("pipe-{}", self.next_sid));
        let _ = std::fs::remove_file(&fifo);
        sys::make_fifo(&fifo)
            .map_err(|e| Error::new("FAILED", format!("a FIFO at {}: {e}", fifo.display())))?;
        let live = Live::Pipe(Piped {
            fifo: fifo.clone(),
            format,
            writers: 0,
            waiting: true,
            last_error: None,
        });
        let mut named = args.clone();
        if named.get_str("name").is_none() {
            named.set("name", self.unique_name("/pipe"));
        }
        // The FIFO is the viewer's own: no check of where it is.
        let sid = self.add_source(
            &named,
            New {
                kind: "pipe",
                target: fifo.display().to_string(),
                references: false,
                play: true,
                session,
                owner,
                edit: None,
                live: Some(live),
            },
        )?;
        let i = self
            .sources
            .iter()
            .position(|s| s.sid == sid)
            .ok_or_else(|| Error::new("FAILED", "the pipe went"))?;
        let focused = self.focus == Some(i);
        Ok(Value::obj()
            .with("source", self.sources[i].to_json(focused, true))
            .with("pipe", fifo.display().to_string())
            .with("format", format.name())
            .with("try", format.example(&fifo))
            .with(
                "next",
                "Write frames into the pipe (a program opening it for writing); they show as they come. When the writer closes it, the last frame stays and the pipe waits for the next.",
            ))
    }

    /// A file changed in a watched file's folder: it's looked at until it
    /// settles. Whether `sid` is a watch.
    pub(super) fn on_watch_live(&mut self, sid: u64) -> bool {
        let Some(source) = self.source_mut(sid) else {
            return false;
        };
        let Some(Live::Watch(w)) = &mut source.live else {
            return false;
        };
        w.changing.get_or_insert_with(Instant::now);
        true
    }

    /// What's due: a settled render shown, a stream tried again.
    pub(super) fn pump_live(&mut self) {
        let now = Instant::now();
        let mut reload = Vec::new();
        for (i, source) in self.sources.iter_mut().enumerate() {
            match &mut source.live {
                Some(Live::Watch(w)) => {
                    let Some(since) = w.changing else { continue };
                    let Some(stamp) = stamp(&w.file) else {
                        // Gone for now: a render that deletes first.
                        continue;
                    };
                    if Some(stamp) == w.shown {
                        w.changing = None;
                        w.seen = None;
                        continue;
                    }
                    let settled =
                        matches!(w.seen, Some((s, at)) if s == stamp && at.elapsed() >= SETTLED);
                    // Still the file it was, as its real path: not through
                    // a folder on the way swapped for a link since.
                    let real = std::fs::canonicalize(&w.file).is_ok_and(|r| r == w.file);
                    if stamp.len > 0 && !real {
                        w.changing = None;
                        w.seen = None;
                        w.last_error = Some("its path now leads elsewhere: not shown again".into());
                    } else if stamp.len > 0 && (settled || since.elapsed() >= AT_MOST) {
                        // Told the human once, not on every render of a
                        // file rendered again and again.
                        let quiet = w.reloaded_at.is_none_or(|t| sys::now() - t >= TELL_AFTER);
                        w.shown = Some(stamp);
                        w.changing = None;
                        w.seen = None;
                        w.reloads += 1;
                        w.reloaded_at = Some(sys::now());
                        reload.push((i, quiet));
                    } else if w.seen.map(|(s, _)| s) != Some(stamp) {
                        w.seen = Some((stamp, now));
                    }
                }
                Some(Live::Stream(s)) if s.retry_at.is_some_and(|at| at <= now) => {
                    s.retry_at = None;
                    s.reconnects += 1;
                    reload.push((i, false));
                }
                _ => {}
            }
        }
        for (i, tell) in reload {
            self.load_again(i);
            if tell {
                let name = self.sources[i].name.clone();
                self.flash(format!("{name} was rendered again: shown."));
            }
        }
    }

    /// Loads a source's target again, in its box: a paused video where it
    /// was.
    fn load_again(&mut self, i: usize) {
        let source = &mut self.sources[i];
        let at = source
            .time_pos
            .filter(|_| source.paused && source.kind == "watch");
        source.state = super::State::Loading;
        source.eof = false;
        // A feed plays again (at its end mpv paused it, to keep its last
        // frame up); a watched file stays as the human left it.
        let resume = source.kind != "watch";
        if resume {
            source.paused = false;
        }
        let target = match &source.live {
            Some(live) => live.load_target(&source.target),
            None => source.target.clone(),
        };
        // A stream's address names its own (HLS's segments); a file's
        // names nothing that's loaded.
        let references = player::remote(&source.target);
        if let Some(player) = &mut source.player {
            player.load(&target, references, at);
            // After the load: still at the end, mpv (keep-open) pauses
            // again whatever it was told before.
            if resume {
                player.set("pause", false);
            }
        }
        self.record_dirty = true;
        self.dirty = true;
    }

    /// The player reached the end: a pipe's writer closed (it waits for the
    /// next), or a stream dropped (it's tried again).
    pub(super) fn live_ended(&mut self, i: usize) {
        match &mut self.sources[i].live {
            Some(Live::Pipe(p)) => {
                p.waiting = true;
                self.load_again(i);
            }
            Some(Live::Stream(s)) => {
                s.last_error = Some("the stream ended".into());
                s.retry_at = Some(Instant::now() + s.backoff);
                s.backoff = (s.backoff * 2).min(LAST_RETRY);
                self.record_dirty = true;
                self.dirty = true;
            }
            _ => {}
        }
    }

    /// The player couldn't play it. A stream is tried again, a pipe waits
    /// for its next writer, a watch for the next render; an `open` waiting
    /// is told. Whether it was a live source.
    pub(super) fn live_failed(&mut self, i: usize, why: &str) -> bool {
        let sid = self.sources[i].sid;
        let name = self.sources[i].name.clone();
        let hint = match &mut self.sources[i].live {
            None => return false,
            Some(Live::Stream(s)) => {
                s.last_error = Some(why.to_owned());
                s.retry_at = Some(Instant::now() + s.backoff);
                s.backoff = (s.backoff * 2).min(LAST_RETRY);
                format!(
                    "It's tried again every few seconds (pepper-agent where {name} says how it goes); close {name} to stop."
                )
            }
            Some(Live::Pipe(p)) => {
                p.last_error = Some(why.to_owned());
                p.waiting = true;
                self.load_again(i);
                "What was written isn't the pipe's format: see pepper-agent where for it.".into()
            }
            Some(Live::Watch(w)) => {
                w.last_error = Some(why.to_owned());
                // A render half written: shown when it's done.
                w.shown = None;
                w.changing.get_or_insert_with(Instant::now);
                "It's shown again once the file is whole (the next render).".into()
            }
        };
        self.sources[i].state = super::State::Loading;
        self.record_dirty = true;
        self.dirty = true;
        self.fail_waits(
            sid,
            &Error::new("LOAD_FAILED", text::redact_urls(why)).hint(hint),
        );
        true
    }

    /// It shows a frame: a stream connected, a pipe's writer began.
    pub(super) fn live_started(&mut self, i: usize) {
        match &mut self.sources[i].live {
            Some(Live::Stream(s)) => {
                s.backoff = FIRST_RETRY;
                s.retry_at = None;
            }
            Some(Live::Pipe(p)) if p.waiting => {
                p.waiting = false;
                p.writers += 1;
            }
            _ => return,
        }
        self.record_dirty = true;
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams() {
        for url in [
            "rtsp://cam.local:554/s",
            "RTSP://cam/s",
            "srt://h:9000?mode=caller",
            "udp://@:1234",
            "rtmp://h/app/key",
            "https://h/live/index.m3u8?token=x",
            "http://h/a.M3U8",
        ] {
            assert!(is_stream(url), "{url}");
        }
        for url in [
            "https://h/v.mp4",
            "/a/b.m3u8",
            "file:///x.m3u8",
            "https://h/m3u8/v.mp4",
        ] {
            assert!(!is_stream(url), "{url}");
        }
    }

    #[test]
    fn pipe_formats() {
        let args = |text: &str| crate::json::parse(text).unwrap();
        assert_eq!(Format::from_args(&args("{}")).unwrap(), Format::Y4m);
        assert_eq!(
            Format::from_args(&args(r#"{"format": "bgra", "width": 640, "height": 360}"#)).unwrap(),
            Format::Bgra {
                width: 640,
                height: 360,
                fps: 30.0
            }
        );
        for bad in [
            r#"{"format": "bgra"}"#,
            r#"{"format": "bgra", "width": 0, "height": 9}"#,
            r#"{"format": "bgra", "width": 9, "height": 9, "fps": 999}"#,
            r#"{"format": "h264"}"#,
        ] {
            assert!(Format::from_args(&args(bad)).is_err(), "{bad}");
        }
        let fifo = Path::new("/t/pipe-3");
        assert!(
            Format::Mjpeg
                .example(fifo)
                .ends_with("-f mjpeg -q:v 3 -y /t/pipe-3")
        );
    }

    #[test]
    fn a_waiting_reader_is_let_go() {
        let dir = std::env::temp_dir().join(format!("pepper-agent-fifo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("p");
        sys::make_fifo(&fifo).unwrap();
        let reader = {
            let fifo = fifo.clone();
            std::thread::spawn(move || std::fs::read(&fifo).map(|b| b.len()))
        };
        std::thread::sleep(Duration::from_millis(100));
        unblock(&fifo);
        assert_eq!(reader.join().unwrap().unwrap(), 0);
        // With no reader, nothing waits.
        unblock(&fifo);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
