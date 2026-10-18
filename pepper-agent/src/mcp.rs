//! `pepper-agent mcp`: every command as an MCP tool over stdio, for agents
//! that take tools rather than a shell (modelcontextprotocol.io). A thin
//! wrapper: each call runs pepper-agent itself with the call's arguments as
//! its command line and answers with what that printed, the JSON as text,
//! and the pictures it names (snapshots, frames, sheets, the crops of what
//! the human pointed at) as images too: those pepper-agent made, where it
//! writes them. It decides nothing the command doesn't.
//!
//! Messages are JSON-RPC, one per line. Calls run side by side, each in a
//! process of its own, so `events --wait` holds up nothing else; a call the
//! client cancels, or every call when the client goes, is stopped.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::error::{Error, Result};
use crate::json::{self, Value};
use crate::{SPECS, Spec, text};

/// The protocol's versions this speaks, newest first.
const VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// Commands that aren't tools: the human's viewer, and setting agents up.
const NOT_TOOLS: &[&str] = &["view", "mcp", "install-skill"];
const MAX_LINE: usize = 4 * 1024 * 1024;
const MAX_OUTPUT: u64 = 16 * 1024 * 1024;
/// A line a command says on stderr, at most.
const MAX_ERR_LINE: usize = 64 * 1024;
/// Calls that run at once, at most: each is a process.
const MAX_CALLS: usize = 32;
/// Pictures sent back with one answer, and their size; more are paths only.
const MAX_IMAGES: usize = 8;
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
const INSTRUCTIONS: &str = "Pepper shows the human video, live pages of your web app and live output in a viewer pane they run, and hands back what they mark and point at. Call `guide` first (the manual: workflows, events, errors), then `status`. You can't see the screen: `snapshot`, `web_shot`, `frames` and `sheet` are your view, and come back as images. Once you have a session (`session_new`), pass its token as `session` on every call.";

struct Server {
    exe: PathBuf,
    out: Mutex<std::io::Stdout>,
    /// Calls that run, by a number of the server's own (two may come with
    /// the same id): their request id (as JSON) and process group.
    running: Mutex<HashMap<u64, (String, u32)>>,
    cancelled: Mutex<HashSet<u64>>,
    next: AtomicU64,
    calls: AtomicUsize,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Server {
    fn send(&self, message: Value) {
        let mut out = lock(&self.out);
        let _ = writeln!(out, "{}", message.with("jsonrpc", "2.0").to_line());
        let _ = out.flush();
    }

    fn reply(&self, id: &Value, result: Value) {
        self.send(Value::obj().with("id", id.clone()).with("result", result));
    }

    fn fail(&self, id: &Value, code: i32, message: &str) {
        let error = Value::obj().with("code", code).with("message", message);
        self.send(Value::obj().with("id", id.clone()).with("error", error));
    }
}

pub fn serve() -> Result<()> {
    let exe = std::env::current_exe()
        .map_err(|e| Error::new("FAILED", format!("where pepper-agent is: {e}")))?;
    let server = Arc::new(Server {
        exe,
        out: Mutex::new(std::io::stdout()),
        running: Mutex::new(HashMap::new()),
        cancelled: Mutex::new(HashSet::new()),
        next: AtomicU64::new(1),
        calls: AtomicUsize::new(0),
    });
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    loop {
        let line = match crate::proto::read_line(&mut reader, MAX_LINE) {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(e) => {
                // A line too long is read on to its end; one that isn't
                // UTF-8 was read whole already.
                if e.to_string() == crate::proto::TOO_LONG {
                    let _ = reader.skip_until(b'\n');
                }
                server.fail(&Value::Null, -32700, "a message too long, or not UTF-8");
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        match json::parse(&line) {
            Ok(message) => handle(&server, &message),
            Err(e) => server.fail(&Value::Null, -32700, &format!("not JSON: {e}")),
        }
    }
    // The client is gone: stop what still runs.
    for (_, pgid) in lock(&server.running).values() {
        crate::sys::terminate_group(*pgid);
    }
    Ok(())
}

fn handle(server: &Arc<Server>, message: &Value) {
    let Some(method) = message.get_str("method") else {
        // A client's answer to a request: this server sends none.
        return;
    };
    let params = message.get("params").cloned().unwrap_or_else(Value::obj);
    let Some(id) = message.get("id").filter(|id| !id.is_null()).cloned() else {
        if method == "notifications/cancelled"
            && let Some(request) = params.get("requestId")
        {
            cancel(server, request);
        }
        return;
    };
    match method {
        "initialize" => server.reply(&id, initialize(&params)),
        "ping" => server.reply(&id, Value::obj()),
        "tools/list" => server.reply(&id, Value::obj().with("tools", tools())),
        "tools/call" => {
            let server = server.clone();
            std::thread::spawn(move || call(&server, &id, &params));
        }
        _ => server.fail(
            &id,
            -32601,
            &format!("no method {}", text::inert(method, 60)),
        ),
    }
}

fn initialize(params: &Value) -> Value {
    let asked = params.get_str("protocolVersion").unwrap_or("");
    let version = VERSIONS
        .iter()
        .find(|v| **v == asked)
        .unwrap_or(&VERSIONS[0]);
    Value::obj()
        .with("protocolVersion", *version)
        .with("capabilities", Value::obj().with("tools", Value::obj()))
        .with(
            "serverInfo",
            Value::obj()
                .with("name", "pepper-agent")
                .with("title", "Pepper")
                .with("version", env!("CARGO_PKG_VERSION")),
        )
        .with("instructions", INSTRUCTIONS)
}

/// `web wait-for` is `web_wait_for`.
fn tool_name(spec: &Spec) -> String {
    spec.name.replace([' ', '-'], "_")
}

fn tool_specs() -> impl Iterator<Item = &'static Spec> {
    SPECS.iter().filter(|s| !NOT_TOOLS.contains(&s.name))
}

fn tools() -> Vec<Value> {
    tool_specs()
        .map(|spec| {
            Value::obj()
                .with("name", tool_name(spec))
                .with("title", format!("pepper-agent {}", spec.name))
                .with(
                    "description",
                    format!(
                        "{}\n\nUsage: pepper-agent {}\nExample: {}",
                        spec.about, spec.usage, spec.example
                    ),
                )
                .with("inputSchema", input_schema(spec))
        })
        .collect()
}

/// The plain arguments a usage line names, as (property, how it's
/// written): `<file|url>` is `file`, a choice like `auto|single|grid` is
/// `choice`. An option's value is skipped, and what follows a lone `|` is
/// another form of the command (`loop off`).
fn positionals(spec: &Spec) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut value_next = false;
    for word in spec
        .usage
        .split_whitespace()
        .skip(spec.name.split(' ').count())
    {
        if std::mem::take(&mut value_next) {
            continue;
        }
        if word == "|" {
            break;
        }
        let inner = word
            .trim_start_matches(['[', '('])
            .trim_end_matches([']', ')']);
        if inner.starts_with('-') {
            value_next = !word.ends_with([']', ')']);
        } else if let Some(arg) = inner.strip_prefix('<').and_then(|w| w.strip_suffix('>')) {
            let name: String = arg
                .split(['|', '.'])
                .next()
                .unwrap_or("arg")
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            out.push((name, arg.to_owned()));
        } else if inner.contains('|') {
            out.push(("choice".to_owned(), inner.to_owned()));
        }
    }
    out
}

/// What the usage line says an option takes: `--at T1,T2,...` is `T1,T2,...`.
fn option_hint(spec: &Spec, option: &str) -> String {
    let flag = if option == "output" {
        "-o".to_owned()
    } else {
        format!("--{option}")
    };
    let words: Vec<&str> = spec.usage.split_whitespace().collect();
    words
        .windows(2)
        .find(|w| w[0].trim_start_matches(['[', '(']) == flag)
        .map(|w| w[1].trim_end_matches([']', ')']).to_owned())
        .unwrap_or_default()
}

fn input_schema(spec: &Spec) -> Value {
    let mut properties = Value::obj();
    let mut required = Vec::new();
    for (i, (name, written)) in positionals(spec).iter().enumerate() {
        let mut p = Value::obj()
            .with("type", vec!["string", "number"])
            .with("description", format!("<{written}>"));
        if name == "choice" {
            p = Value::obj()
                .with("type", "string")
                .with("enum", written.split('|').collect::<Vec<_>>());
        }
        properties.set(name, p);
        if i < spec.min {
            required.push(name.clone());
        }
    }
    for option in spec.values {
        let hint = option_hint(spec, option);
        properties.set(
            option,
            Value::obj()
                .with("type", vec!["string", "number"])
                .with("description", format!("--{option} {hint}").trim_end()),
        );
    }
    for flag in spec.flags {
        properties.set(
            flag,
            Value::obj()
                .with("type", "boolean")
                .with("description", format!("--{flag}")),
        );
    }
    properties.set(
        "viewer",
        Value::obj().with("type", "string").with(
            "description",
            "Which viewer, when several run (status lists them).",
        ),
    );
    properties.set(
        "session",
        Value::obj().with("type", "string").with(
            "description",
            "Your session's token (from session_new): pass it on every call once you have one.",
        ),
    );
    properties.set(
        "force",
        Value::obj()
            .with("type", "boolean")
            .with("description", "Drive a source another session owns anyway."),
    );
    Value::obj()
        .with("type", "object")
        .with("properties", properties)
        .with("required", required)
        .with("additionalProperties", false)
}

/// A call's arguments as the command line: the command's words, options as
/// `--name=value`, then `--` and the plain arguments, so none is taken for
/// an option.
fn command_line(spec: &Spec, arguments: &Value) -> Result<Vec<String>> {
    let bad = |message: String| {
        Error::new("BAD_ARGS", message).hint(format!(
            "The tool's description has its usage and an example: pepper-agent {}",
            spec.usage
        ))
    };
    let members = match arguments {
        Value::Obj(members) => members.as_slice(),
        Value::Null => &[],
        _ => return Err(bad("the arguments: an object".into())),
    };
    let positionals = positionals(spec);
    let mut words: Vec<String> = spec.name.split(' ').map(str::to_owned).collect();
    let mut plain: Vec<Option<String>> = vec![None; positionals.len()];
    for (key, value) in members {
        let shown = text::inert(key, 40);
        let as_text = || match value {
            Value::Str(s) => Ok(s.clone()),
            Value::Num(_) => Ok(value.to_line()),
            _ => Err(bad(format!("{shown}: text or a number"))),
        };
        if value.is_null() {
            continue;
        }
        if let Some(i) = positionals.iter().position(|(n, _)| n == key) {
            plain[i] = Some(as_text()?);
        } else if spec.values.contains(&key.as_str()) || key == "viewer" || key == "session" {
            words.push(format!("--{key}={}", as_text()?));
        } else if spec.flags.contains(&key.as_str()) || key == "force" {
            match value {
                Value::Bool(true) => words.push(format!("--{key}")),
                Value::Bool(false) => {}
                _ => return Err(bad(format!("{shown}: true or false"))),
            }
        } else {
            return Err(bad(format!("{} takes no {shown}", tool_name(spec))));
        }
    }
    words.push("--".into());
    let given = plain.iter().rposition(Option::is_some).map_or(0, |i| i + 1);
    for (i, p) in plain.into_iter().take(given).enumerate() {
        match p {
            Some(p) => words.push(p),
            None => {
                return Err(bad(format!(
                    "{} comes before {}",
                    positionals[i].0,
                    positionals[given - 1].0
                )));
            }
        }
    }
    Ok(words)
}

fn tool_error(text: &str) -> Value {
    Value::obj()
        .with(
            "content",
            vec![Value::obj().with("type", "text").with("text", text)],
        )
        .with("isError", true)
}

fn call(server: &Server, id: &Value, params: &Value) {
    let name = params.get_str("name").unwrap_or("");
    let Some(spec) = tool_specs().find(|s| tool_name(s) == name) else {
        return server.fail(id, -32602, &format!("no tool {}", text::inert(name, 60)));
    };
    let words = match command_line(spec, params.get("arguments").unwrap_or(&Value::Null)) {
        Ok(words) => words,
        Err(e) => return server.reply(id, tool_error(&e.to_json().to_line())),
    };
    let key = id.to_line();
    let token = params.get("_meta").and_then(|m| m.get("progressToken"));
    if server.calls.fetch_add(1, Ordering::Relaxed) >= MAX_CALLS {
        server.calls.fetch_sub(1, Ordering::Relaxed);
        let text = format!("{MAX_CALLS} calls run already: wait for one to end");
        return server.reply(id, tool_error(&text));
    }
    let n = server.next.fetch_add(1, Ordering::Relaxed);
    let result = run(server, n, &key, &words, token);
    server.calls.fetch_sub(1, Ordering::Relaxed);
    if lock(&server.cancelled).remove(&n) {
        return;
    }
    server.reply(id, result);
}

/// Runs the command, telling its progress if the client asked, and makes
/// its answer the call's result.
fn run(server: &Server, n: u64, key: &str, words: &[String], token: Option<&Value>) -> Value {
    let child = Command::new(&server.exe)
        .args(words)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own group, so stopping it stops the ffmpeg it runs too.
        .process_group(0)
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => return tool_error(&format!("cannot run pepper-agent: {e}")),
    };
    lock(&server.running).insert(n, (key.to_owned(), child.id()));
    let stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        if let Some(stdout) = stdout {
            let _ = stdout.take(MAX_OUTPUT).read_to_end(&mut out);
        }
        out
    });
    let mut error = None;
    let mut other: Vec<String> = Vec::new();
    if let Some(stderr) = child.stderr.take() {
        let mut stderr = BufReader::new(stderr);
        loop {
            let line = match crate::proto::read_line(&mut stderr, MAX_ERR_LINE) {
                Ok(Some(line)) => line,
                Ok(None) => break,
                // Too long (the rest of it goes) or not UTF-8: not a line of
                // pepper-agent's.
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                    if e.to_string() == crate::proto::TOO_LONG {
                        let _ = stderr.skip_until(b'\n');
                    }
                    continue;
                }
                Err(_) => break,
            };
            match json::parse(&line) {
                Ok(v) if v.get("progress").is_some() => {
                    if let (Some(token), Some(p)) = (token, v.get("progress")) {
                        let params = Value::obj()
                            .with("progressToken", token.clone())
                            .with("progress", p.get_num("done").unwrap_or(0.0))
                            .with("total", p.get_num("of"))
                            .with("message", p.get_str("step"));
                        server.send(
                            Value::obj()
                                .with("method", "notifications/progress")
                                .with("params", params),
                        );
                    }
                }
                Ok(v) if v.get("error").is_some() => error = Some(line),
                _ => {
                    other.push(text::inert(&line, 400));
                    if other.len() > 5 {
                        other.remove(0);
                    }
                }
            }
        }
    }
    let out = reader.join().unwrap_or_default();
    // Out of the list before it's reaped: a cancel from now on finds
    // nothing, not a group that may be another's.
    lock(&server.running).remove(&n);
    let status = child.wait();
    if !status.is_ok_and(|s| s.success()) {
        let text = error.unwrap_or_else(|| match other.is_empty() {
            true => "the command failed".to_owned(),
            false => other.join("\n"),
        });
        return tool_error(&text);
    }
    let text = String::from_utf8_lossy(&out).trim_end().to_owned();
    let mut content = vec![
        Value::obj()
            .with("type", "text")
            .with("text", text.as_str()),
    ];
    if let Ok(answer) = json::parse(&text) {
        content.extend(attached(&answer, &picture_roots()));
    }
    Value::obj().with("content", content).with("isError", false)
}

/// Where pepper-agent writes pictures, real paths: the work folder's
/// `.pepper/`, each viewer's project's (points' pictures), its run folder,
/// and the viewers' folder for what they write (points' pictures without a
/// project), each a folder of the user's, not a link.
fn picture_roots() -> Vec<PathBuf> {
    let own = |dir: &Path| {
        std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir() && m.uid() == crate::sys::uid())
    };
    let mut roots = Vec::new();
    if let Ok(here) = std::env::current_dir() {
        let top = here.join(".pepper");
        if own(&top) {
            roots.extend(std::fs::canonicalize(&top));
        }
    }
    if let Ok(run) = crate::proto::run_dir() {
        for viewer in crate::proto::viewers(&run) {
            if let Some(project) = viewer.record.get_str("project") {
                let top = Path::new(project).join(".pepper");
                if own(&top) {
                    roots.extend(std::fs::canonicalize(&top));
                }
            }
        }
        roots.extend(std::fs::canonicalize(run));
    }
    if let Some(outputs) = crate::proto::outputs_path()
        && own(&outputs)
    {
        roots.extend(std::fs::canonicalize(outputs));
    }
    roots
}

/// The pictures an answer names that are pepper-agent's (under `roots`),
/// as images, MAX_IMAGES at most.
fn attached(answer: &Value, roots: &[PathBuf]) -> Vec<Value> {
    let mut found = Vec::new();
    pictures(answer, &mut found);
    found
        .iter()
        .filter_map(|p| image(p, roots))
        .take(MAX_IMAGES)
        .collect()
}

/// The pictures an answer names, as its `path`, `crop` or `sheet`; never in
/// a `value`, which is the page's (`web eval`'s), keys and all.
fn pictures(v: &Value, found: &mut Vec<PathBuf>) {
    match v {
        Value::Obj(members) => {
            for (key, value) in members {
                match value {
                    _ if key == "value" => {}
                    Value::Str(s)
                        if matches!(key.as_str(), "path" | "crop" | "sheet")
                            && [".png", ".jpg", ".jpeg"]
                                .iter()
                                .any(|e| s.to_ascii_lowercase().ends_with(e)) =>
                    {
                        found.push(PathBuf::from(s));
                    }
                    _ => pictures(value, found),
                }
            }
        }
        Value::Arr(items) => items.iter().for_each(|item| pictures(item, found)),
        _ => {}
    }
}

/// A picture as an image, if it's one of pepper-agent's: its real path
/// (links followed) under one of `roots`, a PNG or JPEG, not too large.
fn image(path: &Path, roots: &[PathBuf]) -> Option<Value> {
    let real = std::fs::canonicalize(path).ok()?;
    if !roots.iter().any(|root| real.starts_with(root)) {
        return None;
    }
    // Not waiting on a FIFO named like a picture, nor following a link.
    const O_NONBLOCK: i32 = 0x0004;
    const O_NOFOLLOW: i32 = 0x0100;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK | O_NOFOLLOW)
        .open(&real)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_BYTES).read_to_end(&mut bytes).ok()?;
    let mime = if bytes.starts_with(b"\x89PNG") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else {
        return None;
    };
    Some(
        Value::obj()
            .with("type", "image")
            .with("data", base64(&bytes))
            .with("mimeType", mime),
    )
}

fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let byte = |i: usize| u32::from(chunk.get(i).copied().unwrap_or(0));
        let n = byte(0) << 16 | byte(1) << 8 | byte(2);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Stops the calls with that request id (the client's), if they still run.
fn cancel(server: &Server, request: &Value) {
    let key = request.to_line();
    for (n, (k, pgid)) in lock(&server.running).iter() {
        if *k == key {
            lock(&server.cancelled).insert(*n);
            crate::sys::terminate_group(*pgid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> &'static Spec {
        SPECS.iter().find(|s| s.name == name).unwrap()
    }

    fn args(text: &str) -> Value {
        json::parse(text).unwrap()
    }

    #[test]
    fn every_command_is_a_tool() {
        let tools = tools();
        assert_eq!(tools.len(), SPECS.len() - NOT_TOOLS.len());
        let mut names = HashSet::new();
        for tool in &tools {
            let name = tool.get_str("name").unwrap();
            assert!(
                name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "{name}"
            );
            assert!(names.insert(name.to_owned()), "{name} twice");
            let schema = tool.get("inputSchema").unwrap();
            let Some(Value::Obj(props)) = schema.get("properties") else {
                panic!("{name}");
            };
            // No argument is named twice (a plain one and an option).
            let mut keys = HashSet::new();
            for (key, _) in props {
                assert!(keys.insert(key), "{name}: {key} twice");
            }
        }
        assert!(names.contains("web_wait_for") && names.contains("pipe_new"));
        assert!(!names.contains("view") && !names.contains("mcp"));
    }

    #[test]
    fn usage_lines_name_the_arguments() {
        let names = |n: &str| -> Vec<String> {
            positionals(spec(n))
                .into_iter()
                .map(|(name, _)| name)
                .collect()
        };
        assert_eq!(names("open"), ["file"]);
        assert_eq!(names("loop"), ["from", "to"]);
        assert_eq!(names("web type"), ["selector", "text"]);
        assert_eq!(names("edit render"), ["project"]);
        assert_eq!(names("frames"), ["file"]);
        assert_eq!(names("layout"), ["choice"]);
        assert_eq!(names("schema"), ["choice"]);
        assert_eq!(names("compare"), ["a", "b"]);
        assert!(names("events").is_empty() && names("web viewport") == ["device"]);
        assert_eq!(option_hint(spec("frames"), "at"), "T1,T2,...");
        assert_eq!(option_hint(spec("edit render"), "output"), "OUT.mp4");
        assert_eq!(option_hint(spec("silences"), "db"), "-35");
    }

    #[test]
    fn calls_as_command_lines() {
        let line = |name: &str, a: &str| command_line(spec(name), &args(a));
        assert_eq!(
            line(
                "open",
                r#"{"file": "--odd.mp4", "name": "a b", "play": true, "session": "1-ab"}"#
            )
            .unwrap(),
            [
                "open",
                "--name=a b",
                "--play",
                "--session=1-ab",
                "--",
                "--odd.mp4"
            ]
        );
        assert_eq!(
            line("seek", r#"{"time": -2}"#).unwrap(),
            ["seek", "--", "-2"]
        );
        assert_eq!(
            line(
                "web wait-for",
                r##"{"selector": "#x", "timeout": 5, "force": false}"##
            )
            .unwrap(),
            ["web", "wait-for", "--timeout=5", "--", "#x"]
        );
        assert_eq!(line("status", "{}").unwrap(), ["status", "--"]);
        assert_eq!(
            line("review", r#"{"from": "1:00"}"#).unwrap(),
            ["review", "--", "1:00"]
        );
        // Unknown arguments, the wrong kind, a gap in the plain ones.
        assert_eq!(line("status", r#"{"x": 1}"#).unwrap_err().code, "BAD_ARGS");
        assert_eq!(
            line("open", r#"{"play": "yes"}"#).unwrap_err().code,
            "BAD_ARGS"
        );
        assert_eq!(line("loop", r#"{"to": 3}"#).unwrap_err().code, "BAD_ARGS");
        assert_eq!(line("open", "[1]").unwrap_err().code, "BAD_ARGS");
    }

    #[test]
    fn pictures_in_answers() {
        let answer = args(
            r#"{"path": "/a/shot.png", "file": "/a/v.mp4", "frames": [{"path": "/a/f.PNG"}, {"path": "/a/r.mp4"}],
                "events": [{"crop": "/a/c.jpg", "note": "/a/x.png"}], "value": {"path": "/a/page.png", "x": [{"crop": "/a/p.jpg"}]}}"#,
        );
        let mut found = Vec::new();
        pictures(&answer, &mut found);
        assert_eq!(
            found,
            ["/a/shot.png", "/a/f.PNG", "/a/c.jpg"].map(PathBuf::from)
        );
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn only_pepper_agents_pictures_are_attached() {
        let dir = std::env::temp_dir().join(format!("pepper-mcp-pictures-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (snapshots, outside) = (dir.join("work/.pepper/snapshots"), dir.join("outside"));
        std::fs::create_dir_all(&snapshots).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let png = b"\x89PNG\r\n\x1a\n0000";
        let (shot, mine) = (snapshots.join("s.png"), outside.join("x.png"));
        std::fs::write(&shot, png).unwrap();
        std::fs::write(&mine, png).unwrap();
        std::os::unix::fs::symlink(&mine, snapshots.join("link.png")).unwrap();
        std::fs::write(
            snapshots.join("big.png"),
            vec![0x89; MAX_IMAGE_BYTES as usize + 1],
        )
        .unwrap();
        let roots = [std::fs::canonicalize(dir.join("work/.pepper")).unwrap()];
        let answer = |v: &str| {
            args(
                &v.replace("SNAP", &snapshots.display().to_string())
                    .replace("MINE", &mine.display().to_string()),
            )
        };
        let count = |v: &str| attached(&answer(v), &roots).len();

        // A snapshot is attached; a picture of the user's elsewhere isn't,
        // named in eval's value or anywhere, or through a link.
        assert_eq!(count(r#"{"path": "SNAP/s.png"}"#), 1);
        assert_eq!(count(r#"{"viewer": "1", "value": {"path": "MINE"}}"#), 0);
        assert_eq!(
            count(r#"{"path": "MINE", "events": [{"crop": "MINE"}]}"#),
            0
        );
        assert_eq!(count(r#"{"path": "SNAP/link.png"}"#), 0);
        assert_eq!(count(r#"{"path": "SNAP/../../../outside/x.png"}"#), 0);
        assert_eq!(
            count(r#"{"path": "SNAP/big.png", "sheet": "SNAP/none.png"}"#),
            0
        );
        // Those that aren't don't push out those that are.
        let many = format!(
            "{{\"frames\": [{}{{\"path\": \"SNAP/s.png\"}}]}}",
            r#"{"path": "MINE"},"#.repeat(MAX_IMAGES)
        );
        assert_eq!(count(&many), 1);
        let image = &attached(&answer(r#"{"path": "SNAP/s.png"}"#), &roots)[0];
        assert_eq!(image.get_str("mimeType"), Some("image/png"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn calls_with_the_same_id_are_told_apart() {
        let server = Server {
            exe: PathBuf::new(),
            out: Mutex::new(std::io::stdout()),
            running: Mutex::new(HashMap::new()),
            cancelled: Mutex::new(HashSet::new()),
            next: AtomicU64::new(1),
            calls: AtomicUsize::new(0),
        };
        // Groups no process has: nothing is signalled.
        lock(&server.running).insert(1, ("7".into(), 999_998));
        lock(&server.running).insert(2, ("7".into(), 999_999));
        lock(&server.running).insert(3, ("8".into(), 999_997));
        cancel(&server, &Value::from(7.0));
        assert_eq!(*lock(&server.cancelled), HashSet::from([1, 2]));
        lock(&server.running).remove(&1);
        assert_eq!(lock(&server.running).len(), 2);
    }

    #[test]
    fn initialize_and_versions() {
        let r = initialize(&args(r#"{"protocolVersion": "2025-06-18"}"#));
        assert_eq!(r.get_str("protocolVersion"), Some("2025-06-18"));
        let r = initialize(&args(r#"{"protocolVersion": "1999-01-01"}"#));
        assert_eq!(r.get_str("protocolVersion"), Some(VERSIONS[0]));
        assert!(r.get("capabilities").and_then(|c| c.get("tools")).is_some());
    }
}
