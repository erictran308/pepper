//! pepper-agent: lets AI agents use Pepper, a Ghostty build that shows video
//! inside the terminal. The human runs `pepper-agent view` in a Pepper pane;
//! an agent runs the other commands from its own shell, and each prints one
//! JSON object (errors as JSON on stderr). `pepper-agent guide` is the
//! agent's manual (guide.md).
//!
//! The agent can't see what Pepper draws, and nothing here lets it: it sees
//! the same pictures through files made from the source (`snapshot`,
//! `frames`, `sheet`), and the human answers with events (`events`).

mod error;
mod files;
mod font;
mod json;
mod media;
mod proto;
mod sys;
mod text;
mod viewer;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use error::{Error, Result};
use json::Value;

const GUIDE: &str = include_str!("../guide.md");

struct Spec {
    name: &'static str,
    usage: &'static str,
    about: &'static str,
    example: &'static str,
    /// Options that take a value, and those that don't.
    values: &'static [&'static str],
    flags: &'static [&'static str],
    /// How many plain arguments.
    min: usize,
    max: usize,
}

const SPECS: &[Spec] = &[
    Spec {
        name: "guide",
        usage: "guide",
        about: "Prints the guide for agents: what Pepper is, the viewer, every command, the workflows. Read it first.",
        example: "pepper-agent guide",
        values: &[],
        flags: &[],
        min: 0,
        max: 0,
    },
    Spec {
        name: "status",
        usage: "status",
        about: "Whether this shell is in Pepper, the viewers that run and what they show, what's installed, and what to do next.",
        example: "pepper-agent status",
        values: &[],
        flags: &[],
        min: 0,
        max: 0,
    },
    Spec {
        name: "view",
        usage: "view [--project DIR] [--debug]",
        about: "For the human: makes this Pepper pane the agents' viewer, until q. Agents never start it: they ask the human to.",
        example: "pepper-agent view",
        values: &["project"],
        flags: &["debug"],
        min: 0,
        max: 0,
    },
    Spec {
        name: "open",
        usage: "open <file|url> [--name NAME] [--play] [--kind media]",
        about: "Shows a video, a picture or a video's web address in the viewer, paused on its first frame unless --play. Answers once it shows (30 s at most). Opening a --name again shows the new file in that source's box.",
        example: "pepper-agent open raw/take1.mp4 --name take1",
        values: &["name", "kind"],
        flags: &["play"],
        min: 1,
        max: 1,
    },
    Spec {
        name: "close",
        usage: "close <name>",
        about: "Closes a source and takes its box away.",
        example: "pepper-agent close take1",
        values: &[],
        flags: &[],
        min: 1,
        max: 1,
    },
    Spec {
        name: "sources",
        usage: "sources",
        about: "The viewer's sources: name, status, time, size, decoder.",
        example: "pepper-agent sources",
        values: &[],
        flags: &[],
        min: 0,
        max: 0,
    },
    Spec {
        name: "layout",
        usage: "layout auto|single|split-h|split-v|grid",
        about: "How sources share the pane: auto (as they fit best), single (the focused one), split-h (two side by side), split-v (two stacked), grid (up to four).",
        example: "pepper-agent layout split-h",
        values: &[],
        flags: &[],
        min: 1,
        max: 1,
    },
    Spec {
        name: "focus",
        usage: "focus <name>",
        about: "Focuses a source: the human's keys go to it, and single layout shows it.",
        example: "pepper-agent focus take2",
        values: &[],
        flags: &[],
        min: 1,
        max: 1,
    },
    Spec {
        name: "snapshot",
        usage: "snapshot [<name>] [--region X,Y,W,H] [--max PIXELS]",
        about: "A PNG of the frame a source shows now, made from the source (not the screen), at most 1568 px on its long side. --region crops, in the source's pixels. Files go in .pepper/snapshots/.",
        example: "pepper-agent snapshot take1 --region 0,0,960,540",
        values: &["region", "max"],
        flags: &[],
        min: 0,
        max: 1,
    },
    Spec {
        name: "say",
        usage: "say <text>",
        about: "Shows a short message to the human on the viewer's status line (`say ''` clears it).",
        example: "pepper-agent say \"Watch 0:12-0:18, press m where it jumps, Enter when done\"",
        values: &[],
        flags: &[],
        min: 1,
        max: usize::MAX,
    },
    Spec {
        name: "events",
        usage: "events [--wait SECONDS] [--since ID] [--until TYPE,...] [--type TYPE,...] [--source NAME]",
        about: "What the human did: marks (m), notes (n), done (Enter). --wait waits up to that long for one (3600 at most); --until waits for an event of that type and answers with all since. Pass --since the last_id you got, to get only new ones.",
        example: "pepper-agent events --wait 300 --until done --since 0",
        values: &["wait", "since", "until", "type", "source"],
        flags: &[],
        min: 0,
        max: 0,
    },
    Spec {
        name: "play",
        usage: "play [--source NAME]",
        about: "Plays (from the start again if at the end).",
        example: "pepper-agent play",
        values: &["source"],
        flags: &[],
        min: 0,
        max: 0,
    },
    Spec {
        name: "pause",
        usage: "pause [--source NAME]",
        about: "Pauses.",
        example: "pepper-agent pause --source take1",
        values: &["source"],
        flags: &[],
        min: 0,
        max: 0,
    },
    Spec {
        name: "toggle",
        usage: "toggle [--source NAME]",
        about: "Plays if paused, pauses if playing.",
        example: "pepper-agent toggle",
        values: &["source"],
        flags: &[],
        min: 0,
        max: 0,
    },
    Spec {
        name: "seek",
        usage: "seek <time> [--source NAME]",
        about: "Goes to a time: 83.5, 1:23.5, 00:01:23.500, or +5 / -2 from where it is. Answers once that frame shows.",
        example: "pepper-agent seek 1:23.5",
        values: &["source"],
        flags: &[],
        min: 1,
        max: 1,
    },
    Spec {
        name: "loop",
        usage: "loop <from> <to> | loop off [--source NAME]",
        about: "Plays from..to over and over, or stops looping.",
        example: "pepper-agent loop 12 15.5",
        values: &["source"],
        flags: &[],
        min: 1,
        max: 2,
    },
    Spec {
        name: "where",
        usage: "where [<name>]",
        about: "Where playback is: time, length, paused, status.",
        example: "pepper-agent where take1",
        values: &[],
        flags: &[],
        min: 0,
        max: 1,
    },
    Spec {
        name: "review",
        usage: "review <from> [<to>] [--source NAME]",
        about: "Plays from..to for the human (5 s if no end), then pauses there.",
        example: "pepper-agent review 0:12 0:18",
        values: &["source"],
        flags: &[],
        min: 1,
        max: 2,
    },
    Spec {
        name: "probe",
        usage: "probe <file>",
        about: "What a media file holds: length, streams, codecs, size, fps, keyframe interval, and whether this Mac's graphics chip decodes it. Needs no viewer.",
        example: "pepper-agent probe raw/take1.mp4",
        values: &[],
        flags: &[],
        min: 1,
        max: 1,
    },
    Spec {
        name: "frames",
        usage: "frames <file> (--at T1,T2,... | --every SECONDS [--from T] [--to T]) [--max PIXELS]",
        about: "PNGs of a file's frames at those times (100 at most), from the file. Needs no viewer.",
        example: "pepper-agent frames raw/take1.mp4 --at 12,12.5,1:03",
        values: &["at", "every", "from", "to", "max"],
        flags: &[],
        min: 1,
        max: 1,
    },
    Spec {
        name: "sheet",
        usage: "sheet <file> [--from T] [--to T] [--cols 4] [--rows 4] [--max PIXELS]",
        about: "One contact sheet of a file (or from..to): evenly spaced frames, each with its time on it, and the list of times. The cheapest way to look at footage. Needs no viewer.",
        example: "pepper-agent sheet raw/take1.mp4 --from 1:00 --to 2:00",
        values: &["from", "to", "cols", "rows", "max"],
        flags: &[],
        min: 1,
        max: 1,
    },
];

const GROUPS: &[(&str, &[&str])] = &[
    ("Start here", &["guide", "status"]),
    ("The viewer (the human runs it)", &["view"]),
    (
        "Sources",
        &[
            "open", "close", "sources", "layout", "focus", "snapshot", "say",
        ],
    ),
    ("The human's answers", &["events"]),
    (
        "Playback",
        &["play", "pause", "toggle", "seek", "loop", "where", "review"],
    ),
    ("Footage, from the file", &["probe", "frames", "sheet"]),
];

fn usage() -> String {
    let mut out = String::from(
        "pepper-agent: lets AI agents use Pepper (video inside the terminal). Agents: run `pepper-agent guide` first.\n\n\
         usage: pepper-agent <command> [arguments] [--human] [--viewer ID]\n",
    );
    for (group, names) in GROUPS {
        out += &format!("\n{group}:\n");
        for name in *names {
            if let Some(spec) = SPECS.iter().find(|s| s.name == *name) {
                out += &format!(
                    "  {:<10} {}\n",
                    spec.name,
                    spec.about.split(". ").next().unwrap_or("")
                );
            }
        }
    }
    out += "\nEach command prints one JSON object; errors are JSON on stderr with a code and a hint.\n\
            --human prints them for people. `pepper-agent <command> --help` has an example.\n";
    out
}

fn command_help(spec: &Spec) -> String {
    format!(
        "usage: pepper-agent {} [--human] [--viewer ID]\n\n{}\n\nExample:\n  {}\n",
        spec.usage, spec.about, spec.example
    )
}

/// A command line, parsed against its spec.
#[derive(Debug, Default)]
struct Args {
    plain: Vec<String>,
    values: Vec<(String, String)>,
    flags: Vec<String>,
    human: bool,
    viewer: Option<String>,
    help: bool,
}

impl Args {
    fn value(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }
}

fn parse_args(spec: &Spec, args: &[String]) -> Result<Args> {
    let mut out = Args::default();
    let mut i = 0;
    let mut plain_only = false;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        // A negative number is a time (seek -2), not an option.
        let negative = arg.len() > 1 && arg.starts_with('-') && arg.as_bytes()[1].is_ascii_digit();
        if plain_only || !arg.starts_with('-') || arg == "-" || negative {
            out.plain.push(arg.clone());
            continue;
        }
        if arg == "--" {
            plain_only = true;
            continue;
        }
        if arg == "-h" || arg == "--help" {
            out.help = true;
            continue;
        }
        let Some(option) = arg.strip_prefix("--") else {
            return Err(bad_args(
                spec,
                format!("unknown option {}", text::inert(arg, 40)),
            ));
        };
        let (name, inline) = match option.split_once('=') {
            Some((n, v)) => (n, Some(v.to_owned())),
            None => (option, None),
        };
        if name == "human" && inline.is_none() {
            out.human = true;
        } else if name == "viewer" || spec.values.contains(&name) {
            let value = match inline {
                Some(v) => v,
                None if i < args.len() => {
                    i += 1;
                    args[i - 1].clone()
                }
                None => return Err(bad_args(spec, format!("--{name} needs a value"))),
            };
            if name == "viewer" {
                out.viewer = Some(value);
            } else {
                out.values.push((name.to_owned(), value));
            }
        } else if spec.flags.contains(&name) && inline.is_none() {
            out.flags.push(name.to_owned());
        } else {
            return Err(bad_args(
                spec,
                format!("unknown option --{}", text::inert(name, 40)),
            ));
        }
    }
    if !out.help && (out.plain.len() < spec.min || out.plain.len() > spec.max) {
        return Err(bad_args(
            spec,
            format!("usage: pepper-agent {}", spec.usage),
        ));
    }
    Ok(out)
}

fn bad_args(spec: &Spec, message: String) -> Error {
    Error::new("BAD_ARGS", message).hint(format!(
        "Example: {}  (more: pepper-agent {} --help)",
        spec.example, spec.name
    ))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(name) = args.first() else {
        print!("{}", usage());
        return ExitCode::FAILURE;
    };
    match name.as_str() {
        "-h" | "--help" | "help" => {
            print!("{}", usage());
            return ExitCode::SUCCESS;
        }
        "--version" | "version" => {
            println!("pepper-agent {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    let human_anywhere = args.iter().any(|a| a == "--human");
    let Some(spec) = SPECS.iter().find(|s| s.name == name) else {
        let error = Error::new("BAD_ARGS", format!("no command {}", text::inert(name, 40))).hint(
            "Run `pepper-agent --help` for the commands, `pepper-agent guide` for how to use them.",
        );
        return fail(&error, human_anywhere);
    };
    let args = match parse_args(spec, &args[1..]) {
        Ok(args) => args,
        Err(error) => return fail(&error, human_anywhere),
    };
    if args.help {
        print!("{}", command_help(spec));
        return ExitCode::SUCCESS;
    }
    if spec.name == "guide" {
        print!("{GUIDE}");
        return ExitCode::SUCCESS;
    }
    match run(spec, &args) {
        // The viewer prints nothing when the human quits it.
        Ok(Value::Null) => ExitCode::SUCCESS,
        Ok(value) => {
            if args.human {
                print!("{}", human(&value, 0));
            } else {
                println!("{}", value.to_line());
            }
            ExitCode::SUCCESS
        }
        Err(error) => fail(&error, args.human),
    }
}

fn fail(error: &Error, human_output: bool) -> ExitCode {
    if human_output {
        eprintln!(
            "error ({}): {}",
            error.code,
            text::inert(&error.message, 2000)
        );
        eprintln!("hint: {}", error.hint);
    } else {
        eprintln!("{}", error.to_json().to_line());
    }
    if error.code == "BAD_ARGS" {
        ExitCode::from(2)
    } else {
        ExitCode::FAILURE
    }
}

/// A value as indented lines, for people.
fn human(value: &Value, indent: usize) -> String {
    let pad = " ".repeat(indent);
    let scalar = |v: &Value| match v {
        Value::Null => "-".to_owned(),
        Value::Str(s) => text::inert(s, 2000),
        other => other.to_line(),
    };
    let mut out = String::new();
    match value {
        Value::Obj(members) => {
            for (key, v) in members {
                match v {
                    Value::Obj(m) if !m.is_empty() => {
                        out += &format!("{pad}{key}:\n{}", human(v, indent + 2))
                    }
                    Value::Arr(items) if !items.is_empty() => {
                        out += &format!("{pad}{key}:\n{}", human(v, indent + 2))
                    }
                    Value::Arr(_) => out += &format!("{pad}{key}: (none)\n"),
                    _ => out += &format!("{pad}{key}: {}\n", scalar(v)),
                }
            }
        }
        Value::Arr(items) => {
            for item in items {
                match item {
                    Value::Obj(_) | Value::Arr(_) => {
                        let inner = human(item, indent + 2);
                        out += &format!("{pad}- {}", inner.trim_start());
                    }
                    _ => out += &format!("{pad}- {}\n", scalar(item)),
                }
            }
        }
        _ => out += &format!("{pad}{}\n", scalar(value)),
    }
    out
}

/// Where this command keeps its files: `.pepper/` in the current folder, or
/// pepper-agent's folder in the user's temp if that can't be written.
fn work_dir() -> Result<PathBuf> {
    let here = std::env::current_dir().map_err(|e| Error::new("FAILED", e.to_string()))?;
    match files::pepper_dir(&here, &[]) {
        Ok(dir) => Ok(dir),
        Err(_) => {
            let dir = proto::run_dir()?.join("files");
            files::private_dir(&dir).map_err(|e| Error::new("FAILED", e.to_string()))?;
            Ok(dir)
        }
    }
}

fn run(spec: &Spec, args: &Args) -> Result<Value> {
    match spec.name {
        "status" => status(args),
        "view" => {
            viewer::run(viewer::Options {
                project: args.value("project").map(PathBuf::from),
                debug: args.flag("debug") || std::env::var_os("PEPPER_AGENT_DEBUG").is_some(),
            })?;
            Ok(Value::Null)
        }
        "probe" => {
            let file = media::local_file(&args.plain[0])?;
            Ok(media::probe(&work_dir()?, &file)?.json)
        }
        "frames" => {
            let file = media::local_file(&args.plain[0])?;
            let at = match args.value("at") {
                Some(list) => list
                    .split(',')
                    .map(|t| text::parse_at(t).map_err(|e| Error::new("BAD_TIME", e)))
                    .collect::<Result<Vec<f64>>>()?,
                None => Vec::new(),
            };
            let every = args
                .value("every")
                .map(|s| {
                    s.parse::<f64>()
                        .map_err(|_| Error::new("BAD_TIME", "--every: seconds, like 2 or 0.5"))
                })
                .transpose()?;
            let ask = media::FramesAsk {
                at,
                every,
                from: time_option(args, "from")?,
                to: time_option(args, "to")?,
                max_side: max_side(args)?,
            };
            media::frames(&work_dir()?, &file, &ask)
        }
        "sheet" => {
            let file = media::local_file(&args.plain[0])?;
            let count = |name: &str| -> Result<u32> {
                args.value(name)
                    .map(|v| {
                        v.parse().map_err(|_| {
                            Error::new("BAD_ARGS", format!("--{name}: a number, 1 to 8"))
                        })
                    })
                    .unwrap_or(Ok(4))
            };
            let ask = media::SheetAsk {
                from: time_option(args, "from")?,
                to: time_option(args, "to")?,
                cols: count("cols")?,
                rows: count("rows")?,
                max_side: max_side(args)?,
            };
            media::sheet(&work_dir()?, &file, &ask)
        }
        "snapshot" => snapshot(args),
        _ => to_viewer(spec, args),
    }
}

fn time_option(args: &Args, name: &str) -> Result<Option<f64>> {
    args.value(name)
        .map(|t| text::parse_at(t).map_err(|e| Error::new("BAD_TIME", format!("--{name}: {e}"))))
        .transpose()
}

fn max_side(args: &Args) -> Result<u32> {
    match args.value("max") {
        None => Ok(media::MAX_SIDE),
        Some(v) => v
            .parse::<u32>()
            .ok()
            .filter(|n| (64..=8192).contains(n))
            .ok_or_else(|| Error::new("BAD_ARGS", "--max: pixels on the long side, 64 to 8192")),
    }
}

/// The request a viewer command makes, from its arguments.
fn request_args(spec: &Spec, args: &Args) -> Result<Value> {
    let mut v = Value::obj();
    if let Some(source) = args.value("source") {
        v.set("source", source);
    }
    let first = args.plain.first().map(String::as_str);
    match spec.name {
        "open" => {
            let given = first.unwrap_or("");
            let target = open_target(given)?;
            let kind = args.value("kind").unwrap_or("media");
            if kind != "media" {
                return Err(Error::new(
                    "UNSUPPORTED",
                    format!(
                        "--kind {}: this version shows media only",
                        text::inert(kind, 20)
                    ),
                ));
            }
            v.set("target", target);
            v.set("kind", kind);
            if let Some(name) = args.value("name") {
                v.set("name", name);
            }
            v.set("play", args.flag("play"));
        }
        "close" => v.set("name", first),
        "focus" => v.set("source", first),
        "where" => {
            if let Some(name) = first {
                v.set("source", name);
            }
        }
        "layout" => v.set("mode", first),
        "say" => v.set("text", args.plain.join(" ")),
        "seek" => v.set("to", first),
        "loop" => match (first, args.plain.get(1)) {
            (Some("off"), None) => v.set("off", true),
            (Some(from), Some(to)) => {
                v.set("from", from);
                v.set("to", to.as_str());
            }
            _ => {
                return Err(bad_args(
                    spec,
                    format!("usage: pepper-agent {}", spec.usage),
                ));
            }
        },
        "review" => {
            v.set("from", first);
            if let Some(to) = args.plain.get(1) {
                v.set("to", to.as_str());
            }
        }
        "events" => {
            if let Some(wait) = args.value("wait") {
                let wait: f64 = wait
                    .parse()
                    .ok()
                    .filter(|w: &f64| (0.0..=viewer::MAX_WAIT).contains(w))
                    .ok_or_else(|| Error::new("BAD_ARGS", "--wait: seconds, 0 to 3600"))?;
                v.set("wait", wait);
            }
            if let Some(since) = args.value("since") {
                let since: f64 = since
                    .parse()
                    .ok()
                    .filter(|s: &f64| *s >= 0.0 && s.fract() == 0.0)
                    .ok_or_else(|| {
                        Error::new("BAD_ARGS", "--since: an event id (last_id from before)")
                    })?;
                v.set("since", since);
            }
            for (option, key) in [("type", "types"), ("until", "until")] {
                if let Some(list) = args.value(option) {
                    let items: Vec<Value> =
                        list.split(',').map(|t| Value::from(t.trim())).collect();
                    v.set(key, Value::Arr(items));
                }
            }
        }
        _ => {}
    }
    Ok(v)
}

/// What `open` loads: an absolute path to a file, or a web address.
fn open_target(given: &str) -> Result<String> {
    if let Some((scheme, _)) = given.split_once("://")
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
    {
        let lower = scheme.to_ascii_lowercase();
        return match lower.as_str() {
            "http" | "https" => {
                let host = given[scheme.len() + 3..]
                    .split(['/', '?', '#'])
                    .next()
                    .unwrap_or("")
                    .rsplit('@')
                    .next()
                    .unwrap_or("");
                let host = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
                if ["localhost", "127.0.0.1", "[::1]"].contains(&host) {
                    Err(Error::new(
                        "UNSUPPORTED",
                        "a local web page is a web preview, which this version doesn't have yet",
                    )
                    .hint("For a video file served locally, open the file itself."))
                } else {
                    Ok(given.to_owned())
                }
            }
            "file" => media::local_file(&given[7..]).map(|p| p.display().to_string()),
            _ => Err(Error::new(
                "UNSUPPORTED",
                format!(
                    "{}:// addresses aren't shown yet: files and http(s) only",
                    text::inert(scheme, 20)
                ),
            )),
        };
    }
    media::local_file(given).map(|p| p.display().to_string())
}

/// How long to wait for a viewer's answer.
fn answer_wait(spec: &Spec, args: &Args) -> Duration {
    let base = match spec.name {
        "open" => 45,
        "seek" | "loop" | "review" => 20,
        _ => 10,
    };
    let wait = args
        .value("wait")
        .and_then(|w| w.parse::<f64>().ok())
        .unwrap_or(0.0);
    Duration::from_secs(base) + Duration::from_secs_f64(wait.clamp(0.0, viewer::MAX_WAIT))
}

fn to_viewer(spec: &Spec, args: &Args) -> Result<Value> {
    let request = request_args(spec, args)?;
    let dir = proto::run_dir()?;
    let viewer = proto::pick(&dir, args.viewer.as_deref())?;
    let mut answer = proto::request(&dir, &viewer, spec.name, request, answer_wait(spec, args))?;
    if let Value::Obj(members) = &mut answer {
        members.insert(0, ("viewer".into(), Value::from(viewer.id.as_str())));
    }
    Ok(answer)
}

fn snapshot(args: &Args) -> Result<Value> {
    let region = match args.value("region") {
        Some(text) => {
            let parts: Vec<u32> = text
                .split(',')
                .map(|p| p.trim().parse::<u32>())
                .collect::<std::result::Result<_, _>>()
                .map_err(|_| Error::new("BAD_ARGS", "--region: X,Y,W,H in the source's pixels"))?;
            let [x, y, w, h] = parts[..] else {
                return Err(Error::new(
                    "BAD_ARGS",
                    "--region: X,Y,W,H in the source's pixels",
                ));
            };
            if w == 0 || h == 0 {
                return Err(Error::new(
                    "BAD_ARGS",
                    "--region: a width and height of 1 or more",
                ));
            }
            Some([x, y, w, h])
        }
        None => None,
    };
    let max = max_side(args)?;
    let mut request = Value::obj();
    if let Some(name) = args.plain.first() {
        request.set("source", name.as_str());
    }
    let dir = proto::run_dir()?;
    let viewer = proto::pick(&dir, args.viewer.as_deref())?;
    let shot = proto::request(&dir, &viewer, "snapshot", request, Duration::from_secs(20))?;
    let raw = PathBuf::from(shot.get_str("raw").unwrap_or(""));
    // Only a file in this user's own folder for the viewers.
    if !raw.starts_with(&dir) || !raw.is_file() {
        return Err(Error::new(
            "FAILED",
            "the viewer's snapshot isn't where it should be",
        ));
    }
    let name = shot.get_str("source").unwrap_or("source");
    let stamp: String = sys::iso_time(sys::now())
        .chars()
        .filter(char::is_ascii_digit)
        .take(14)
        .collect();
    let out_dir = work_dir()?.join("snapshots");
    files::private_dir(&out_dir).map_err(|e| Error::new("FAILED", e.to_string()))?;
    let at = shot
        .get_num("time_pos")
        .map(|t| format!("-{}ms", (t * 1000.0).round() as u64))
        .unwrap_or_default();
    let mut out = out_dir.join(format!("{}-{stamp}{at}.png", files::name_part(name)));
    for k in 2.. {
        if !out.exists() {
            break;
        }
        out = out_dir.join(format!("{}-{stamp}{at}-{k}.png", files::name_part(name)));
    }
    let made = media::shrink_png(&raw, &out, region, max);
    let _ = std::fs::remove_file(&raw);
    let media::Shrunk {
        size: (w, h),
        full: (sw, sh),
        region,
    } = made?;
    let mut answer = Value::obj()
        .with("viewer", viewer.id.as_str())
        .with("path", out.display().to_string())
        .with("width", w)
        .with("height", h)
        .with("source_width", sw)
        .with("source_height", sh);
    if let Some(r) = region {
        answer.set(
            "region",
            r.iter().map(|&n| Value::from(n)).collect::<Vec<_>>(),
        );
    }
    for key in [
        "source",
        "kind",
        "file",
        "time_pos",
        "time_text",
        "duration",
        "paused",
    ] {
        if let Some(v) = shot.get(key) {
            answer.set(key, v.clone());
        }
    }
    Ok(answer)
}

/// What's installed, for `status`.
fn deps() -> Value {
    let found = |path: Option<PathBuf>, install: &str| match path {
        Some(p) => Value::obj()
            .with("ok", true)
            .with("path", p.display().to_string()),
        None => Value::obj().with("ok", false).with("install", install),
    };
    let libmpv = std::env::var_os("PEPPER_LIBMPV")
        .map(PathBuf::from)
        .into_iter()
        .chain(
            [
                "/opt/homebrew/lib/libmpv.2.dylib",
                "/opt/local/lib/libmpv.2.dylib",
                "/usr/local/lib/libmpv.2.dylib",
            ]
            .map(PathBuf::from),
        )
        .find(|p| p.is_file());
    Value::obj()
        .with(
            "ffmpeg",
            found(media::which("ffmpeg"), "brew install ffmpeg"),
        )
        .with(
            "ffprobe",
            found(media::which("ffprobe"), "brew install ffmpeg"),
        )
        .with(
            "pepper_play",
            found(
                viewer::find_play(),
                "comes with Pepper: run this in a Pepper tab",
            ),
        )
        .with("libmpv", found(libmpv, "brew install mpv"))
        .with(
            "yt_dlp",
            found(
                media::which("yt-dlp"),
                "brew install yt-dlp (only for web addresses)",
            ),
        )
}

fn status(args: &Args) -> Result<Value> {
    let port = std::env::var("PEPPER_PORT").ok().filter(|p| !p.is_empty());
    let tmux = std::env::var_os("TMUX").is_some();
    let in_pepper = port.is_some() && !tmux;
    let dir = proto::run_dir()?;
    let all = proto::viewers(&dir);
    let listed: Vec<Value> = all
        .iter()
        .map(|v| {
            Value::obj()
                .with("id", v.id.as_str())
                .with("pid", v.pid)
                .with("started", v.record.get_str("started"))
                .with("project", v.record.get_str("project"))
                .with("same_pepper", port.is_some() && v.pepper_port == port)
                .with(
                    "sources",
                    v.record
                        .get("sources")
                        .cloned()
                        .unwrap_or(Value::Arr(Vec::new())),
                )
        })
        .collect();
    let picked = proto::pick(&dir, args.viewer.as_deref());
    let viewer = match &picked {
        Ok(v) => proto::request(&dir, v, "status", Value::obj(), Duration::from_secs(5)).ok(),
        Err(_) => None,
    };
    let sources = viewer
        .as_ref()
        .and_then(|v| v.get("sources"))
        .and_then(Value::arr)
        .unwrap_or(&[]);
    let hwdec: Vec<Value> = sources
        .iter()
        .filter_map(|s| {
            Some(
                Value::obj()
                    .with("source", s.get_str("name")?)
                    .with("hwdec", s.get_str("hwdec").unwrap_or("no")),
            )
        })
        .collect();
    let deps = deps();
    let missing: Vec<&str> = ["ffmpeg", "pepper_play", "libmpv"]
        .into_iter()
        .filter(|d| deps.get(d).and_then(|v| v.get_bool("ok")) != Some(true))
        .collect();
    let next = match (&viewer, sources.is_empty()) {
        (None, _) if !in_pepper && !tmux && port.is_none() => {
            "No viewer, and this shell isn't in Pepper. Ask the user to open a split in Pepper and run: \
             pepper-agent view. probe, frames and sheet work without one."
                .to_owned()
        }
        (None, _) => format!("No viewer. {}", error::NO_VIEWER_HINT),
        (Some(_), true) => {
            "The viewer is empty: pepper-agent open <file>, then pepper-agent say what to look for."
                .to_owned()
        }
        (Some(v), false) => format!(
            "Waiting for the human? pepper-agent events --wait 300 --since {} --until done",
            v.get_num("last_event_id").unwrap_or(0.0)
        ),
    };
    let next = if missing.is_empty() {
        next
    } else {
        format!(
            "Missing {}: see deps for how to install. {next}",
            missing.join(", ")
        )
    };
    Ok(Value::obj()
        .with("in_pepper", in_pepper)
        .with("pepper_port", port.is_some())
        .with("tmux", tmux)
        .with("viewer", viewer.unwrap_or(Value::Null))
        .with("viewers", listed)
        .with("socket_dir", dir.display().to_string())
        .with("hwdec", hwdec)
        .with("deps", deps)
        .with("next", next))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> &'static Spec {
        SPECS.iter().find(|s| s.name == name).unwrap()
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn every_command_has_help_and_a_group() {
        for s in SPECS {
            assert!(
                s.example.starts_with(&format!("pepper-agent {}", s.name)),
                "{}",
                s.name
            );
            assert!(
                GROUPS.iter().any(|(_, names)| names.contains(&s.name)),
                "{}",
                s.name
            );
        }
        assert!(
            usage()
                .lines()
                .next()
                .unwrap()
                .contains("pepper-agent guide")
        );
        for (_, names) in GROUPS {
            for name in *names {
                assert!(SPECS.iter().any(|s| s.name == *name), "{name}");
            }
        }
        let viewer_side: Vec<&str> = SPECS
            .iter()
            .map(|s| s.name)
            .filter(|n| !["guide", "view", "probe", "frames", "sheet", "snapshot"].contains(n))
            .collect();
        for name in viewer_side {
            assert!(proto::COMMANDS.contains(&name), "{name}");
        }
    }

    #[test]
    fn command_lines() {
        let a = parse_args(
            spec("open"),
            &strings(&["a.mp4", "--name=x", "--play", "--viewer", "123"]),
        )
        .unwrap();
        assert_eq!(a.plain, ["a.mp4"]);
        assert_eq!(a.value("name"), Some("x"));
        assert!(a.flag("play"));
        assert_eq!(a.viewer.as_deref(), Some("123"));
        let a = parse_args(spec("seek"), &strings(&["-2", "--source", "clip"])).unwrap();
        assert_eq!(a.plain, ["-2"]);
        assert_eq!(a.value("source"), Some("clip"));
        let a = parse_args(spec("say"), &strings(&["watch", "this", "--human"])).unwrap();
        assert_eq!(a.plain.join(" "), "watch this");
        assert!(a.human);
        let a = parse_args(spec("open"), &strings(&["--", "--odd.mp4"])).unwrap();
        assert_eq!(a.plain, ["--odd.mp4"]);
        let code =
            |name: &str, args: &[&str]| parse_args(spec(name), &strings(args)).unwrap_err().code;
        assert_eq!(code("open", &[]), "BAD_ARGS");
        assert_eq!(code("open", &["a", "b"]), "BAD_ARGS");
        assert_eq!(code("open", &["a", "--nope"]), "BAD_ARGS");
        assert_eq!(code("open", &["a", "--play=yes"]), "BAD_ARGS");
        assert_eq!(code("events", &["--wait"]), "BAD_ARGS");
        assert!(
            parse_args(spec("open"), &strings(&["--help"]))
                .unwrap()
                .help
        );
    }

    #[test]
    fn requests() {
        let req = |name: &str, args: &[&str]| {
            let s = spec(name);
            request_args(s, &parse_args(s, &strings(args)).unwrap())
        };
        assert_eq!(req("loop", &["off"]).unwrap().get_bool("off"), Some(true));
        assert_eq!(req("loop", &["1", "2"]).unwrap().get_str("to"), Some("2"));
        assert_eq!(req("loop", &["1"]).unwrap_err().code, "BAD_ARGS");
        let e = req(
            "events",
            &[
                "--wait",
                "5",
                "--since",
                "3",
                "--until",
                "done",
                "--type",
                "mark,note",
            ],
        )
        .unwrap();
        assert_eq!(e.get_num("wait"), Some(5.0));
        assert_eq!(e.get_num("since"), Some(3.0));
        assert_eq!(
            e.get("types").and_then(Value::arr).map(|a| a.len()),
            Some(2)
        );
        assert_eq!(
            req("events", &["--wait", "9999"]).unwrap_err().code,
            "BAD_ARGS"
        );
        assert_eq!(
            req("events", &["--since", "-1"]).unwrap_err().code,
            "BAD_ARGS"
        );
        assert_eq!(
            req("say", &["a", "b"]).unwrap().get_str("text"),
            Some("a b")
        );
    }

    #[test]
    fn open_targets() {
        assert_eq!(
            open_target("https://example.com/v?x=1").unwrap(),
            "https://example.com/v?x=1"
        );
        assert_eq!(
            open_target("http://localhost:3000").unwrap_err().code,
            "UNSUPPORTED"
        );
        assert_eq!(
            open_target("http://u:p@127.0.0.1/").unwrap_err().code,
            "UNSUPPORTED"
        );
        assert_eq!(open_target("rtsp://cam/x").unwrap_err().code, "UNSUPPORTED");
        assert_eq!(open_target("av://lavfi:x").unwrap_err().code, "UNSUPPORTED");
        assert_eq!(
            open_target("no-such-file.mp4").unwrap_err().code,
            "NOT_FOUND"
        );
        let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
        assert_eq!(open_target(manifest).unwrap(), manifest);
        assert_eq!(
            open_target(&format!("file://{manifest}")).unwrap(),
            manifest
        );
    }

    #[test]
    fn people_read_it() {
        let v = Value::obj()
            .with("a", 1u32)
            .with("b", Value::obj().with("c", "x\x1b"))
            .with("list", vec![Value::obj().with("d", true)])
            .with("none", Value::Arr(Vec::new()));
        assert_eq!(
            human(&v, 0),
            "a: 1\nb:\n  c: x\\u{1b}\nlist:\n  - d: true\nnone: (none)\n"
        );
    }
}
