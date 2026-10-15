//! Web sources: a page in the viewer's own headless Chrome, shown live in
//! its box (Chrome's screencast, drawn by `frames.rs`), driven by the
//! agent's `web` commands, and pointed at by the human.
//!
//! Each page has a browser context of its own (cookies, storage, cache).
//! By default it reaches only this machine: the context's proxy is a port
//! that answers nothing, bypassed only for localhost, 127.0.0.1 and ::1, so
//! neither the page nor anything it loads reaches the network unless it was
//! opened with --allow-remote. What the page says (its title, console,
//! errors, element text) is the page's: it's escaped wherever it's shown.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use super::Msg;
use super::frames::FrameMsg;
use crate::cdp::{self, Browser, CALL_WAIT, Conn, Event};
use crate::error::{Error, Result};
use crate::json::Value;
use crate::{files, media, text};

/// A proxy that answers nothing: everything not bypassed fails to connect.
const NO_PROXY: &str = "http://127.0.0.1:9";
const LOCAL_ONLY: &str = "localhost;*.localhost;127.0.0.1;[::1]";
/// How long a navigation waits for the page's load event.
const LOAD_WAIT: Duration = Duration::from_secs(30);
const CONSOLE_KEPT: usize = 500;
const ERRORS_KEPT: usize = 500;
/// Requests followed for network-idle, at most.
const REQUESTS_KEPT: usize = 2000;
/// No request for this long: the network is idle.
const IDLE_AFTER: Duration = Duration::from_millis(500);
/// Pictures are at most this many pixels on their long side.
pub const MAX_SIDE: f64 = 1568.0;
const JPEG_QUALITY: u32 = 85;
/// Chrome sends every second frame of an animation: 30 a second. Making
/// 2880×1800 JPEGs costs it 151% of a core at 60, 85% at 30
/// (notes/agent.md); a still page sends none.
const EVERY_NTH_FRAME: u32 = 2;

/// The size a page is shown at: a preset or WxH@DPR.
#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub dpr: f64,
    pub mobile: bool,
}

pub const DEFAULT_DEVICE: &str = "desktop-1440";
pub const DEVICES: &[(&str, u32, u32, f64, bool)] = &[
    ("desktop-1920", 1920, 1080, 1.0, false),
    ("desktop-1440", 1440, 900, 2.0, false),
    ("laptop-1280", 1280, 800, 2.0, false),
    ("ipad", 820, 1180, 2.0, true),
    ("iphone-15", 393, 852, 3.0, true),
    ("pixel-8", 412, 915, 2.625, true),
];
const IPHONE_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1";
const IPAD_UA: &str = "Mozilla/5.0 (iPad; CPU OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1";
const ANDROID_UA: &str = "Mozilla/5.0 (Linux; Android 15; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Mobile Safari/537.36";

impl Device {
    /// A preset's name, or `WxH` / `WxH@DPR` (CSS pixels, 100 to 4000).
    pub fn parse(text: &str) -> Option<Device> {
        if let Some(&(name, width, height, dpr, mobile)) = DEVICES.iter().find(|d| d.0 == text) {
            return Some(Device {
                name: name.into(),
                width,
                height,
                dpr,
                mobile,
            });
        }
        let (size, dpr) = match text.split_once('@') {
            Some((size, dpr)) => (size, dpr.parse::<f64>().ok()?),
            None => (text, 1.0),
        };
        let (w, h) = size.split_once(['x', 'X'])?;
        let (width, height) = (w.parse::<u32>().ok()?, h.parse::<u32>().ok()?);
        let ok = |n: u32| (100..=4000).contains(&n);
        (ok(width) && ok(height) && (0.5..=4.0).contains(&dpr)).then(|| Device {
            name: format!("{width}x{height}@{dpr}"),
            width,
            height,
            dpr,
            mobile: false,
        })
    }

    pub fn to_json(&self) -> Value {
        Value::obj()
            .with("name", self.name.as_str())
            .with("width", self.width)
            .with("height", self.height)
            .with("dpr", self.dpr)
            .with("mobile", self.mobile)
    }

    fn user_agent(&self, desktop: &str) -> String {
        match self.name.as_str() {
            "iphone-15" => IPHONE_UA.into(),
            "ipad" => IPAD_UA.into(),
            "pixel-8" => ANDROID_UA.into(),
            _ => desktop.into(),
        }
    }
}

/// Whether `url` may be opened without --allow-remote: a page on this
/// machine (localhost, a name under .localhost, 127.x.x.x, ::1), a file,
/// or a blank page.
pub fn is_local(url: &str) -> bool {
    if url == "about:blank" {
        return true;
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    match scheme.to_ascii_lowercase().as_str() {
        "file" => true,
        "http" | "https" => {
            let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
            let host = authority.rsplit('@').next().unwrap_or("");
            let host = if let Some(v6) = host.strip_prefix('[') {
                v6.split(']').next().unwrap_or("")
            } else {
                host.split(':').next().unwrap_or("")
            };
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            host == "localhost"
                || host.ends_with(".localhost")
                || host == "::1"
                || host
                    .parse::<std::net::Ipv4Addr>()
                    .is_ok_and(|ip| ip.is_loopback())
        }
        _ => false,
    }
}

/// A web address as given, checked: http(s), file or about:blank, no
/// spaces or control characters.
pub fn check_url(url: &str) -> Result<()> {
    if url.len() > 4096 {
        return Err(Error::new("BAD_ARGS", "the address is too long"));
    }
    if url.chars().any(|c| c.is_control() || c == ' ') {
        return Err(Error::new(
            "BAD_ARGS",
            "a web address holds no spaces or control characters",
        ));
    }
    let scheme = url.split_once("://").map(|(s, _)| s.to_ascii_lowercase());
    match scheme.as_deref() {
        Some("http" | "https" | "file") => Ok(()),
        _ if url == "about:blank" => Ok(()),
        _ => Err(Error::new(
            "BAD_ARGS",
            "a page's address is http://, https:// or file://",
        )),
    }
}

/// The viewer's browser: launched with the first page, closed with the last.
pub struct Host {
    browser: Mutex<Browser>,
    conn: Arc<Conn>,
    pages: Mutex<HashMap<String, Weak<Page>>>,
    /// Chrome's own user agent, without "Headless".
    user_agent: String,
}

impl Host {
    pub fn launch(chrome: &Path, profile: &Path, log: Option<&Path>) -> Result<Arc<Host>> {
        files::private_dir(profile)
            .map_err(|e| Error::new("FAILED", format!("{}: {e}", profile.display())))?;
        let mut browser = Browser::launch(chrome, profile, log)?;
        let conn = browser.conn.clone();
        let version = conn.call("Browser.getVersion", Value::obj(), None, CALL_WAIT);
        let user_agent = version
            .as_ref()
            .ok()
            .and_then(|v| v.get_str("userAgent"))
            .unwrap_or("")
            .replace("HeadlessChrome", "Chrome");
        if let Err(e) = conn.call(
            "Target.setDiscoverTargets",
            Value::obj().with("discover", true),
            None,
            CALL_WAIT,
        ) {
            browser.close();
            return Err(e);
        }
        let host = Arc::new(Host {
            browser: Mutex::new(browser),
            conn: conn.clone(),
            pages: Mutex::new(HashMap::new()),
            user_agent,
        });
        let (tx, rx) = mpsc::channel();
        conn.on_browser_events(tx);
        {
            let host = Arc::downgrade(&host);
            std::thread::spawn(move || browser_events(&host, rx));
        }
        Ok(host)
    }

    pub fn version(&self) -> String {
        lock(&self.browser).version.clone()
    }

    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    /// Whether the browser still runs.
    pub fn alive(&self) -> bool {
        !self.conn.is_closed()
    }

    pub fn close(&self) {
        lock(&self.browser).close();
    }

    fn page(&self, target: &str) -> Option<Arc<Page>> {
        lock(&self.pages).get(target).and_then(Weak::upgrade)
    }
}

fn browser_events(host: &Weak<Host>, rx: Receiver<Event>) {
    for event in rx {
        let Some(host) = host.upgrade() else { return };
        let params = &event.params;
        match event.method.as_str() {
            "Target.targetInfoChanged" => {
                // Its title is often the address: the page's own is asked.
                let info = params.get("targetInfo").unwrap_or(&Value::Null);
                if let Some(page) = info.get_str("targetId").and_then(|t| host.page(t)) {
                    page.refresh_title();
                }
            }
            "Target.targetCrashed" => {
                if let Some(page) = params.get_str("targetId").and_then(|t| host.page(t)) {
                    page.state().crashed = Some("the page crashed".into());
                    page.changed.notify_all();
                    page.notify();
                }
            }
            "Target.detachedFromTarget" => {
                if let Some(page) = params.get_str("targetId").and_then(|t| host.page(t)) {
                    let mut state = page.state();
                    if state.crashed.is_none() {
                        state.crashed = Some("the page went away".into());
                    }
                    drop(state);
                    page.changed.notify_all();
                    page.notify();
                }
            }
            _ => {}
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct PageState {
    pub url: String,
    pub title: String,
    pub device: Device,
    pub dark: bool,
    loads: u64,
    /// Why the page is gone (crashed, closed), if it is.
    pub crashed: Option<String>,
    console: VecDeque<Value>,
    next_console: u64,
    errors: VecDeque<Value>,
    next_error: u64,
    requests: HashMap<String, (String, String)>,
    last_request: Instant,
    main_frame: Option<String>,
    /// pepper-agent's own isolated world in the main frame: the page's
    /// scripts can't change what runs there.
    world: Option<i64>,
    /// The last input an agent's command made, until a frame shows it.
    input_at: Option<Instant>,
    pub input_to_frame_ms: Option<f64>,
    pub input_to_shown_ms: Option<f64>,
    pub frames: u64,
    title_asked: bool,
}

/// A page, shared by the viewer, its events' thread and the commands'.
pub struct Page {
    pub conn: Arc<Conn>,
    pub session: String,
    pub target: String,
    pub context: String,
    pub allow_remote: bool,
    sid: u64,
    tx: Sender<Msg>,
    state: Mutex<PageState>,
    changed: Condvar,
}

/// What `open` asks for.
pub struct OpenAsk {
    pub url: String,
    pub device: Device,
    pub dark: bool,
    pub allow_remote: bool,
    /// The box in pixels, for the screencast's size.
    pub box_px: (u32, u32),
}

/// Makes a page in a context of its own, shows `ask.url` in it, and starts
/// its screencast to `frames`.
pub fn open(
    host: &Arc<Host>,
    ask: &OpenAsk,
    sid: u64,
    tx: Sender<Msg>,
    frames: Sender<FrameMsg>,
) -> Result<Arc<Page>> {
    let conn = host.conn.clone();
    let mut params = Value::obj().with("disposeOnDetach", true);
    if !ask.allow_remote {
        params.set("proxyServer", NO_PROXY);
        params.set("proxyBypassList", LOCAL_ONLY);
    }
    let context = conn
        .call("Target.createBrowserContext", params, None, CALL_WAIT)?
        .get_str("browserContextId")
        .unwrap_or("")
        .to_owned();
    let made = (|| {
        let _ = conn.call(
            "Browser.setDownloadBehavior",
            Value::obj()
                .with("behavior", "deny")
                .with("browserContextId", context.as_str()),
            None,
            CALL_WAIT,
        );
        let target = conn
            .call(
                "Target.createTarget",
                Value::obj()
                    .with("url", "about:blank")
                    .with("browserContextId", context.as_str()),
                None,
                CALL_WAIT,
            )?
            .get_str("targetId")
            .unwrap_or("")
            .to_owned();
        let session = conn
            .call(
                "Target.attachToTarget",
                Value::obj()
                    .with("targetId", target.as_str())
                    .with("flatten", true),
                None,
                CALL_WAIT,
            )?
            .get_str("sessionId")
            .unwrap_or("")
            .to_owned();
        Ok((target, session))
    })();
    let (target, session) = match made {
        Ok(t) => t,
        Err(e) => {
            dispose(&conn, &context);
            return Err(e);
        }
    };
    let (etx, erx) = mpsc::channel();
    conn.route(&session, etx);
    let page = Arc::new(Page {
        conn: conn.clone(),
        session,
        target: target.clone(),
        context,
        allow_remote: ask.allow_remote,
        sid,
        tx,
        state: Mutex::new(PageState {
            url: "about:blank".into(),
            title: String::new(),
            device: ask.device.clone(),
            dark: ask.dark,
            loads: 0,
            crashed: None,
            console: VecDeque::new(),
            next_console: 1,
            errors: VecDeque::new(),
            next_error: 1,
            requests: HashMap::new(),
            last_request: Instant::now(),
            main_frame: None,
            world: None,
            input_at: None,
            input_to_frame_ms: None,
            input_to_shown_ms: None,
            frames: 0,
            title_asked: false,
        }),
        changed: Condvar::new(),
    });
    lock(&host.pages).insert(target, Arc::downgrade(&page));
    {
        let page = page.clone();
        std::thread::spawn(move || page_events(&page, erx, &frames));
    }
    let set_up = (|| {
        for domain in ["Page", "Runtime", "Network", "Log"] {
            page.call(&format!("{domain}.enable"), Value::obj())?;
        }
        page.emulate(&ask.device, ask.dark, &host.user_agent)?;
        page.navigate(&ask.url)?;
        page.screencast(ask.box_px)
    })();
    if let Err(e) = set_up {
        page.close();
        return Err(e);
    }
    Ok(page)
}

fn dispose(conn: &Conn, context: &str) {
    conn.send(
        "Target.disposeBrowserContext",
        Value::obj().with("browserContextId", context),
        None,
    );
}

/// A frame's picture from its message: taken out already, or still in it.
fn frame_data(event: &mut Event) -> Option<Vec<u8>> {
    event.data.take().or_else(|| {
        event
            .params
            .get_str("data")
            .and_then(|d| cdp::base64(d.as_bytes()))
    })
}

fn page_events(page: &Arc<Page>, rx: Receiver<Event>, frames: &Sender<FrameMsg>) {
    for mut event in rx {
        let method = event.method.clone();
        match method.as_str() {
            "Page.screencastFrame" => {
                let ack = event.params.get_num("sessionId").unwrap_or(0.0);
                let input = {
                    let mut state = page.state();
                    state.frames += 1;
                    let input = state.input_at.take();
                    if let Some(at) = input {
                        state.input_to_frame_ms = Some(ms(at.elapsed()));
                    }
                    input
                };
                let done_page = page.clone();
                let done = Box::new(move |shown: bool| {
                    done_page.conn.send(
                        "Page.screencastFrameAck",
                        Value::obj().with("sessionId", ack),
                        Some(&done_page.session),
                    );
                    if shown && let Some(at) = input {
                        done_page.state().input_to_shown_ms = Some(ms(at.elapsed()));
                    }
                });
                match frame_data(&mut event) {
                    Some(jpeg) => {
                        let _ = frames.send(FrameMsg::Frame(jpeg, done));
                    }
                    None => done(false),
                }
            }
            "Page.frameNavigated" => {
                let frame = event.params.get("frame").unwrap_or(&Value::Null);
                if frame.get("parentId").is_none() {
                    let mut state = page.state();
                    state.main_frame = frame.get_str("id").map(str::to_owned);
                    state.world = None;
                    if let Some(url) = frame.get_str("url") {
                        state.url = format!("{url}{}", frame.get_str("urlFragment").unwrap_or(""));
                    }
                    drop(state);
                    page.notify();
                }
            }
            "Page.navigatedWithinDocument" => {
                let mut state = page.state();
                if event.params.get_str("frameId") == state.main_frame.as_deref()
                    && let Some(url) = event.params.get_str("url")
                {
                    state.url = url.to_owned();
                    drop(state);
                    page.refresh_title();
                    page.notify();
                }
            }
            "Page.loadEventFired" => {
                page.state().loads += 1;
                page.changed.notify_all();
                page.refresh_title();
                page.notify();
            }
            "Runtime.executionContextsCleared" => page.state().world = None,
            "Page.javascriptDialogOpening" => {
                // Nothing in a headless page would answer it: it'd hang.
                page.conn.send(
                    "Page.handleJavaScriptDialog",
                    Value::obj().with("accept", true),
                    Some(&page.session),
                );
                let text = format!(
                    "{} dialog, accepted: {}",
                    event.params.get_str("type").unwrap_or("a"),
                    event.params.get_str("message").unwrap_or("")
                );
                page.console(
                    Value::obj()
                        .with("level", "info")
                        .with("text", clip(&text, 2000)),
                );
            }
            "Runtime.consoleAPICalled" => {
                let params = &event.params;
                let args: Vec<String> = params
                    .get("args")
                    .and_then(Value::arr)
                    .unwrap_or(&[])
                    .iter()
                    .map(describe_remote)
                    .collect();
                let mut entry = Value::obj()
                    .with("level", params.get_str("type").unwrap_or("log"))
                    .with("text", clip(&args.join(" "), 2000));
                if let Some(frame) = first_frame(params.get("stackTrace")) {
                    entry.set("url", clip(frame.get_str("url").unwrap_or(""), 500));
                    entry.set("line", frame.get_num("lineNumber").map(|l| l + 1.0));
                }
                page.console(entry);
            }
            "Log.entryAdded" => {
                let entry = event.params.get("entry").unwrap_or(&Value::Null);
                let mut out = Value::obj()
                    .with("level", entry.get_str("level").unwrap_or("info"))
                    .with("text", clip(entry.get_str("text").unwrap_or(""), 2000))
                    .with("from", entry.get_str("source").unwrap_or(""));
                if let Some(url) = entry.get_str("url") {
                    out.set("url", clip(url, 500));
                }
                page.console(out);
            }
            "Runtime.exceptionThrown" => {
                let details = event.params.get("exceptionDetails").unwrap_or(&Value::Null);
                let text = details
                    .get("exception")
                    .and_then(|e| e.get_str("description"))
                    .or_else(|| details.get_str("text"))
                    .unwrap_or("an exception");
                let mut error = Value::obj()
                    .with("kind", "exception")
                    .with("text", clip(text, 2000));
                if let Some(url) = details.get_str("url") {
                    error.set("url", clip(url, 500));
                    error.set("line", details.get_num("lineNumber").map(|l| l + 1.0));
                    error.set("column", details.get_num("columnNumber").map(|c| c + 1.0));
                }
                page.error(error);
            }
            "Network.requestWillBeSent" => {
                let params = &event.params;
                // Streams that never end don't hold network-idle back.
                let kind = params.get_str("type").unwrap_or("");
                if !matches!(kind, "EventSource" | "WebSocket")
                    && let Some(id) = params.get_str("requestId")
                {
                    let request = params.get("request").unwrap_or(&Value::Null);
                    let mut state = page.state();
                    if state.requests.len() < REQUESTS_KEPT {
                        state.requests.insert(
                            id.to_owned(),
                            (
                                request.get_str("method").unwrap_or("GET").to_owned(),
                                request.get_str("url").unwrap_or("").to_owned(),
                            ),
                        );
                    }
                    state.last_request = Instant::now();
                }
            }
            "Network.responseReceived" => {
                let params = &event.params;
                let response = params.get("response").unwrap_or(&Value::Null);
                let status = response.get_num("status").unwrap_or(0.0);
                if status >= 400.0 {
                    let method = params
                        .get_str("requestId")
                        .and_then(|id| page.state().requests.get(id).map(|r| r.0.clone()))
                        .unwrap_or_else(|| "GET".into());
                    page.error(
                        Value::obj()
                            .with("kind", "http")
                            .with("status", status)
                            .with("method", method)
                            .with("url", clip(response.get_str("url").unwrap_or(""), 500)),
                    );
                }
            }
            "Network.loadingFinished" => {
                let mut state = page.state();
                if let Some(id) = event.params.get_str("requestId") {
                    state.requests.remove(id);
                }
                state.last_request = Instant::now();
            }
            "Network.loadingFailed" => {
                let params = &event.params;
                let request = params
                    .get_str("requestId")
                    .and_then(|id| page.state().requests.remove(id));
                page.state().last_request = Instant::now();
                if params.get_bool("canceled") != Some(true) {
                    let (method, url) = request.unwrap_or_default();
                    let mut error = Value::obj()
                        .with("kind", "network")
                        .with("text", clip(params.get_str("errorText").unwrap_or(""), 300))
                        .with("method", method)
                        .with("url", clip(&url, 500));
                    if let Some(reason) = params.get_str("blockedReason") {
                        error.set("blocked", reason);
                    }
                    if !page.allow_remote
                        && params.get_str("errorText") == Some("net::ERR_PROXY_CONNECTION_FAILED")
                    {
                        error.set("note", "not on this machine: open the page with --allow-remote to let it reach the network");
                    }
                    page.error(error);
                }
            }
            "Inspector.targetCrashed" => {
                page.state().crashed = Some("the page crashed".into());
                page.changed.notify_all();
                page.notify();
            }
            cdp::CLOSED => {
                let mut state = page.state();
                if state.crashed.is_none() {
                    state.crashed = Some("the browser ended".into());
                }
                drop(state);
                page.changed.notify_all();
                page.notify();
                return;
            }
            _ => {}
        }
    }
}

fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 10000.0).round() / 10.0
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

fn first_frame(stack: Option<&Value>) -> Option<&Value> {
    stack?.get("callFrames")?.arr()?.first()
}

/// A console argument as text: its value, or the description of an object.
fn describe_remote(arg: &Value) -> String {
    match arg.get("value") {
        Some(Value::Str(s)) => s.clone(),
        Some(v) if !v.is_null() => v.to_line(),
        _ => arg
            .get_str("description")
            .or_else(|| arg.get_str("unserializableValue"))
            .or_else(|| arg.get_str("type"))
            .unwrap_or("")
            .to_owned(),
    }
}

/// Where a command's pictures go.
pub struct Files {
    /// The viewer's own folder: the CLI takes them from there.
    pub shots: PathBuf,
    pub count: u64,
}

impl Page {
    pub fn state(&self) -> MutexGuard<'_, PageState> {
        lock(&self.state)
    }

    fn notify(&self) {
        let _ = self.tx.send(Msg::Web(self.sid));
    }

    /// Asks the page for its title, on a thread (one at a time).
    fn refresh_title(self: &Arc<Self>) {
        {
            let mut state = self.state();
            if state.title_asked {
                return;
            }
            state.title_asked = true;
        }
        let page = self.clone();
        std::thread::spawn(move || {
            let title = page.run("function() { return document.title; }", Vec::new());
            let mut state = page.state();
            state.title_asked = false;
            if let Ok(Value::Str(title)) = title
                && state.title != title
            {
                state.title = title;
                drop(state);
                page.notify();
            }
        });
    }

    fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.conn
            .call(method, params, Some(&self.session), CALL_WAIT)
    }

    fn alive(&self) -> Result<()> {
        match &self.state().crashed {
            Some(why) => Err(Error::new("FAILED", why.clone())
                .hint("Open it again: `pepper-agent web open <url> --name <name>`.")),
            None => Ok(()),
        }
    }

    fn console(&self, mut entry: Value) {
        let mut state = self.state();
        let id = state.next_console;
        state.next_console += 1;
        entry.set("id", id);
        entry.set("at", crate::sys::iso_time(crate::sys::now()));
        state.console.push_back(entry);
        if state.console.len() > CONSOLE_KEPT {
            state.console.pop_front();
        }
    }

    fn error(&self, mut entry: Value) {
        let mut state = self.state();
        let id = state.next_error;
        state.next_error += 1;
        entry.set("id", id);
        entry.set("at", crate::sys::iso_time(crate::sys::now()));
        state.errors.push_back(entry);
        if state.errors.len() > ERRORS_KEPT {
            state.errors.pop_front();
        }
    }

    /// Closes the page and its context.
    pub fn close(&self) {
        self.conn.send(
            "Target.closeTarget",
            Value::obj().with("targetId", self.target.as_str()),
            None,
        );
        dispose(&self.conn, &self.context);
        self.conn.unroute(&self.session);
    }

    pub fn emulate(&self, device: &Device, dark: bool, desktop_ua: &str) -> Result<()> {
        self.call(
            "Emulation.setDeviceMetricsOverride",
            Value::obj()
                .with("width", device.width)
                .with("height", device.height)
                .with("deviceScaleFactor", device.dpr)
                .with("mobile", device.mobile)
                .with("screenWidth", device.width)
                .with("screenHeight", device.height),
        )?;
        self.call(
            "Emulation.setTouchEmulationEnabled",
            Value::obj()
                .with("enabled", device.mobile)
                .with("maxTouchPoints", if device.mobile { 5 } else { 1 }),
        )?;
        let ua = device.user_agent(desktop_ua);
        if !ua.is_empty() {
            self.call(
                "Emulation.setUserAgentOverride",
                Value::obj().with("userAgent", ua),
            )?;
        }
        self.call(
            "Emulation.setEmulatedMedia",
            Value::obj().with(
                "features",
                vec![
                    Value::obj()
                        .with("name", "prefers-color-scheme")
                        .with("value", if dark { "dark" } else { "light" }),
                ],
            ),
        )?;
        let mut state = self.state();
        state.device = device.clone();
        state.dark = dark;
        Ok(())
    }

    /// Runs the screencast at most `w`×`h` pixels: the box.
    pub fn screencast(&self, (w, h): (u32, u32)) -> Result<()> {
        let _ = self.call("Page.stopScreencast", Value::obj());
        self.call(
            "Page.startScreencast",
            Value::obj()
                .with("format", "jpeg")
                .with("quality", JPEG_QUALITY)
                .with("maxWidth", w.max(64))
                .with("maxHeight", h.max(64))
                .with("everyNthFrame", EVERY_NTH_FRAME),
        )
        .map(|_| ())
    }

    fn check_origin(&self, url: &str) -> Result<()> {
        check_url(url)?;
        if !self.allow_remote && !is_local(url) {
            return Err(Error::new(
                "ORIGIN_BLOCKED",
                format!(
                    "{} isn't on this machine",
                    text::inert(&text::redact_url(url), 200)
                ),
            ));
        }
        Ok(())
    }

    /// Runs `go`, then waits for the page's load event.
    fn and_wait_for_load(&self, go: impl FnOnce() -> Result<bool>) -> Result<Value> {
        let before = self.state().loads;
        let waits = go()?;
        let deadline = Instant::now() + LOAD_WAIT;
        let mut state = self.state();
        let mut loaded = !waits;
        while !loaded && state.crashed.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            state = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
            loaded = state.loads > before;
        }
        if let Some(why) = &state.crashed {
            return Err(Error::new("LOAD_FAILED", why.clone()));
        }
        // The title, for the answer: the page's own.
        drop(state);
        if let Ok(Value::Str(title)) = self.run("function() { return document.title; }", Vec::new())
        {
            self.state().title = title;
        }
        let state = self.state();
        let mut answer = Value::obj()
            .with("url", text::redact_url(&state.url))
            .with("title", clip(&state.title, 300));
        if !loaded {
            answer.set(
                "note",
                "still loading after 30 s: `pepper-agent web wait-for network-idle` waits more",
            );
        }
        Ok(answer)
    }

    pub fn navigate(&self, url: &str) -> Result<Value> {
        self.check_origin(url)?;
        self.and_wait_for_load(|| {
            let answer = self.call("Page.navigate", Value::obj().with("url", url))?;
            if let Some(error) = answer.get_str("errorText") {
                let hint = if error == "net::ERR_PROXY_CONNECTION_FAILED" {
                    "It isn't on this machine: open it with --allow-remote if the user wants that."
                } else if error == "net::ERR_CONNECTION_REFUSED" {
                    "Nothing answers there: is the dev server running, on that port?"
                } else {
                    "Check the address, and that the server is running."
                };
                return Err(Error::new(
                    "LOAD_FAILED",
                    format!(
                        "{}: {}",
                        text::inert(&text::redact_url(url), 200),
                        text::inert(error, 100)
                    ),
                )
                .hint(hint));
            }
            // A move within the page (#…) fires no load.
            Ok(answer.get_str("loaderId").is_some())
        })
    }

    pub fn reload(&self) -> Result<Value> {
        self.alive()?;
        self.and_wait_for_load(|| self.call("Page.reload", Value::obj()).map(|_| true))
    }

    pub fn back(&self) -> Result<Value> {
        self.alive()?;
        let history = self.call("Page.getNavigationHistory", Value::obj())?;
        let index = history.get_num("currentIndex").unwrap_or(0.0) as usize;
        let entries = history.get("entries").and_then(Value::arr).unwrap_or(&[]);
        let Some(entry) = index.checked_sub(1).and_then(|i| entries.get(i)) else {
            return Err(Error::new("FAILED", "there's no page to go back to"));
        };
        let url = entry.get_str("url").unwrap_or("");
        self.check_origin(url)?;
        let id = entry.get_num("id").unwrap_or(0.0);
        self.and_wait_for_load(|| {
            self.call(
                "Page.navigateToHistoryEntry",
                Value::obj().with("entryId", id),
            )
            .map(|_| true)
        })
    }

    /// pepper-agent's isolated world in the main frame.
    fn world(&self) -> Result<i64> {
        if let Some(world) = self.state().world {
            return Ok(world);
        }
        let frame = match self.state().main_frame.clone() {
            Some(f) => f,
            None => self
                .call("Page.getFrameTree", Value::obj())?
                .get("frameTree")
                .and_then(|t| t.get("frame"))
                .and_then(|f| f.get_str("id"))
                .unwrap_or("")
                .to_owned(),
        };
        let world = self
            .call(
                "Page.createIsolatedWorld",
                Value::obj()
                    .with("frameId", frame.as_str())
                    .with("worldName", "pepper-agent")
                    .with("grantUniveralAccess", false),
            )?
            .get_num("executionContextId")
            .ok_or_else(|| Error::new("FAILED", "the page has no frame to look in"))?
            as i64;
        self.state().world = Some(world);
        Ok(world)
    }

    /// Calls `function` in pepper-agent's world with `args`; its value.
    fn run(&self, function: &str, args: Vec<Value>) -> Result<Value> {
        self.alive()?;
        let mut tried = false;
        loop {
            let world = self.world()?;
            let answer = self.call(
                "Runtime.callFunctionOn",
                Value::obj()
                    .with("functionDeclaration", function)
                    .with("executionContextId", world as f64)
                    .with(
                        "arguments",
                        args.iter()
                            .map(|a| Value::obj().with("value", a.clone()))
                            .collect::<Vec<_>>(),
                    )
                    .with("returnByValue", true)
                    .with("awaitPromise", true),
            );
            match answer {
                // The page moved on and its world with it: once more.
                Err(e) if !tried && e.message.contains("context") => {
                    tried = true;
                    self.state().world = None;
                }
                Err(e) => return Err(e),
                Ok(answer) => return result_value(&answer),
            }
        }
    }

    fn mark_input(&self) {
        self.state().input_at = Some(Instant::now());
    }

    fn mouse(&self, kind: &str, x: f64, y: f64, buttons: u32) -> Result<()> {
        let mut params = Value::obj()
            .with("type", kind)
            .with("x", x)
            .with("y", y)
            .with("buttons", buttons);
        if kind != "mouseMoved" {
            params.set("button", "left");
            params.set("clickCount", 1);
        }
        self.call("Input.dispatchMouseEvent", params).map(|_| ())
    }

    /// Where `target` is, in CSS pixels of the viewport: `x,y`, or the
    /// middle of the element a selector finds (scrolled into view).
    fn locate(&self, target: &str) -> Result<(f64, f64, Value)> {
        if let Some((x, y)) = parse_xy(target) {
            let device = self.state().device.clone();
            if x < 0.0 || y < 0.0 || x >= device.width as f64 || y >= device.height as f64 {
                return Err(Error::new(
                    "BAD_ARGS",
                    format!(
                        "{x},{y} is outside the page's {}×{} viewport",
                        device.width, device.height
                    ),
                ));
            }
            return Ok((x, y, Value::Null));
        }
        let found = self.run(FIND_JS, vec![Value::from(target), Value::from(true)])?;
        element_center(target, found)
    }

    pub fn click(&self, target: &str) -> Result<Value> {
        let (x, y, element) = self.locate(target)?;
        self.mark_input();
        self.mouse("mouseMoved", x, y, 0)?;
        self.mouse("mousePressed", x, y, 1)?;
        self.mouse("mouseReleased", x, y, 0)?;
        std::thread::sleep(Duration::from_millis(100));
        Ok(self.after_input(x, y, element))
    }

    pub fn hover(&self, target: &str) -> Result<Value> {
        let (x, y, element) = self.locate(target)?;
        self.mark_input();
        self.mouse("mouseMoved", x, y, 0)?;
        Ok(self.after_input(x, y, element))
    }

    fn after_input(&self, x: f64, y: f64, element: Value) -> Value {
        let state = self.state();
        Value::obj()
            .with("x", round1(x))
            .with("y", round1(y))
            .with("element", element)
            .with("url", text::redact_url(&state.url))
    }

    pub fn type_text(&self, selector: &str, typed: &str, clear: bool) -> Result<Value> {
        let found = self.run(FOCUS_JS, vec![Value::from(selector), Value::from(clear)])?;
        if found.is_null() {
            return Err(no_match(selector));
        }
        self.mark_input();
        self.call("Input.insertText", Value::obj().with("text", typed))?;
        Ok(Value::obj()
            .with("typed", typed.chars().count())
            .with("element", found))
    }

    pub fn press(&self, key: &str) -> Result<Value> {
        let press = KeyPress::parse(key).ok_or_else(|| {
            Error::new("BAD_ARGS", format!("{:?} isn't a key", text::inert(key, 40))).hint(
                "Keys: Enter, Tab, Escape, Backspace, Delete, ArrowUp/Down/Left/Right, Home, End, PageUp, PageDown, Space, F1-F12, a letter or digit; with Control+, Shift+, Alt+ or Meta+ in front.",
            )
        })?;
        self.mark_input();
        let mut down = Value::obj()
            .with(
                "type",
                if press.text.is_empty() {
                    "rawKeyDown"
                } else {
                    "keyDown"
                },
            )
            .with("key", press.key.as_str())
            .with("code", press.code.as_str())
            .with("windowsVirtualKeyCode", press.key_code)
            .with("modifiers", press.modifiers);
        if !press.text.is_empty() {
            down.set("text", press.text.as_str());
        }
        self.call("Input.dispatchKeyEvent", down)?;
        self.call(
            "Input.dispatchKeyEvent",
            Value::obj()
                .with("type", "keyUp")
                .with("key", press.key.as_str())
                .with("code", press.code.as_str())
                .with("windowsVirtualKeyCode", press.key_code)
                .with("modifiers", press.modifiers),
        )?;
        std::thread::sleep(Duration::from_millis(50));
        Ok(Value::obj().with("pressed", key))
    }

    /// `pixels` down (up if negative), to `top` or `bottom`, or to an element.
    pub fn scroll(&self, what: &str, at: Option<(f64, f64)>) -> Result<Value> {
        self.mark_input();
        let result = match (what, what.parse::<f64>()) {
            (_, Ok(dy)) => {
                let device = self.state().device.clone();
                let (x, y) = at.unwrap_or((device.width as f64 / 2.0, device.height as f64 / 2.0));
                self.call(
                    "Input.dispatchMouseEvent",
                    Value::obj()
                        .with("type", "mouseWheel")
                        .with("x", x)
                        .with("y", y)
                        .with("deltaX", 0)
                        .with("deltaY", dy),
                )?;
                std::thread::sleep(Duration::from_millis(80));
                self.run(SCROLL_JS, vec![Value::Null])?
            }
            ("top" | "bottom", _) => self.run(SCROLL_JS, vec![Value::from(what)])?,
            (selector, _) => {
                let found = self.run(FIND_JS, vec![Value::from(selector), Value::from(true)])?;
                element_center(selector, found)?;
                self.run(SCROLL_JS, vec![Value::Null])?
            }
        };
        Ok(result)
    }

    pub fn set_viewport(&self, device: &Device, dark: bool, host_ua: &str) -> Result<Value> {
        self.emulate(device, dark, host_ua)?;
        Ok(Value::obj()
            .with("viewport", device.to_json())
            .with("dark", dark))
    }

    pub fn wait_for(&self, what: &str, wait: Duration) -> Result<Value> {
        let started = Instant::now();
        let deadline = started + wait;
        loop {
            self.alive()?;
            let done = if what == "network-idle" {
                let state = self.state();
                state.requests.is_empty() && state.last_request.elapsed() >= IDLE_AFTER
            } else {
                !self
                    .run(FIND_JS, vec![Value::from(what), Value::from(false)])?
                    .is_null()
            };
            if done {
                return Ok(Value::obj()
                    .with("waited_for", what)
                    .with("seconds", round1(started.elapsed().as_secs_f64())));
            }
            if Instant::now() >= deadline {
                let pending = self.state().requests.len();
                let mut error = Error::new(
                    "TIMEOUT",
                    format!(
                        "{} didn't come in {} s",
                        text::inert(what, 100),
                        wait.as_secs()
                    ),
                );
                if what == "network-idle" {
                    error.message += &format!(" ({pending} requests still open)");
                }
                return Err(error);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// A PNG of the page into `files.shots`: what the viewport shows, the
    /// whole page, or one element.
    pub fn shot(&self, files: &Files, full_page: bool, selector: Option<&str>) -> Result<Value> {
        self.alive()?;
        let mut params = Value::obj().with("format", "png");
        let device = self.state().device.clone();
        let mut clip_rect = None;
        if let Some(selector) = selector {
            let found = self.run(FIND_JS, vec![Value::from(selector), Value::from(false)])?;
            if found.is_null() {
                return Err(no_match(selector));
            }
            let r = found.get("page").unwrap_or(&Value::Null);
            let rect = (
                r.get_num("x").unwrap_or(0.0),
                r.get_num("y").unwrap_or(0.0),
                r.get_num("width").unwrap_or(0.0),
                r.get_num("height").unwrap_or(0.0),
            );
            if rect.2 < 1.0 || rect.3 < 1.0 {
                return Err(Error::new(
                    "FAILED",
                    format!("{} has no size on the page", text::inert(selector, 100)),
                ));
            }
            clip_rect = Some(rect);
        } else if full_page {
            let metrics = self.call("Page.getLayoutMetrics", Value::obj())?;
            let size = metrics.get("cssContentSize").unwrap_or(&Value::Null);
            let width = size.get_num("width").unwrap_or(device.width as f64);
            // Chrome draws at most 16384 device pixels down, at its own
            // scale of 2 or the page's, the larger.
            let height = size
                .get_num("height")
                .unwrap_or(device.height as f64)
                .min(16384.0 / device.dpr.max(2.0));
            clip_rect = Some((0.0, 0.0, width, height));
        }
        if let Some((x, y, w, h)) = clip_rect {
            params.set("captureBeyondViewport", true);
            params.set(
                "clip",
                Value::obj()
                    .with("x", x)
                    .with("y", y)
                    .with("width", w)
                    .with("height", h)
                    .with("scale", 1),
            );
        }
        let path = files
            .shots
            .join(format!("web-{}-{}.png", self.sid, files.count));
        self.screenshot(params, &path)?;
        let state = self.state();
        let mut answer = Value::obj()
            .with("raw", path.display().to_string())
            .with("url", text::redact_url(&state.url))
            .with("title", clip(&state.title, 300))
            .with("viewport", state.device.to_json());
        if let Some((x, y, w, h)) = clip_rect {
            answer.set("clip", vec![round1(x), round1(y), round1(w), round1(h)]);
        }
        Ok(answer)
    }

    fn screenshot(&self, params: Value, path: &Path) -> Result<()> {
        let answer = self.conn.call(
            "Page.captureScreenshot",
            params,
            Some(&self.session),
            Duration::from_secs(30),
        )?;
        let png = answer
            .get_str("data")
            .and_then(|d| cdp::base64(d.as_bytes()))
            .ok_or_else(|| Error::new("FAILED", "the browser sent no picture"))?;
        files::write_private(path, &png)
            .map_err(|e| Error::new("FAILED", format!("{}: {e}", path.display())))
    }

    pub fn console_since(&self, since: u64, levels: &[String]) -> Value {
        let state = self.state();
        let entries: Vec<Value> = state
            .console
            .iter()
            .filter(|e| e.get_num("id").unwrap_or(0.0) as u64 > since)
            .filter(|e| {
                levels.is_empty()
                    || levels
                        .iter()
                        .any(|l| Some(l.as_str()) == e.get_str("level"))
            })
            .cloned()
            .collect();
        Value::obj()
            .with("entries", entries)
            .with("last_id", state.next_console - 1)
    }

    pub fn errors_since(&self, since: u64) -> Value {
        let state = self.state();
        let errors: Vec<Value> = state
            .errors
            .iter()
            .filter(|e| e.get_num("id").unwrap_or(0.0) as u64 > since)
            .cloned()
            .collect();
        Value::obj()
            .with("errors", errors)
            .with("last_id", state.next_error - 1)
            .with("url", text::redact_url(&state.url))
    }

    pub fn dom(&self, selector: &str, max: usize) -> Result<Value> {
        let found = self.run(DOM_JS, vec![Value::from(selector), Value::from(max as f64)])?;
        if found.get_num("matches") == Some(0.0) {
            return Err(no_match(selector));
        }
        Ok(found)
    }

    /// The accessibility tree, one line a node: role, name, value.
    pub fn a11y(&self, selector: Option<&str>, max: usize) -> Result<Value> {
        self.alive()?;
        let tree = self.conn.call(
            "Accessibility.getFullAXTree",
            Value::obj(),
            Some(&self.session),
            Duration::from_secs(30),
        )?;
        let nodes = tree.get("nodes").and_then(Value::arr).unwrap_or(&[]);
        let by_id: HashMap<&str, &Value> = nodes
            .iter()
            .filter_map(|n| Some((n.get_str("nodeId")?, n)))
            .collect();
        let root = match selector {
            Some(selector) => {
                let found = self.dom_node(selector)?;
                nodes
                    .iter()
                    .find(|n| n.get_num("backendDOMNodeId") == Some(found))
                    .ok_or_else(|| {
                        Error::new(
                            "FAILED",
                            format!(
                                "{} isn't in the accessibility tree",
                                text::inert(selector, 100)
                            ),
                        )
                    })?
            }
            None => nodes
                .first()
                .ok_or_else(|| Error::new("FAILED", "the page has no accessibility tree"))?,
        };
        let mut lines = Vec::new();
        let mut truncated = false;
        a11y_lines(root, &by_id, 0, "", max, &mut lines, &mut truncated);
        Ok(Value::obj()
            .with("tree", lines.join("\n"))
            .with("lines", lines.len())
            .with("truncated", truncated))
    }

    fn dom_node(&self, selector: &str) -> Result<f64> {
        let document = self.call("DOM.getDocument", Value::obj().with("depth", 0))?;
        let root = document
            .get("root")
            .and_then(|r| r.get_num("nodeId"))
            .unwrap_or(0.0);
        let node = self
            .call(
                "DOM.querySelector",
                Value::obj().with("nodeId", root).with("selector", selector),
            )
            .map_err(|_| bad_selector(selector))?
            .get_num("nodeId")
            .unwrap_or(0.0);
        if node == 0.0 {
            return Err(no_match(selector));
        }
        self.call("DOM.describeNode", Value::obj().with("nodeId", node))?
            .get("node")
            .and_then(|n| n.get_num("backendNodeId"))
            .ok_or_else(|| no_match(selector))
    }

    /// The page's own JavaScript, in its own world: only on this machine.
    pub fn eval(&self, js: &str) -> Result<Value> {
        self.alive()?;
        let url = self.state().url.clone();
        if !is_local(&url) {
            return Err(Error::new(
                "ORIGIN_BLOCKED",
                format!(
                    "eval runs only on pages on this machine, not {}",
                    text::inert(&text::redact_url(&url), 200)
                ),
            ));
        }
        let answer = self.conn.call(
            "Runtime.evaluate",
            Value::obj()
                .with("expression", js)
                .with("returnByValue", true)
                .with("awaitPromise", true)
                .with("userGesture", false)
                .with("timeout", 10000),
            Some(&self.session),
            Duration::from_secs(15),
        )?;
        let value = result_value(&answer)?;
        let line = value.to_line();
        Ok(if line.len() > 100_000 {
            Value::obj()
                .with("value", clip(&line, 100_000))
                .with("truncated", true)
        } else {
            Value::obj().with("value", value)
        })
    }

    /// What's at a point of the page (CSS pixels of the viewport), or the
    /// smallest element holding a region: selector, role, name, text, box,
    /// styles; and where in the app's source it's from, when a dev build
    /// says (only on this machine).
    pub fn describe(&self, x: f64, y: f64, region: Option<[f64; 4]>) -> Result<Value> {
        self.alive()?;
        let world = self.world()?;
        let region_value = region.map_or(Value::Null, |r| {
            Value::Arr(r.iter().map(|&n| Value::from(n)).collect())
        });
        let picked = self.call(
            "Runtime.callFunctionOn",
            Value::obj()
                .with("functionDeclaration", PICK_JS)
                .with("executionContextId", world as f64)
                .with(
                    "arguments",
                    vec![
                        Value::obj().with("value", x),
                        Value::obj().with("value", y),
                        Value::obj().with("value", region_value),
                    ],
                )
                .with("objectGroup", "pepper-agent"),
        )?;
        let object = picked
            .get("result")
            .and_then(|r| r.get_str("objectId"))
            .map(str::to_owned);
        let Some(object) = object else {
            return Ok(Value::Null);
        };
        let described = (|| {
            let info = result_value(
                &self.call(
                    "Runtime.callFunctionOn",
                    Value::obj()
                        .with("functionDeclaration", DESCRIBE_JS)
                        .with("objectId", object.as_str())
                        .with("returnByValue", true),
                )?,
            )?;
            let mut info = info;
            let backend = self
                .call(
                    "DOM.describeNode",
                    Value::obj().with("objectId", object.as_str()),
                )?
                .get("node")
                .and_then(|n| n.get_num("backendNodeId"));
            if let Some(backend) = backend {
                if let Ok(ax) = self.call(
                    "Accessibility.getPartialAXTree",
                    Value::obj()
                        .with("backendNodeId", backend)
                        .with("fetchRelatives", false),
                ) && let Some(node) =
                    ax.get("nodes").and_then(Value::arr).and_then(|n| n.first())
                {
                    info.set("role", ax_text(node.get("role")));
                    info.set("name", clip(&ax_text(node.get("name")), 200));
                }
                if is_local(&self.state().url)
                    && let Some(source) = self.app_source(backend)
                {
                    info.set("source", source);
                }
            }
            Ok(info)
        })();
        self.conn.send(
            "Runtime.releaseObjectGroup",
            Value::obj().with("objectGroup", "pepper-agent"),
            Some(&self.session),
        );
        described
    }

    /// Where a dev build says an element comes from (React, Vue, Svelte, or
    /// an inspector plugin's attribute). In the page's own world: only for
    /// pages on this machine, the user's own app.
    fn app_source(&self, backend: f64) -> Option<Value> {
        let object = self
            .call(
                "DOM.resolveNode",
                Value::obj()
                    .with("backendNodeId", backend)
                    .with("objectGroup", "pepper-agent"),
            )
            .ok()?;
        let object = object.get("object")?.get_str("objectId")?.to_owned();
        let answer = self
            .call(
                "Runtime.callFunctionOn",
                Value::obj()
                    .with("functionDeclaration", SOURCE_JS)
                    .with("objectId", object.as_str())
                    .with("returnByValue", true),
            )
            .ok()?;
        result_value(&answer).ok().filter(|v| !v.is_null())
    }

    /// For a point the human made: what's there, a picture of the viewport
    /// and one of the part around it, into `dir`.
    pub fn capture(
        &self,
        x: f64,
        y: f64,
        region: Option<[f64; 4]>,
        dir: &Path,
        stem: &str,
    ) -> Result<Value> {
        self.alive()?;
        let element = self.describe(x, y, region).unwrap_or(Value::Null);
        let metrics = self.call("Page.getLayoutMetrics", Value::obj())?;
        let view = metrics.get("cssVisualViewport").unwrap_or(&Value::Null);
        let (page_x, page_y) = (
            view.get_num("pageX").unwrap_or(0.0),
            view.get_num("pageY").unwrap_or(0.0),
        );
        let device = self.state().device.clone();
        let (vw, vh) = (device.width as f64, device.height as f64);
        // The part around: the region, or the element under the point (if
        // it isn't most of the page), or a window round the point.
        let near = match region {
            Some([rx, ry, rw, rh]) => pad((rx, ry, rw, rh), 12.0),
            None => {
                let rect = element.get("rect").map(|r| {
                    (
                        r.get_num("x").unwrap_or(0.0),
                        r.get_num("y").unwrap_or(0.0),
                        r.get_num("width").unwrap_or(0.0),
                        r.get_num("height").unwrap_or(0.0),
                    )
                });
                match rect {
                    Some(r) if r.2 >= 2.0 && r.3 >= 2.0 && r.2 * r.3 < vw * vh * 0.5 => {
                        pad(r, 24.0)
                    }
                    _ => (x - 200.0, y - 120.0, 400.0, 240.0),
                }
            }
        };
        let crop = clamp_rect(near, vw, vh);
        let scale = |w: f64, h: f64| (MAX_SIDE / (w.max(h) * device.dpr)).min(1.0);
        let full_path = dir.join(format!("{stem}-full.png"));
        let crop_path = dir.join(format!("{stem}-crop.png"));
        let shot = |path: &Path, (cx, cy, cw, ch): (f64, f64, f64, f64)| {
            self.screenshot(
                Value::obj().with("format", "png").with(
                    "clip",
                    Value::obj()
                        .with("x", page_x + cx)
                        .with("y", page_y + cy)
                        .with("width", cw)
                        .with("height", ch)
                        .with("scale", scale(cw, ch)),
                ),
                path,
            )
        };
        shot(&full_path, (0.0, 0.0, vw, vh))?;
        shot(&crop_path, crop)?;
        // Chrome's own scale (2) can make them bigger than asked.
        for path in [&full_path, &crop_path] {
            if media::png_size(path).is_some_and(|(w, h)| w.max(h) > MAX_SIDE as u32) {
                let smaller = path.with_extension("small.png");
                media::shrink_png(path, &smaller, None, MAX_SIDE as u32)?;
                std::fs::rename(&smaller, path)
                    .map_err(|e| Error::new("FAILED", format!("{}: {e}", path.display())))?;
            }
        }
        Ok(Value::obj()
            .with("element", element)
            .with("snapshot", full_path.display().to_string())
            .with("crop", crop_path.display().to_string())
            .with(
                "crop_rect",
                vec![
                    round1(crop.0),
                    round1(crop.1),
                    round1(crop.2),
                    round1(crop.3),
                ],
            ))
    }
}

fn pad((x, y, w, h): (f64, f64, f64, f64), by: f64) -> (f64, f64, f64, f64) {
    (x - by, y - by, w + 2.0 * by, h + 2.0 * by)
}

/// A rectangle inside `w`×`h`, at least 16 pixels each way.
fn clamp_rect((x, y, rw, rh): (f64, f64, f64, f64), w: f64, h: f64) -> (f64, f64, f64, f64) {
    let rw = rw.clamp(16.0_f64.min(w), w);
    let rh = rh.clamp(16.0_f64.min(h), h);
    (x.clamp(0.0, w - rw), y.clamp(0.0, h - rh), rw, rh)
}

fn round1(n: f64) -> f64 {
    (n * 10.0).round() / 10.0
}

fn ax_text(value: Option<&Value>) -> String {
    match value.and_then(|v| v.get("value")) {
        Some(Value::Str(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_line(),
    }
}

fn a11y_lines(
    node: &Value,
    by_id: &HashMap<&str, &Value>,
    depth: usize,
    parent_name: &str,
    max: usize,
    out: &mut Vec<String>,
    truncated: &mut bool,
) {
    if out.len() >= max {
        *truncated = true;
        return;
    }
    let role = ax_text(node.get("role"));
    let name = ax_text(node.get("name"));
    // Text boxes repeat their text's.
    if role == "InlineTextBox" {
        return;
    }
    let ignored = node.get_bool("ignored") == Some(true);
    // Nodes with nothing to say are left out, and text that only repeats
    // its parent's name; their children move up.
    let plain = ignored
        || (name.is_empty() && matches!(role.as_str(), "generic" | "none" | "LineBreak" | ""))
        || (role == "StaticText" && name == parent_name);
    let child_depth = if plain { depth } else { depth + 1 };
    if !plain {
        let mut line = format!("{}{role}", "  ".repeat(depth));
        if !name.is_empty() {
            line += &format!(" {:?}", clip(&name, 120));
        }
        let value = ax_text(node.get("value"));
        if !value.is_empty() {
            line += &format!(" = {:?}", clip(&value, 80));
        }
        out.push(line);
    }
    let name_below = if plain { parent_name } else { &name };
    for child in node.get("childIds").and_then(Value::arr).unwrap_or(&[]) {
        if let Some(child) = child.str().and_then(|id| by_id.get(id)) {
            a11y_lines(child, by_id, child_depth, name_below, max, out, truncated);
        }
    }
}

/// The value of a Runtime call's answer, or its exception as an error.
fn result_value(answer: &Value) -> Result<Value> {
    if let Some(details) = answer.get("exceptionDetails") {
        let text = details
            .get("exception")
            .and_then(|e| e.get_str("description"))
            .or_else(|| details.get_str("text"))
            .unwrap_or("an exception");
        let code = if text.contains("is not a valid selector") {
            "BAD_ARGS"
        } else {
            "FAILED"
        };
        return Err(Error::new(
            code,
            format!("in the page: {}", clip(text, 500)),
        ));
    }
    Ok(answer
        .get("result")
        .and_then(|r| r.get("value"))
        .cloned()
        .unwrap_or(Value::Null))
}

fn no_match(selector: &str) -> Error {
    Error::new(
        "NOT_FOUND",
        format!("no element matches {}", text::inert(selector, 200)),
    )
    .hint("Check the selector with `pepper-agent web dom <selector>`, or look with `pepper-agent web shot`.")
}

fn bad_selector(selector: &str) -> Error {
    Error::new(
        "BAD_ARGS",
        format!("{} isn't a CSS selector", text::inert(selector, 200)),
    )
}

fn element_center(selector: &str, found: Value) -> Result<(f64, f64, Value)> {
    if found.is_null() {
        return Err(no_match(selector));
    }
    let r = found.get("view").unwrap_or(&Value::Null);
    let (x, y, w, h) = (
        r.get_num("x").unwrap_or(0.0),
        r.get_num("y").unwrap_or(0.0),
        r.get_num("width").unwrap_or(0.0),
        r.get_num("height").unwrap_or(0.0),
    );
    if w < 1.0 || h < 1.0 {
        return Err(Error::new(
            "FAILED",
            format!("{} isn't visible (no size)", text::inert(selector, 200)),
        ));
    }
    let element = Value::obj()
        .with("tag", found.get_str("tag").unwrap_or(""))
        .with("text", found.get_str("text").unwrap_or(""))
        .with("matches", found.get_num("matches").unwrap_or(1.0));
    Ok((x + w / 2.0, y + h / 2.0, element))
}

fn parse_xy(text: &str) -> Option<(f64, f64)> {
    let (x, y) = text.split_once(',')?;
    let (x, y) = (x.trim().parse::<f64>().ok()?, y.trim().parse::<f64>().ok()?);
    (x.is_finite() && y.is_finite()).then_some((x, y))
}

/// A key as `web press` takes it: a name or a character, with modifiers.
#[derive(Debug, PartialEq)]
struct KeyPress {
    key: String,
    code: String,
    text: String,
    key_code: u32,
    modifiers: u32,
}

impl KeyPress {
    fn parse(spec: &str) -> Option<KeyPress> {
        let mut parts: Vec<&str> = spec.split('+').collect();
        let mut name = parts.pop()?;
        if name.is_empty() && spec.ends_with("++") {
            parts.pop();
            name = "+";
        }
        let mut modifiers = 0;
        for m in parts {
            modifiers |= match m.to_ascii_lowercase().as_str() {
                "alt" | "option" => 1,
                "control" | "ctrl" => 2,
                "meta" | "cmd" | "command" => 4,
                "shift" => 8,
                _ => return None,
            };
        }
        let named: Option<(&str, &str, u32, &str)> = match name.to_ascii_lowercase().as_str() {
            "enter" | "return" => Some(("Enter", "Enter", 13, "\r")),
            "tab" => Some(("Tab", "Tab", 9, "")),
            "escape" | "esc" => Some(("Escape", "Escape", 27, "")),
            "backspace" => Some(("Backspace", "Backspace", 8, "")),
            "delete" => Some(("Delete", "Delete", 46, "")),
            "arrowup" | "up" => Some(("ArrowUp", "ArrowUp", 38, "")),
            "arrowdown" | "down" => Some(("ArrowDown", "ArrowDown", 40, "")),
            "arrowleft" | "left" => Some(("ArrowLeft", "ArrowLeft", 37, "")),
            "arrowright" | "right" => Some(("ArrowRight", "ArrowRight", 39, "")),
            "home" => Some(("Home", "Home", 36, "")),
            "end" => Some(("End", "End", 35, "")),
            "pageup" => Some(("PageUp", "PageUp", 33, "")),
            "pagedown" => Some(("PageDown", "PageDown", 34, "")),
            "space" => Some((" ", "Space", 32, " ")),
            _ => None,
        };
        if let Some((key, code, key_code, text)) = named {
            return Some(KeyPress {
                key: key.into(),
                code: code.into(),
                text: if modifiers & 7 != 0 {
                    String::new()
                } else {
                    text.into()
                },
                key_code,
                modifiers,
            });
        }
        if let Some(n) = name
            .strip_prefix(['F', 'f'])
            .and_then(|n| n.parse::<u32>().ok())
            .filter(|n| (1..=12).contains(n))
        {
            return Some(KeyPress {
                key: format!("F{n}"),
                code: format!("F{n}"),
                text: String::new(),
                key_code: 111 + n,
                modifiers,
            });
        }
        let mut chars = name.chars();
        let c = chars.next()?;
        if chars.next().is_some() || c.is_control() {
            return None;
        }
        let (code, key_code) = match c {
            'a'..='z' | 'A'..='Z' => (
                format!("Key{}", c.to_ascii_uppercase()),
                c.to_ascii_uppercase() as u32,
            ),
            '0'..='9' => (format!("Digit{c}"), c as u32),
            _ => (String::new(), 0),
        };
        let shown = if modifiers & 8 != 0 {
            c.to_uppercase().next().unwrap_or(c)
        } else {
            c
        };
        Some(KeyPress {
            key: shown.to_string(),
            code,
            text: if modifiers & 7 != 0 {
                String::new()
            } else {
                shown.to_string()
            },
            key_code,
            modifiers,
        })
    }
}

/// `(selector, scroll)`: the first element a selector finds, scrolled into
/// view if asked; its box in the viewport and on the page.
const FIND_JS: &str = r#"function(selector, scroll) {
  const el = document.querySelector(selector);
  if (!el) return null;
  if (scroll) el.scrollIntoView({block: 'center', inline: 'center', behavior: 'instant'});
  const r = el.getBoundingClientRect();
  return {
    tag: el.localName,
    text: (el.innerText || el.textContent || '').trim().replace(/\s+/g, ' ').slice(0, 80),
    matches: document.querySelectorAll(selector).length,
    view: {x: r.x, y: r.y, width: r.width, height: r.height},
    page: {x: r.x + scrollX, y: r.y + scrollY, width: r.width, height: r.height},
  };
}"#;

/// `(selector, clear)`: focuses an element to type into, its text selected
/// if it's to be replaced.
const FOCUS_JS: &str = r#"function(selector, clear) {
  const el = document.querySelector(selector);
  if (!el) return null;
  el.scrollIntoView({block: 'center', behavior: 'instant'});
  el.focus();
  if (clear) {
    if (typeof el.select === 'function') el.select();
    else { const range = document.createRange(); range.selectNodeContents(el); const s = getSelection(); s.removeAllRanges(); s.addRange(range); }
  }
  return {tag: el.localName, type: el.getAttribute('type'), focused: document.activeElement === el};
}"#;

/// `(to)`: scrolls to the top or the bottom (or not at all); where it is.
const SCROLL_JS: &str = r#"function(to) {
  const s = document.scrollingElement || document.documentElement;
  if (to === 'top') scrollTo({top: 0, behavior: 'instant'});
  if (to === 'bottom') scrollTo({top: s.scrollHeight, behavior: 'instant'});
  return {scroll_x: Math.round(scrollX), scroll_y: Math.round(scrollY), page_height: s.scrollHeight, viewport_height: innerHeight};
}"#;

/// `(selector, max)`: the elements a selector finds, as HTML.
const DOM_JS: &str = r#"function(selector, max) {
  const all = document.querySelectorAll(selector);
  let html = '';
  for (const el of all) { html += el.outerHTML + '\n'; if (html.length > max) break; }
  return {selector, matches: all.length, html: html.slice(0, max), truncated: html.length > max};
}"#;

/// `(x, y, region)`: the element at a point (into open shadow roots), or,
/// for a region, the smallest one around it that holds it all.
const PICK_JS: &str = r#"function(x, y, region) {
  let el = document.elementFromPoint(x, y);
  while (el && el.shadowRoot) {
    const inner = el.shadowRoot.elementFromPoint(x, y);
    if (!inner || inner === el) break;
    el = inner;
  }
  if (el && region) {
    const [rx, ry, rw, rh] = region;
    for (let n = el; n && n.nodeType === 1; n = n.parentElement) {
      const r = n.getBoundingClientRect();
      if (r.left <= rx + 1 && r.top <= ry + 1 && r.right >= rx + rw - 1 && r.bottom >= ry + rh - 1) { el = n; break; }
    }
  }
  return el;
}"#;

/// An element, described: a selector that finds only it, what it is, its
/// box, the styles that place it, and its parent's.
const DESCRIBE_JS: &str = r#"function() {
  const el = this;
  const esc = (s) => CSS.escape(s);
  const unique = (s) => { try { return document.querySelectorAll(s).length === 1; } catch (e) { return false; } };
  function selector(el) {
    if (el.id && unique('#' + esc(el.id))) return '#' + esc(el.id);
    for (const a of ['data-testid', 'data-test', 'data-cy', 'data-qa', 'name', 'aria-label']) {
      const v = el.getAttribute(a);
      if (v) { const s = el.localName + '[' + a + '="' + esc(v) + '"]'; if (unique(s)) return s; }
    }
    const parts = [];
    for (let n = el; n && n.nodeType === 1; n = n.parentElement) {
      if (n !== el && n.id && unique('#' + esc(n.id))) { parts.unshift('#' + esc(n.id)); break; }
      let part = n.localName;
      const classes = [...n.classList].filter((c) => /^[A-Za-z_-][\w-]*$/.test(c)).slice(0, 3);
      if (classes.length) part += '.' + classes.map(esc).join('.');
      const p = n.parentElement;
      if (p) {
        let same = 2;
        try { same = p.querySelectorAll(':scope > ' + part).length; } catch (e) {}
        if (same > 1) part += ':nth-of-type(' + ([...p.children].filter((c) => c.localName === n.localName).indexOf(n) + 1) + ')';
      }
      parts.unshift(part);
      const s = parts.join(' > ');
      if (unique(s)) return s;
    }
    return parts.join(' > ');
  }
  const round = (v) => Math.round(v * 10) / 10;
  const box = (n) => { const r = n.getBoundingClientRect(); return {x: round(r.x), y: round(r.y), width: round(r.width), height: round(r.height)}; };
  const cs = getComputedStyle(el);
  const styles = {};
  for (const name of ['display', 'position', 'box-sizing', 'width', 'height', 'margin', 'padding', 'border', 'font-family', 'font-size', 'font-weight', 'line-height', 'color', 'background-color', 'text-align', 'align-items', 'justify-content', 'gap']) {
    const v = cs.getPropertyValue(name);
    if (v) styles[name] = v.slice(0, 120);
  }
  const parent = el.parentElement;
  return {
    selector: selector(el).slice(0, 500),
    tag: el.localName,
    id: el.id || null,
    classes: [...el.classList].slice(0, 20),
    text: (el.innerText || el.textContent || '').trim().replace(/\s+/g, ' ').slice(0, 200),
    rect: box(el),
    styles,
    parent: parent && parent !== document.documentElement ? {selector: selector(parent).slice(0, 500), rect: box(parent)} : null,
    href: el.getAttribute('href'),
  };
}"#;

/// Where a dev build says an element is made: React's debug source (17,
/// 18) or the first app frame of its debug stack (19), Vue's component
/// file, Svelte's location, or an inspector plugin's attribute.
const SOURCE_JS: &str = r#"function() {
  const out = (file, line, column, from) => file ? {file: String(file).slice(0, 300), line: line ?? null, column: column ?? null, from} : null;
  for (let el = this; el && el.nodeType === 1; el = el.parentElement) {
    const key = Object.keys(el).find((k) => k.startsWith('__reactFiber$'));
    if (key) {
      for (let f = el[key]; f; f = f.return) {
        const s = f._debugSource;
        if (s && s.fileName) return out(s.fileName, s.lineNumber, s.columnNumber, 'react');
        const stack = f._debugStack && f._debugStack.stack;
        if (typeof stack === 'string') {
          for (const line of stack.split('\n').slice(1)) {
            const m = line.match(/\(?((?:https?|file):\/\/[^\s)]+?):(\d+):(\d+)\)?\s*$/);
            if (m && !/node_modules|\/\.vite\/|\/@fs\/|\/@react-refresh|react-dom|jsx-runtime|jsx-dev-runtime|\/_next\/static\/chunks\/(?:node_modules|webpack)/.test(m[1])) {
              let file = m[1];
              try { file = new URL(file).pathname; } catch (e) {}
              return out(file, Number(m[2]), Number(m[3]), 'react-stack');
            }
          }
        }
      }
    }
    const vue = el.__vueParentComponent;
    if (vue && vue.type && vue.type.__file) return out(vue.type.__file, null, null, 'vue');
    const svelte = el.__svelte_meta;
    if (svelte && svelte.loc) return out(svelte.loc.file, svelte.loc.line, svelte.loc.column, 'svelte');
    for (const a of el.attributes) {
      if (/^data-(inspector-relative-path|source|loc|locator-id|component-path)/.test(a.name)) return out(a.value, null, null, a.name);
    }
  }
  return null;
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices() {
        let d = Device::parse("iphone-15").unwrap();
        assert_eq!((d.width, d.height, d.dpr, d.mobile), (393, 852, 3.0, true));
        let d = Device::parse("1280x720@2").unwrap();
        assert_eq!(
            (d.width, d.height, d.dpr, d.mobile),
            (1280, 720, 2.0, false)
        );
        assert_eq!(Device::parse("800x600").unwrap().dpr, 1.0);
        for bad in ["", "big", "10x10", "800x600@9", "800x", "x600", "1e3x600"] {
            assert!(Device::parse(bad).is_none(), "{bad}");
        }
        assert!(Device::parse(DEFAULT_DEVICE).is_some());
    }

    #[test]
    fn only_this_machine_by_default() {
        for local in [
            "http://localhost:5173/",
            "http://LOCALHOST",
            "https://app.localhost:3000/x",
            "http://127.0.0.1:8080",
            "http://127.1.2.3/",
            "http://[::1]:3000/",
            "http://user:pw@localhost:3000/",
            "file:///Users/me/site/index.html",
            "about:blank",
        ] {
            assert!(is_local(local), "{local}");
        }
        for remote in [
            "https://example.com/",
            "http://localhost.example.com/",
            "http://evil.com/?localhost",
            "http://evil.com#@localhost",
            "http://localhost@evil.com/",
            "http://192.168.1.2:3000/",
            "http://[::2]/",
            "http://0.0.0.0:3000/",
            "ftp://localhost/",
            "javascript:alert(1)",
            "localhost:3000",
        ] {
            assert!(!is_local(remote), "{remote}");
        }
    }

    #[test]
    fn urls_are_checked() {
        assert!(check_url("http://localhost:3000/a?b=c").is_ok());
        assert!(check_url("about:blank").is_ok());
        for bad in [
            "javascript:alert(1)",
            "data:text/html,x",
            "http://a b",
            "http://a\x1b[2J",
        ] {
            assert_eq!(check_url(bad).unwrap_err().code, "BAD_ARGS", "{bad}");
        }
    }

    #[test]
    fn keys() {
        let enter = KeyPress::parse("Enter").unwrap();
        assert_eq!(
            (enter.key.as_str(), enter.key_code, enter.text.as_str()),
            ("Enter", 13, "\r")
        );
        let select_all = KeyPress::parse("Meta+a").unwrap();
        assert_eq!(
            (
                select_all.code.as_str(),
                select_all.modifiers,
                select_all.text.as_str()
            ),
            ("KeyA", 4, "")
        );
        let shifted = KeyPress::parse("Shift+b").unwrap();
        assert_eq!((shifted.key.as_str(), shifted.text.as_str()), ("B", "B"));
        assert_eq!(KeyPress::parse("F5").unwrap().key_code, 116);
        assert_eq!(KeyPress::parse("Control++").unwrap().key, "+");
        assert!(KeyPress::parse("Hyper+a").is_none());
        assert!(KeyPress::parse("word").is_none());
        assert!(KeyPress::parse("\x1b").is_none());
    }

    #[test]
    fn crops_stay_in_the_viewport() {
        assert_eq!(
            clamp_rect((-10.0, -5.0, 100.0, 50.0), 800.0, 600.0),
            (0.0, 0.0, 100.0, 50.0)
        );
        assert_eq!(
            clamp_rect((780.0, 590.0, 100.0, 50.0), 800.0, 600.0),
            (700.0, 550.0, 100.0, 50.0)
        );
        assert_eq!(
            clamp_rect((5.0, 5.0, 2.0, 2.0), 800.0, 600.0),
            (5.0, 5.0, 16.0, 16.0)
        );
        assert_eq!(
            clamp_rect((0.0, 0.0, 2000.0, 2000.0), 800.0, 600.0),
            (0.0, 0.0, 800.0, 600.0)
        );
    }

    #[test]
    fn points() {
        assert_eq!(parse_xy("12,34.5"), Some((12.0, 34.5)));
        assert_eq!(parse_xy(" 1 , 2 "), Some((1.0, 2.0)));
        assert_eq!(parse_xy("#a,#b"), None);
        assert_eq!(parse_xy("nan,1"), None);
    }
}
