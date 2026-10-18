//! Recordings (spec 4.5): what a source shows, to a file. A page's
//! screencast goes to an ffmpeg in the viewer's own folder at a steady
//! frame rate, its latest frame each tick (Chrome sends frames only when
//! the page changes: a still page is a still picture, not a gap); a video's
//! recording is the stretches that played (a seek starts another), which
//! `pepper-agent record stop` cuts from the file itself. The viewer writes
//! only in its own folder; the command moves the file into
//! `.pepper/recordings/`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::frames::jpeg_size;
use super::{Viewer, WaitFor};
use crate::error::{Error, Result};
use crate::json::Value;
use crate::{media, text};

/// A page's recording, frames a second.
const FPS: u32 = 30;
/// The longest recording, and the one unless asked.
pub const MAX_SECONDS: f64 = 600.0;
const DEFAULT_SECONDS: f64 = 120.0;
/// How long `record stop` waits for ffmpeg to finish the file.
const STOP_WAIT: Duration = Duration::from_secs(60);
/// How long a page's recording waits for its first frame at its size.
const FIRST_WAIT: Duration = Duration::from_millis(500);
/// A video's position moving on by more than this, or back, is a jump: a
/// new stretch of what played.
const MAX_STEP: f64 = 1.0;
/// The shortest stretch kept, and the most stretches.
const MIN_SPAN: f64 = 0.05;
const MAX_SPANS: usize = 64;

/// What a page's recorder is sent.
pub enum RecMsg {
    Frame(Arc<Vec<u8>>),
    Stop,
}

/// A recording under way.
pub struct Recording {
    pub sid: u64,
    started: Instant,
    max: Duration,
    /// A page's: its recorder.
    web: Option<WebRecorder>,
    /// A video's: the stretches that played since it started, in order.
    played: Option<Vec<(f64, f64)>>,
    /// A pipe's or a stream's: the file mpv writes what it gets into.
    stream: Option<PathBuf>,
}

/// A page's frames to ffmpeg, on a thread of their own.
pub struct WebRecorder {
    tx: Sender<RecMsg>,
    thread: JoinHandle<Result<Value>>,
}

impl WebRecorder {
    /// Starts recording into `path` (an .mp4), with `first` the frame the
    /// page shows now, if it sent one.
    pub fn start(ffmpeg: PathBuf, path: PathBuf, first: Option<Arc<Vec<u8>>>) -> WebRecorder {
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || write_frames(&ffmpeg, &path, first, rx));
        WebRecorder { tx, thread }
    }

    pub fn sender(&self) -> Sender<RecMsg> {
        self.tx.clone()
    }

    /// Ends the recording and waits for the file: its frames, length, size.
    pub fn stop(self) -> Result<Value> {
        let _ = self.tx.send(RecMsg::Stop);
        self.thread
            .join()
            .unwrap_or_else(|_| Err(Error::new("FAILED", "the recorder stopped")))
    }
}

/// The recorder's thread: the latest frame to ffmpeg every 1/FPS s, from
/// the first frame on, until told to stop. The page's screencast starts
/// again at the recording's size, which sends a frame; `fallback` (the
/// frame shown, at the box's size) begins it if none comes in FIRST_WAIT.
fn write_frames(
    ffmpeg: &Path,
    path: &Path,
    fallback: Option<Arc<Vec<u8>>>,
    rx: Receiver<RecMsg>,
) -> Result<Value> {
    let tick = Duration::from_secs_f64(1.0 / FPS as f64);
    let mut latest = None;
    let mut fallback = fallback.map(|f| (f, Instant::now() + FIRST_WAIT));
    let mut encoder: Option<(Child, ChildStdin, (u32, u32))> = None;
    let mut errors = None;
    let mut began: Option<Instant> = None;
    let mut written = 0u64;
    loop {
        let due = match (began, &latest, &fallback) {
            (Some(b), _, _) => Some(b + tick * written as u32),
            (None, Some(_), _) => Some(Instant::now()),
            (None, None, Some((_, at))) => Some(*at),
            (None, None, None) => None,
        };
        let next = match due {
            Some(due) => rx.recv_timeout(due.saturating_duration_since(Instant::now())),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match next {
            Ok(RecMsg::Frame(jpeg)) => {
                latest = Some(jpeg);
                if began.is_some() {
                    continue;
                }
            }
            Ok(RecMsg::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                if latest.is_none() {
                    latest = fallback.take().map(|(f, _)| f);
                }
            }
        }
        let Some(jpeg) = &latest else { continue };
        if encoder.is_none() {
            let Some(size) = jpeg_size(jpeg) else {
                continue;
            };
            let size = (even(size.0), even(size.1));
            let (child, stdin, stderr) = spawn(ffmpeg, path, size)?;
            errors = Some(stderr);
            encoder = Some((child, stdin, size));
            began = Some(Instant::now());
        }
        if let Some((_, stdin, _)) = &mut encoder
            && stdin.write_all(jpeg).is_err()
        {
            break;
        }
        written += 1;
    }
    let Some((mut child, stdin, (w, h))) = encoder else {
        return Err(Error::new(
            "FAILED",
            "the page sent no frame to record: is it showing in the viewer?",
        ));
    };
    drop(stdin);
    let status = child.wait();
    let errors = errors.and_then(|e| e.join().ok()).unwrap_or_default();
    // Made empty beforehand (Viewer::output): written, and still its own.
    if !status.is_ok_and(|s| s.success()) || !crate::proto::lone_file(path) {
        let tail = errors.lines().rev().take(3).collect::<Vec<_>>().join(" | ");
        return Err(Error::new(
            "FAILED",
            format!(
                "ffmpeg couldn't write the recording: {}",
                text::inert(&tail, 300)
            ),
        ));
    }
    Ok(Value::obj()
        .with("raw", path.display().to_string())
        .with("frames", written)
        .with("fps", FPS)
        .with(
            "duration",
            (written as f64 / FPS as f64 * 1000.0).round() / 1000.0,
        )
        .with("width", w)
        .with("height", h))
}

fn even(n: u32) -> u32 {
    (n / 2 * 2).max(16)
}

/// ffmpeg taking JPEGs on its stdin at FPS, each fitted to `size`, into an
/// H.264 .mp4 on the graphics chip's encoder; its stderr read on a thread.
fn spawn(
    ffmpeg: &Path,
    path: &Path,
    (w, h): (u32, u32),
) -> Result<(Child, ChildStdin, JoinHandle<String>)> {
    let mut child = media::command(ffmpeg)
        .args(["-nostdin", "-v", "error", "-y", "-f", "image2pipe", "-framerate"])
        .arg(FPS.to_string())
        .args(["-c:v", "mjpeg", "-i", "pipe:0", "-vf"])
        .arg(format!(
            "scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1,format=yuv420p"
        ))
        .args([
            "-c:v",
            "h264_videotoolbox",
            "-q:v",
            "60",
            "-movflags",
            "+faststart",
            "-f",
            "mp4",
        ])
        .arg(format!("file:{}", path.display()))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::new("FAILED", format!("cannot run ffmpeg: {e}")))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| Error::new("FAILED", "ffmpeg has no stdin"))?;
    let stderr = child.stderr.take();
    let errors = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(stderr) = stderr {
            let _ = stderr.take(1 << 16).read_to_string(&mut text);
        }
        text
    });
    Ok((child, stdin, errors))
}

impl Viewer {
    /// `record start`: a source's recording begins.
    pub(super) fn record_start(&mut self, args: &Value) -> Result<Value> {
        let i = self.pick_ready(args)?;
        self.check_owner(i, args)?;
        let sid = self.sources[i].sid;
        if self.recordings.iter().any(|r| r.sid == sid) {
            return Err(Error::new(
                "BAD_ARGS",
                format!("{} is being recorded already", self.sources[i].name),
            )
            .hint("pepper-agent record stop first."));
        }
        let max = args.get_num("max").unwrap_or(DEFAULT_SECONDS);
        if !(1.0..=MAX_SECONDS).contains(&max) {
            return Err(Error::new(
                "BAD_ARGS",
                format!("--max: seconds, 1 to {MAX_SECONDS}"),
            ));
        }
        if self.sources[i]
            .live
            .as_ref()
            .is_some_and(|l| l.unrecordable())
        {
            return Err(Error::new(
                "UNSUPPORTED",
                "a pipe's frames can't be recorded here yet",
            )
            .hint("Have the program write its frames to a file as well (ffmpeg's tee muxer, or a second output), and show that with pepper-agent play."));
        }
        self.finished.retain(|(s, _)| *s != sid);
        let source = &self.sources[i];
        let recording = if source.web.is_some() {
            let page = source
                .web
                .as_ref()
                .and_then(|w| w.page.clone())
                .ok_or_else(|| Error::new("TIMEOUT", "the page is still loading"))?;
            let ffmpeg = media::tool("ffmpeg")?;
            let path = self.output(&format!("rec-{sid}"), "mp4")?;
            let recorder = WebRecorder::start(ffmpeg, path, page.last_frame());
            page.record_to(Some(recorder.sender()));
            Recording {
                sid,
                started: Instant::now(),
                max: Duration::from_secs_f64(max),
                web: Some(recorder),
                played: None,
                stream: None,
            }
        } else if source
            .live
            .as_ref()
            .is_some_and(|l| l.records_as_it_comes())
        {
            // What comes is all there is: mpv writes it as it gets it.
            let path = self.output(&format!("rec-{sid}"), "mkv")?;
            if let Some(player) = &mut self.sources[i].player {
                player.set("stream-record", path.display().to_string());
            }
            Recording {
                sid,
                started: Instant::now(),
                max: Duration::from_secs_f64(max),
                web: None,
                played: None,
                stream: Some(path),
            }
        } else {
            Recording {
                sid,
                started: Instant::now(),
                max: Duration::from_secs_f64(max),
                web: None,
                played: Some({
                    let t = source.time_pos.unwrap_or(0.0);
                    vec![(t, t)]
                }),
                stream: None,
            }
        };
        let source = &self.sources[i];
        let answer = Value::obj()
            .with("source", source.name.as_str())
            .with("kind", source.kind)
            .with("recording", true)
            .with("max", max)
            .with(
                "from",
                recording
                    .played
                    .as_ref()
                    .and_then(|p| p.first())
                    .map(|s| s.0),
            );
        self.recordings.push(recording);
        self.dirty = true;
        Ok(answer)
    }

    /// `record stop`: a page's file once ffmpeg has finished it, or a
    /// video's stretch, for the command to cut from the file.
    pub(super) fn record_stop(&mut self, req: super::Request) {
        match self.stop_recording(&req.args) {
            Ok((sid, Finished::Job(job))) => self.wait(req, STOP_WAIT, WaitFor::Job { job, sid }),
            Ok((sid, Finished::Reply(request, result))) => self.wait(
                req,
                STOP_WAIT,
                WaitFor::Reply {
                    sid,
                    request,
                    result,
                },
            ),
            Ok((_, Finished::Now(answer))) => req.answer(answer),
            Err(e) => req.answer(Err(e)),
        }
    }

    fn stop_recording(&mut self, args: &Value) -> Result<(u64, Finished)> {
        let i = self.pick(args)?;
        self.check_owner(i, args)?;
        let sid = self.sources[i].sid;
        if let Some(k) = self.finished.iter().position(|(s, _)| *s == sid) {
            // It reached its --max and stopped by itself.
            return Ok((sid, Finished::Now(self.finished.remove(k).1)));
        }
        let k = self
            .recordings
            .iter()
            .position(|r| r.sid == sid)
            .ok_or_else(|| {
                Error::new(
                    "BAD_ARGS",
                    format!("{} isn't being recorded", self.sources[i].name),
                )
                .hint("pepper-agent record start first.")
            })?;
        let recording = self.recordings.remove(k);
        self.dirty = true;
        Ok((sid, self.finish(i, recording)))
    }

    /// Ends a recording: a page's in a job, a pipe's or a stream's once
    /// mpv has closed its file, a video's at once.
    fn finish(&mut self, i: usize, recording: Recording) -> Finished {
        let source = &self.sources[i];
        let mut about = Value::obj()
            .with("source", source.name.as_str())
            .with("kind", source.kind);
        source.origin(&mut about);
        if let Some(path) = recording.stream {
            about.set("raw", path.display().to_string());
            return match &mut self.sources[i].player {
                Some(player) => Finished::Reply(player.set("stream-record", ""), about),
                None => Finished::Now(Ok(about)),
            };
        }
        match recording.web {
            Some(recorder) => {
                if let Some(page) = source.web.as_ref().and_then(|w| w.page.clone()) {
                    page.record_to(None);
                }
                Finished::Job(self.spawn_job(move || {
                    recorder.stop().map(|mut done| {
                        if let Value::Obj(members) = about {
                            for (key, value) in members {
                                done.set(&key, value);
                            }
                        }
                        done
                    })
                }))
            }
            None => {
                let played: Vec<(f64, f64)> = recording
                    .played
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|(a, b)| b - a >= MIN_SPAN)
                    .collect();
                let (Some(first), Some(last)) = (played.first(), played.last()) else {
                    return Finished::Now(Err(Error::new(
                        "FAILED",
                        "nothing played while it was recorded",
                    )
                    .hint(
                        "Play it (pepper-agent play, or review a b) between record start and stop.",
                    )));
                };
                let ms = |t: f64| (t * 1000.0).round() / 1000.0;
                about.set("from", ms(first.0));
                about.set("to", ms(last.1));
                about.set(
                    "played",
                    played
                        .iter()
                        .map(|&(a, b)| Value::from(vec![Value::from(ms(a)), Value::from(ms(b))]))
                        .collect::<Vec<_>>(),
                );
                Finished::Now(Ok(about))
            }
        }
    }

    /// A video's position moved: what's being recorded grows while it plays
    /// on (or steps on), and a jump (a seek, `review`) starts another
    /// stretch.
    pub(super) fn track_played(&mut self, i: usize) {
        let (sid, Some(t)) = (self.sources[i].sid, self.sources[i].time_pos) else {
            return;
        };
        let Some(played) = self
            .recordings
            .iter_mut()
            .find(|r| r.sid == sid)
            .and_then(|r| r.played.as_mut())
        else {
            return;
        };
        if let Some(last) = played.last_mut() {
            if t >= last.1 && t - last.1 <= MAX_STEP {
                last.1 = t;
                return;
            }
            // Too short to keep: where it is now instead.
            if last.1 - last.0 < MIN_SPAN {
                *last = (t, t);
                return;
            }
        }
        if played.len() < MAX_SPANS {
            played.push((t, t));
        }
    }

    /// Recordings past their --max end by themselves; `record stop` then
    /// answers with the file.
    pub(super) fn end_long_recordings(&mut self) {
        let late: Vec<u64> = self
            .recordings
            .iter()
            .filter(|r| r.started.elapsed() >= r.max)
            .map(|r| r.sid)
            .collect();
        for sid in late {
            let Some(k) = self.recordings.iter().position(|r| r.sid == sid) else {
                continue;
            };
            let recording = self.recordings.remove(k);
            let Some(i) = self.sources.iter().position(|s| s.sid == sid) else {
                continue;
            };
            let name = self.sources[i].name.clone();
            match self.finish(i, recording) {
                Finished::Job(job) => self.auto_stops.push((job, sid)),
                Finished::Reply(_, done) => self.finished.push((sid, Ok(done))),
                Finished::Now(done) => self.finished.push((sid, done)),
            }
            self.flash(format!(
                "The recording of {name} reached its length and stopped."
            ));
        }
    }

    /// A job that ended a recording by itself: kept for `record stop`.
    /// Whether the job was one.
    pub(super) fn auto_stopped(&mut self, job: u64, result: &Result<Value>) -> bool {
        let Some(k) = self.auto_stops.iter().position(|(j, _)| *j == job) else {
            return false;
        };
        let (_, sid) = self.auto_stops.remove(k);
        self.finished.push((sid, result.clone()));
        true
    }

    /// Whether a source is being recorded, for its label.
    pub(super) fn recording(&self, sid: u64) -> Option<f64> {
        self.recordings
            .iter()
            .find(|r| r.sid == sid)
            .map(|r| r.started.elapsed().as_secs_f64())
    }
}

/// How a recording ended: a page's file is being finished by a job; a
/// pipe's or a stream's once the player answers this request; a video's
/// stretch (or why there's none) is known now.
enum Finished {
    Job(u64),
    Reply(u64, Value),
    Now(Result<Value>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn a_still_page_records_as_a_still_picture() {
        let Some(ffmpeg) = media::which("ffmpeg") else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("pepper-agent-rec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let jpeg = |size: &str| {
            let made = Command::new(&ffmpeg)
                .args(["-v", "error", "-f", "lavfi", "-i"])
                .arg(format!("color=0x2050c0:s={size}"))
                .args(["-frames:v", "1", "-f", "mjpeg", "-"])
                .output()
                .unwrap();
            Arc::new(made.stdout)
        };
        // A 64×48 frame, then nothing more for half a second: the fallback
        // (another size) is never used.
        let path = dir.join("rec.mp4");
        let recorder = WebRecorder::start(ffmpeg.clone(), path.clone(), Some(jpeg("32x24")));
        let _ = recorder.sender().send(RecMsg::Frame(jpeg("64x48")));
        std::thread::sleep(Duration::from_millis(500));
        let done = recorder.stop().unwrap();
        let frames = done.get_num("frames").unwrap();
        assert!((13.0..=17.0).contains(&frames), "{done:?}");
        assert_eq!(done.get_num("width"), Some(64.0));
        assert!(path.is_file());
        // No frame at all for FIRST_WAIT: the fallback begins it.
        let path = dir.join("fallback.mp4");
        let recorder = WebRecorder::start(ffmpeg.clone(), path.clone(), Some(jpeg("32x24")));
        std::thread::sleep(FIRST_WAIT + Duration::from_millis(500));
        let done = recorder.stop().unwrap();
        let frames = done.get_num("frames").unwrap();
        assert!((13.0..=17.0).contains(&frames), "{done:?}");
        assert_eq!(done.get_num("width"), Some(32.0));
        // No frame at all: an error, no file.
        let none = WebRecorder::start(
            PathBuf::from("/nonexistent/ffmpeg"),
            dir.join("none.mp4"),
            None,
        );
        assert_eq!(none.stop().unwrap_err().code, "FAILED");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
