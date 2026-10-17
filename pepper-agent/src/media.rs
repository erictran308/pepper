//! Looking at footage without the viewer: `probe`, `frames` and `sheet`,
//! run on the file with ffprobe and ffmpeg. Results are kept in
//! `.pepper/cache/<file's fingerprint>/`, so asking again costs nothing; a
//! changed file has a new fingerprint.
//!
//! Files are given to ffmpeg as `file:` and an absolute path, so a name is
//! never taken for one of its protocols.

use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};
use crate::font::Picture;
use crate::json::{self, Value};
use crate::text;

/// Pictures for the agent are at most this many pixels on their long side
/// (what a model's image input takes without shrinking them).
pub const MAX_SIDE: u32 = 1568;
const MAX_FRAMES: usize = 100;
const MAX_TILES: u32 = 64;
const JOBS: usize = 6;
const GAP: usize = 4;

/// An absolute path to a file that exists.
pub fn local_file(given: &str) -> Result<PathBuf> {
    let path = std::path::absolute(given)
        .map_err(|e| Error::new("NOT_FOUND", format!("{}: {e}", text::inert(given, 300))))?;
    if !path.is_file() {
        return Err(Error::new(
            "NOT_FOUND",
            format!(
                "no file at {}",
                text::inert(&path.display().to_string(), 300)
            ),
        ));
    }
    Ok(path)
}

/// A tool's path on the PATH, or MISSING_DEP with how to install it.
pub fn tool(name: &str) -> Result<PathBuf> {
    which(name).ok_or_else(|| {
        Error::new(
            "MISSING_DEP",
            format!("{name} isn't installed (or not on the PATH)"),
        )
        .hint("Install it: brew install ffmpeg")
    })
}

pub fn which(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&paths)
        .chain(["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"].map(PathBuf::from))
        .map(|dir| dir.join(name))
        .find(|p| p.is_absolute() && p.is_file())
}

pub fn file_arg(path: &Path) -> String {
    format!("file:{}", path.display())
}

/// Runs a tool and returns its stdout; its stderr, made inert, if it fails.
pub fn run(tool: &Path, args: &[String]) -> Result<Vec<u8>> {
    let out = Command::new(tool)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Error::new("FAILED", format!("cannot run {}: {e}", tool.display())))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.lines().rev().take(3).collect::<Vec<_>>().join(" | ");
        let name = tool
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        return Err(Error::new(
            "FAILED",
            format!("{name} failed: {}", text::inert(&err, 400)),
        ));
    }
    Ok(out.stdout)
}

/// 64-bit FNV-1a.
struct Fnv(u64);

impl Fnv {
    fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }

    fn add(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x100_0000_01b3);
        }
    }
}

/// A file's fingerprint: its path, size, time and inode, and its first and
/// last 64 KB, so a re-rendered file gets new cache entries.
pub fn fingerprint(path: &Path) -> Result<String> {
    let fail = |e: std::io::Error| Error::new("FAILED", format!("{}: {e}", path.display()));
    let meta = std::fs::metadata(path).map_err(fail)?;
    let mut hash = Fnv::new();
    hash.add(path.as_os_str().as_encoded_bytes());
    for n in [
        meta.len(),
        meta.mtime() as u64,
        meta.mtime_nsec() as u64,
        meta.ino(),
    ] {
        hash.add(&n.to_le_bytes());
    }
    let mut file = std::fs::File::open(path).map_err(fail)?;
    let mut buf = vec![0u8; 64 * 1024];
    let n = file.read(&mut buf).map_err(fail)?;
    hash.add(&buf[..n]);
    if meta.len() > buf.len() as u64 {
        use std::io::Seek;
        file.seek(std::io::SeekFrom::End(-(buf.len() as i64)))
            .map_err(fail)?;
        let n = file.read(&mut buf).map_err(fail)?;
        hash.add(&buf[..n]);
    }
    Ok(format!("{:016x}", hash.0))
}

/// What `probe` found, and what the other commands need of it.
pub struct Probe {
    pub json: Value,
    pub duration: Option<f64>,
    /// The picture's size as shown (sample aspect and rotation applied).
    pub display: Option<(u32, u32)>,
    pub fps: Option<f64>,
}

/// `30000/1001` or `4:3`, as a number.
pub fn ratio(text: &str) -> Option<f64> {
    let (a, b) = text.split_once(['/', ':'])?;
    let (a, b): (f64, f64) = (a.parse().ok()?, b.parse().ok()?);
    (a > 0.0 && b > 0.0).then(|| a / b)
}

fn num_field(v: &Value, key: &str) -> Option<f64> {
    v.get_num(key)
        .or_else(|| v.get_str(key).and_then(|s| s.parse().ok()))
}

/// Whether VideoToolbox decodes this codec here, for the codecs mpv's
/// `hwdec=auto-safe` hands it.
pub fn hw_decodable(codec: &str) -> bool {
    #[link(name = "VideoToolbox", kind = "framework")]
    unsafe extern "C" {
        fn VTIsHardwareDecodeSupported(codec: u32) -> u8;
        fn VTRegisterSupplementalVideoDecoderIfAvailable(codec: u32);
    }
    let fourcc = |s: &[u8; 4]| u32::from_be_bytes(*s);
    let (code, supplemental) = match codec {
        "h264" => (fourcc(b"avc1"), false),
        "hevc" => (fourcc(b"hvc1"), false),
        "prores" => (fourcc(b"apcn"), false),
        "vp9" => (fourcc(b"vp09"), true),
        "av1" => (fourcc(b"av01"), true),
        _ => return false,
    };
    // SAFETY: plain calls with a codec type.
    unsafe {
        if supplemental {
            VTRegisterSupplementalVideoDecoderIfAvailable(code);
        }
        VTIsHardwareDecodeSupported(code) != 0
    }
}

/// The cache folder for a file.
pub fn cache_dir(work: &Path, key: &str) -> Result<PathBuf> {
    let dir = work.join("cache").join(key);
    crate::files::private_dir(&dir)
        .map_err(|e| Error::new("FAILED", format!("{}: {e}", dir.display())))?;
    Ok(dir)
}

pub fn probe(work: &Path, file: &Path) -> Result<Probe> {
    let key = fingerprint(file)?;
    let dir = cache_dir(work, &key)?;
    let cached = dir.join("probe.json");
    let json = match std::fs::read_to_string(&cached)
        .ok()
        .and_then(|t| json::parse(&t).ok())
    {
        Some(json) => json,
        None => {
            let json = probe_file(file)?;
            let _ = crate::files::write_private(&cached, json.to_pretty().as_bytes());
            json
        }
    };
    let video = json.get("video").filter(|v| !v.is_null());
    let display = video.and_then(|v| {
        Some((
            v.get_num("display_width")? as u32,
            v.get_num("display_height")? as u32,
        ))
    });
    Ok(Probe {
        duration: json.get_num("duration"),
        fps: video.and_then(|v| v.get_num("fps")),
        display,
        json,
    })
}

fn probe_file(file: &Path) -> Result<Value> {
    let ffprobe = tool("ffprobe")?;
    let out = run(
        &ffprobe,
        &[
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            "-i",
        ]
        .map(String::from)
        .into_iter()
        .chain([file_arg(file)])
        .collect::<Vec<_>>(),
    )?;
    let raw = json::parse(&String::from_utf8_lossy(&out))
        .map_err(|e| Error::new("FAILED", format!("ffprobe's output: {e}")))?;
    let format = raw.get("format").cloned().unwrap_or(Value::Null);
    let streams = raw.get("streams").and_then(Value::arr).unwrap_or(&[]);
    let duration = num_field(&format, "duration");
    let tag = |v: &Value, key: &str| {
        v.get("tags")
            .and_then(|t| t.get_str(key))
            .map(|s| text::inert(s, 200))
    };

    let mut video = Value::Null;
    let mut audio = Vec::new();
    let mut subtitles = Vec::new();
    let mut others = 0;
    for s in streams {
        let codec = s.get_str("codec_name").unwrap_or("unknown");
        match s.get_str("codec_type") {
            Some("video")
                if video.is_null()
                    && s.get("disposition").and_then(|d| d.get_num("attached_pic"))
                        != Some(1.0) =>
            {
                let (w, h) = (
                    s.get_num("width").unwrap_or(0.0),
                    s.get_num("height").unwrap_or(0.0),
                );
                let sar = s
                    .get_str("sample_aspect_ratio")
                    .and_then(ratio)
                    .unwrap_or(1.0);
                let rotation = s
                    .get("side_data_list")
                    .and_then(Value::arr)
                    .and_then(|l| l.iter().find_map(|d| d.get_num("rotation")))
                    .unwrap_or(0.0);
                let (mut dw, mut dh) = ((w * sar).round(), h);
                if (rotation.abs() - 90.0).abs() < 1.0 {
                    std::mem::swap(&mut dw, &mut dh);
                }
                let fps = s
                    .get_str("avg_frame_rate")
                    .and_then(ratio)
                    .or_else(|| s.get_str("r_frame_rate").and_then(ratio));
                video = Value::obj()
                    .with("codec", codec)
                    .with("profile", s.get_str("profile"))
                    .with("width", w)
                    .with("height", h)
                    .with("display_width", dw)
                    .with("display_height", dh)
                    .with("rotation", rotation)
                    .with("fps", fps.map(|f| (f * 1000.0).round() / 1000.0))
                    .with("pix_fmt", s.get_str("pix_fmt"))
                    .with("bit_rate", num_field(s, "bit_rate"))
                    .with("color_space", s.get_str("color_space"))
                    .with("color_transfer", s.get_str("color_transfer"))
                    .with("color_primaries", s.get_str("color_primaries"))
                    .with("color_range", s.get_str("color_range"))
                    .with("frames", num_field(s, "nb_frames"))
                    .with("hw_decodable", hw_decodable(codec));
            }
            Some("audio") => audio.push(
                Value::obj()
                    .with("codec", codec)
                    .with("channels", s.get_num("channels"))
                    .with("channel_layout", s.get_str("channel_layout"))
                    .with("sample_rate", num_field(s, "sample_rate"))
                    .with("language", tag(s, "language"))
                    .with("title", tag(s, "title")),
            ),
            Some("subtitle") => subtitles.push(
                Value::obj()
                    .with("codec", codec)
                    .with("language", tag(s, "language"))
                    .with("title", tag(s, "title")),
            ),
            _ => others += 1,
        }
    }
    if !video.is_null()
        && let Some(interval) = keyframe_interval(file)
    {
        video.set("keyframe_interval", interval);
    }
    Ok(Value::obj()
        .with("file", file.display().to_string())
        .with("size_bytes", std::fs::metadata(file).map(|m| m.len()).ok())
        .with("format", format.get_str("format_name"))
        .with("duration", duration.map(|d| (d * 1000.0).round() / 1000.0))
        .with("duration_text", duration.map(text::clock))
        .with("bit_rate", num_field(&format, "bit_rate"))
        .with("title", tag(&format, "title"))
        .with("video", video)
        .with("audio", audio)
        .with("subtitles", subtitles)
        .with("other_streams", others as u32))
}

/// The mean time between keyframes over the first 30 s, from the packets'
/// flags (no decoding).
fn keyframe_interval(file: &Path) -> Option<f64> {
    let ffprobe = tool("ffprobe").ok()?;
    let args: Vec<String> = [
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "packet=pts_time,flags",
        "-read_intervals",
        "%+30",
        "-of",
        "csv=p=0",
        "-i",
    ]
    .map(String::from)
    .into_iter()
    .chain([file_arg(file)])
    .collect();
    let out = run(&ffprobe, &args).ok()?;
    let mut keys: Vec<f64> = String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|l| {
            let (t, flags) = l.split_once(',')?;
            flags.starts_with('K').then(|| t.parse().ok()).flatten()
        })
        .collect();
    keys.sort_by(f64::total_cmp);
    (keys.len() >= 2).then(|| {
        let mean = (keys[keys.len() - 1] - keys[0]) / (keys.len() - 1) as f64;
        (mean * 1000.0).round() / 1000.0
    })
}

/// A size with the picture's shape, at most `max` on its long side, even.
pub fn fit_size((w, h): (u32, u32), max: u32) -> (u32, u32) {
    let (w, h) = (w.max(2) as f64, h.max(2) as f64);
    let scale = (max as f64 / w.max(h)).min(1.0);
    let even = |v: f64| ((v * scale / 2.0).round() as u32 * 2).max(2);
    (even(w), even(h))
}

/// A time a frame can be taken at: inside the file, a little before its end.
fn frame_time(t: f64, probe: &Probe) -> f64 {
    let t = t.max(0.0);
    match probe.duration {
        Some(d) if d > 0.0 => {
            let last = d - probe.fps.map(|f| 1.0 / f).unwrap_or(0.04).max(0.04);
            t.min(last.max(0.0))
        }
        _ => t,
    }
}

fn ffmpeg_frame_args(file: &Path, t: f64, (w, h): (u32, u32)) -> Vec<String> {
    vec![
        "-nostdin".into(),
        "-v".into(),
        "error".into(),
        "-hwaccel".into(),
        "videotoolbox".into(),
        "-ss".into(),
        format!("{t:.3}"),
        "-i".into(),
        file_arg(file),
        "-frames:v".into(),
        "1".into(),
        "-an".into(),
        "-sn".into(),
        "-vf".into(),
        format!("scale={w}:{h}:flags=lanczos"),
    ]
}

/// Runs `jobs` on JOBS threads, telling progress on stderr when there are
/// many.
fn parallel<T: Send + 'static>(
    jobs: Vec<Box<dyn FnOnce() -> Result<T> + Send>>,
    step: &'static str,
) -> Result<Vec<T>> {
    let total = jobs.len();
    let queue = Arc::new(Mutex::new(jobs.into_iter().enumerate().collect::<Vec<_>>()));
    let results = Arc::new(Mutex::new(
        (0..total).map(|_| None).collect::<Vec<Option<Result<T>>>>(),
    ));
    let done = Arc::new(Mutex::new(0usize));
    let workers: Vec<_> = (0..JOBS.min(total))
        .map(|_| {
            let (queue, results, done) = (queue.clone(), results.clone(), done.clone());
            std::thread::spawn(move || {
                loop {
                    let Some((i, job)) = queue.lock().unwrap_or_else(|e| e.into_inner()).pop()
                    else {
                        return;
                    };
                    let result = job();
                    results.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(result);
                    let mut done = done.lock().unwrap_or_else(|e| e.into_inner());
                    *done += 1;
                    if total > 4 {
                        let line = Value::obj().with(
                            "progress",
                            Value::obj()
                                .with("step", step)
                                .with("done", *done)
                                .with("of", total),
                        );
                        eprintln!("{}", line.to_line());
                    }
                }
            })
        })
        .collect();
    for w in workers {
        let _ = w.join();
    }
    let results = std::mem::take(&mut *results.lock().unwrap_or_else(|e| e.into_inner()));
    results
        .into_iter()
        .map(|r| r.unwrap_or_else(|| Err(Error::new("FAILED", "a job didn't finish"))))
        .collect()
}

pub struct FramesAsk {
    pub at: Vec<f64>,
    pub every: Option<f64>,
    pub from: Option<f64>,
    pub to: Option<f64>,
    pub max_side: u32,
}

pub fn frames(work: &Path, file: &Path, ask: &FramesAsk) -> Result<Value> {
    let probe = probe(work, file)?;
    let display = probe
        .display
        .ok_or_else(|| Error::new("NOT_PLAYABLE", "the file has no picture (sound only)"))?;
    let times: Vec<f64> = if let Some(every) = ask.every {
        if every.is_nan() || every <= 0.0 {
            return Err(Error::new(
                "BAD_TIME",
                "--every must be more than 0 seconds",
            ));
        }
        let from = ask.from.unwrap_or(0.0);
        let to = ask.to.or(probe.duration).unwrap_or(from);
        let count = ((to - from) / every).floor() as usize + 1;
        if count > MAX_FRAMES {
            return Err(Error::new(
                "BAD_ARGS",
                format!("that's {count} frames; at most {MAX_FRAMES} at once"),
            )
            .hint("Use a larger --every, --from/--to, or `pepper-agent sheet` for an overview."));
        }
        (0..count).map(|i| from + i as f64 * every).collect()
    } else {
        ask.at.clone()
    };
    if times.is_empty() {
        return Err(Error::new(
            "BAD_ARGS",
            "frames: give --at T1,T2,… or --every SECONDS",
        ));
    }
    if times.len() > MAX_FRAMES {
        return Err(Error::new(
            "BAD_ARGS",
            format!("at most {MAX_FRAMES} frames at once"),
        ));
    }
    let size = fit_size(display, ask.max_side);
    let dir = cache_dir(work, &fingerprint(file)?)?;
    let ffmpeg = tool("ffmpeg")?;
    let jobs: Vec<Box<dyn FnOnce() -> Result<Value> + Send>> = times
        .iter()
        .map(|&asked| {
            let t = frame_time(asked, &probe);
            let path = dir.join(format!(
                "frame-{}-{}.png",
                (t * 1000.0).round() as u64,
                ask.max_side
            ));
            let (ffmpeg, file) = (ffmpeg.clone(), file.to_owned());
            Box::new(move || {
                if !path.is_file() {
                    let tmp = path.with_extension("tmp.png");
                    let mut args = ffmpeg_frame_args(&file, t, size);
                    args.extend(["-update", "1", "-y"].map(String::from));
                    args.push(tmp.display().to_string());
                    run(&ffmpeg, &args)?;
                    if !tmp.is_file() {
                        return Err(Error::new(
                            "FAILED",
                            format!("no frame at {}", text::clock(t)),
                        ));
                    }
                    std::fs::rename(&tmp, &path)
                        .map_err(|e| Error::new("FAILED", e.to_string()))?;
                }
                Ok(Value::obj()
                    .with("time", (t * 1000.0).round() / 1000.0)
                    .with("time_text", text::clock(t))
                    .with("path", path.display().to_string()))
            }) as Box<dyn FnOnce() -> Result<Value> + Send>
        })
        .collect();
    let frames = parallel(jobs, "frames")?;
    Ok(Value::obj()
        .with("file", file.display().to_string())
        .with("width", size.0)
        .with("height", size.1)
        .with("frames", frames))
}

pub struct SheetAsk {
    pub from: Option<f64>,
    pub to: Option<f64>,
    pub cols: u32,
    pub rows: u32,
    pub max_side: u32,
}

/// The tiles' size: the picture's shape, the whole sheet at most
/// `max_side` on its long side.
fn tile_size(display: (u32, u32), cols: usize, rows: usize, max_side: usize) -> (usize, usize) {
    let aspect = display.0.max(1) as f64 / display.1.max(1) as f64;
    let mut tw = (max_side.saturating_sub(GAP * (cols + 1))) as f64 / cols as f64;
    let mut th = tw / aspect;
    if th * rows as f64 + (GAP * (rows + 1)) as f64 > max_side as f64 {
        th = (max_side.saturating_sub(GAP * (rows + 1))) as f64 / rows as f64;
        tw = th * aspect;
    }
    let even = |v: f64| ((v / 2.0).floor() as usize * 2).max(2);
    (even(tw), even(th))
}

pub fn sheet(work: &Path, file: &Path, ask: &SheetAsk) -> Result<Value> {
    if !(1..=8).contains(&ask.cols)
        || !(1..=8).contains(&ask.rows)
        || ask.cols * ask.rows > MAX_TILES
    {
        return Err(Error::new("BAD_ARGS", "--cols and --rows: 1 to 8 each"));
    }
    let probe = probe(work, file)?;
    let display = probe
        .display
        .ok_or_else(|| Error::new("NOT_PLAYABLE", "the file has no picture (sound only)"))?;
    let duration = probe.duration.unwrap_or(0.0);
    let from = ask.from.unwrap_or(0.0).clamp(0.0, duration.max(0.0));
    let to = ask.to.unwrap_or(duration).clamp(from, duration.max(from));
    if to <= from && duration > 0.0 {
        return Err(Error::new("BAD_TIME", "--to must come after --from"));
    }
    let (cols, rows) = (ask.cols as usize, ask.rows as usize);
    let n = cols * rows;
    let (tw, th) = tile_size(display, cols, rows, ask.max_side as usize);
    let (sw, sh) = (cols * tw + GAP * (cols + 1), rows * th + GAP * (rows + 1));
    let times: Vec<f64> = (0..n)
        .map(|i| frame_time(from + (i as f64 + 0.5) * (to - from) / n as f64, &probe))
        .collect();

    let dir = cache_dir(work, &fingerprint(file)?)?;
    let stem = format!(
        "sheet-{}-{}-{cols}x{rows}-{}",
        (from * 1000.0).round() as u64,
        (to * 1000.0).round() as u64,
        ask.max_side
    );
    let path = dir.join(format!("{stem}.png"));
    let tiles: Vec<Value> = times
        .iter()
        .enumerate()
        .map(|(i, &t)| {
            Value::obj()
                .with("index", i)
                .with("time", (t * 1000.0).round() / 1000.0)
                .with("time_text", text::clock(t))
                .with("x", GAP + (i % cols) * (tw + GAP))
                .with("y", GAP + (i / cols) * (th + GAP))
        })
        .collect();
    let answer = Value::obj()
        .with("file", file.display().to_string())
        .with("path", path.display().to_string())
        .with("width", sw)
        .with("height", sh)
        .with("cols", cols)
        .with("rows", rows)
        .with("tile_width", tw)
        .with("tile_height", th)
        .with("from", (from * 1000.0).round() / 1000.0)
        .with("to", (to * 1000.0).round() / 1000.0)
        .with("tiles", tiles);
    if path.is_file() {
        return Ok(answer);
    }

    let ffmpeg = tool("ffmpeg")?;
    let jobs: Vec<Box<dyn FnOnce() -> Result<Vec<u8>> + Send>> = times
        .iter()
        .map(|&t| {
            let (ffmpeg, file) = (ffmpeg.clone(), file.to_owned());
            Box::new(move || {
                let mut args = ffmpeg_frame_args(&file, t, (tw as u32, th as u32));
                args.extend(["-pix_fmt", "rgb24", "-f", "rawvideo", "pipe:1"].map(String::from));
                let rgb = run(&ffmpeg, &args)?;
                Ok(rgb)
            }) as Box<dyn FnOnce() -> Result<Vec<u8>> + Send>
        })
        .collect();
    let frames = parallel(jobs, "sheet")?;
    let mut picture = Picture::new(sw, sh, [32, 32, 32]);
    let scale = if tw >= 300 { 2 } else { 1 };
    for (i, (rgb, &t)) in frames.iter().zip(&times).enumerate() {
        let (x, y) = (GAP + (i % cols) * (tw + GAP), GAP + (i / cols) * (th + GAP));
        if rgb.len() == tw * th * 3 {
            picture.paste(x, y, tw, th, rgb);
        }
        let label = text::clock(t);
        let label = label
            .strip_suffix("00")
            .or(label.strip_suffix('0'))
            .unwrap_or(&label);
        let h = 7 * scale + 4 * scale;
        picture.label(x, (y + th).saturating_sub(h), label, scale);
    }
    encode_png(&ffmpeg, &picture, &path)?;
    Ok(answer)
}

/// Writes an RGB picture as a PNG, through ffmpeg.
fn encode_png(ffmpeg: &Path, picture: &Picture, path: &Path) -> Result<()> {
    let tmp = path.with_extension("tmp.png");
    let mut child = Command::new(ffmpeg)
        .args([
            "-nostdin",
            "-v",
            "error",
            "-f",
            "rawvideo",
            "-pixel_format",
            "rgb24",
            "-video_size",
        ])
        .arg(format!("{}x{}", picture.width, picture.height))
        .args(["-i", "pipe:0", "-frames:v", "1", "-update", "1", "-y"])
        .arg(&tmp)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::new("FAILED", format!("cannot run ffmpeg: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&picture.pixels);
    }
    let out = child
        .wait_with_output()
        .map_err(|e| Error::new("FAILED", format!("ffmpeg: {e}")))?;
    if !out.status.success() || !tmp.is_file() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(Error::new(
            "FAILED",
            format!(
                "ffmpeg couldn't write the PNG: {}",
                text::inert(err.trim(), 300)
            ),
        ));
    }
    std::fs::rename(&tmp, path).map_err(|e| Error::new("FAILED", e.to_string()))
}

/// A PNG's width and height, from its header.
pub fn png_size(path: &Path) -> Option<(u32, u32)> {
    let mut head = [0u8; 24];
    std::fs::File::open(path).ok()?.read_exact(&mut head).ok()?;
    if &head[..8] != b"\x89PNG\r\n\x1a\n" || &head[12..16] != b"IHDR" {
        return None;
    }
    let be = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    Some((be(&head[16..20]), be(&head[20..24])))
}

/// What [`shrink_png`] made: its size, the full picture's, and the region
/// taken, as it fitted in the picture.
pub struct Shrunk {
    pub size: (u32, u32),
    pub full: (u32, u32),
    pub region: Option<[u32; 4]>,
}

/// A snapshot the viewer's player wrote at the video's own size, cropped
/// (`region`: x, y, w, h in its pixels) and made at most `max_side`, as
/// `out`.
pub fn shrink_png(
    raw: &Path,
    out: &Path,
    region: Option<[u32; 4]>,
    max_side: u32,
) -> Result<Shrunk> {
    let full = png_size(raw).ok_or_else(|| Error::new("FAILED", "the snapshot isn't a PNG"))?;
    let region = match region {
        Some([x, y, w, h]) => {
            if x >= full.0 || y >= full.1 {
                return Err(Error::new(
                    "BAD_ARGS",
                    format!(
                        "the region starts outside the {}×{} picture",
                        full.0, full.1
                    ),
                ));
            }
            let (w, h) = (w.min(full.0 - x).max(2), h.min(full.1 - y).max(2));
            Some([x, y, w, h])
        }
        None => None,
    };
    let from = region.map(|[_, _, w, h]| (w, h)).unwrap_or(full);
    let size = fit_size(from, max_side);
    let mut filter = String::new();
    if let Some([x, y, w, h]) = region {
        filter += &format!("crop={w}:{h}:{x}:{y},");
    }
    filter += &format!("scale={}:{}:flags=lanczos", size.0, size.1);
    let ffmpeg = tool("ffmpeg")?;
    let tmp = out.with_extension("tmp.png");
    let args: Vec<String> = ["-nostdin", "-v", "error", "-i"]
        .map(String::from)
        .into_iter()
        .chain([file_arg(raw), "-vf".into(), filter])
        .chain(["-frames:v", "1", "-update", "1", "-y"].map(String::from))
        .chain([tmp.display().to_string()])
        .collect();
    run(&ffmpeg, &args)?;
    std::fs::rename(&tmp, out).map_err(|e| Error::new("FAILED", e.to_string()))?;
    Ok(Shrunk { size, full, region })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(fit_size((1920, 1080), 1568), (1568, 882));
        assert_eq!(fit_size((640, 360), 1568), (640, 360));
        assert_eq!(fit_size((1080, 1920), 1568), (882, 1568));
        assert_eq!(fit_size((3840, 1600), 1568), (1568, 654));
        assert_eq!(fit_size((1, 1), 1568), (2, 2));
        // 4×4 of 16:9 in 1568: the width is what limits.
        let (tw, th) = tile_size((1920, 1080), 4, 4, 1568);
        assert_eq!((tw, th), (386, 216));
        assert!(4 * tw + 5 * GAP <= 1568);
        // Tall pictures: the height limits.
        let (tw, th) = tile_size((1080, 1920), 4, 4, 1568);
        assert!(4 * th + 5 * GAP <= 1568 && tw < th);
    }

    #[test]
    fn ratios() {
        assert_eq!(
            ratio("30000/1001").map(|r| (r * 1000.0).round()),
            Some(29970.0)
        );
        assert_eq!(ratio("4:3"), Some(4.0 / 3.0));
        assert_eq!(ratio("0/0"), None);
        assert_eq!(ratio("x"), None);
    }

    #[test]
    fn fingerprints_change_with_the_file() {
        let dir = std::env::temp_dir().join(format!("pepper-agent-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.bin");
        std::fs::write(&file, b"one").unwrap();
        let a = fingerprint(&file).unwrap();
        assert_eq!(a, fingerprint(&file).unwrap());
        std::fs::write(&file, b"two").unwrap();
        assert_ne!(a, fingerprint(&file).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn videotoolbox_here() {
        // Every Apple silicon Mac decodes H.264 and HEVC on its graphics chip.
        assert!(hw_decodable("h264"));
        assert!(hw_decodable("hevc"));
        assert!(!hw_decodable("mpeg2video"));
    }
}
