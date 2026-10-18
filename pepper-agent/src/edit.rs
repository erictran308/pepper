//! Edit projects: the agent's cut as a small JSON file (`"pepper_edit": 1`),
//! checked all at once (`edit validate`), shown without rendering as an mpv
//! EDL (`edit preview`), mapped between the output's time and the footage's
//! (`edit timeline`, and the human's marks on a preview), and rendered with
//! ffmpeg (`edit render`).
//!
//! ```json
//! {
//!   "pepper_edit": 1,
//!   "output": {"width": 1920, "height": 1080, "fps": 30, "fit": "pad"},
//!   "clips": [
//!     {"src": "raw/a.mp4", "in": "0:03.2", "out": "0:11"},
//!     {"src": "raw/b.mp4", "in": 40, "out": 52.5, "audio": {"fade_in": 0.2}}
//!   ]
//! }
//! ```
//!
//! Paths are from the project file's folder. A member version 1 doesn't
//! have is a problem, not ignored, so a typo never goes unseen; later
//! versions add their own (overlays, titles) under a new `pepper_edit`.

use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use crate::error::{Error, Result};
use crate::json::{self, Value};
use crate::{files, media, text};

/// The format's version, the one this program reads.
pub const VERSION: f64 = 1.0;
/// The most clips in a project (a preview's timeline goes to the viewer in
/// one request).
pub const MAX_CLIPS: usize = 300;
const MAX_FILE: u64 = 1024 * 1024;
const MAX_NAME: usize = 64;
const MAX_PATH: usize = 4096;
const MAX_FADE: f64 = 60.0;
const PROJECT_KEYS: &[&str] = &["pepper_edit", "output", "clips"];
const OUTPUT_KEYS: &[&str] = &["width", "height", "fps", "fit"];
const CLIP_KEYS: &[&str] = &["src", "in", "out", "name", "audio"];
const AUDIO_KEYS: &[&str] = &["fade_in", "fade_out"];

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Fit {
    /// The whole picture, with bars where its shape differs.
    Pad,
    /// The frame filled, the picture's edges cut.
    Crop,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Output {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// As ffmpeg takes it: `30`, `30000/1001`.
    pub fps_text: String,
    pub fit: Fit,
}

impl Output {
    pub fn to_json(&self) -> Value {
        Value::obj()
            .with("width", self.width)
            .with("height", self.height)
            .with("fps", round3(self.fps))
            .with(
                "fit",
                match self.fit {
                    Fit::Pad => "pad",
                    Fit::Crop => "crop",
                },
            )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Clip {
    /// An absolute path.
    pub src: PathBuf,
    pub name: Option<String>,
    /// `in` and `out`, in the file's time.
    pub start: f64,
    pub end: f64,
    pub fade_in: f64,
    pub fade_out: f64,
    /// Whether the file has sound.
    pub audio: bool,
    /// A video-use EDL's: the source's name, and what's said in the range.
    pub key: Option<String>,
    pub quote: Option<String>,
}

/// What a project file is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Format {
    /// `"pepper_edit": 1`, pepper-agent's own.
    Pepper,
    /// video-use's `edl.json` (github.com/browser-use/video-use): `sources`
    /// and `ranges`, rendered by its own `helpers/render.py`.
    VideoUse,
}

impl Clip {
    pub fn length(&self) -> f64 {
        self.end - self.start
    }
}

pub struct Project {
    pub path: PathBuf,
    pub format: Format,
    pub output: Output,
    pub clips: Vec<Clip>,
    pub warnings: Vec<Value>,
}

impl Project {
    pub fn duration(&self) -> f64 {
        self.clips.iter().map(Clip::length).sum()
    }

    /// The parts of the cut from each `(from, to)` (the output's times), in
    /// that order, as a project of its own: what a recording of its preview
    /// played. A part that starts inside a clip's fade-out (or ends inside
    /// its fade-in) fades over what's left of it.
    pub fn slice(&self, spans: &[(f64, f64)]) -> Project {
        let mut clips = Vec::new();
        for &(from, to) in spans {
            let mut at = 0.0;
            for clip in &self.clips {
                let (start, end) = (at, at + clip.length());
                at = end;
                let (a, b) = (from.max(start), to.min(end));
                if b - a > 1e-6 {
                    let mut part = clip.clone();
                    part.start = clip.start + (a - start);
                    part.end = clip.start + (b - start);
                    let length = part.length();
                    part.fade_in = if a > start {
                        0.0
                    } else {
                        clip.fade_in.min(length)
                    };
                    part.fade_out = if b < end {
                        0.0
                    } else {
                        clip.fade_out.min(length)
                    };
                    clips.push(part);
                }
            }
        }
        Project {
            path: self.path.clone(),
            format: Format::Pepper,
            output: self.output.clone(),
            clips,
            warnings: Vec::new(),
        }
    }

    /// A name for what's made of it: its file's stem.
    pub fn stem(&self) -> String {
        let stem = self
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        files::name_part(&stem)
    }
}

/// What a project needs to know of a file, from `probe`.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    pub duration: Option<f64>,
    pub display: Option<(u32, u32)>,
    pub fps: Option<f64>,
    pub audio: bool,
    pub hdr: bool,
}

impl Facts {
    fn of(probe: &media::Probe) -> Facts {
        let transfer = probe
            .json
            .get("video")
            .and_then(|v| v.get_str("color_transfer"))
            .unwrap_or("");
        Facts {
            duration: probe.duration,
            display: probe.display,
            fps: probe.fps,
            audio: probe
                .json
                .get("audio")
                .and_then(Value::arr)
                .is_some_and(|a| !a.is_empty()),
            hdr: matches!(transfer, "smpte2084" | "arib-std-b67"),
        }
    }
}

fn round3(t: f64) -> f64 {
    (t * 1000.0).round() / 1000.0
}

fn problem(at: &str, message: impl Into<String>) -> Value {
    Value::obj().with("at", at).with("problem", message.into())
}

fn warning(at: &str, message: impl Into<String>) -> Value {
    Value::obj().with("at", at).with("warning", message.into())
}

fn invalid(problems: Vec<Value>) -> Error {
    let n = problems.len();
    let mut error = Error::new(
        "INVALID_PROJECT",
        format!(
            "the project has {n} problem{}",
            if n == 1 { "" } else { "s" }
        ),
    );
    error.problems = problems;
    error
}

fn join(at: &str, key: &str) -> String {
    if at.is_empty() {
        key.to_owned()
    } else {
        format!("{at}.{key}")
    }
}

/// Reads and checks a project file: the project, with its warnings, or
/// INVALID_PROJECT with every problem found.
pub fn load(work: &Path, given: &str) -> Result<Project> {
    media::tool("ffprobe")?;
    let path = media::local_file(given)?;
    let shown = || text::inert(&path.display().to_string(), 300);
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if size > MAX_FILE {
        return Err(invalid(vec![problem(
            "",
            "the file is over 1 MB: a project is a small JSON file",
        )]));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .and_then(|f| f.take(MAX_FILE).read_to_end(&mut bytes))
        .map_err(|e| Error::new("FAILED", format!("{}: {e}", shown())))?;
    let text =
        String::from_utf8(bytes).map_err(|_| invalid(vec![problem("", "not UTF-8 text")]))?;
    let value = json::parse(&text).map_err(|e| {
        invalid(vec![problem(
            "",
            format!("not JSON: {}", text::inert(&e, 200)),
        )])
    })?;
    let base = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/"));
    let mut facts = |src: &Path| {
        media::probe(work, src)
            .map(|p| Facts::of(&p))
            .map_err(|e| e.message)
    };
    if is_video_use(&value) {
        let vu = from_video_use(&value, &base);
        let named = |problems: Vec<Value>| -> Vec<Value> {
            problems
                .into_iter()
                .map(|v| to_video_use(v, &vu.ranges))
                .collect()
        };
        // Every problem at once: the ranges' own, and their clips'.
        let built = if vu.ranges.is_empty() {
            Err(Vec::new())
        } else {
            build(&vu.pepper, &base, &mut facts)
        };
        let (output, mut clips, warnings) = match built {
            Ok(built) if vu.problems.is_empty() => built,
            Ok(_) => return Err(invalid(vu.problems)),
            Err(problems) => {
                let mut all = vu.problems;
                all.extend(named(problems));
                return Err(invalid(all));
            }
        };
        for (clip, (key, quote)) in clips.iter_mut().zip(vu.extras.clone()) {
            clip.key = Some(key);
            clip.quote = quote;
        }
        let mut notes = vu.notes.clone();
        notes.extend(named(warnings));
        let mut project = Project {
            path,
            format: Format::VideoUse,
            output,
            clips,
            warnings: notes,
        };
        let total = project.duration();
        if let Some(said) = value.get_num("total_duration_s")
            && (said - total).abs() > 0.05
        {
            project.warnings.push(warning(
                "total_duration_s",
                format!(
                    "it says {} s; the ranges add up to {} s",
                    round3(said),
                    round3(total)
                ),
            ));
        }
        return Ok(project);
    }
    let (output, clips, warnings) = build(&value, &base, &mut facts).map_err(invalid)?;
    Ok(Project {
        path,
        format: Format::Pepper,
        output,
        clips,
        warnings,
    })
}

/// Where a word-level transcript of `src` is, for an edit at `project` (or
/// for the file alone): video-use keeps one per source in
/// `<edit>/transcripts/<name>.json`, `name` being the EDL's source name or
/// the file's stem, with `edit/` beside the footage.
pub fn transcript_for(project: Option<&Path>, src: &Path) -> Option<PathBuf> {
    let key = project.and_then(|p| source_key(p, src));
    let stem = src.file_stem()?.to_str()?.to_owned();
    let names: Vec<String> = key.into_iter().chain([stem]).collect();
    let mut dirs = Vec::new();
    if let Some(dir) = project.and_then(Path::parent) {
        dirs.push(dir.join("transcripts"));
        dirs.push(dir.join("edit").join("transcripts"));
    }
    if let Some(dir) = src.parent() {
        dirs.push(dir.join("edit").join("transcripts"));
        dirs.push(dir.join("transcripts"));
    }
    dirs.iter()
        .flat_map(|d| names.iter().map(move |n| d.join(format!("{n}.json"))))
        .find(|p| p.is_file())
}

/// The name a video-use EDL at `project` gives the file `src`, if it is one.
fn source_key(project: &Path, src: &Path) -> Option<String> {
    if std::fs::metadata(project).ok()?.len() > MAX_FILE {
        return None;
    }
    let edl = json::parse(&std::fs::read_to_string(project).ok()?).ok()?;
    if !is_video_use(&edl) {
        return None;
    }
    let base = project.parent()?;
    let Value::Obj(sources) = edl.get("sources")? else {
        return None;
    };
    sources.iter().find_map(|(name, path)| {
        let path = path.str()?;
        let full = tidy(&base.join(path));
        // A name is a file name, never a path.
        let plain = !name.is_empty() && !name.contains(['/', '\0']) && !name.starts_with('.');
        (plain && full == src).then(|| name.clone())
    })
}

/// A file from `from` to `to`, as a project of one clip at the file's own
/// size and rate: what a recording of the file played.
pub fn excerpt(work: &Path, file: &Path, spans: &[(f64, f64)]) -> Result<Project> {
    let probe = media::probe(work, file)?;
    let facts = Facts::of(&probe);
    let (w, h) = facts
        .display
        .ok_or_else(|| Error::new("NOT_PLAYABLE", "it has no picture (sound only)"))?;
    let (fps, fps_text) = snap(
        facts
            .fps
            .filter(|f| (1.0..=240.0).contains(f))
            .unwrap_or(30.0),
    );
    let clips: Vec<Clip> = spans
        .iter()
        .filter_map(|&(from, to)| {
            let (start, end) = (from.max(0.0), facts.duration.map_or(to, |d| to.min(d)));
            (end - start >= 0.04).then(|| Clip {
                src: file.to_owned(),
                name: None,
                start,
                end,
                fade_in: 0.0,
                fade_out: 0.0,
                audio: facts.audio,
                key: None,
                quote: None,
            })
        })
        .collect();
    if clips.is_empty() {
        return Err(Error::new("FAILED", "nothing of it was played"));
    }
    Ok(Project {
        path: file.to_owned(),
        format: Format::Pepper,
        output: Output {
            width: even(w),
            height: even(h),
            fps,
            fps_text,
            fit: Fit::Pad,
        },
        clips,
        warnings: Vec::new(),
    })
}

/// Whether a JSON is video-use's EDL: `sources` by name and `ranges` of them.
pub fn is_video_use(value: &Value) -> bool {
    value.get("pepper_edit").is_none()
        && matches!(value.get("sources"), Some(Value::Obj(_)))
        && matches!(value.get("ranges"), Some(Value::Arr(_)))
}

/// A video-use EDL as a pepper_edit project, to be checked like any other.
struct FromVideoUse {
    /// The ranges that name a source and their times, as pepper_edit clips
    /// (paths resolved as video-use's `render.py` does: absolute, or from
    /// the EDL's folder).
    pepper: Value,
    /// Which range each clip is.
    ranges: Vec<usize>,
    /// Each clip's source name and quote.
    extras: Vec<(String, Option<String>)>,
    /// What a preview of it leaves out, and the like.
    notes: Vec<Value>,
    /// The ranges that couldn't be made clips.
    problems: Vec<Value>,
}

fn from_video_use(edl: &Value, base: &Path) -> FromVideoUse {
    let mut problems = Vec::new();
    let mut notes = Vec::new();
    if let Some(v) = edl.get_num("version")
        && v != 1.0
    {
        notes.push(warning(
            "version",
            format!("video-use EDL version {v}: this pepper-agent knows version 1"),
        ));
    }
    let sources = edl.get("sources").unwrap_or(&Value::Null);
    let ranges = edl.get("ranges").and_then(Value::arr).unwrap_or(&[]);
    if ranges.is_empty() {
        problems.push(problem("ranges", "empty: list at least one range"));
    }
    let mut clips = Vec::new();
    let mut kept = Vec::new();
    let mut extras = Vec::new();
    for (i, r) in ranges.iter().enumerate() {
        let here = format!("ranges[{i}]");
        let before = problems.len();
        let key = r.get_str("source");
        let path = key.and_then(|k| sources.get_str(k));
        match (key, path) {
            (None, _) => problems.push(problem(
                &format!("{here}.source"),
                "missing: the name of one of sources",
            )),
            (Some(key), None) => problems.push(problem(
                &format!("{here}.source"),
                format!("no source called {:?} in sources", text::inert(key, 64)),
            )),
            _ => {}
        }
        for from in ["start", "end"] {
            if r.get(from).is_none() {
                problems.push(problem(
                    &format!("{here}.{from}"),
                    "missing: seconds in the source",
                ));
            }
        }
        let (Some(key), Some(path)) = (key, path) else {
            continue;
        };
        if problems.len() > before {
            continue;
        }
        let src = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            base.join(path)
        };
        let mut clip = Value::obj()
            .with("src", src.display().to_string())
            .with("in", r.get("start").cloned().unwrap_or(Value::Null))
            .with("out", r.get("end").cloned().unwrap_or(Value::Null));
        let beat = r
            .get_str("beat")
            .map(|b| text::typed(b, MAX_NAME))
            .filter(|b| !b.trim().is_empty());
        if let Some(beat) = beat {
            clip.set("name", beat.trim());
        }
        clips.push(clip);
        kept.push(i);
        let quote = r
            .get_str("quote")
            .map(|q| text::typed(q, 300))
            .filter(|q| !q.is_empty());
        extras.push((key.to_owned(), quote));
    }
    let left_out: Vec<&str> = ["grade", "overlays", "subtitles"]
        .into_iter()
        .filter(|k| {
            edl.get(k).is_some_and(|v| {
                !v.is_null() && v.str() != Some("none") && v.arr().is_none_or(|a| !a.is_empty())
            })
        })
        .collect();
    if !left_out.is_empty() {
        notes.push(warning(
            "preview",
            format!(
                "edit preview shows the cuts only, not the {}: review render.py's output with \
                 pepper-agent open <its output> --edit <this edl.json>",
                left_out.join(", ")
            ),
        ));
    }
    FromVideoUse {
        pepper: Value::obj()
            .with("pepper_edit", VERSION)
            .with("clips", Value::Arr(clips)),
        ranges: kept,
        extras,
        notes,
        problems,
    }
}

/// A problem or warning about the pepper_edit form of a video-use EDL, said
/// of the EDL: `clips[2].out` is `ranges[k].end`, k being clip 2's range.
fn to_video_use(mut v: Value, ranges: &[usize]) -> Value {
    if let Some(at) = v.get_str("at").map(str::to_owned)
        && let Some(rest) = at.strip_prefix("clips[")
        && let Some((n, rest)) = rest.split_once(']')
        && let Some(&k) = n.parse::<usize>().ok().and_then(|n| ranges.get(n))
    {
        let rest = rest
            .replace(".in", ".start")
            .replace(".out", ".end")
            .replace(".src", ".source")
            .replace(".name", ".beat");
        v.set("at", format!("ranges[{k}]{rest}"));
    }
    v
}

type Probe<'a> = dyn FnMut(&Path) -> std::result::Result<Facts, String> + 'a;

/// A project from its JSON: the output, the clips and warnings, or every
/// problem (`{"at": "clips[1].out", "problem": …}`). `facts` probes a file.
#[allow(clippy::type_complexity)]
pub fn build(
    project: &Value,
    base: &Path,
    facts: &mut Probe,
) -> std::result::Result<(Output, Vec<Clip>, Vec<Value>), Vec<Value>> {
    if !matches!(project, Value::Obj(_)) {
        return Err(vec![problem(
            "",
            "a project is a JSON object: {\"pepper_edit\": 1, \"clips\": […]}",
        )]);
    }
    let mut problems = Vec::new();
    let mut warnings = Vec::new();
    unknown(project, "", PROJECT_KEYS, "a project", &mut problems);
    match project.get("pepper_edit") {
        Some(Value::Num(n)) if *n == VERSION => {}
        Some(Value::Num(n)) => problems.push(problem(
            "pepper_edit",
            format!("version {n}: this pepper-agent reads version 1"),
        )),
        Some(_) => problems.push(problem("pepper_edit", "the format's version: the number 1")),
        None => problems.push(problem(
            "pepper_edit",
            "missing: a project starts {\"pepper_edit\": 1, …}",
        )),
    }
    let mut parsed: Vec<(usize, Clip)> = Vec::new();
    let mut first = None;
    match project.get("clips") {
        Some(Value::Arr(items)) if items.is_empty() => {
            problems.push(problem("clips", "empty: list at least one clip"))
        }
        Some(Value::Arr(items)) if items.len() > MAX_CLIPS => problems.push(problem(
            "clips",
            format!("{} clips: at most {MAX_CLIPS}", items.len()),
        )),
        Some(Value::Arr(items)) => {
            for (i, item) in items.iter().enumerate() {
                if let Some((clip, f)) = clip(i, item, base, facts, &mut problems, &mut warnings) {
                    if i == 0 {
                        first = Some(f);
                    }
                    parsed.push((i, clip));
                }
            }
        }
        Some(_) => problems.push(problem(
            "clips",
            "a list of clips: [{\"src\": \"a.mp4\", \"in\": 3, \"out\": 8}, …]",
        )),
        None => problems.push(problem("clips", "missing: the list of clips")),
    }
    let output = output(project.get("output"), first.as_ref(), &mut problems);
    if let Some(o) = &output {
        for (i, clip) in &parsed {
            if clip.length() < 1.0 / o.fps - 1e-9 {
                problems.push(problem(
                    &format!("clips[{i}].out"),
                    format!(
                        "the clip is {} s: shorter than a frame at {} fps",
                        round3(clip.length()),
                        o.fps_text
                    ),
                ));
            }
        }
    }
    match output {
        Some(o) if problems.is_empty() => {
            Ok((o, parsed.into_iter().map(|(_, c)| c).collect(), warnings))
        }
        _ if problems.is_empty() => Err(vec![problem(
            "output",
            "its size couldn't be worked out: give width and height",
        )]),
        _ => Err(problems),
    }
}

/// Members not in version 1, each a problem.
fn unknown(v: &Value, at: &str, known: &[&str], what: &str, problems: &mut Vec<Value>) {
    if let Value::Obj(members) = v {
        for (key, _) in members {
            if !known.contains(&key.as_str()) {
                problems.push(problem(
                    &join(at, &text::inert(key, 40)),
                    format!(
                        "not part of {what} in pepper_edit 1, which has {}",
                        known.join(", ")
                    ),
                ));
            }
        }
    }
}

fn clip(
    i: usize,
    item: &Value,
    base: &Path,
    facts: &mut Probe,
    problems: &mut Vec<Value>,
    warnings: &mut Vec<Value>,
) -> Option<(Clip, Facts)> {
    let here = format!("clips[{i}]");
    let at = |key: &str| join(&here, key);
    if !matches!(item, Value::Obj(_)) {
        problems.push(problem(
            &here,
            "a clip is an object: {\"src\": \"a.mp4\", \"in\": 3, \"out\": 8}",
        ));
        return None;
    }
    let before = problems.len();
    unknown(item, &here, CLIP_KEYS, "a clip", problems);
    let name = match item.get("name") {
        None => None,
        Some(Value::Str(s))
            if (1..=MAX_NAME).contains(&s.chars().count())
                && s.chars().all(|c| !c.is_control() && !text::invisible(c)) =>
        {
            Some(s.clone())
        }
        Some(_) => {
            problems.push(problem(
                &at("name"),
                format!("a name for people: text of 1 to {MAX_NAME} characters"),
            ));
            None
        }
    };
    let start = time(item.get("in"), &at("in"), problems);
    let end = time(item.get("out"), &at("out"), problems);
    let fades = fades(item.get("audio"), &at("audio"), problems);
    let src = match item.get("src") {
        Some(Value::Str(s)) if !s.is_empty() && s.len() <= MAX_PATH && !s.contains('\0') => {
            let path = tidy(&base.join(s));
            if path.is_file() {
                Some(path)
            } else {
                problems.push(problem(
                    &at("src"),
                    format!(
                        "no file at {}",
                        text::inert(&path.display().to_string(), 300)
                    ),
                ));
                None
            }
        }
        Some(_) => {
            problems.push(problem(
                &at("src"),
                "a file's path, from the project's folder",
            ));
            None
        }
        None => {
            problems.push(problem(
                &at("src"),
                "missing: the file this clip is cut from",
            ));
            None
        }
    };
    let src = src?;
    if src.to_str().is_none() {
        problems.push(problem(&at("src"), "its path isn't UTF-8"));
        return None;
    }
    let f = match facts(&src) {
        Ok(f) => f,
        Err(e) => {
            problems.push(problem(
                &at("src"),
                format!("can't be read as media: {}", text::inert(&e, 300)),
            ));
            return None;
        }
    };
    if f.display.is_none() {
        problems.push(problem(
            &at("src"),
            "has no picture: a clip needs video (sound-only files aren't in pepper_edit 1)",
        ));
    }
    let (Ok(start), Ok(end), Ok((fade_in, fade_out))) = (start, end, fades) else {
        return None;
    };
    let length = f.duration.filter(|d| d.is_finite() && *d > 0.0);
    let start = start.unwrap_or(0.0);
    let end = match (end, length) {
        (Some(end), _) => end,
        (None, Some(d)) => d,
        (None, None) => {
            problems.push(problem(
                &at("out"),
                "the file's length isn't known: give out",
            ));
            return None;
        }
    };
    if let Some(d) = length {
        if start >= d {
            problems.push(problem(
                &at("in"),
                format!(
                    "{} is past the file's end ({})",
                    text::clock(start),
                    text::clock(d)
                ),
            ));
        } else if end > d + 0.001 {
            problems.push(problem(
                &at("out"),
                format!(
                    "{} is past the file's end: it's {} long",
                    text::clock(end),
                    text::clock(d)
                ),
            ));
        }
    }
    if end <= start {
        problems.push(problem(
            &at("out"),
            format!(
                "out ({}) must come after in ({})",
                text::clock(end),
                text::clock(start)
            ),
        ));
    } else if fade_in + fade_out > end - start + 1e-9 {
        problems.push(problem(
            &at("audio"),
            format!(
                "the fades ({} s) are longer than the clip ({} s)",
                round3(fade_in + fade_out),
                round3(end - start)
            ),
        ));
    }
    if fade_in + fade_out > 0.0 && !f.audio {
        warnings.push(warning(
            &at("audio"),
            "the file has no sound: its fades do nothing",
        ));
    }
    if f.hdr {
        warnings.push(warning(
            &at("src"),
            "HDR footage: renders are 8-bit SDR, not tone-mapped, so it will look washed out",
        ));
    }
    if problems.len() > before {
        return None;
    }
    let clip = Clip {
        src,
        name,
        start,
        end,
        fade_in,
        fade_out,
        audio: f.audio,
        key: None,
        quote: None,
    };
    Some((clip, f))
}

/// A path without `.` and `..` in it, when that's the same file (a `..`
/// after a link to a folder may not be).
fn tidy(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    let same = match (std::fs::metadata(path), std::fs::metadata(&out)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    };
    if same { out } else { path.to_owned() }
}

/// A time member: seconds (3.2) or as people write it ("0:03.2"); Ok(None)
/// when it isn't there, Err once its problem is listed.
fn time(
    v: Option<&Value>,
    at: &str,
    problems: &mut Vec<Value>,
) -> std::result::Result<Option<f64>, ()> {
    let t = match v {
        None => return Ok(None),
        Some(Value::Num(n)) if n.is_finite() && *n >= 0.0 && *n < 1e9 => *n,
        Some(Value::Str(s)) => match text::parse_at(s) {
            Ok(t) => t,
            Err(e) => {
                problems.push(problem(at, e));
                return Err(());
            }
        },
        Some(_) => {
            problems.push(problem(
                at,
                "a time from the file's start: 12.5 or \"0:12.5\"",
            ));
            return Err(());
        }
    };
    Ok(Some(t))
}

fn fades(
    v: Option<&Value>,
    at: &str,
    problems: &mut Vec<Value>,
) -> std::result::Result<(f64, f64), ()> {
    let Some(v) = v else {
        return Ok((0.0, 0.0));
    };
    if !matches!(v, Value::Obj(_)) {
        problems.push(problem(
            at,
            "an object: {\"fade_in\": 0.5, \"fade_out\": 1}",
        ));
        return Err(());
    }
    let before = problems.len();
    unknown(v, at, AUDIO_KEYS, "a clip's audio", problems);
    let mut get = |key: &str| match v.get(key) {
        None => 0.0,
        Some(Value::Num(n)) if n.is_finite() && (0.0..=MAX_FADE).contains(n) => *n,
        Some(_) => {
            problems.push(problem(&join(at, key), format!("seconds, 0 to {MAX_FADE}")));
            0.0
        }
    };
    let (fade_in, fade_out) = (get("fade_in"), get("fade_out"));
    if problems.len() > before {
        Err(())
    } else {
        Ok((fade_in, fade_out))
    }
}

fn output(v: Option<&Value>, first: Option<&Facts>, problems: &mut Vec<Value>) -> Option<Output> {
    let empty = Value::obj();
    let v = match v {
        None => &empty,
        Some(v @ Value::Obj(_)) => v,
        Some(_) => {
            problems.push(problem(
                "output",
                "an object: {\"width\": 1920, \"height\": 1080, \"fps\": 30}",
            ));
            return None;
        }
    };
    let before = problems.len();
    unknown(v, "output", OUTPUT_KEYS, "output", problems);
    let mut dim = |key: &str| match v.get(key) {
        Some(Value::Num(n))
            if n.fract() == 0.0 && (16.0..=8192.0).contains(n) && (*n as u32).is_multiple_of(2) =>
        {
            Some(*n as u32)
        }
        Some(_) => {
            problems.push(problem(
                &format!("output.{key}"),
                "an even number of pixels, 16 to 8192",
            ));
            None
        }
        None => None,
    };
    let (width, height) = (dim("width"), dim("height"));
    let size = match (v.get("width"), v.get("height")) {
        (None, None) => first
            .and_then(|f| f.display)
            .map(|(w, h)| (even(w), even(h))),
        (Some(_), Some(_)) => width.zip(height),
        _ => {
            problems.push(problem(
                "output",
                "width and height go together: give both, or neither for the first clip's size",
            ));
            None
        }
    };
    let rate = |f: f64| (1.0..=240.0).contains(&f).then_some(f);
    let fps = match v.get("fps") {
        None => Some(first.and_then(|f| f.fps).and_then(rate).unwrap_or(30.0)),
        Some(Value::Num(n)) => rate(*n),
        Some(Value::Str(s)) => media::ratio(s).and_then(rate),
        Some(_) => None,
    };
    if fps.is_none() {
        problems.push(problem(
            "output.fps",
            "frames a second, 1 to 240: 30, 29.97 or \"30000/1001\"",
        ));
    }
    let fit = match v.get("fit").map(|f| f.str()) {
        None | Some(Some("pad")) => Some(Fit::Pad),
        Some(Some("crop")) => Some(Fit::Crop),
        Some(_) => {
            problems.push(problem(
                "output.fit",
                "pad (the whole picture, bars where its shape differs) or crop (the frame filled, edges cut)",
            ));
            None
        }
    };
    if problems.len() > before {
        return None;
    }
    let ((width, height), (fps, fps_text), fit) = (size?, snap(fps?), fit?);
    Some(Output {
        width,
        height,
        fps,
        fps_text,
        fit,
    })
}

fn even(n: u32) -> u32 {
    (n / 2 * 2).clamp(16, 8192)
}

/// Frames a second as ffmpeg takes them: NTSC's rates as their exact
/// fractions (29.97 is 30000/1001), others to a thousandth.
fn snap(fps: f64) -> (f64, String) {
    for n in [24000u32, 30000, 48000, 60000, 120000] {
        let exact = n as f64 / 1001.0;
        if (fps - exact).abs() < 0.002 {
            return (exact, format!("{n}/1001"));
        }
    }
    let fps = round3(fps);
    (fps, format!("{fps}"))
}

/// The clips as a table: each one's span in the file and in the output.
pub fn table(project: &Project) -> Vec<Value> {
    let mut at = 0.0;
    project
        .clips
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let length = c.length();
            let mut v = Value::obj().with("clip", i);
            if let Some(name) = &c.name {
                v.set("name", name.as_str());
            }
            v.set("src", c.src.display().to_string());
            v.set("in", round3(c.start));
            v.set("out", round3(c.end));
            v.set("length", round3(length));
            v.set("start", round3(at));
            v.set("end", round3(at + length));
            v.set("start_text", text::clock(at));
            v.set("end_text", text::clock(at + length));
            v.set("audio", c.audio);
            if let Some(key) = &c.key {
                v.set("source", key.as_str());
            }
            if let Some(quote) = &c.quote {
                v.set("quote", quote.as_str());
            }
            if c.fade_in > 0.0 {
                v.set("fade_in", c.fade_in);
            }
            if c.fade_out > 0.0 {
                v.set("fade_out", c.fade_out);
            }
            at += length;
            v
        })
        .collect()
}

/// The cut as an mpv EDL: each clip's file and span, played one after the
/// other with nothing rendered. Paths are length-prefixed (`%N%`), so no
/// character in them is read as syntax.
pub fn edl(project: &Project) -> String {
    let mut out = String::from("# mpv EDL v0\n");
    for clip in &project.clips {
        let path = clip.src.display().to_string();
        out += &format!(
            "%{}%{path},start={:.6},length={:.6}\n",
            path.len(),
            clip.start,
            clip.length()
        );
    }
    out
}

/// Where each clip lands in the output: the map between the output's time
/// and the footage's, as the viewer keeps it for a preview.
#[derive(Clone, Debug, PartialEq)]
pub struct Timeline {
    pub project: String,
    pub srcs: Vec<String>,
    pub cuts: Vec<Cut>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cut {
    /// Its file, in `srcs`.
    pub src: usize,
    pub name: Option<String>,
    /// In and out, in the file's time.
    pub start: f64,
    pub end: f64,
    /// Where it starts in the output.
    pub at: f64,
}

impl Timeline {
    pub fn of(project: &Project) -> Timeline {
        let mut srcs: Vec<String> = Vec::new();
        let mut cuts = Vec::new();
        let mut at = 0.0;
        for c in &project.clips {
            let path = c.src.display().to_string();
            let src = match srcs.iter().position(|s| *s == path) {
                Some(k) => k,
                None => {
                    srcs.push(path);
                    srcs.len() - 1
                }
            };
            cuts.push(Cut {
                src,
                name: c.name.clone(),
                start: c.start,
                end: c.end,
                at,
            });
            at += c.length();
        }
        Timeline {
            project: project.path.display().to_string(),
            srcs,
            cuts,
        }
    }

    /// As it goes to the viewer: each file once, the clips by number.
    pub fn to_json(&self) -> Value {
        let clips: Vec<Value> = self
            .cuts
            .iter()
            .map(|c| {
                let v = Value::obj()
                    .with("src", c.src)
                    .with("in", c.start)
                    .with("out", c.end);
                match &c.name {
                    Some(name) => v.with("name", name.as_str()),
                    None => v,
                }
            })
            .collect();
        Value::obj()
            .with("project", self.project.as_str())
            .with(
                "srcs",
                self.srcs
                    .iter()
                    .map(|s| Value::from(s.as_str()))
                    .collect::<Vec<_>>(),
            )
            .with("clips", clips)
    }

    /// As the viewer reads it from a request: bounded, every number checked.
    pub fn from_json(v: &Value) -> Result<Timeline> {
        let bad = |what: &str| Error::new("BAD_REQUEST", format!("edit: {what}"));
        let project = v
            .get_str("project")
            .filter(|p| p.len() <= MAX_PATH)
            .ok_or_else(|| bad("no project"))?;
        let list = |key: &str| {
            v.get(key)
                .and_then(Value::arr)
                .filter(|a| !a.is_empty() && a.len() <= MAX_CLIPS)
                .ok_or_else(|| bad(&format!("{key}: 1 to {MAX_CLIPS}")))
        };
        let srcs = list("srcs")?
            .iter()
            .map(|s| {
                s.str()
                    .filter(|s| s.len() <= MAX_PATH)
                    .map(str::to_owned)
                    .ok_or_else(|| bad("a src"))
            })
            .collect::<Result<Vec<String>>>()?;
        let mut cuts = Vec::new();
        let mut at = 0.0;
        for item in list("clips")? {
            let src = item
                .get_num("src")
                .filter(|n| n.fract() == 0.0 && *n >= 0.0 && (*n as usize) < srcs.len())
                .ok_or_else(|| bad("a clip's src"))? as usize;
            let (start, end) = match (item.get_num("in"), item.get_num("out")) {
                (Some(a), Some(b))
                    if a.is_finite() && a >= 0.0 && b.is_finite() && b > a && b < 1e9 =>
                {
                    (a, b)
                }
                _ => return Err(bad("a clip's in and out")),
            };
            let name = match item.get("name") {
                None | Some(Value::Null) => None,
                Some(Value::Str(s)) if s.chars().count() <= MAX_NAME => Some(s.clone()),
                Some(_) => return Err(bad("a clip's name")),
            };
            cuts.push(Cut {
                src,
                name,
                start,
                end,
                at,
            });
            at += end - start;
        }
        Ok(Timeline {
            project: project.to_owned(),
            srcs,
            cuts,
        })
    }

    /// The clip at the output's time `t`, and the time in its file. A cut
    /// belongs to the clip after it; the very end, to the last.
    pub fn locate(&self, t: f64) -> Option<(usize, f64)> {
        let t = if t.is_finite() { t.max(0.0) } else { 0.0 };
        let last = self.cuts.len().checked_sub(1)?;
        let k = self
            .cuts
            .iter()
            .position(|c| t < c.at + (c.end - c.start))
            .unwrap_or(last);
        let cut = &self.cuts[k];
        Some((k, cut.start + (t - cut.at).clamp(0.0, cut.end - cut.start)))
    }

    /// Where a file's time is in the cut, if a clip has it: so a mark made
    /// on an earlier cut finds its place in this one, or has none once its
    /// moment is cut out.
    pub fn place(&self, src: &str, src_time: f64) -> Option<f64> {
        let of = |c: &&Cut| self.srcs[c.src] == src;
        self.cuts
            .iter()
            .filter(of)
            .find(|c| src_time >= c.start && src_time < c.end)
            // A mark at a clip's very end (the cut's last moment).
            .or_else(|| self.cuts.iter().filter(of).find(|c| src_time == c.end))
            .map(|c| c.at + src_time - c.start)
    }

    /// Where the output's time `t` is in the footage, as events say it.
    pub fn describe(&self, t: f64, into: &mut Value) {
        into.set("project", self.project.as_str());
        if let Some((k, src_time)) = self.locate(t) {
            let cut = &self.cuts[k];
            into.set("clip", k);
            if let Some(name) = &cut.name {
                into.set("clip_name", name.as_str());
            }
            into.set("src", self.srcs[cut.src].as_str());
            into.set("src_time", round3(src_time));
            into.set("src_time_text", text::clock(src_time));
        }
    }

    /// For the human: `clip 2 of 3 (b.mp4 0:45.300)`.
    pub fn say(&self, t: f64) -> Option<String> {
        let (k, src_time) = self.locate(t)?;
        let file = Path::new(&self.srcs[self.cuts[k].src])
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Some(format!(
            "clip {} of {} ({} {})",
            k + 1,
            self.cuts.len(),
            text::inert(&file, 40),
            text::clock(src_time)
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Quality {
    /// Quick, on the graphics chip's encoder (VideoToolbox).
    Draft,
    /// The best, on the CPU (x264, x265).
    Final,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Codec {
    H264,
    Hevc,
}

pub struct RenderAsk {
    pub out: Option<String>,
    pub quality: Quality,
    pub codec: Codec,
    pub overwrite: bool,
}

/// The encoder and its options.
fn encoder(quality: Quality, codec: Codec) -> (&'static str, &'static [&'static str]) {
    match (quality, codec) {
        (Quality::Draft, Codec::H264) => ("h264_videotoolbox", &["-q:v", "60"]),
        (Quality::Draft, Codec::Hevc) => ("hevc_videotoolbox", &["-q:v", "60", "-tag:v", "hvc1"]),
        (Quality::Final, Codec::H264) => ("libx264", &["-preset", "slow", "-crf", "18"]),
        (Quality::Final, Codec::Hevc) => (
            "libx265",
            &[
                "-preset",
                "medium",
                "-crf",
                "20",
                "-tag:v",
                "hvc1",
                "-x265-params",
                "log-level=error",
            ],
        ),
    }
}

/// ffmpeg's arguments for a render into `out` (of `format`, mp4 or mov):
/// each clip read from its in for its length, fitted to the output's size
/// and rate, its sound (or silence, for a file without) made 48 kHz stereo
/// and faded, all joined in order. Progress goes to stdout.
pub fn render_args(
    project: &Project,
    out: &Path,
    format: &str,
    quality: Quality,
    codec: Codec,
) -> Vec<String> {
    let mut args: Vec<String> = [
        "-nostdin",
        "-v",
        "error",
        "-nostats",
        "-progress",
        "pipe:1",
        "-y",
    ]
    .map(String::from)
    .into();
    for clip in &project.clips {
        args.extend([
            "-ss".into(),
            format!("{:.6}", clip.start),
            "-t".into(),
            format!("{:.6}", clip.length()),
            "-hwaccel".into(),
            "videotoolbox".into(),
            "-i".into(),
            format!("file:{}", clip.src.display()),
        ]);
    }
    let o = &project.output;
    let (w, h) = (o.width, o.height);
    let fit = match o.fit {
        Fit::Pad => format!(
            "scale={w}:{h}:force_original_aspect_ratio=decrease:force_divisible_by=2:out_color_matrix=bt709:out_range=tv,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black"
        ),
        Fit::Crop => format!(
            "scale={w}:{h}:force_original_aspect_ratio=increase:out_color_matrix=bt709:out_range=tv,crop={w}:{h}"
        ),
    };
    let sound = project.clips.iter().any(|c| c.audio);
    let mut graph = String::new();
    let mut joined = String::new();
    for (i, clip) in project.clips.iter().enumerate() {
        let length = format!("{:.6}", clip.length());
        // Square pixels first, so the fit keeps the picture's shape.
        graph += &format!(
            "[{i}:v:0]scale=trunc(iw*sar/2)*2:ih,setsar=1,{fit},setsar=1,fps={}:start_time=0,trim=duration={length},setpts=PTS-STARTPTS,format=yuv420p[v{i}];",
            o.fps_text
        );
        joined += &format!("[v{i}]");
        if !sound {
            continue;
        }
        if clip.audio {
            graph += &format!(
                "[{i}:a:0]aresample=48000:first_pts=0,aformat=sample_fmts=fltp:channel_layouts=stereo"
            );
            if clip.fade_in > 0.0 {
                graph += &format!(",afade=t=in:st=0:d={:.6}", clip.fade_in);
            }
            if clip.fade_out > 0.0 {
                graph += &format!(
                    ",afade=t=out:st={:.6}:d={:.6}",
                    clip.length() - clip.fade_out,
                    clip.fade_out
                );
            }
            graph += &format!(",apad,atrim=duration={length},asetpts=PTS-STARTPTS[a{i}];");
        } else {
            graph += &format!(
                "anullsrc=r=48000:cl=stereo,atrim=duration={length},aformat=sample_fmts=fltp:channel_layouts=stereo[a{i}];"
            );
        }
        joined += &format!("[a{i}]");
    }
    graph += &format!(
        "{joined}concat=n={}:v=1:a={}[v]{}",
        project.clips.len(),
        sound as u8,
        if sound { "[a]" } else { "" }
    );
    args.extend(["-filter_complex".into(), graph, "-map".into(), "[v]".into()]);
    if sound {
        args.extend(["-map", "[a]", "-c:a", "aac", "-b:a", "192k"].map(String::from));
    }
    let (name, options) = encoder(quality, codec);
    args.extend(["-c:v".into(), name.into()]);
    args.extend(options.iter().map(|s| s.to_string()));
    args.extend(
        [
            "-pix_fmt",
            "yuv420p",
            "-colorspace",
            "bt709",
            "-color_primaries",
            "bt709",
            "-color_trc",
            "bt709",
            "-color_range",
            "tv",
            "-movflags",
            "+faststart",
            "-f",
        ]
        .map(String::from),
    );
    args.push(format.into());
    args.push(format!("file:{}", out.display()));
    args
}

/// Where a render goes: `-o`, or `.pepper/renders/<project>.mp4` (draft:
/// `<project>.draft.mp4`). Never one of the project's files; a file that
/// exists outside `.pepper/` only with --overwrite.
fn render_path(work: &Path, project: &Project, ask: &RenderAsk) -> Result<PathBuf> {
    let Some(given) = &ask.out else {
        let dir =
            files::sub_dir(work, &["renders"]).map_err(|e| Error::new("FAILED", e.to_string()))?;
        let draft = if ask.quality == Quality::Draft {
            ".draft"
        } else {
            ""
        };
        return Ok(dir.join(format!("{}{draft}.mp4", project.stem())));
    };
    let out = std::path::absolute(given).map_err(|e| Error::new("BAD_ARGS", format!("-o: {e}")))?;
    let shown = text::inert(&out.display().to_string(), 300);
    let ext = out
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if !["mp4", "m4v", "mov"].contains(&ext.as_str()) {
        return Err(Error::new(
            "BAD_ARGS",
            format!("-o {shown}: a .mp4, .m4v or .mov file"),
        ));
    }
    // Its folder as it is (no `..`, no links): `.pepper/../x.mp4` is outside.
    let (Some(folder), Some(name)) = (
        out.parent().and_then(|p| p.canonicalize().ok()),
        out.file_name(),
    ) else {
        return Err(Error::new("NOT_FOUND", format!("no folder for {shown}")));
    };
    let out = folder.join(name);
    let inside = work.canonicalize().is_ok_and(|w| out.starts_with(w));
    if let Ok(meta) = std::fs::metadata(&out) {
        if meta.is_dir() {
            return Err(Error::new("BAD_ARGS", format!("{shown} is a folder")));
        }
        let same = |p: &Path| {
            std::fs::metadata(p).is_ok_and(|m| m.dev() == meta.dev() && m.ino() == meta.ino())
        };
        if same(&project.path) || project.clips.iter().any(|c| same(&c.src)) {
            return Err(Error::new(
                "BAD_ARGS",
                format!("{shown} is the project or one of its clips' files"),
            )
            .hint("Source media is never written over: choose another -o."));
        }
        if !ask.overwrite && !inside {
            return Err(Error::new("BAD_ARGS", format!("{shown} already exists")).hint(
                "Choose another -o, or pass --overwrite to replace it (only if the user's fine with that).",
            ));
        }
    }
    Ok(out)
}

/// Renders a project with ffmpeg, telling progress on stderr; the file, its
/// length as made, and a contact sheet of it to look at.
pub fn render(work: &Path, project: &Project, ask: &RenderAsk) -> Result<Value> {
    if project.format == Format::VideoUse {
        return Err(Error::new(
            "UNSUPPORTED",
            "a video-use EDL is rendered by video-use's own helpers/render.py (grade, overlays, subtitles, loudness)",
        )
        .hint(
            "python <video-use>/helpers/render.py edit/edl.json -o edit/preview.mp4 --preview, \
             then show it: pepper-agent open edit/preview.mp4 --edit edit/edl.json",
        ));
    }
    if project.clips.is_empty() {
        return Err(Error::new(
            "BAD_ARGS",
            "nothing to render: the cut is empty",
        ));
    }
    let ffmpeg = media::tool("ffmpeg")?;
    let out = render_path(work, project, ask)?;
    let format = match out.extension().and_then(|e| e.to_str()) {
        Some(e) if e.eq_ignore_ascii_case("mov") => "mov",
        _ => "mp4",
    };
    let file_name = out
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Written aside, then moved: never half a file under the name. One left
    // by a render that was killed goes.
    let prefix = format!(".{file_name}.part");
    if let Some(Ok(entries)) = out.parent().map(std::fs::read_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let pid = name
                .to_str()
                .and_then(|n| n.strip_prefix(&prefix))
                .and_then(|p| p.parse::<u32>().ok());
            if pid.is_some_and(|p| !crate::sys::alive(p)) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let part = out.with_file_name(format!("{prefix}{}", std::process::id()));
    files::make_way(&part).map_err(|e| Error::new("FAILED", format!("{}: {e}", part.display())))?;
    let args = render_args(project, &part, format, ask.quality, ask.codec);
    let total = project.duration();
    let started = Instant::now();
    let mut child = Command::new(&ffmpeg)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::new("FAILED", format!("cannot run ffmpeg: {e}")))?;
    let stderr = child.stderr.take();
    let errors = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(stderr) = stderr {
            let _ = stderr.take(1 << 20).read_to_string(&mut text);
        }
        text
    });
    if let Some(stdout) = child.stdout.take() {
        let mut done = 0.0;
        for line in BufReader::new(stdout)
            .lines()
            .map_while(std::result::Result::ok)
        {
            if let Some(us) = line
                .strip_prefix("out_time_us=")
                .and_then(|v| v.parse::<f64>().ok())
            {
                done = (us / 1e6).clamp(0.0, total);
            } else if line.starts_with("progress=") {
                let progress = Value::obj().with(
                    "progress",
                    Value::obj()
                        .with("step", "render")
                        .with("done", round3(done))
                        .with("of", round3(total)),
                );
                eprintln!("{}", progress.to_line());
            }
        }
    }
    let status = child.wait();
    let errors = errors.join().unwrap_or_default();
    if !status.is_ok_and(|s| s.success()) || !part.is_file() {
        let _ = std::fs::remove_file(&part);
        let tail = errors.lines().rev().take(3).collect::<Vec<_>>().join(" | ");
        return Err(Error::new(
            "FAILED",
            format!("ffmpeg couldn't render it: {}", text::inert(&tail, 400)),
        ));
    }
    std::fs::rename(&part, &out).map_err(|e| {
        let _ = std::fs::remove_file(&part);
        Error::new("FAILED", format!("{}: {e}", out.display()))
    })?;
    let took = started.elapsed().as_secs_f64();
    let duration = media::probe(work, &out).ok().and_then(|p| p.duration);
    let mut warnings = project.warnings.clone();
    if let Some(d) = duration
        && (d - total).abs() > 1.5 / project.output.fps
    {
        warnings.push(warning(
            "",
            format!(
                "the render is {} long, the project {}",
                text::clock(d),
                text::clock(total)
            ),
        ));
    }
    let sheet = media::sheet(
        work,
        &out,
        &media::SheetAsk {
            from: None,
            to: None,
            cols: 4,
            rows: 4,
            max_side: media::MAX_SIDE,
        },
    )
    .ok()
    .and_then(|s| s.get_str("path").map(str::to_owned));
    let (encoder, _) = encoder(ask.quality, ask.codec);
    Ok(Value::obj()
        .with("path", out.display().to_string())
        .with("project", project.path.display().to_string())
        .with(
            "quality",
            match ask.quality {
                Quality::Draft => "draft",
                Quality::Final => "final",
            },
        )
        .with("encoder", encoder)
        .with("output", project.output.to_json())
        .with("duration", duration.map(round3))
        .with("expected_duration", round3(total))
        .with("size_bytes", std::fs::metadata(&out).map(|m| m.len()).ok())
        .with("took", round3(took))
        .with("speed", round3(total / took.max(0.001)))
        .with("sheet", sheet)
        .with("warnings", warnings))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = env!("CARGO_MANIFEST_DIR");

    /// Files that exist (this crate's), each with made-up facts.
    fn facts(path: &Path) -> std::result::Result<Facts, String> {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        match name {
            "Cargo.toml" => Ok(Facts {
                duration: Some(20.0),
                display: Some((1280, 720)),
                fps: Some(30000.0 / 1001.0),
                audio: true,
                hdr: false,
            }),
            "Cargo.lock" => Ok(Facts {
                duration: Some(15.0),
                display: Some((853, 480)),
                fps: Some(25.0),
                audio: false,
                hdr: true,
            }),
            "README.md" => Ok(Facts {
                duration: Some(4.0),
                display: None,
                fps: None,
                audio: true,
                hdr: false,
            }),
            _ => Err("not media".into()),
        }
    }

    fn parse(text: &str) -> std::result::Result<(Output, Vec<Clip>, Vec<Value>), Vec<Value>> {
        let v = json::parse(text).unwrap();
        build(&v, Path::new(BASE), &mut facts)
    }

    fn at_list(problems: &[Value]) -> Vec<&str> {
        problems.iter().map(|p| p.get_str("at").unwrap()).collect()
    }

    #[test]
    fn a_good_project() {
        let (o, clips, warnings) = parse(
            r#"{"pepper_edit": 1, "clips": [
                {"src": "Cargo.toml", "in": "0:03.2", "out": 11, "name": "intro"},
                {"src": "Cargo.lock", "in": 5, "audio": {"fade_in": 0.5}},
                {"src": "Cargo.toml"}
            ]}"#,
        )
        .unwrap();
        // The first clip's size and rate, NTSC's as its fraction.
        assert_eq!((o.width, o.height, o.fit), (1280, 720, Fit::Pad));
        assert_eq!(o.fps_text, "30000/1001");
        assert_eq!(clips.len(), 3);
        assert_eq!((clips[0].start, clips[0].end), (3.2, 11.0));
        assert_eq!(clips[0].src, Path::new(BASE).join("Cargo.toml"));
        // No out: to the file's end; no in: from its start.
        assert_eq!((clips[1].start, clips[1].end), (5.0, 15.0));
        assert_eq!((clips[2].start, clips[2].end), (0.0, 20.0));
        // Fades on a silent file, and HDR, are warnings.
        assert_eq!(at_list(&warnings), ["clips[1].audio", "clips[1].src"]);
        let (o, ..) = parse(
            r#"{"pepper_edit": 1, "output": {"width": 1920, "height": 1080, "fps": 25, "fit": "crop"},
                "clips": [{"src": "Cargo.lock"}]}"#,
        )
        .unwrap();
        assert_eq!(
            (o.width, o.height, o.fps_text.as_str(), o.fit),
            (1920, 1080, "25", Fit::Crop)
        );
        // 853 wide is made even.
        let (o, ..) = parse(r#"{"pepper_edit": 1, "clips": [{"src": "Cargo.lock"}]}"#).unwrap();
        assert_eq!((o.width, o.height), (852, 480));
    }

    #[test]
    fn every_problem_at_once() {
        let problems = parse(
            r#"{"pepper_edit": 2, "outptu": {}, "output": {"width": 1921, "fps": 0}, "clips": [
                {"src": "Cargo.toml", "in": "1:99", "out": 30},
                {"src": "Cargo.toml", "in": 8, "out": 4, "trim": 1},
                {"src": "nope.mp4"},
                {"src": "Cargo.toml", "out": 25},
                {"src": "Cargo.toml", "in": 1, "out": 2, "audio": {"fade_in": 0.8, "fade_out": 0.5}},
                {"src": "README.md"},
                {"src": "Cargo.toml", "in": -1, "name": ""},
                "x"
            ]}"#,
        )
        .unwrap_err();
        assert_eq!(
            at_list(&problems),
            [
                "outptu",
                "pepper_edit",
                "clips[0].in",
                "clips[1].trim",
                "clips[1].out",
                "clips[2].src",
                "clips[3].out",
                "clips[4].audio",
                "clips[5].src",
                "clips[6].name",
                "clips[6].in",
                "clips[7]",
                "output.width",
                "output",
                "output.fps",
            ],
            "{problems:#?}"
        );
        assert!(parse("[1]").is_err());
        let none = parse(r#"{"clips": []}"#).unwrap_err();
        assert_eq!(at_list(&none), ["pepper_edit", "clips"]);
        // Shorter than a frame.
        let short =
            parse(r#"{"pepper_edit": 1, "clips": [{"src": "Cargo.toml", "in": 1, "out": 1.01}]}"#)
                .unwrap_err();
        assert_eq!(at_list(&short), ["clips[0].out"]);
    }

    fn project(clips: Vec<Clip>) -> Project {
        Project {
            path: PathBuf::from("/p/cut.json"),
            format: Format::Pepper,
            output: Output {
                width: 1280,
                height: 720,
                fps: 30.0,
                fps_text: "30".into(),
                fit: Fit::Pad,
            },
            clips,
            warnings: Vec::new(),
        }
    }

    fn cut(src: &str, start: f64, end: f64, audio: bool) -> Clip {
        Clip {
            src: PathBuf::from(src),
            name: None,
            start,
            end,
            fade_in: 0.0,
            fade_out: 0.0,
            audio,
            key: None,
            quote: None,
        }
    }

    /// video-use's EDL, as its SKILL.md shows it; this crate's files for
    /// the footage.
    fn video_use_edl(ranges: &str) -> Value {
        json::parse(&format!(
            r#"{{"version": 1,
                "sources": {{"C0103": "Cargo.toml", "C0108": "{BASE}/Cargo.lock"}},
                "ranges": {ranges},
                "grade": "warm_cinematic",
                "overlays": [{{"file": "edit/animations/slot_1/render.mp4", "start_in_output": 0.0, "duration": 5.0}}],
                "subtitles": "edit/master.srt",
                "total_duration_s": 18.0}}"#
        ))
        .unwrap()
    }

    #[test]
    fn video_use_edls() {
        let edl = video_use_edl(
            r#"[{"source": "C0103", "start": 2.42, "end": 6.85, "beat": "HOOK", "quote": "Ninety percent\u001b[2J wasted", "reason": "cleanest"},
                {"source": "C0108", "start": 1.0, "end": 14.57, "beat": "SOLUTION"}]"#,
        );
        assert!(is_video_use(&edl));
        let vu = from_video_use(&edl, Path::new(BASE));
        assert!(vu.problems.is_empty());
        // What a preview leaves out is said, once.
        assert_eq!(vu.notes.len(), 1);
        assert_eq!(vu.notes[0].get_str("at"), Some("preview"));
        assert!(
            vu.notes[0]
                .get_str("warning")
                .unwrap()
                .contains("grade, overlays, subtitles")
        );
        assert_eq!(
            vu.extras[0],
            ("C0103".into(), Some("Ninety percent[2J wasted".into()))
        );
        let (_, clips, _) = build(&vu.pepper, Path::new(BASE), &mut facts).unwrap();
        // Paths as render.py has them: from the EDL's folder, or absolute.
        assert_eq!(clips[0].src, Path::new(BASE).join("Cargo.toml"));
        assert_eq!(clips[1].src, Path::new(BASE).join("Cargo.lock"));
        assert_eq!((clips[0].start, clips[0].end), (2.42, 6.85));
        assert_eq!(clips[0].name.as_deref(), Some("HOOK"));
        // Problems are said of the EDL (ranges[k].end, not clips[i].out),
        // the ranges' own and those of the clips made of the rest.
        let edl = video_use_edl(
            r#"[{"source": "C9999", "start": 1.0, "end": 2.0},
                {"source": "C0103", "start": 2.0, "end": 30.0},
                {"source": "C0108", "start": 3.0}]"#,
        );
        let vu = from_video_use(&edl, Path::new(BASE));
        assert_eq!(at_list(&vu.problems), ["ranges[0].source", "ranges[2].end"]);
        assert_eq!(vu.ranges, [1]);
        let problems: Vec<Value> = build(&vu.pepper, Path::new(BASE), &mut facts)
            .unwrap_err()
            .into_iter()
            .map(|p| to_video_use(p, &vu.ranges))
            .collect();
        assert_eq!(at_list(&problems), ["ranges[1].end"]);
        // Not video-use's: a pepper_edit project, or something else.
        assert!(!is_video_use(
            &json::parse(r#"{"pepper_edit": 1, "sources": {}, "ranges": []}"#).unwrap()
        ));
        assert!(!is_video_use(&json::parse(r#"{"ranges": []}"#).unwrap()));
    }

    #[test]
    fn transcripts_beside_the_edit() {
        let dir = std::env::temp_dir().join(format!("pepper-agent-tr-{}", std::process::id()));
        let edit = dir.join("edit");
        std::fs::create_dir_all(edit.join("transcripts")).unwrap();
        let take = dir.join("take one.mp4");
        std::fs::write(&take, b"x").unwrap();
        let edl = edit.join("edl.json");
        std::fs::write(
            &edl,
            r#"{"sources": {"T1": "../take one.mp4", "../x": "../take one.mp4"}, "ranges": []}"#,
        )
        .unwrap();
        // By the EDL's name for the source.
        let by_key = edit.join("transcripts").join("T1.json");
        std::fs::write(&by_key, b"{}").unwrap();
        assert_eq!(transcript_for(Some(&edl), &take), Some(by_key.clone()));
        // Without the EDL: video-use's edit/ beside the footage, by stem.
        assert_eq!(transcript_for(None, &take), None);
        let by_stem = edit.join("transcripts").join("take one.json");
        std::fs::write(&by_stem, b"{}").unwrap();
        assert_eq!(transcript_for(None, &take), Some(by_stem));
        assert_eq!(transcript_for(Some(&edl), &take), Some(by_key));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn times_both_ways() {
        let p = project(vec![
            cut("/m/a.mp4", 2.0, 5.0, true),
            cut("/m/b.mp4", 5.0, 9.0, false),
            cut("/m/a.mp4", 1.5, 4.0, true),
        ]);
        let t = Timeline::of(&p);
        assert_eq!(t.srcs, ["/m/a.mp4", "/m/b.mp4"]);
        assert_eq!(t.cuts[2].at, 7.0);
        // A part of the cut, as a project: 2.5 to 8 is the end of clip 0,
        // all of clip 1, the start of clip 2.
        let part = p.slice(&[(2.5, 8.0)]);
        let spans: Vec<(f64, f64)> = part.clips.iter().map(|c| (c.start, c.end)).collect();
        assert_eq!(spans, [(4.5, 5.0), (5.0, 9.0), (1.5, 2.5)]);
        assert_eq!(part.duration(), 5.5);
        assert!(p.slice(&[(20.0, 30.0)]).clips.is_empty());
        // Two stretches (a seek between), in the order they played.
        let two = p.slice(&[(7.5, 8.0), (0.0, 1.0)]);
        let spans: Vec<(f64, f64)> = two.clips.iter().map(|c| (c.start, c.end)).collect();
        assert_eq!(spans, [(2.0, 2.5), (2.0, 3.0)]);
        // Starting inside a fade-out: it fades over what's left.
        let mut faded = p.clips[0].clone();
        faded.fade_out = 1.0;
        faded.fade_in = 0.5;
        let f = project(vec![faded]);
        let tail = f.slice(&[(2.7, 3.0)]);
        assert!((tail.clips[0].fade_out - 0.3).abs() < 1e-9 && tail.clips[0].fade_in == 0.0);
        let head = f.slice(&[(0.0, 0.2)]);
        assert!((head.clips[0].fade_in - 0.2).abs() < 1e-9 && head.clips[0].fade_out == 0.0);
        assert_eq!(t.locate(0.0), Some((0, 2.0)));
        assert_eq!(t.locate(2.5), Some((0, 4.5)));
        // A cut belongs to the clip after it.
        assert_eq!(t.locate(3.0), Some((1, 5.0)));
        assert_eq!(t.locate(7.0), Some((2, 1.5)));
        assert_eq!(t.locate(9.5), Some((2, 4.0)));
        assert_eq!(t.locate(20.0), Some((2, 4.0)));
        assert_eq!(t.locate(-1.0), Some((0, 2.0)));
        assert_eq!(t.locate(f64::NAN), Some((0, 2.0)));
        let mut v = Value::obj();
        t.describe(4.25, &mut v);
        assert_eq!(v.get_num("clip"), Some(1.0));
        assert_eq!(v.get_str("src"), Some("/m/b.mp4"));
        assert_eq!(v.get_num("src_time"), Some(6.25));
        assert_eq!(v.get_str("src_time_text"), Some("0:06.250"));
        assert_eq!(t.say(4.25).unwrap(), "clip 2 of 3 (b.mp4 0:06.250)");
        // And back: a file's time in the cut, if a clip has it.
        assert_eq!(t.place("/m/b.mp4", 6.25), Some(4.25));
        assert_eq!(t.place("/m/a.mp4", 1.5), Some(7.0));
        assert_eq!(t.place("/m/a.mp4", 4.0), Some(2.0));
        assert_eq!(t.place("/m/b.mp4", 9.0), Some(7.0));
        assert_eq!(t.place("/m/a.mp4", 0.5), None);
        assert_eq!(t.place("/m/c.mp4", 1.0), None);
        // Through the viewer's request and back.
        let back = Timeline::from_json(&json::parse(&t.to_json().to_line()).unwrap()).unwrap();
        assert_eq!(back, t);
        for bad in [
            r#"{"project": "p", "srcs": ["a"], "clips": [{"src": 1, "in": 0, "out": 1}]}"#,
            r#"{"project": "p", "srcs": ["a"], "clips": [{"src": 0, "in": 2, "out": 1}]}"#,
            r#"{"project": "p", "srcs": ["a"], "clips": [{"src": 0.5, "in": 0, "out": 1}]}"#,
            r#"{"project": "p", "srcs": ["a"], "clips": []}"#,
            r#"{"srcs": ["a"], "clips": [{"src": 0, "in": 0, "out": 1}]}"#,
        ] {
            assert!(
                Timeline::from_json(&json::parse(bad).unwrap()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn edl_and_table() {
        let odd = "/m/a, b;\nc%.mp4";
        let p = project(vec![
            cut(odd, 2.0, 5.0, true),
            cut("/m/é.mp4", 0.5, 1.25, true),
        ]);
        assert_eq!(
            edl(&p),
            format!(
                "# mpv EDL v0\n%{}%{odd},start=2.000000,length=3.000000\n%9%/m/é.mp4,start=0.500000,length=0.750000\n",
                odd.len()
            )
        );
        let table = table(&p);
        assert_eq!(table[1].get_num("start"), Some(3.0));
        assert_eq!(table[1].get_num("end"), Some(3.75));
        assert_eq!(table[1].get_str("end_text"), Some("0:03.750"));
    }

    #[test]
    fn render_graph() {
        let mut faded = cut("/m/a.mp4", 2.0, 5.0, true);
        faded.fade_in = 0.3;
        faded.fade_out = 0.5;
        let p = project(vec![faded, cut("/m/b.mp4", 5.0, 9.0, false)]);
        let args = render_args(
            &p,
            Path::new("/o/x.mp4"),
            "mp4",
            Quality::Draft,
            Codec::H264,
        );
        let graph = &args[args.iter().position(|a| a == "-filter_complex").unwrap() + 1];
        assert!(graph.contains("afade=t=in:st=0:d=0.300000"));
        assert!(graph.contains("afade=t=out:st=2.500000:d=0.500000"));
        // Silence for the file without sound, so the joins line up.
        assert!(graph.contains("anullsrc=r=48000:cl=stereo,atrim=duration=4.000000"));
        assert!(graph.ends_with("[v0][a0][v1][a1]concat=n=2:v=1:a=1[v][a]"));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-c:v" && w[1] == "h264_videotoolbox")
        );
        assert!(args.contains(&"file:/m/b.mp4".to_string()));
        assert_eq!(args.last().unwrap(), "file:/o/x.mp4");
        // No sound anywhere: none in the output.
        let p = project(vec![cut("/m/b.mp4", 5.0, 9.0, false)]);
        let args = render_args(
            &p,
            Path::new("/o/x.mov"),
            "mov",
            Quality::Final,
            Codec::Hevc,
        );
        let graph = &args[args.iter().position(|a| a == "-filter_complex").unwrap() + 1];
        assert!(graph.ends_with("[v0]concat=n=1:v=1:a=0[v]"));
        assert!(!args.contains(&"[a]".to_string()));
        assert!(args.windows(2).any(|w| w[0] == "-c:v" && w[1] == "libx265"));
    }

    #[test]
    fn rates() {
        assert_eq!(snap(29.97).1, "30000/1001");
        assert_eq!(snap(23.976).1, "24000/1001");
        assert_eq!(snap(25.0), (25.0, "25".into()));
        assert_eq!(snap(12.5).1, "12.5");
    }
}
