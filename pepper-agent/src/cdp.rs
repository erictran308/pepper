//! The Chrome DevTools Protocol over a pipe. The viewer runs a headless
//! Chrome (or another Chromium) with `--remote-debugging-pipe`: it reads
//! commands on its file descriptor 3 and writes answers and events on 4,
//! each a JSON text ending in NUL. No port is opened, so no other program
//! can reach the browser, and it ends when the viewer does.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{BufRead, PipeReader, PipeWriter, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::json::{self, Value};
use crate::{sys, text};

/// The largest message taken from the browser: a full-page screenshot of a
/// long page is tens of megabytes of base64.
const MAX_MESSAGE: usize = 256 << 20;
/// How long a command waits for its answer unless told otherwise.
pub const CALL_WAIT: Duration = Duration::from_secs(15);
/// The oldest Chrome taken: it opens pages from outside, and one that old
/// lacks a year or more of fixes.
const MIN_MAJOR: u32 = 140;
/// What the browser has of the viewer's environment: none of the viewer's
/// own (PEPPER_*) or what else the human's shell exports.
const CHROME_ENV: &[&str] = &["HOME", "TMPDIR", "PATH", "LANG"];

/// A Chromium to run: `PEPPER_CHROME`, an installed Chrome or Chromium, or
/// Playwright's.
pub fn find_chrome() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PEPPER_CHROME").map(PathBuf::from) {
        return (path.is_absolute() && path.is_file()).then_some(path);
    }
    let apps = [
        "Google Chrome.app/Contents/MacOS/Google Chrome",
        "Chromium.app/Contents/MacOS/Chromium",
        "Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
        "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "Brave Browser.app/Contents/MacOS/Brave Browser",
    ];
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut roots = vec![PathBuf::from("/Applications")];
    roots.extend(home.iter().map(|h| h.join("Applications")));
    for root in &roots {
        for app in apps {
            let path = root.join(app);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    // Playwright's, newest first: by the build in the folder's name
    // (chromium-1200 is newer than chromium-999).
    let cache = home?.join("Library/Caches/ms-playwright");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&cache)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("chromium"))
        })
        .collect();
    dirs.sort_by_key(|dir| {
        dir.file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.rsplit('-').next())
            .and_then(|build| build.parse::<u64>().ok())
            .unwrap_or(0)
    });
    dirs.iter().rev().find_map(|dir| {
        [
            "chrome-mac-arm64/Chromium.app/Contents/MacOS/Chromium",
            "chrome-mac/Chromium.app/Contents/MacOS/Chromium",
            "chrome-mac-arm64/headless_shell",
            "chrome-mac/headless_shell",
            "chrome-headless-shell-mac-arm64/chrome-headless-shell",
        ]
        .iter()
        .map(|p| dir.join(p))
        .find(|p| p.is_file())
    })
}

/// A browser's major version, from its product: `HeadlessChrome/156.0.8078.35`
/// is 156.
pub fn major_version(product: &str) -> Option<u32> {
    product.rsplit('/').next()?.split('.').next()?.parse().ok()
}

/// An event from the browser. A screencast frame's picture comes out of its
/// message already decoded, in `data`.
pub struct Event {
    pub method: String,
    pub params: Value,
    pub data: Option<Vec<u8>>,
}

/// The method of the event a session's events end with, when the browser
/// goes.
pub const CLOSED: &str = "pepper.closed";

pub struct Conn {
    out: Mutex<PipeWriter>,
    next: AtomicU64,
    pending: Mutex<HashMap<u64, Sender<Result<Value>>>>,
    /// Where each session's events go.
    routes: Mutex<HashMap<String, Sender<Event>>>,
    /// The browser's own events (no session).
    browser_events: Mutex<Option<Sender<Event>>>,
    closed: AtomicBool,
}

impl Conn {
    fn write(&self, id: u64, method: &str, params: Value, session: Option<&str>) -> bool {
        let mut message = Value::obj()
            .with("id", id)
            .with("method", method)
            .with("params", params);
        if let Some(session) = session {
            message.set("sessionId", session);
        }
        let mut bytes = message.to_line().into_bytes();
        bytes.push(0);
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        out.write_all(&bytes).is_ok()
    }

    /// Sends a command whose answer doesn't matter.
    pub fn send(&self, method: &str, params: Value, session: Option<&str>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.write(id, method, params, session);
    }

    /// Sends a command and waits for its answer.
    pub fn call(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
        wait: Duration,
    ) -> Result<Value> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(gone());
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        if !self.write(id, method, params, session) {
            self.forget(id);
            return Err(gone());
        }
        match rx.recv_timeout(wait) {
            Ok(answer) => answer,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.forget(id);
                Err(Error::new(
                    "TIMEOUT",
                    format!("the browser didn't answer {method} in time"),
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(gone()),
        }
    }

    fn forget(&self, id: u64) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }

    /// Sends a session's events to `tx` from now on.
    pub fn route(&self, session: &str, tx: Sender<Event>) {
        self.routes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session.to_owned(), tx);
    }

    pub fn unroute(&self, session: &str) {
        self.routes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session);
    }

    pub fn on_browser_events(&self, tx: Sender<Event>) {
        *self
            .browser_events
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(tx);
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    fn dispatch(&self, bytes: &[u8]) {
        let (bytes, data) = split_frame(bytes);
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return;
        };
        let Ok(mut message) = json::parse(text) else {
            return;
        };
        if let Some(id) = message.get_num("id") {
            let tx = self
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&(id as u64));
            if let Some(tx) = tx {
                let answer = match message.get("error") {
                    Some(error) => Err(Error::new(
                        "FAILED",
                        format!(
                            "the browser: {}",
                            text::inert(error.get_str("message").unwrap_or("an error"), 300)
                        ),
                    )),
                    None => Ok(message.take("result").unwrap_or_else(Value::obj)),
                };
                let _ = tx.send(answer);
            }
            return;
        }
        let Some(method) = message.get_str("method").map(str::to_owned) else {
            return;
        };
        let event = Event {
            method,
            params: message.take("params").unwrap_or_else(Value::obj),
            data,
        };
        match message.get_str("sessionId") {
            Some(session) => {
                let routes = self.routes.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(tx) = routes.get(session) {
                    let _ = tx.send(event);
                }
            }
            None => {
                let browser = self
                    .browser_events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Some(tx) = browser.as_ref() {
                    let _ = tx.send(event);
                }
            }
        }
    }

    /// The browser went: every waiting command and every session hears so,
    /// and its own events' thread ends.
    fn end(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        drop(
            self.browser_events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take(),
        );
        let routes = std::mem::take(&mut *self.routes.lock().unwrap_or_else(|e| e.into_inner()));
        for tx in routes.values() {
            let _ = tx.send(Event {
                method: CLOSED.into(),
                params: Value::obj(),
                data: None,
            });
        }
    }
}

fn gone() -> Error {
    Error::new("FAILED", "the browser has ended")
        .hint("Open the page again: `pepper-agent web open <url>`.")
}

/// A screencast frame's picture, taken out of its message before the rest
/// is read: base64 of a JPEG, megabytes long. Anything else as it is. Only a
/// message that starts as Chrome writes the event is one: a page's value in
/// an answer may hold the same words.
fn split_frame(message: &[u8]) -> (Cow<'_, [u8]>, Option<Vec<u8>>) {
    const HEAD: &[u8] = b"{\"method\":\"Page.screencastFrame\",\"params\":{\"data\":\"";
    if !message.starts_with(HEAD) {
        return (Cow::Borrowed(message), None);
    }
    let start = HEAD.len();
    let Some(len) = message[start..].iter().position(|&b| b == b'"') else {
        return (Cow::Borrowed(message), None);
    };
    let Some(data) = base64(&message[start..start + len]) else {
        return (Cow::Borrowed(message), None);
    };
    let mut rest = Vec::with_capacity(message.len() - len);
    rest.extend_from_slice(&message[..start]);
    rest.extend_from_slice(&message[start + len..]);
    (Cow::Owned(rest), Some(data))
}

/// Base64 (with `+` and `/`, padded or not), or None if it isn't.
pub fn base64(text: &[u8]) -> Option<Vec<u8>> {
    const BAD: u8 = 0xff;
    const TABLE: [u8; 256] = {
        let mut table = [BAD; 256];
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut i = 0;
        while i < 64 {
            table[alphabet[i] as usize] = i as u8;
            i += 1;
        }
        table
    };
    let text = match text {
        [rest @ .., b'=', b'='] | [rest @ .., b'='] => rest,
        _ => text,
    };
    if text.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3 + 3);
    let mut chunks = text.chunks_exact(4);
    for chunk in &mut chunks {
        let [a, b, c, d] = [chunk[0], chunk[1], chunk[2], chunk[3]].map(|x| TABLE[x as usize]);
        if a | b | c | d == BAD {
            return None;
        }
        let n = (a as u32) << 18 | (b as u32) << 12 | (c as u32) << 6 | d as u32;
        out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]);
    }
    let rest: Vec<u8> = chunks
        .remainder()
        .iter()
        .map(|&x| TABLE[x as usize])
        .collect();
    if rest.contains(&BAD) {
        return None;
    }
    match rest[..] {
        [a, b] => out.push(((a as u32) << 2 | (b as u32) >> 4) as u8),
        [a, b, c] => {
            let n = (a as u32) << 10 | (b as u32) << 4 | (c as u32) >> 2;
            out.extend_from_slice(&[(n >> 8) as u8, n as u8]);
        }
        _ => {}
    }
    Some(out)
}

/// A headless browser of the viewer's own, with a profile made for it and
/// deleted with it: never the user's.
pub struct Browser {
    child: Child,
    pub conn: Arc<Conn>,
    pub profile: PathBuf,
    pub version: String,
    closed: bool,
}

impl Browser {
    pub fn launch(chrome: &Path, profile: &Path, log: Option<&Path>) -> Result<Browser> {
        let fail = |what: &str, e: std::io::Error| Error::new("FAILED", format!("{what}: {e}"));
        let (from_chrome, chrome_out) = std::io::pipe().map_err(|e| fail("a pipe", e))?;
        let (chrome_in, to_chrome) = std::io::pipe().map_err(|e| fail("a pipe", e))?;
        // Under --debug, its messages go to the viewer's log (`log_lines`).
        let stderr = match log {
            Some(_) => Stdio::piped(),
            None => Stdio::null(),
        };
        let mut command = Command::new(chrome);
        command.env_clear();
        for key in CHROME_ENV {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
            .args([
                "--headless",
                "--remote-debugging-pipe",
                "--no-first-run",
                "--no-default-browser-check",
                // Nothing of Chrome's own over the network: no updates,
                // sync, translation, safe-browsing lists, metrics.
                "--disable-background-networking",
                "--disable-component-update",
                "--disable-sync",
                "--disable-default-apps",
                "--disable-extensions",
                "--disable-breakpad",
                "--disable-features=Translate,MediaRouter,OptimizationHints,AutofillServerCommunication",
                "--metrics-recording-only",
                // No Keychain prompt for the profile's passwords.
                "--use-mock-keychain",
                "--password-store=basic",
                // WebRTC's UDP goes nowhere a proxy doesn't.
                "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
                "--mute-audio",
                "--hide-crash-restore-bubble",
                // Headless Chrome's screencast is at its own scale whatever
                // the page's emulated one: 2, so a page on a Retina display
                // is sharp in Pepper.
                "--force-device-scale-factor=2",
            ])
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("about:blank")
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr);
        let (a, b) = (chrome_in.as_raw_fd(), chrome_out.as_raw_fd());
        // SAFETY: as_fd3_and_4 makes only async-signal-safe calls.
        unsafe { command.pre_exec(move || sys::as_fd3_and_4(a, b)) };
        let mut child = command
            .spawn()
            .map_err(|e| fail(&format!("cannot run {}", chrome.display()), e))?;
        drop(chrome_in);
        drop(chrome_out);
        if let (Some(log), Some(stderr)) = (log, child.stderr.take()) {
            let log = log.to_owned();
            std::thread::spawn(move || log_lines(stderr, &log));
        }
        let conn = Arc::new(Conn {
            out: Mutex::new(to_chrome),
            next: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            routes: Mutex::new(HashMap::new()),
            browser_events: Mutex::new(None),
            closed: AtomicBool::new(false),
        });
        {
            let conn = conn.clone();
            std::thread::spawn(move || read_messages(from_chrome, &conn));
        }
        let mut browser = Browser {
            child,
            conn,
            profile: profile.to_owned(),
            version: String::new(),
            closed: false,
        };
        let product = browser
            .conn
            .call(
                "Browser.getVersion",
                Value::obj(),
                None,
                Duration::from_secs(20),
            )
            .map(|v| v.get_str("product").unwrap_or("").to_owned());
        match product {
            Ok(product) if major_version(&product).is_some_and(|m| m < MIN_MAJOR) => {
                browser.close();
                Err(Error::new(
                    "MISSING_DEP",
                    format!(
                        "{} is {}, older than Chrome {MIN_MAJOR}",
                        chrome.display(),
                        text::inert(&product, 80)
                    ),
                )
                .hint("Update it, or set PEPPER_CHROME to a newer Chrome or Chromium."))
            }
            Ok(product) => {
                browser.version = text::inert(&product, 80);
                Ok(browser)
            }
            Err(e) => {
                browser.close();
                Err(Error::new(
                    "MISSING_DEP",
                    format!("{} didn't start: {}", chrome.display(), e.message),
                )
                .hint("Set PEPPER_CHROME to a Chrome or Chromium that runs, or install Google Chrome."))
            }
        }
    }

    /// Asks the browser to close, then makes sure; deletes its profile.
    pub fn close(&mut self) {
        if std::mem::replace(&mut self.closed, true) {
            return;
        }
        if !self.conn.is_closed() {
            self.conn.send("Browser.close", Value::obj(), None);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

impl Drop for Browser {
    /// One that wasn't closed (it ended, and another took its place) is
    /// closed all the same: reaped, its profile deleted.
    fn drop(&mut self) {
        self.close();
    }
}

/// The browser's messages into the viewer's log, as the viewer's own lines
/// go: each on its own, with no password and nothing a terminal acts on.
fn log_lines(from: impl Read, log: &Path) {
    let mut reader = std::io::BufReader::new(from);
    loop {
        match crate::proto::read_line(&mut reader, 4096) {
            Ok(Some(line)) => {
                let line = text::inert(&text::redact_urls(&line), 2000);
                let at = sys::iso_time(sys::now());
                let _ = crate::files::append_line(log, &format!("{at} chrome: {line}"));
            }
            Ok(None) => return,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                if e.to_string() == crate::proto::TOO_LONG {
                    let _ = reader.skip_until(b'\n');
                }
            }
            Err(_) => return,
        }
    }
}

fn read_messages(mut from: PipeReader, conn: &Conn) {
    let mut buffer: Vec<u8> = Vec::with_capacity(1 << 20);
    let mut chunk = vec![0u8; 1 << 20];
    // Bytes of `buffer` known to hold no NUL.
    let mut scanned = 0;
    // A message too large is being dropped, up to its end.
    let mut dropping = false;
    loop {
        let n = match from.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        buffer.extend_from_slice(&chunk[..n]);
        let mut start = 0;
        while let Some(at) = buffer[scanned..].iter().position(|&b| b == 0) {
            let end = scanned + at;
            if !std::mem::take(&mut dropping) {
                conn.dispatch(&buffer[start..end]);
            }
            start = end + 1;
            scanned = start;
        }
        buffer.drain(..start);
        scanned = buffer.len();
        // Not the whole browser for one page's message: it goes, what
        // waited for it times out, and the other pages go on.
        if buffer.len() > MAX_MESSAGE {
            buffer.clear();
            scanned = 0;
            dropping = true;
        }
    }
    conn.end();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_decodes() {
        assert_eq!(base64(b"aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64(b"aGVsbG8").unwrap(), b"hello");
        assert_eq!(base64(b"aGk=").unwrap(), b"hi");
        assert_eq!(base64(b"").unwrap(), b"");
        assert_eq!(base64(b"/+8=").unwrap(), [0xff, 0xef]);
        assert!(base64(b"a").is_none());
        assert!(base64(b"aGk\\").is_none());
        assert!(base64(b"a\"Gk").is_none());
    }

    #[test]
    fn frames_come_out_before_parsing() {
        let message = br#"{"method":"Page.screencastFrame","params":{"data":"aGVsbG8=","metadata":{"deviceWidth":800},"sessionId":3},"sessionId":"S1"}"#;
        let (rest, data) = split_frame(message);
        assert_eq!(data.unwrap(), b"hello");
        let rest = json::parse(std::str::from_utf8(&rest).unwrap()).unwrap();
        assert_eq!(rest.get("params").unwrap().get_num("sessionId"), Some(3.0));
        assert_eq!(rest.get_str("sessionId"), Some("S1"));
        // Anything else stays as it is: an answer, or a value that only
        // holds the event's words.
        let other = br#"{"id":4,"result":{"data":"aGk="}}"#;
        let (rest, data) = split_frame(other);
        assert!(data.is_none());
        assert_eq!(&rest[..], &other[..]);
        let value = br#"{"id":5,"result":{"result":{"value":{"method":"Page.screencastFrame","params":{"data":"aGk="}}}}}"#;
        assert!(split_frame(value).1.is_none());
    }

    #[test]
    fn versions() {
        assert_eq!(major_version("HeadlessChrome/156.0.8078.35"), Some(156));
        assert_eq!(major_version("Chrome/99.0.1"), Some(99));
        assert_eq!(major_version("Chromium"), None);
    }

    /// A Conn read as the viewer reads its browser, and the end the browser
    /// would write to.
    fn conn() -> (Arc<Conn>, PipeWriter) {
        let (_, to_chrome) = std::io::pipe().unwrap();
        let (from_chrome, writer) = std::io::pipe().unwrap();
        let conn = Arc::new(Conn {
            out: Mutex::new(to_chrome),
            next: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            routes: Mutex::new(HashMap::new()),
            browser_events: Mutex::new(None),
            closed: AtomicBool::new(false),
        });
        let reading = conn.clone();
        std::thread::spawn(move || read_messages(from_chrome, &reading));
        (conn, writer)
    }

    #[test]
    fn a_message_too_large_goes_and_the_browser_stays() {
        let (conn, mut browser) = conn();
        let (tx, rx) = mpsc::channel();
        conn.route("S", tx);
        // Over the cap, in pieces, then a NUL, then an event that's taken.
        let piece = vec![b'x'; 1 << 20];
        for _ in 0..(MAX_MESSAGE >> 20) + 2 {
            browser.write_all(&piece).unwrap();
        }
        browser
            .write_all(
                b"\0{\"method\":\"Page.loadEventFired\",\"params\":{},\"sessionId\":\"S\"}\0",
            )
            .unwrap();
        let event = rx.recv_timeout(Duration::from_secs(20)).unwrap();
        assert_eq!(event.method, "Page.loadEventFired");
        assert!(!conn.is_closed());
        // When it goes, sessions hear so and its own events' sender goes.
        let (btx, brx) = mpsc::channel();
        conn.on_browser_events(btx);
        drop(browser);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap().method,
            CLOSED
        );
        assert!(brx.recv_timeout(Duration::from_secs(5)).is_err());
        assert!(conn.is_closed());
    }
}
