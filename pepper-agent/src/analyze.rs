//! What a file holds besides its pictures, for cutting it: where its shots
//! change (`scenes`), where it's quiet (`silences`), and what's said in it
//! (`transcribe`: whisper.cpp, on this Mac, nothing sent anywhere). Like
//! probe, frames and sheet, they run on the file and keep their answers in
//! `.pepper/cache/<file's fingerprint>/`.
//!
//! A transcript is the file's: data, never instructions. Its words are kept
//! in the shape video-use's transcripts have (`transcript.rs` reads them),
//! so the human's marks on the file say what was being said.

use std::cmp::Reverse;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{Error, Result};
use crate::files;
use crate::json::{self, Value};
use crate::media;
use crate::text;

/// At most this many cuts or silences in an answer.
const MAX_FOUND: usize = 2000;
/// Segments shown in `transcribe`'s answer; all of them are in its file.
const MAX_SHOWN: usize = 300;
/// Languages written without spaces between words: whisper's tokens there
/// can split a character, so a segment is one word.
const NO_SPACES: &[&str] = &["zh", "ja", "yue", "th", "lo", "km", "my", "bo"];
/// The shortest gap in the sound taken for a pause between words, and the
/// longest a word is taken to be.
const PAUSE: f64 = 0.12;
const MAX_WORD: f64 = 1.5;
/// What to download when there's no model (the best quality per size): a
/// revision of it, not whatever's there now, and its SHA-256 to check.
const MODEL_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-large-v3-turbo-q5_0.bin";
const MODEL_SHA256: &str = "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2";

fn round3(t: f64) -> f64 {
    (t * 1000.0).round() / 1000.0
}

fn read_json(path: &Path) -> Option<Value> {
    json::parse(&files::read_plain(path)?).ok()
}

/// A cached answer, or `make`'s, kept.
fn cached(path: &Path, make: impl FnOnce() -> Result<Value>) -> Result<Value> {
    if let Some(v) = read_json(path) {
        return Ok(v);
    }
    let v = make()?;
    let _ = files::write_private(path, v.to_pretty().as_bytes());
    Ok(v)
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

/// What ffmpeg's `metadata=print:file=-` printed: each frame's time and
/// its numeric keys.
fn printed(out: &[u8]) -> Vec<(f64, Vec<(String, f64)>)> {
    let mut frames: Vec<(f64, Vec<(String, f64)>)> = Vec::new();
    for line in String::from_utf8_lossy(out).lines() {
        if line.starts_with("frame:") {
            let t = line
                .split_whitespace()
                .find_map(|w| w.strip_prefix("pts_time:"))
                .and_then(|t| t.parse().ok());
            frames.push((t.unwrap_or(f64::NAN), Vec::new()));
        } else if let (Some((key, value)), Some(frame)) = (line.split_once('='), frames.last_mut())
            && let Ok(value) = value.trim().parse()
        {
            frame.1.push((key.trim().to_owned(), value));
        }
    }
    frames
}

fn key(keys: &[(String, f64)], name: &str) -> Option<f64> {
    keys.iter().find(|(k, _)| k == name).map(|(_, v)| *v)
}

/// The quiet stretches ffmpeg's `silencedetect` printed; one that lasts
/// to the end has no end of its own.
fn quiet_stretches(out: &[u8], duration: f64) -> Vec<(f64, f64)> {
    let mut quiet = Vec::new();
    let mut open = None;
    for (_, keys) in printed(out) {
        if let Some(start) = key(&keys, "lavfi.silence_start") {
            open = Some(start.max(0.0));
        }
        if let (Some(end), Some(start)) = (key(&keys, "lavfi.silence_end"), open) {
            quiet.push((start, end));
            open = None;
        }
    }
    if let Some(start) = open.filter(|&s| duration > s) {
        quiet.push((start, duration));
    }
    quiet
}

/// `scenes`: where the picture cuts to another shot, as ffmpeg's scene
/// score sees it (0 to 1, how much the picture changed), and the scenes
/// between the cuts.
pub fn scenes(work: &Path, file: &Path, threshold: f64) -> Result<Value> {
    if !(threshold > 0.0 && threshold < 1.0) {
        return Err(Error::new(
            "BAD_ARGS",
            "--threshold: more than 0 and less than 1 (0.3 by default; lower finds more cuts)",
        ));
    }
    let probe = media::probe(work, file)?;
    if probe.display.is_none() {
        return Err(Error::new(
            "NOT_PLAYABLE",
            "the file has no picture (sound only)",
        ));
    }
    let duration = probe.duration.unwrap_or(0.0);
    let dir = media::cache_dir(work, &media::fingerprint(file)?)?;
    let name = format!("scenes-{}.json", (threshold * 1000.0).round() as u32);
    let found = cached(&dir.join(name), || {
        let ffmpeg = media::tool("ffmpeg")?;
        // Shots are told apart as well 320 pixels wide, and faster.
        let mut list = args(&["-nostdin", "-v", "error", "-hwaccel", "videotoolbox"]);
        list.extend(media::input(file));
        list.extend(args(&["-map", "0:V:0", "-an", "-sn", "-dn", "-vf"]));
        list.push(format!(
            "scale=320:-2,select='gt(scene,{threshold})',metadata=print:file=-"
        ));
        list.extend(args(&["-f", "null", "-"]));
        let cuts: Vec<Value> = printed(&media::run(&ffmpeg, &list)?)
            .into_iter()
            .filter(|(t, _)| t.is_finite() && *t > 0.0)
            .filter_map(|(t, keys)| {
                let score = key(&keys, "lavfi.scene_score")?;
                Some(
                    Value::obj()
                        .with("time", round3(t))
                        .with("score", round3(score)),
                )
            })
            .collect();
        Ok(Value::obj().with("cuts", cuts))
    })?;
    let cuts = found.get("cuts").and_then(Value::arr).unwrap_or(&[]);
    let mut starts = vec![(0.0, None)];
    starts.extend(
        cuts.iter()
            .take(MAX_FOUND)
            .filter_map(|c| Some((c.get_num("time")?, c.get_num("score")))),
    );
    let scenes: Vec<Value> = starts
        .iter()
        .enumerate()
        .map(|(i, &(start, score))| {
            let end = starts.get(i + 1).map_or(duration.max(start), |s| s.0);
            Value::obj()
                .with("index", i)
                .with("start", round3(start))
                .with("start_text", text::clock(start))
                .with("end", round3(end))
                .with("duration", round3(end - start))
                .with("score", score)
        })
        .collect();
    let looks: Vec<String> = starts
        .iter()
        .take(16)
        .map(|(t, _)| format!("{}", round3(*t)))
        .collect();
    let mut answer = Value::obj()
        .with("file", file.display().to_string())
        .with("threshold", threshold)
        .with("duration", round3(duration))
        .with("cuts", starts.len() - 1)
        .with("scenes", scenes);
    if cuts.len() > MAX_FOUND {
        answer.set("truncated", true);
    }
    answer.set(
        "next",
        format!(
            "See each scene's first frame: pepper-agent frames <file> --at {}. Too many cuts (flashes, fast motion): a higher --threshold; missed ones: lower.",
            looks.join(",")
        ),
    );
    Ok(answer)
}

/// `silences`: where the sound stays under `db` for `min` seconds or more,
/// and the stretches with sound between them.
pub fn silences(work: &Path, file: &Path, db: f64, min: f64) -> Result<Value> {
    if !(db.is_finite() && (-100.0..0.0).contains(&db)) {
        return Err(Error::new(
            "BAD_ARGS",
            "--db: a level under 0, like -35 (-45 counts only deeper quiet; -25 counts soft sound as quiet too)",
        ));
    }
    if !(min.is_finite() && min > 0.0 && min <= 600.0) {
        return Err(Error::new(
            "BAD_ARGS",
            "--min: seconds, like 0.5 (the shortest quiet that counts)",
        ));
    }
    let probe = media::probe(work, file)?;
    if probe
        .json
        .get("audio")
        .and_then(Value::arr)
        .is_none_or(<[Value]>::is_empty)
    {
        return Err(Error::new("NOT_PLAYABLE", "the file has no sound"));
    }
    let duration = probe.duration.unwrap_or(0.0);
    let dir = media::cache_dir(work, &media::fingerprint(file)?)?;
    let name = format!(
        "silences-{}-{}.json",
        (db * 10.0).round() as i32,
        (min * 1000.0).round() as u32
    );
    let found = cached(&dir.join(name), || {
        let ffmpeg = media::tool("ffmpeg")?;
        let mut list = args(&["-nostdin", "-v", "error"]);
        list.extend(media::input(file));
        list.extend(args(&["-map", "0:a:0", "-vn", "-sn", "-dn", "-af"]));
        list.push(format!(
            "silencedetect=noise={db}dB:d={min},ametadata=print:file=-"
        ));
        list.extend(args(&["-f", "null", "-"]));
        let quiet: Vec<Value> = quiet_stretches(&media::run(&ffmpeg, &list)?, duration)
            .into_iter()
            .map(|(a, b)| Value::obj().with("start", round3(a)).with("end", round3(b)))
            .collect();
        Ok(Value::obj().with("silences", quiet))
    })?;
    let quiet: Vec<(f64, f64)> = found
        .get("silences")
        .and_then(Value::arr)
        .unwrap_or(&[])
        .iter()
        .take(MAX_FOUND)
        .filter_map(|s| Some((s.get_num("start")?, s.get_num("end")?)))
        .collect();
    let stretch = |(a, b): (f64, f64)| {
        Value::obj()
            .with("start", round3(a))
            .with("end", round3(b))
            .with("duration", round3(b - a))
    };
    let mut sound = Vec::new();
    let mut from = 0.0;
    for &(a, b) in &quiet {
        if a - from > 0.01 {
            sound.push(stretch((from, a)));
        }
        from = b;
    }
    if duration - from > 0.01 {
        sound.push(stretch((from, duration)));
    }
    let total: f64 = quiet.iter().map(|(a, b)| b - a).sum();
    Ok(Value::obj()
        .with("file", file.display().to_string())
        .with("db", db)
        .with("min", min)
        .with("duration", round3(duration))
        .with("silent_total", round3(total))
        .with(
            "silences",
            quiet.iter().map(|&s| stretch(s)).collect::<Vec<_>>(),
        )
        .with("sound", sound)
        .with(
            "next",
            "To cut the quiet out, make each `sound` stretch a clip of an edit project (pepper-agent schema edit), with about 0.1 s kept either side.",
        ))
}

pub struct TranscribeAsk<'a> {
    pub lang: &'a str,
    pub model: Option<&'a str>,
}

/// Where whisper models are looked for.
pub fn models_dir() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    // whisper-cli runs in `/`: a model's path must hold on its own.
    home.is_absolute()
        .then(|| home.join(".cache/pepper-agent/whisper"))
}

/// The model `transcribe` uses without `--model`, if there's one.
pub fn model_found() -> Option<PathBuf> {
    model(None, "auto").ok()
}

/// The model to use: `--model`, PEPPER_WHISPER_MODEL, or the largest in
/// [`models_dir`] (one for every language first, unless `lang` is `en`).
fn model(given: Option<&str>, lang: &str) -> Result<PathBuf> {
    let given = given
        .map(str::to_owned)
        .or_else(|| std::env::var("PEPPER_WHISPER_MODEL").ok())
        .filter(|s| !s.is_empty());
    if let Some(given) = given {
        return media::local_file(&given);
    }
    let dir = models_dir().unwrap_or_default();
    let mut found: Vec<(bool, u64, PathBuf)> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let path = e.ok()?.path();
            let name = path.file_name()?.to_str()?.to_owned();
            let size = path.metadata().ok().filter(|m| m.is_file())?.len();
            // Silero's voice detector is a ggml file too, not a model.
            (name.starts_with("ggml-") && name.ends_with(".bin") && !name.contains("silero"))
                .then(|| (name.ends_with(".en.bin"), size, path))
        })
        .collect();
    found.sort_by_key(|(english, size, _)| (lang != "en" && *english, Reverse(*size)));
    found.into_iter().next().map(|(_, _, p)| p).ok_or_else(|| {
        let shown = dir.display();
        Error::new(
            "MISSING_DEP",
            format!("no whisper model in {shown} (ggml-*.bin)"),
        )
        .hint(format!(
            "Ask the human first (about 550 MB), then: mkdir -p {shown} && curl -L -o {shown}/ggml-large-v3-turbo-q5_0.bin {MODEL_URL} && echo '{MODEL_SHA256}  {shown}/ggml-large-v3-turbo-q5_0.bin' | shasum -a 256 -c (ggml-base.bin, 148 MB, is faster and rougher). Or pass --model PATH."
        ))
    })
}

/// `transcribe`: what's said in a file, as segments and words with their
/// times, in a JSON file and an SRT beside it.
pub fn transcribe(work: &Path, file: &Path, ask: &TranscribeAsk) -> Result<Value> {
    let lang = ask.lang.to_ascii_lowercase();
    if !(lang == "auto"
        || ((2..=3).contains(&lang.len()) && lang.chars().all(|c| c.is_ascii_lowercase())))
    {
        return Err(Error::new(
            "BAD_ARGS",
            "--lang: auto, or a language's code, like en, fr or ja",
        ));
    }
    let probe = media::probe(work, file)?;
    if probe
        .json
        .get("audio")
        .and_then(Value::arr)
        .is_none_or(<[Value]>::is_empty)
    {
        return Err(Error::new("NOT_PLAYABLE", "the file has no sound"));
    }
    let whisper = media::which("whisper-cli").ok_or_else(|| {
        Error::new(
            "MISSING_DEP",
            "whisper.cpp isn't installed (whisper-cli, or not on the PATH)",
        )
        .hint("Install it: brew install whisper.cpp. It also needs a model: run transcribe again after, it says which.")
    })?;
    let model = model(ask.model, &lang)?;
    let model_name = model
        .file_stem()
        .map(|s| files::name_part(&s.to_string_lossy()))
        .unwrap_or_default();
    let dir = media::cache_dir(work, &media::fingerprint(file)?)?;
    let stem = format!("transcript-{model_name}-{lang}");
    let (json_path, srt_path) = (
        dir.join(format!("{stem}.json")),
        dir.join(format!("{stem}.srt")),
    );
    let transcript = match read_json(&json_path).filter(|_| files::is_plain(&srt_path)) {
        Some(t) => t,
        None => {
            let duration = probe.duration.unwrap_or(0.0);
            let (raw, pauses) = run_whisper(&whisper, file, &model, &lang, &dir, duration)?;
            let (language, mut segments) = parse_whisper(&raw, &lang);
            place_words(&mut segments, &pauses);
            let t = transcript_json(&language, &segments)
                .with("file", file.display().to_string())
                .with("model", model_name.as_str())
                .with("duration", probe.duration.map(round3));
            let fail = |e: std::io::Error| Error::new("FAILED", format!("{}: {e}", dir.display()));
            files::write_private(&srt_path, srt(&segments).as_bytes()).map_err(fail)?;
            files::write_private(&json_path, t.to_pretty().as_bytes()).map_err(fail)?;
            t
        }
    };
    let segments = transcript
        .get("segments")
        .and_then(Value::arr)
        .unwrap_or(&[]);
    let words = transcript
        .get("words")
        .and_then(Value::arr)
        .map_or(0, <[Value]>::len);
    let mut answer = Value::obj();
    for key in ["file", "language", "model", "duration"] {
        answer.set(key, transcript.get(key).cloned().unwrap_or(Value::Null));
    }
    answer.set("segment_count", segments.len());
    answer.set("word_count", words);
    answer.set(
        "segments",
        segments.iter().take(MAX_SHOWN).cloned().collect::<Vec<_>>(),
    );
    if segments.len() > MAX_SHOWN {
        answer.set("segments_shown", MAX_SHOWN);
    }
    answer.set("json", json_path.display().to_string());
    answer.set("srt", srt_path.display().to_string());
    answer.set(
        "next",
        "Every word with its time is in the json file (start and end in seconds of the file). Cut between words, in a pause; marks the human makes on this file now say what was being said there.",
    );
    Ok(answer)
}

/// The name whisper.cpp gives a model's alignment heads (`-dtw`), from its
/// file's: `ggml-large-v3-turbo-q5_0.bin` is `large.v3.turbo`.
fn dtw_preset(model: &Path) -> Option<String> {
    let stem = model.file_stem()?.to_str()?.strip_prefix("ggml-")?;
    let stem = match stem.rsplit_once("-q") {
        Some((name, quant)) if quant.chars().all(|c| c.is_ascii_digit() || c == '_') => name,
        _ => stem,
    };
    let preset = stem.replace('-', ".");
    [
        "tiny",
        "tiny.en",
        "base",
        "base.en",
        "small",
        "small.en",
        "medium",
        "medium.en",
        "large.v1",
        "large.v2",
        "large.v3",
        "large.v3.turbo",
    ]
    .contains(&preset.as_str())
    .then_some(preset)
}

/// Runs whisper.cpp on the file's sound, telling progress on stderr, and
/// returns its full JSON and the pauses in the sound.
fn run_whisper(
    whisper: &Path,
    file: &Path,
    model: &Path,
    lang: &str,
    dir: &Path,
    duration: f64,
) -> Result<(Value, Vec<(f64, f64)>)> {
    let ffmpeg = media::tool("ffmpeg")?;
    let id = std::process::id();
    let wav = dir.join(format!("audio-16k-{id}.wav"));
    let raw = dir.join(format!("whisper-{id}"));
    let raw_json = raw.with_extension("json");
    for path in [&wav, &raw_json] {
        files::make_way(path)
            .map_err(|e| Error::new("FAILED", format!("{}: {e}", path.display())))?;
    }
    // 16 kHz mono, what whisper reads.
    let mut list = args(&["-nostdin", "-v", "error"]);
    list.extend(media::input(file));
    list.extend(args(&[
        "-map",
        "0:a:0",
        "-vn",
        "-sn",
        "-dn",
        "-ac",
        "1",
        "-ar",
        "16000",
        "-c:a",
        "pcm_s16le",
        "-f",
        "wav",
        "-y",
    ]));
    list.push(media::file_arg(&wav));
    if let Err(e) = media::run(&ffmpeg, &list) {
        let _ = std::fs::remove_file(&wav);
        return Err(e);
    }
    let mut list = args(&["-nostdin", "-v", "error"]);
    list.extend(media::input(&wav));
    list.push("-af".into());
    list.push(format!(
        "silencedetect=noise=-35dB:d={PAUSE},ametadata=print:file=-"
    ));
    list.extend(args(&["-f", "null", "-"]));
    let pauses = media::run(&ffmpeg, &list)
        .map(|out| quiet_stretches(&out, duration))
        .unwrap_or_default();
    let child = whisper_command(whisper, model, lang, &raw, &wav)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            let _ = std::fs::remove_file(&wav);
            return Err(Error::new("FAILED", format!("cannot run whisper-cli: {e}")));
        }
    };
    let mut last = Vec::new();
    if let Some(err) = child.stderr.take() {
        for line in BufReader::new(err)
            .lines()
            .map_while(std::result::Result::ok)
        {
            if let Some(percent) = line
                .split_once("progress =")
                .and_then(|(_, p)| p.trim().trim_end_matches('%').parse::<u32>().ok())
            {
                let progress = Value::obj().with(
                    "progress",
                    Value::obj()
                        .with("step", "transcribe")
                        .with("done", percent)
                        .with("of", 100),
                );
                eprintln!("{}", progress.to_line());
            } else if !line.trim().is_empty() {
                last.push(line);
                if last.len() > 3 {
                    last.remove(0);
                }
            }
        }
    }
    let status = child.wait();
    let _ = std::fs::remove_file(&wav);
    let mut text = Vec::new();
    let read = std::fs::File::open(&raw_json).and_then(|mut f| f.read_to_end(&mut text));
    let _ = std::fs::remove_file(&raw_json);
    if !status.is_ok_and(|s| s.success()) || read.is_err() {
        return Err(Error::new(
            "FAILED",
            format!(
                "whisper.cpp failed: {}",
                text::inert(&last.join(" | "), 400)
            ),
        ));
    }
    let raw = json::parse(&String::from_utf8_lossy(&text))
        .map_err(|e| Error::new("FAILED", format!("whisper.cpp's output: {e}")))?;
    Ok((raw, pauses))
}

/// whisper-cli's command for `wav`, its JSON written as `raw`.json: in `/`,
/// not the project, since whisper.cpp's ggml loads its backends from the
/// folder it runs in too; so it's given absolute paths only.
fn whisper_command(whisper: &Path, model: &Path, lang: &str, raw: &Path, wav: &Path) -> Command {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    let mut command = media::command(whisper);
    command.arg("-m").arg(model).args([
        "-l",
        lang,
        "-t",
        &threads.to_string(),
        "-ojf",
        "-np",
        "-pp",
    ]);
    // The alignment that times words well needs flash attention off.
    if let Some(preset) = dtw_preset(model) {
        command.args(["-nfa", "-dtw", &preset]);
    }
    command.arg("-of").arg(raw).arg("-f").arg(wav);
    command
}

#[derive(Clone, Debug, PartialEq)]
struct Word {
    text: String,
    start: f64,
    end: f64,
    /// The least likely of its tokens, as whisper rated them.
    p: f64,
    event: bool,
    /// Where whisper's alignment (DTW) has its last token.
    aligned: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
struct Segment {
    start: f64,
    end: f64,
    text: String,
    words: Vec<Word>,
}

/// `[Music]`, `(laughs)` or `♪`: a sound, not words.
fn is_event(text: &str) -> bool {
    (text.starts_with('[') && text.ends_with(']'))
        || (text.starts_with('(') && text.ends_with(')'))
        || text.chars().all(|c| c == '♪' || c == ' ')
}

/// whisper.cpp's `-ojf` JSON as segments of words. A token that starts with
/// a space starts a word, the others go on the one before; punctuation
/// joins its word without moving its times (whisper times it late).
fn parse_whisper(raw: &Value, asked: &str) -> (String, Vec<Segment>) {
    let language = raw
        .get("result")
        .and_then(|r| r.get_str("language"))
        .filter(|l| (2..=3).contains(&l.len()) && l.chars().all(|c| c.is_ascii_lowercase()))
        .unwrap_or(asked)
        .to_owned();
    let no_spaces = NO_SPACES.contains(&language.as_str());
    let ms = |v: Option<&Value>, key: &str| v.and_then(|o| o.get_num(key)).map(|n| n / 1000.0);
    // In hundredths of a second, -1 without the alignment.
    let dtw = |tok: &Value| {
        tok.get_num("t_dtw")
            .filter(|t| *t >= 0.0)
            .map(|t| t / 100.0)
    };
    let mut segments = Vec::new();
    for seg in raw.get("transcription").and_then(Value::arr).unwrap_or(&[]) {
        let text = text::typed(seg.get_str("text").unwrap_or("").trim(), 2000);
        if text.is_empty() || text == "[BLANK_AUDIO]" {
            continue;
        }
        let (Some(from), Some(to)) = (ms(seg.get("offsets"), "from"), ms(seg.get("offsets"), "to"))
        else {
            continue;
        };
        let mut words: Vec<Word> = Vec::new();
        if is_event(&text) || no_spaces {
            let event = is_event(&text);
            let shown = if event {
                text.trim_matches(['[', ']', '(', ')', ' '])
            } else {
                &text
            };
            words.push(Word {
                text: text::typed(shown, 200),
                start: from,
                end: to.max(from),
                p: 1.0,
                event,
                aligned: None,
            });
        } else {
            let mut pending = String::new();
            for tok in seg.get("tokens").and_then(Value::arr).unwrap_or(&[]) {
                let raw_text = tok.get_str("text").unwrap_or("");
                let piece = text::typed(raw_text.trim(), 80).replace('\u{FFFD}', "");
                if piece.is_empty() || (raw_text.starts_with("[_") && raw_text.ends_with(']')) {
                    continue;
                }
                let (Some(a), Some(b)) =
                    (ms(tok.get("offsets"), "from"), ms(tok.get("offsets"), "to"))
                else {
                    continue;
                };
                let p = tok.get_num("p").unwrap_or(1.0);
                if !piece.chars().any(char::is_alphanumeric) {
                    let opening = raw_text.starts_with(' ')
                        && piece.starts_with(['"', '\'', '(', '[', '¿', '¡', '«', '“', '‘']);
                    match words.last_mut() {
                        Some(w) if !opening => w.text.push_str(&piece),
                        _ => pending.push_str(&piece),
                    }
                    continue;
                }
                match words.last_mut() {
                    Some(w) if !raw_text.starts_with(' ') && pending.is_empty() => {
                        w.text.push_str(&piece);
                        w.end = b.max(w.start);
                        w.p = w.p.min(p);
                        w.aligned = dtw(tok).or(w.aligned);
                    }
                    _ => {
                        words.push(Word {
                            text: format!("{pending}{piece}"),
                            start: a,
                            end: b.max(a),
                            p,
                            event: false,
                            aligned: dtw(tok),
                        });
                        pending.clear();
                    }
                }
            }
        }
        let start = words.first().map_or(from, |w| w.start);
        let end = words.last().map_or(to, |w| w.end).max(start);
        segments.push(Segment {
            start,
            end,
            text,
            words,
        });
    }
    (language, segments)
}

/// Puts each word where it was said. whisper's alignment (DTW) tells well
/// where a word ends; its token times less so (the word before a long pause
/// can land after it). So a word runs from the end of the one before, or
/// of a pause between them, to its aligned end, and stops where a pause
/// begins; words aligned to one time share it by their length. Without the
/// alignment, the token times are kept, cut at pauses the same way.
fn place_words(segments: &mut [Segment], pauses: &[(f64, f64)]) {
    let mut words: Vec<&mut Word> = segments
        .iter_mut()
        .flat_map(|s| s.words.iter_mut())
        .filter(|w| !w.event)
        .collect();
    let aligned = words.iter().all(|w| w.aligned.is_some());
    let mut before: Option<f64> = None;
    for w in words.iter_mut() {
        let (mut start, mut end) = (w.start, w.end);
        if aligned {
            end = w.aligned.unwrap_or(end).max(before.unwrap_or(0.0));
            start = before.unwrap_or(start).max(end - MAX_WORD).min(end);
        }
        if let Some(&(_, b)) = pauses.iter().rev().find(|&&(_, b)| b > start && b < end) {
            start = b;
        }
        if let Some(&(a, _)) = pauses
            .iter()
            .find(|&&(a, b)| a > start && a < end && b >= end)
        {
            end = a;
        }
        w.start = start;
        w.end = end.max(start);
        before = Some(w.end);
    }
    let mut i = 0;
    while i < words.len() {
        let mut j = i + 1;
        while j < words.len()
            && words[j].end - words[j].start < 0.01
            && (words[j].start - words[j - 1].end).abs() < 0.01
        {
            j += 1;
        }
        if j > i + 1 {
            let (from, to) = (words[i].start, words[j - 1].end);
            let length = |w: &Word| w.text.chars().count().max(1) as f64;
            let total: f64 = words[i..j].iter().map(|w| length(w)).sum();
            let mut at = from;
            for w in words[i..j].iter_mut() {
                w.start = at;
                at += (to - from) * length(w) / total;
                w.end = at;
            }
        }
        i = j;
    }
    for s in segments.iter_mut() {
        if let (Some(first), Some(last)) = (s.words.first(), s.words.last()) {
            s.start = first.start;
            s.end = last.end.max(first.start);
        }
    }
}

/// The transcript as `transcribe` keeps it: segments, and words in the
/// shape `transcript::Words` reads.
fn transcript_json(language: &str, segments: &[Segment]) -> Value {
    let segs: Vec<Value> = segments
        .iter()
        .map(|s| {
            Value::obj()
                .with("start", round3(s.start))
                .with("end", round3(s.end))
                .with("text", s.text.as_str())
        })
        .collect();
    let words: Vec<Value> = segments
        .iter()
        .flat_map(|s| &s.words)
        .map(|w| {
            Value::obj()
                .with("text", w.text.as_str())
                .with("start", round3(w.start))
                .with("end", round3(w.end))
                .with("type", if w.event { "audio_event" } else { "word" })
                .with("p", (w.p * 100.0).round() / 100.0)
        })
        .collect();
    Value::obj()
        .with("pepper_transcript", 1)
        .with("language", language)
        .with("segments", segs)
        .with("words", words)
}

/// `00:01:02,500`.
fn srt_time(t: f64) -> String {
    let ms = (t.max(0.0) * 1000.0).round() as u64;
    format!(
        "{:02}:{:02}:{:02},{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

fn srt(segments: &[Segment]) -> String {
    let mut out = String::new();
    for (i, s) in segments.iter().enumerate() {
        out += &format!(
            "{}\n{} --> {}\n{}\n\n",
            i + 1,
            srt_time(s.start),
            srt_time(s.end),
            s.text
        );
    }
    out
}

/// The newest transcript `transcribe` made of `src` in this folder's
/// `.pepper/`, if there is one. Only a plain file has one: a pipe's FIFO
/// isn't opened (that would wait for a writer, and take its frames). As
/// for the cache's other readers, nothing through a link: its folders are
/// the user's own, the transcript a plain file.
pub fn transcript_of(src: &Path) -> Option<PathBuf> {
    if !std::fs::metadata(src).is_ok_and(|m| m.is_file()) {
        return None;
    }
    let mut dir = std::env::current_dir().ok()?;
    for part in [".pepper", "cache", &media::fingerprint(src).ok()?] {
        dir.push(part);
        if !files::is_own_dir(&dir) {
            return None;
        }
    }
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| Some(e.ok()?.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("transcript-") && n.ends_with(".json"))
                && files::is_plain(p)
        })
        .max_by_key(|p| std::fs::symlink_metadata(p).and_then(|m| m.modified()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_metadata() {
        let out = b"frame:0    pts:38400   pts_time:3\nlavfi.scene_score=0.557025\n\
                    frame:1    pts:64000   pts_time:5\nlavfi.scene_score=0.5\nlavfi.other=x\n";
        let frames = printed(out);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].0, 3.0);
        assert_eq!(key(&frames[0].1, "lavfi.scene_score"), Some(0.557025));
        // Keys that aren't numbers are left out.
        assert_eq!(frames[1].1.len(), 1);
    }

    /// whisper.cpp's -ojf shape, cut down: special tokens, punctuation
    /// timed late, a word in two tokens, an escape sequence.
    const WHISPER: &str = r#"{"result": {"language": "en"}, "transcription": [
        {"offsets": {"from": 0, "to": 4640}, "text": " Hello there, this is transcription.",
         "tokens": [
            {"text": "[_BEG_]", "offsets": {"from": 0, "to": 0}, "p": 0.99},
            {"text": " Hello", "offsets": {"from": 50, "to": 400}, "p": 0.97},
            {"text": " there", "offsets": {"from": 400, "to": 620}, "p": 0.97},
            {"text": ",", "offsets": {"from": 900, "to": 960}, "p": 0.84},
            {"text": " \"trans", "offsets": {"from": 960, "to": 1500}, "p": 0.9},
            {"text": "cription\u001b[2J", "offsets": {"from": 1500, "to": 2700}, "p": 0.6},
            {"text": ".\"", "offsets": {"from": 4420, "to": 4640}, "p": 0.9},
            {"text": "[_TT_232]", "offsets": {"from": 4640, "to": 4640}, "p": 0.01}
         ]},
        {"offsets": {"from": 4640, "to": 6000}, "text": " [BLANK_AUDIO]", "tokens": []},
        {"offsets": {"from": 6000, "to": 7000}, "text": " [Music]", "tokens": []}
    ]}"#;

    #[test]
    fn whisper_words() {
        let raw = json::parse(WHISPER).unwrap();
        let (language, segments) = parse_whisper(&raw, "auto");
        assert_eq!(language, "en");
        assert_eq!(segments.len(), 2);
        let words: Vec<(&str, f64, f64)> = segments[0]
            .words
            .iter()
            .map(|w| (w.text.as_str(), w.start, w.end))
            .collect();
        assert_eq!(
            words,
            [
                ("Hello", 0.05, 0.4),
                ("there,", 0.4, 0.62),
                // Its punctuation, timed in the pause after it, doesn't
                // stretch it; the escape is dropped.
                ("\"transcription[2J.\"", 0.96, 2.7),
            ]
        );
        assert_eq!(segments[0].words[2].p, 0.6);
        assert_eq!((segments[0].start, segments[0].end), (0.05, 2.7));
        // A sound is an event, without its brackets.
        let music = &segments[1].words[0];
        assert!(music.event && music.text == "Music");

        // Kept in the shape transcript.rs reads, so marks say what's said.
        let kept = transcript_json(&language, &segments);
        let words = crate::transcript::Words::parse(&kept).unwrap();
        let at = words.at(0.5).unwrap();
        assert_eq!(
            at.get_str("phrase"),
            Some("Hello there, \"transcription[2J.\"")
        );
        assert_eq!(
            at.get("word").and_then(|w| w.get_str("text")),
            Some("there,")
        );
        assert_eq!(words.at(6.5).unwrap().get_str("phrase"), Some("(Music)"));

        let text = srt(&segments);
        assert!(text.starts_with("1\n00:00:00,050 --> 00:00:02,700\nHello there,"));
        assert!(text.contains("2\n00:00:06,000 --> 00:00:07,000\n[Music]\n\n"));
        assert_eq!(srt_time(3725.5), "01:02:05,500");
    }

    /// What whisper said of `say`'s "Hello there. This is a test of the
    /// transcription. (1.5 s) After a pause, we fixed the bug, and the…",
    /// and the pauses ffmpeg heard: its token times put "transcription"
    /// after the long pause; the alignment knows better.
    #[test]
    fn words_where_they_were_said() {
        let word = |text: &str, start: f64, end: f64, aligned: f64| Word {
            text: text.into(),
            start,
            end,
            p: 1.0,
            event: false,
            aligned: Some(aligned),
        };
        let mut segments = vec![
            Segment {
                start: 0.05,
                end: 4.19,
                text: String::new(),
                words: vec![
                    word("Hello", 0.05, 0.51, 0.24),
                    word("there,", 0.51, 1.02, 0.60),
                    word("this", 1.23, 1.54, 1.04),
                    word("the", 2.54, 2.69, 2.60),
                    word("transcription.", 2.69, 4.19, 2.60),
                ],
            },
            Segment {
                start: 4.61,
                end: 6.83,
                text: String::new(),
                words: vec![
                    word("After", 4.61, 4.91, 4.70),
                    word("pause,", 5.01, 5.18, 5.32),
                    word("we", 5.54, 5.69, 5.62),
                    word("bug", 6.34, 6.53, 6.48),
                    word("and", 6.53, 6.70, 6.78),
                ],
            },
        ];
        let pauses = [(0.648, 0.898), (2.707, 4.419), (5.31, 5.545), (6.476, 6.7)];
        place_words(&mut segments, &pauses);
        let times: Vec<(&str, f64, f64)> = segments
            .iter()
            .flat_map(|s| &s.words)
            .map(|w| (w.text.as_str(), round3(w.start), round3(w.end)))
            .collect();
        assert_eq!(
            times,
            [
                ("Hello", 0.05, 0.24),
                ("there,", 0.24, 0.6),
                // After the pause between them.
                ("this", 0.898, 1.04),
                // Aligned to one time: they share it, by their length (a
                // word is 1.5 s at most).
                ("the", 1.1, 1.365),
                ("transcription.", 1.365, 2.6),
                ("After", 4.419, 4.7),
                // Stops where a pause begins.
                ("pause,", 4.7, 5.31),
                ("we", 5.545, 5.62),
                ("bug", 5.62, 6.476),
                ("and", 6.7, 6.78),
            ]
        );
        assert_eq!((segments[1].start, segments[1].end), (4.419, 6.78));

        // Without the alignment: token times, cut at pauses.
        let mut plain = vec![Segment {
            start: 0.0,
            end: 1.5,
            text: String::new(),
            words: vec![Word {
                aligned: None,
                ..word("long", 0.2, 1.5, 0.0)
            }],
        }];
        place_words(&mut plain, &[(0.9, 2.0)]);
        assert_eq!((plain[0].words[0].start, plain[0].words[0].end), (0.2, 0.9));

        let preset = |name: &str| dtw_preset(Path::new(name));
        assert_eq!(preset("/m/ggml-base.bin").as_deref(), Some("base"));
        assert_eq!(preset("ggml-base.en.bin").as_deref(), Some("base.en"));
        assert_eq!(
            preset("ggml-large-v3-turbo-q5_0.bin").as_deref(),
            Some("large.v3.turbo")
        );
        assert_eq!(preset("ggml-small.en-tdrz.bin"), None);
        assert_eq!(preset("model.bin"), None);
    }

    /// whisper-cli runs in `/`, not the project it's asked from (its ggml
    /// would load a project's libggml-*.so), with absolute paths only.
    #[test]
    fn whisper_runs_outside_the_project() {
        let command = whisper_command(
            Path::new("/opt/homebrew/bin/whisper-cli"),
            Path::new("/m/ggml-base.bin"),
            "auto",
            Path::new("/p/.pepper/cache/k/whisper-1"),
            Path::new("/p/.pepper/cache/k/audio-16k-1.wav"),
        );
        assert_eq!(command.get_current_dir(), Some(Path::new("/")));
        let args: Vec<_> = command.get_args().collect();
        for flag in ["-m", "-of", "-f"] {
            let at = args.iter().position(|a| *a == flag).unwrap();
            assert!(Path::new(args[at + 1]).is_absolute(), "{flag}");
        }
    }

    #[test]
    fn languages_without_spaces() {
        let raw = json::parse(
            r#"{"result": {"language": "ja"}, "transcription": [
                {"offsets": {"from": 1000, "to": 2500}, "text": "こんにちは。", "tokens": [
                    {"text": "こん", "offsets": {"from": 1000, "to": 1500}}]}]}"#,
        )
        .unwrap();
        let (language, segments) = parse_whisper(&raw, "auto");
        assert_eq!(language, "ja");
        assert_eq!(segments[0].words.len(), 1);
        assert_eq!(segments[0].words[0].text, "こんにちは。");
        assert_eq!((segments[0].start, segments[0].end), (1.0, 2.5));
    }
}
