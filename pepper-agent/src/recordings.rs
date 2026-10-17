//! The commands' side of recordings and comparing (spec 4.5): `record stop`
//! takes the viewer's file into `.pepper/recordings/` (a page's) or cuts
//! the stretch that played from the file itself (a video's, a cut's);
//! `record run` records a page while it drives it through a script;
//! `compare` opens what isn't a source yet and puts the two side by side;
//! `play <file>` shows a recording.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::json::{self, Value};
use crate::{Args, Target, edit, files, media, proto, sys, text};

/// The most steps in a script, and the longest `wait` in one.
const MAX_STEPS: usize = 200;
const MAX_PAUSE: f64 = 30.0;
/// Still frames at a recorded demo's start and end.
const LEAD: Duration = Duration::from_millis(600);

/// Sends a request to the viewer (pick_viewer), with the session and
/// --force: the viewer and its answer.
pub fn ask(
    args: &Args,
    cmd: &str,
    mut request: Value,
    wait: Duration,
) -> Result<(proto::Viewer, Value)> {
    if let Some(session) = &args.session {
        request.set("session", session.as_str());
    }
    if args.force {
        request.set("force", true);
    }
    let dir = proto::run_dir()?;
    let viewer = pick_viewer(&dir, args)?;
    let answer = proto::request(&dir, &viewer, cmd, request, wait)?;
    Ok((viewer, answer))
}

/// The viewer `--viewer` names; else the one that gave the session (its id
/// begins the token), while it runs; else the latest (proto::pick).
pub fn pick_viewer(dir: &Path, args: &Args) -> Result<proto::Viewer> {
    let gave = args
        .session
        .as_deref()
        .and_then(|s| s.split_once('-'))
        .map(|(id, _)| id)
        .filter(|id| proto::viewers(dir).iter().any(|v| v.id == *id));
    proto::pick(dir, args.viewer.as_deref().or(gave))
}

fn with_viewer(viewer: &proto::Viewer, mut answer: Value) -> Value {
    if let Value::Obj(members) = &mut answer {
        members.insert(0, ("viewer".into(), Value::from(viewer.id.as_str())));
    }
    answer
}

/// `record stop [<name>]`.
pub fn stop(args: &Args, source: Option<&str>) -> Result<Value> {
    let mut request = Value::obj();
    if let Some(name) = source {
        request.set("source", name);
    }
    let (viewer, done) = ask(args, "record.stop", request, Duration::from_secs(90))?;
    let work = crate::work_dir()?;
    let dir =
        files::sub_dir(&work, &["recordings"]).map_err(|e| Error::new("FAILED", e.to_string()))?;
    let name = done.get_str("source").unwrap_or("source").to_owned();
    let stamp: String = sys::iso_time(sys::now())
        .chars()
        .filter(char::is_ascii_digit)
        .take(14)
        .collect();
    let mut out = dir.join(format!("{}-{stamp}.mp4", files::name_part(&name)));
    for k in 2.. {
        if !out.exists() {
            break;
        }
        out = dir.join(format!("{}-{stamp}-{k}.mp4", files::name_part(&name)));
    }
    let mut answer = if let Some(raw) = done.get_str("raw") {
        take_raw(Path::new(raw), &out)?;
        let duration = media::probe(&work, &out).ok().and_then(|p| p.duration);
        let sheet = sheet_of(&work, &out);
        Value::obj()
            .with("path", out.display().to_string())
            .with("duration", duration.map(|d| (d * 1000.0).round() / 1000.0))
            .with("frames", done.get("frames").cloned().unwrap_or(Value::Null))
            .with("width", done.get("width").cloned().unwrap_or(Value::Null))
            .with("height", done.get("height").cloned().unwrap_or(Value::Null))
            .with("sheet", sheet)
    } else {
        let (from, to) = (
            done.get_num("from").unwrap_or(0.0),
            done.get_num("to").unwrap_or(0.0),
        );
        // A render shown with its cut is cut from the render; a preview
        // (no file), from the cut's own clips.
        let project = match (done.get_str("file"), done.get_str("project")) {
            (Some(file), _) if file.starts_with('/') => {
                edit::excerpt(&work, Path::new(file), from, to)?
            }
            (None, Some(project)) => edit::load(&work, project)?.slice(from, to),
            _ => {
                return Err(Error::new(
                    "UNSUPPORTED",
                    "only a file's stretch can be cut out (a web address can't)",
                ));
            }
        };
        let ask = edit::RenderAsk {
            out: Some(out.display().to_string()),
            quality: edit::Quality::Draft,
            codec: edit::Codec::H264,
            overwrite: true,
        };
        let made = edit::render(&work, &project, &ask)?;
        let mut v = Value::obj();
        for key in ["path", "duration", "sheet"] {
            v.set(key, made.get(key).cloned().unwrap_or(Value::Null));
        }
        v.set("from", from);
        v.set("to", to);
        v
    };
    for key in ["source", "kind", "owner", "file", "url", "project"] {
        if let Some(value) = done.get(key) {
            answer.set(key, value.clone());
        }
    }
    answer.set(
        "next",
        "Look at its sheet; then show it to the human: pepper-agent play <path>, and wait for y or x (events --until approve,reject).",
    );
    Ok(with_viewer(&viewer, answer))
}

/// The viewer's recording, from its own folder into `out`.
fn take_raw(raw: &Path, out: &Path) -> Result<()> {
    let run = proto::run_dir()?;
    if raw.starts_with(&run) && raw.extension().is_some_and(|e| e == "mkv") && !raw.is_file() {
        return Err(Error::new("FAILED", "nothing came while it was recorded")
            .hint("Record while it plays: pepper-agent where says its status."));
    }
    if !raw.starts_with(&run) || !raw.is_file() {
        return Err(Error::new(
            "FAILED",
            "the viewer's recording isn't where it should be",
        ));
    }
    files::make_way(out).map_err(|e| Error::new("FAILED", format!("{}: {e}", out.display())))?;
    if raw.extension().is_some_and(|e| e == "mkv") {
        // mpv answers before it has closed the file: until it stops growing.
        let mut last = None;
        for _ in 0..50 {
            let len = std::fs::metadata(raw).map(|m| m.len()).ok();
            if len.is_some() && len == last {
                break;
            }
            last = len;
            std::thread::sleep(Duration::from_millis(100));
        }
        let made = to_mp4(raw, out);
        let _ = std::fs::remove_file(raw);
        return made;
    }
    if std::fs::rename(raw, out).is_err() {
        std::fs::copy(raw, out)
            .map_err(|e| Error::new("FAILED", format!("{}: {e}", out.display())))?;
        let _ = std::fs::remove_file(raw);
    }
    Ok(())
}

/// A stream's recording (what mpv got, kept in Matroska) as an MP4: its
/// packets as they came if MP4 takes them (H.264, HEVC, AAC), else encoded.
fn to_mp4(raw: &Path, out: &Path) -> Result<()> {
    let ffmpeg = media::tool("ffmpeg")?;
    let made = |codecs: &[&str]| -> std::result::Result<(), String> {
        let done = std::process::Command::new(&ffmpeg)
            .args(["-nostdin", "-v", "error", "-y", "-i"])
            .arg(format!("file:{}", raw.display()))
            .args(["-map", "0:v:0", "-map", "0:a?"])
            .args(codecs)
            .args(["-movflags", "+faststart", "-f", "mp4"])
            .arg(format!("file:{}", out.display()))
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| e.to_string())?;
        if done.status.success() {
            return Ok(());
        }
        let err = String::from_utf8_lossy(&done.stderr);
        Err(err.lines().rev().take(2).collect::<Vec<_>>().join(" | "))
    };
    let encoded = [
        "-vf",
        "format=yuv420p",
        "-c:v",
        "h264_videotoolbox",
        "-q:v",
        "60",
        "-c:a",
        "aac",
    ];
    let why = match made(&["-c", "copy"]).or_else(|_| made(&encoded)) {
        Ok(()) => return Ok(()),
        Err(why) => why,
    };
    let _ = std::fs::remove_file(out);
    Err(Error::new(
        "FAILED",
        format!(
            "the recording couldn't be made an MP4: {}",
            text::inert(&text::redact_urls(&why), 300)
        ),
    )
    .hint("Record while it shows frames: pepper-agent where says its status."))
}

fn sheet_of(work: &Path, file: &Path) -> Option<String> {
    let ask = media::SheetAsk {
        from: None,
        to: None,
        cols: 4,
        rows: 4,
        max_side: media::MAX_SIDE,
    };
    media::sheet(work, file, &ask)
        .ok()
        .and_then(|s| s.get_str("path").map(str::to_owned))
}

/// One step of a script: a page command with its arguments, a pause, or a
/// line for the human.
#[derive(Debug, PartialEq)]
enum Step {
    Web(&'static str, Value),
    Pause(f64),
    Say(String),
}

/// A script: a list of steps, each an object of one verb, or
/// `{"source": ..., "steps": [...]}`. Every problem at once.
fn steps(script: &Value) -> std::result::Result<(Option<String>, Vec<Step>), Vec<Value>> {
    let (source, list) = match script {
        Value::Arr(list) => (None, list.as_slice()),
        Value::Obj(_) => (
            script.get_str("source").map(str::to_owned),
            script.get("steps").and_then(Value::arr).unwrap_or(&[]),
        ),
        _ => (None, &[][..]),
    };
    let problem = |at: usize, what: String| {
        Value::obj()
            .with("at", format!("steps[{at}]"))
            .with("problem", what)
    };
    if list.is_empty() {
        return Err(vec![Value::obj().with("at", "steps").with(
            "problem",
            "no steps: [{\"click\": \"#start\"}, {\"wait\": 1}, …]",
        )]);
    }
    if list.len() > MAX_STEPS {
        return Err(vec![
            Value::obj()
                .with("at", "steps")
                .with("problem", format!("at most {MAX_STEPS} steps")),
        ]);
    }
    let mut out = Vec::new();
    let mut problems = Vec::new();
    for (i, step) in list.iter().enumerate() {
        let Value::Obj(members) = step else {
            problems.push(problem(
                i,
                "a step is an object of one verb: {\"click\": \"#start\"}".into(),
            ));
            continue;
        };
        let [(verb, arg)] = members.as_slice() else {
            problems.push(problem(i, "one verb a step".into()));
            continue;
        };
        let text = |v: &Value| {
            v.str()
                .filter(|s| !s.is_empty() && s.len() <= 4096)
                .map(str::to_owned)
        };
        let made = match (verb.as_str(), arg) {
            ("click" | "hover", v) => text(v).map(|t| {
                Step::Web(
                    if verb == "click" {
                        "web.click"
                    } else {
                        "web.hover"
                    },
                    Value::obj().with("target", t),
                )
            }),
            ("type", Value::Arr(pair)) => match pair.as_slice() {
                [s, t] => text(s).zip(t.str()).map(|(s, t)| {
                    Step::Web(
                        "web.type",
                        Value::obj()
                            .with("selector", s)
                            .with("text", t)
                            .with("clear", false),
                    )
                }),
                _ => None,
            },
            ("type", v @ Value::Obj(_)) => text(v.get("selector").unwrap_or(&Value::Null))
                .zip(v.get_str("text"))
                .map(|(s, t)| {
                    Step::Web(
                        "web.type",
                        Value::obj()
                            .with("selector", s)
                            .with("text", t)
                            .with("clear", v.get_bool("clear").unwrap_or(false)),
                    )
                }),
            ("press", v) => text(v).map(|k| Step::Web("web.press", Value::obj().with("key", k))),
            ("scroll", Value::Num(n)) => Some(Step::Web(
                "web.scroll",
                Value::obj().with("to", n.to_string()),
            )),
            ("scroll", v) => text(v).map(|t| Step::Web("web.scroll", Value::obj().with("to", t))),
            ("goto", v) => text(v)
                .and_then(|u| crate::web_target(&u).ok())
                .map(|u| Step::Web("web.goto", Value::obj().with("url", u))),
            ("reload", _) => Some(Step::Web("web.reload", Value::obj())),
            ("back", _) => Some(Step::Web("web.back", Value::obj())),
            ("wait", Value::Num(n)) if (0.0..=MAX_PAUSE).contains(n) => Some(Step::Pause(*n)),
            ("wait_for", v) => {
                text(v).map(|w| Step::Web("web.wait-for", Value::obj().with("what", w)))
            }
            ("viewport", v) => {
                text(v).map(|d| Step::Web("web.viewport", Value::obj().with("device", d)))
            }
            ("say", v) => text(v).map(Step::Say),
            _ => {
                problems.push(problem(
                    i,
                    format!(
                        "{:?} with that isn't a step: click, hover, type, press, scroll, goto, reload, back, wait (seconds, up to {MAX_PAUSE}), wait_for, viewport, say",
                        text::inert(verb, 30)
                    ),
                ));
                continue;
            }
        };
        match made {
            Some(step) => out.push(step),
            None => problems.push(problem(
                i,
                format!(
                    "{:?}: its argument isn't right (see record run --help)",
                    text::inert(verb, 30)
                ),
            )),
        }
    }
    if problems.is_empty() {
        Ok((source, out))
    } else {
        Err(problems)
    }
}

/// `record run <script.json>`: records a page while driving it through the
/// script; the recording, and each step's answer. A step that fails ends
/// the run; the recording up to it is kept (it may be the bug).
pub fn run(args: &Args) -> Result<Value> {
    let path = media::local_file(&args.plain[0])?;
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > 1024 * 1024 {
        return Err(Error::new("BAD_ARGS", "the script is over 1 MB"));
    }
    let script = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| json::parse(&t).ok())
        .ok_or_else(|| Error::new("BAD_ARGS", "the script isn't JSON"))?;
    let (in_script, steps) = steps(&script).map_err(|problems| {
        let mut e = Error::new(
            "BAD_ARGS",
            format!("the script has {} problem(s)", problems.len()),
        )
        .hint("Fix each of problems; pepper-agent record run --help shows a script.");
        e.problems = problems;
        e
    })?;
    let source = args.value("source").map(str::to_owned).or(in_script);
    let mut start = Value::obj();
    if let Some(name) = &source {
        start.set("source", name.as_str());
    }
    if let Some(max) = args.value("max") {
        let max: f64 = max
            .parse()
            .map_err(|_| Error::new("BAD_ARGS", "--max: seconds"))?;
        start.set("max", max);
    }
    let (_, started) = ask(args, "record.start", start, Duration::from_secs(10))?;
    let name = started.get_str("source").unwrap_or("").to_owned();
    std::thread::sleep(LEAD);
    let mut done = Vec::new();
    let mut failed = Value::Null;
    for (i, step) in steps.iter().enumerate() {
        let result = match step {
            Step::Pause(s) => {
                std::thread::sleep(Duration::from_secs_f64(*s));
                Ok(Value::obj().with("waited", *s))
            }
            Step::Say(text) => ask(
                args,
                "say",
                Value::obj().with("text", text.as_str()),
                Duration::from_secs(10),
            )
            .map(|(_, v)| v),
            Step::Web(cmd, step_args) => {
                let mut request = step_args.clone();
                request.set("source", name.as_str());
                ask(args, cmd, request, Duration::from_secs(70)).map(|(_, v)| v)
            }
        };
        match result {
            Ok(v) => done.push(v),
            Err(e) => {
                failed = Value::obj().with("step", i).with(
                    "error",
                    e.to_json().get("error").cloned().unwrap_or(Value::Null),
                );
                break;
            }
        }
    }
    std::thread::sleep(LEAD);
    let mut answer = stop(args, Some(&name))?;
    answer.set("completed", failed.is_null());
    answer.set("steps", done);
    if !failed.is_null() {
        answer.set("failed", failed);
    }
    Ok(answer)
}

/// `compare <a> <b> [--sync]` and `compare off`: each a source's name, or a
/// file or address opened as `before` or `after`.
pub fn compare(args: &Args) -> Result<Value> {
    if args.plain.len() == 1 {
        if args.plain[0] != "off" {
            return Err(Error::new("BAD_ARGS", "compare <a> <b>, or compare off"));
        }
        let (viewer, v) = ask(
            args,
            "compare",
            Value::obj().with("off", true),
            Duration::from_secs(10),
        )?;
        return Ok(with_viewer(&viewer, v));
    }
    let (_, listed) = ask(args, "sources", Value::obj(), Duration::from_secs(10))?;
    let names: Vec<String> = listed
        .get("sources")
        .and_then(Value::arr)
        .unwrap_or(&[])
        .iter()
        .filter_map(|s| s.get_str("name").map(str::to_owned))
        .collect();
    let mut pair = Vec::new();
    for (given, role) in args.plain.iter().zip(["before", "after"]) {
        if names.iter().any(|n| n == given) {
            pair.push(given.clone());
            continue;
        }
        let (cmd, mut request) = match crate::open_target(given)? {
            Target::Media(t) => ("open", Value::obj().with("target", t).with("kind", "media")),
            Target::Stream(t) => (
                "open",
                Value::obj().with("target", t).with("kind", "stream"),
            ),
            Target::Web(t) => (
                "web.open",
                Value::obj().with("target", t).with("kind", "web"),
            ),
        };
        request.set("name", role);
        ask(args, cmd, request, Duration::from_secs(60))?;
        pair.push(role.to_owned());
    }
    let request = Value::obj()
        .with("a", pair[0].as_str())
        .with("b", pair[1].as_str())
        .with("sync", args.flag("sync"));
    let (viewer, mut v) = ask(args, "compare", request, Duration::from_secs(10))?;
    v.set(
        "next",
        "Tell the human what to compare (say), then wait: pepper-agent events --wait 300 --until approve,reject,done --since <last_id>.",
    );
    Ok(with_viewer(&viewer, v))
}

/// `play <recording>`: shows a file as a recording, playing.
pub fn play(args: &Args) -> Result<Value> {
    let file: PathBuf = media::local_file(&args.plain[0])?;
    let name = args.value("name").map(str::to_owned).unwrap_or_else(|| {
        let stem = file
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let stem = files::name_part(&stem);
        if crate::viewer::valid_name(&stem) {
            stem
        } else {
            "recording".into()
        }
    });
    let request = Value::obj()
        .with("target", file.display().to_string())
        .with("kind", "recording")
        .with("name", name)
        .with("play", true);
    let (viewer, v) = ask(args, "open", request, Duration::from_secs(60))?;
    Ok(with_viewer(&viewer, v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts() {
        let script = json::parse(
            r##"{"source": "app", "steps": [
                {"click": "#start"}, {"type": ["#email", "a@b.co"]},
                {"type": {"selector": "#name", "text": "Ada", "clear": true}},
                {"press": "Enter"}, {"scroll": 600}, {"scroll": "bottom"}, {"wait": 0.5},
                {"wait_for": ".toast"}, {"goto": "localhost:5173/cart"}, {"say": "done"}, {"back": true}
            ]}"##,
        )
        .unwrap();
        let (source, list) = steps(&script).unwrap();
        assert_eq!(source.as_deref(), Some("app"));
        assert_eq!(list.len(), 11);
        assert_eq!(
            list[0],
            Step::Web("web.click", Value::obj().with("target", "#start"))
        );
        assert_eq!(
            list[4],
            Step::Web("web.scroll", Value::obj().with("to", "600"))
        );
        assert_eq!(list[6], Step::Pause(0.5));
        assert_eq!(
            list[8],
            Step::Web(
                "web.goto",
                Value::obj().with("url", "http://localhost:5173/cart")
            )
        );
        // Every problem at once, each with its step.
        let bad = json::parse(
            r#"[{"click": 3}, {"fly": "away"}, {"wait": 99}, "x", {"click": "a", "press": "b"}]"#,
        )
        .unwrap();
        let problems = steps(&bad).unwrap_err();
        let ats: Vec<&str> = problems.iter().map(|p| p.get_str("at").unwrap()).collect();
        assert_eq!(
            ats,
            ["steps[0]", "steps[1]", "steps[2]", "steps[3]", "steps[4]"]
        );
        assert!(steps(&json::parse("[]").unwrap()).is_err());
    }
}
