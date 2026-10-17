//! The viewer's web sources: opening a page (the browser starts with the
//! first), the agent's `web` commands, each run on a thread of its own and
//! answered when done, and what the box and the keys do for a page.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::frames::{FrameMsg, Presenter};
use super::watch::Watch;
use super::web::{self, Device, Files, Host, OpenAsk, Page};
use super::{Cells, Msg, Request, Source, State, Viewer, WaitFor, valid_name};
use crate::cdp;
use crate::error::{Error, Result};
use crate::json::Value;
use crate::text;

/// How long a web command waits unless it says otherwise.
const WEB_WAIT: Duration = Duration::from_secs(40);
/// The longest `web wait-for`.
pub const MAX_WAIT_FOR: f64 = 120.0;
/// A burst of changes under --watch reloads once, this long after the last.
const RELOAD_AFTER: Duration = Duration::from_millis(150);

pub struct WebSource {
    /// None while it opens.
    pub page: Option<Arc<Page>>,
    presenter: Option<Presenter>,
    pub device: Device,
    pub dark: bool,
    pub allow_remote: bool,
    _watch: Option<Watch>,
    pub watch_dir: Option<PathBuf>,
    reload_at: Option<Instant>,
    /// The size the screencast runs at.
    cast: Option<(u32, u32)>,
    /// The box, in pixels.
    box_px: Option<(u32, u32)>,
}

impl WebSource {
    /// What the label says after the name and status: where it is, its
    /// title.
    pub fn label(&self) -> String {
        let Some(page) = &self.page else {
            return String::new();
        };
        let state = page.state();
        let url = text::redact_url(&state.url);
        let short = url.split_once("://").map(|(_, rest)| rest).unwrap_or(&url);
        let mut out = format!("  {}", text::inert(short, 60));
        if !state.title.is_empty() {
            out += &format!("  {}", text::inert(&state.title, 60));
        }
        out
    }

    pub fn to_json(&self, source: &Source, focused: bool, visible: bool) -> Value {
        let mut v = Value::obj()
            .with("name", source.name.as_str())
            .with("kind", "web")
            .with("target", source.shown_target())
            .with("status", source.status())
            .with("playable", false);
        if let Some(page) = &self.page {
            let state = page.state();
            v.set("url", text::redact_url(&state.url));
            v.set("title", state.title.as_str());
            v.set(
                "latency_ms",
                Value::obj()
                    .with("input_to_frame", state.input_to_frame_ms)
                    .with("input_to_shown", state.input_to_shown_ms),
            );
            let mut frames = Value::obj().with("received", state.frames);
            if let Some(presenter) = &self.presenter {
                let stats = presenter
                    .stats
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                frames.set("shown", stats.shown);
                frames.set("dropped", stats.dropped);
                if stats.shown > 0 {
                    frames.set(
                        "draw_ms_avg",
                        (stats.draw_ms / stats.shown as f64 * 100.0).round() / 100.0,
                    );
                }
            }
            v.set("frames", frames);
        }
        v.set("viewport", self.device.to_json());
        v.set("dark", self.dark);
        v.set("allow_remote", self.allow_remote);
        v.set(
            "watching",
            self.watch_dir.as_ref().map(|d| d.display().to_string()),
        );
        v.set("width", self.device.width);
        v.set("height", self.device.height);
        v.set("focused", focused);
        v.set("owner", source.owner.as_deref());
        v.set("visible", visible);
        v.set(
            "box",
            source
                .placed
                .map(|c| vec![c.col as u32, c.row as u32, c.cols as u32, c.rows as u32]),
        );
        if let Some(error) = source.error() {
            v.set("error", error);
        }
        v
    }

    /// Closes the page and takes its stream away from Pepper.
    pub fn close(self) {
        if let Some(page) = &self.page {
            page.close();
        }
        if let Some(presenter) = self.presenter {
            presenter.stop();
        }
    }
}

impl Viewer {
    /// Opens a page: a source now, loading; the page on a thread, which
    /// says when it's there (`Msg::WebOpened`).
    pub(super) fn start_web(&mut self, args: &Value) -> Result<u64> {
        let target = args
            .get_str("target")
            .ok_or_else(|| Error::new("BAD_ARGS", "web open: which address?"))?;
        web::check_url(target)?;
        let allow_remote = args.get_bool("allow_remote").unwrap_or(false);
        if !allow_remote && !web::is_local(target) {
            return Err(Error::new(
                "ORIGIN_BLOCKED",
                format!(
                    "{} isn't on this machine",
                    text::inert(&text::redact_url(target), 200)
                ),
            ));
        }
        let device = match args.get_str("device") {
            Some(name) => Device::parse(name).ok_or_else(|| bad_device(name))?,
            None => Device::parse(web::DEFAULT_DEVICE).unwrap_or_else(|| unreachable!()),
        };
        let dark = args.get_bool("dark").unwrap_or(false);
        let watch_dir = match args.get_str("watch") {
            Some(dir) => {
                let dir = PathBuf::from(dir);
                if !dir.is_absolute() || !dir.is_dir() {
                    return Err(Error::new(
                        "NOT_FOUND",
                        format!(
                            "no folder at {}",
                            text::inert(&dir.display().to_string(), 300)
                        ),
                    ));
                }
                Some(dir)
            }
            None => None,
        };
        let name = args.get_str("name");
        if let Some(name) = name
            && !valid_name(name)
        {
            return Err(Error::new(
                "BAD_ARGS",
                format!("{:?} isn't a source name", text::inert(name, 70)),
            ));
        }
        let (session, owner) = self.claim(args)?;
        // The same name again: that page goes to the new address.
        if let Some(i) = name.and_then(|n| self.sources.iter().position(|s| s.name == n)) {
            self.check_owner(i, args)?;
            if let Some(page) = self.sources[i].web.as_ref().and_then(|w| w.page.clone())
                && page.allow_remote == allow_remote
            {
                let source = &mut self.sources[i];
                source.target = target.to_owned();
                source.state = State::Loading;
                let sid = source.sid;
                let (tx, target) = (self.tx.clone(), target.to_owned());
                let ua_device = device.clone();
                let host = self.host.clone();
                std::thread::spawn(move || {
                    let ua = host
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_ref()
                        .map(|h| h.user_agent().to_owned())
                        .unwrap_or_default();
                    let result = page
                        .emulate(&ua_device, dark, &ua)
                        .and_then(|_| page.navigate(&target))
                        .map(|_| page.clone());
                    let _ = tx.send(Msg::WebOpened(sid, result));
                });
                self.sources[i].size = (Some(device.width as f64), Some(device.height as f64));
                if let Some(web) = &mut self.sources[i].web {
                    web.device = device;
                    web.dark = dark;
                }
                self.focus = Some(i);
                self.record_dirty = true;
                self.relayout();
                return Ok(sid);
            }
            self.remove_source(i);
        }
        if self.sources.len() >= super::MAX_SOURCES {
            return Err(Error::new(
                "LIMIT_REACHED",
                format!("this viewer already shows {} sources", super::MAX_SOURCES),
            ));
        }
        let chrome = cdp::find_chrome().ok_or_else(|| {
            Error::new("MISSING_DEP", "no Chrome or Chromium was found").hint(
                "Install Google Chrome (or Chromium), or set PEPPER_CHROME to one's program, then open the page again.",
            )
        })?;
        let name = match name {
            Some(n) => n.to_owned(),
            None => self.web_name(target),
        };
        let token =
            crate::sys::random_hex().map_err(|e| Error::new("FAILED", format!("a token: {e}")))?;
        let sid = self.next_sid;
        self.next_sid += 1;
        let presenter = Presenter::start(self.pepper.clone(), token.clone(), self.log.clone());
        let frames = presenter.sender();
        let watch = watch_dir
            .as_ref()
            .and_then(|dir| Watch::start(dir, sid, self.tx.clone()));
        self.sources.push(Source {
            sid,
            name,
            kind: "web",
            target: target.to_owned(),
            token,
            player: None,
            state: State::Loading,
            time_pos: None,
            duration: None,
            paused: true,
            eof: false,
            buffering: false,
            size: (Some(device.width as f64), Some(device.height as f64)),
            hwdec: None,
            review_to: None,
            loop_ab: None,
            placed: None,
            log: Vec::new(),
            web: Some(WebSource {
                page: None,
                presenter: Some(presenter),
                device: device.clone(),
                dark,
                allow_remote,
                _watch: watch,
                watch_dir,
                reload_at: None,
                cast: None,
                box_px: None,
            }),
            edit: None,
            session,
            owner,
            fps: None,
            scrub: super::seekbar::Scrub::default(),
        });
        self.focus = Some(self.sources.len() - 1);
        self.record_dirty = true;
        self.relayout();
        let box_px = self
            .sources
            .last()
            .and_then(|s| s.web.as_ref())
            .and_then(|w| w.box_px)
            .unwrap_or((1600, 1000));
        if let Some(web) = self.sources.last_mut().and_then(|s| s.web.as_mut()) {
            web.cast = Some(box_px);
        }
        let ask = OpenAsk {
            url: target.to_owned(),
            device,
            dark,
            allow_remote,
            box_px,
        };
        let host = self.host.clone();
        let profile = self.dir.join(format!("{}-chrome-{sid}", self.id));
        let log = self.log.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = (|| {
                let host = {
                    let mut slot = host.lock().unwrap_or_else(|e| e.into_inner());
                    match &*slot {
                        Some(h) if h.alive() => h.clone(),
                        _ => {
                            let h = Host::launch(&chrome, &profile, log.as_deref())?;
                            *slot = Some(h.clone());
                            h
                        }
                    }
                };
                web::open(&host, &ask, sid, tx.clone(), frames)
            })();
            let _ = tx.send(Msg::WebOpened(sid, result));
        });
        Ok(sid)
    }

    /// A name from the address: its host's first part, or its file's.
    fn web_name(&self, target: &str) -> String {
        let rest = target.split_once("://").map(|(_, r)| r).unwrap_or(target);
        let host = rest.split(['/', '?', '#']).next().unwrap_or("");
        let host = host.rsplit('@').next().unwrap_or("");
        let base = if target.starts_with("file:") {
            rest.trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("")
                .split('.')
                .next()
                .unwrap_or("")
        } else {
            host.split([':', '.']).next().unwrap_or("")
        };
        let base: String = base
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || "_-".contains(*c))
            .take(32)
            .collect();
        let base = if base.is_empty() || base == "localhost" || base == "127" {
            "web".to_owned()
        } else {
            base
        };
        let taken = |n: &str| self.sources.iter().any(|s| s.name == n);
        if !taken(&base) {
            return base;
        }
        (2..)
            .map(|k| format!("{base}-{k}"))
            .find(|n| !taken(n))
            .unwrap_or(base)
    }

    pub(super) fn on_web_opened(&mut self, sid: u64, result: Result<Arc<Page>>) {
        let Some(i) = self.sources.iter().position(|s| s.sid == sid) else {
            // Closed while it opened.
            if let Ok(page) = result {
                page.close();
            }
            self.close_browser_if_unused();
            return;
        };
        match result {
            Ok(page) => {
                let source = &mut self.sources[i];
                source.state = State::Ready;
                if let Some(web) = &mut source.web {
                    web.page = Some(page);
                }
                self.record_dirty = true;
                self.dirty = true;
                // The box may have moved while it opened.
                self.web_box_changed(sid);
                self.on_restart(sid);
            }
            Err(e) => {
                self.sources[i].state = State::Failed(e.message.clone());
                self.record_dirty = true;
                self.dirty = true;
                self.fail_waits(sid, &e);
            }
        }
    }

    pub(super) fn on_web(&mut self, sid: u64) {
        if let Some(source) = self.source_mut(sid)
            && let Some(page) = source.web.as_ref().and_then(|w| w.page.as_ref())
            && let Some(why) = page.state().crashed.clone()
            && source.state == State::Ready
        {
            source.state = State::Failed(why);
        }
        self.dirty = true;
        self.record_dirty = true;
    }

    /// Where a web source's box is now: the screencast sends frames no
    /// larger (Pepper fits them to it).
    pub(super) fn web_box_changed(&mut self, sid: u64) {
        let (cw, ch) = self.cell();
        let Some(source) = self.source_mut(sid) else {
            return;
        };
        let placed = source.placed;
        let Some(web) = &mut source.web else { return };
        let Some(at) = placed else { return };
        let px = (
            (at.cols as f64 * cw).round() as u32,
            (at.rows as f64 * ch).round() as u32,
        );
        web.box_px = Some(px);
        if let Some(page) = &web.page
            && web.cast != Some(px)
        {
            web.cast = Some(px);
            let page = page.clone();
            std::thread::spawn(move || page.screencast(px));
        }
    }

    pub(super) fn on_box_size(&mut self, token: &str, size: crate::pepper::BoxSize) {
        if let Some(web) = self
            .sources
            .iter()
            .find(|s| s.token == token)
            .and_then(|s| s.web.as_ref())
            && let Some(presenter) = &web.presenter
        {
            presenter.send(FrameMsg::Size(size));
        }
    }

    pub(super) fn on_watch(&mut self, sid: u64) {
        if let Some(web) = self.source_mut(sid).and_then(|s| s.web.as_mut()) {
            web.reload_at = Some(Instant::now() + RELOAD_AFTER);
        }
    }

    pub(super) fn reload_watched(&mut self) {
        let now = Instant::now();
        let due: Vec<Arc<Page>> = self
            .sources
            .iter_mut()
            .filter_map(|s| s.web.as_mut())
            .filter(|w| w.reload_at.is_some_and(|at| at <= now))
            .filter_map(|w| {
                w.reload_at = None;
                w.page.clone()
            })
            .collect();
        for page in due {
            std::thread::spawn(move || page.reload());
        }
    }

    pub(super) fn focused_web(&self) -> Option<usize> {
        self.focused().filter(|&i| self.sources[i].web.is_some())
    }

    fn page_of(&self, i: usize) -> Option<Arc<Page>> {
        self.sources[i].web.as_ref().and_then(|w| w.page.clone())
    }

    pub(super) fn web_key_reload(&mut self) {
        if let Some(page) = self.focused_web().and_then(|i| self.page_of(i)) {
            std::thread::spawn(move || page.reload());
            self.flash("Reloading the page.");
        }
    }

    /// Scrolls the focused page (or the one under the mouse, `at`).
    pub(super) fn web_key_scroll(&mut self, by: f64, at: Option<(usize, (f64, f64))>) {
        let (i, point) = match at {
            Some((i, p)) => (Some(i), Some(p)),
            None => (self.focused_web(), None),
        };
        if let Some(page) = i.and_then(|i| self.page_of(i)) {
            std::thread::spawn(move || page.scroll(&by.to_string(), point));
        }
    }

    /// Runs `work` on a thread of its own; `Msg::Job` brings its result.
    pub(super) fn spawn_job(
        &mut self,
        work: impl FnOnce() -> Result<Value> + Send + 'static,
    ) -> u64 {
        let job = self.next_job;
        self.next_job += 1;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Job(job, work()));
        });
        job
    }

    pub(super) fn on_job(&mut self, job: u64, result: Result<Value>) {
        if self.auto_stopped(job, &result) || self.difference_done(job, &result) {
            return;
        }
        if let Some(point) = self.point_jobs.remove(&job) {
            self.captured(point, result);
            return;
        }
        let waits = std::mem::take(&mut self.waits);
        let mut answered = None;
        for wait in waits {
            match wait.what {
                WaitFor::Job { job: j, sid } if j == job => {
                    let name = self.source(sid).map(|s| s.name.clone());
                    let result = result.clone().map(|mut v| {
                        if let (Value::Obj(members), Some(name)) = (&mut v, name) {
                            members.insert(0, ("source".into(), Value::from(name)));
                        }
                        v
                    });
                    wait.req.answer(result);
                    answered = Some(sid);
                }
                what => self.waits.push(super::Wait { what, ..wait }),
            }
        }
        // A command may have changed the page's size: the box follows.
        if let Some(sid) = answered
            && let Some(source) = self.source_mut(sid)
            && let Some(web) = &mut source.web
            && let Some(page) = &web.page
        {
            let state = page.state();
            let (device, dark) = (state.device.clone(), state.dark);
            drop(state);
            if device != web.device || dark != web.dark {
                source.size = (Some(device.width as f64), Some(device.height as f64));
                web.device = device;
                web.dark = dark;
                self.record_dirty = true;
                self.relayout();
            }
        }
    }

    /// The web source a request names, or the focused one if it's a page,
    /// or the only page.
    fn pick_web(&self, args: &Value) -> Result<usize> {
        let own = self.own_source(args, |i| self.sources[i].web.is_some());
        let i = if let Some(i) = own {
            i
        } else if let Some(name) = args.get_str("source") {
            self.sources
                .iter()
                .position(|s| s.name == name)
                .ok_or_else(|| {
                    Error::new(
                        "NO_SOURCE",
                        format!("no source is called {:?}", text::inert(name, 64)),
                    )
                })?
        } else {
            let pages: Vec<usize> = (0..self.sources.len())
                .filter(|&i| self.sources[i].web.is_some())
                .collect();
            match (self.focused_web(), pages.as_slice()) {
                (Some(i), _) => i,
                (None, [only]) => *only,
                (None, []) => {
                    return Err(Error::new("NO_SOURCE", "no web page is open").hint(
                        "Open one: `pepper-agent web open http://localhost:5173` (your dev server's address).",
                    ));
                }
                (None, _) => {
                    return Err(
                        Error::new("NO_SOURCE", "more than one page is open: which?").hint(
                            "Name it with --source NAME (`pepper-agent sources` lists them).",
                        ),
                    );
                }
            }
        };
        let source = &self.sources[i];
        if source.web.is_none() {
            return Err(
                Error::new("NO_SOURCE", format!("{} isn't a web page", source.name))
                    .hint("`pepper-agent sources` says which sources are pages (kind web)."),
            );
        }
        match &source.state {
            State::Ready => Ok(i),
            State::Loading => Err(Error::new(
                "TIMEOUT",
                format!("{} is still loading", source.name),
            )
            .hint("Run `pepper-agent sources` until its status is ready, then again.")),
            State::Failed(e) | State::Gone(e) => {
                Err(Error::new("LOAD_FAILED", format!("{}: {e}", source.name))
                    .hint("Open it again: `pepper-agent web open <url> --name <name>`."))
            }
        }
    }

    fn files(&mut self) -> Files {
        self.shot_count += 1;
        Files {
            shots: self.shots.clone(),
            count: self.shot_count,
        }
    }

    pub(super) fn web_snapshot(&mut self, req: Request, i: usize) {
        let args = Value::obj().with("source", self.sources[i].name.as_str());
        let picked = self.pick_web(&args).and_then(|i| {
            self.page_of(i)
                .ok_or_else(|| Error::new("TIMEOUT", "the page is still loading"))
                .map(|p| (i, p))
        });
        match picked {
            Ok((i, page)) => {
                let files = self.files();
                let sid = self.sources[i].sid;
                let job = self.spawn_job(move || {
                    page.shot(&files, false, None)
                        .map(|v| v.with("kind", "web"))
                });
                self.wait(req, WEB_WAIT, WaitFor::Job { job, sid });
            }
            Err(e) => req.answer(Err(e)),
        }
    }

    pub(super) fn web_request(&mut self, req: Request) {
        let started = self.start_web_request(&req);
        match started {
            Ok(Started::Now(value)) => req.answer(Ok(value)),
            Ok(Started::Job(job, sid, wait)) => self.wait(req, wait, WaitFor::Job { job, sid }),
            Err(e) => req.answer(Err(e)),
        }
    }

    fn start_web_request(&mut self, req: &Request) -> Result<Started> {
        let args = req.args.clone();
        let i = self.pick_web(&args)?;
        // Looking is anyone's; driving, the page's owner's.
        if !matches!(
            req.cmd.as_str(),
            "web.shot" | "web.console" | "web.errors" | "web.dom" | "web.a11y"
        ) {
            self.check_owner(i, &args)?;
        }
        let sid = self.sources[i].sid;
        let name = self.sources[i].name.clone();
        let page = self
            .page_of(i)
            .ok_or_else(|| Error::new("TIMEOUT", "the page is still loading"))?;
        let arg = |key: &str| -> Result<String> {
            args.get_str(key)
                .map(str::to_owned)
                .ok_or_else(|| Error::new("BAD_ARGS", format!("{}: {key}?", req.cmd)))
        };
        let job = |viewer: &mut Viewer, work: Box<dyn FnOnce() -> Result<Value> + Send>| {
            Started::Job(viewer.spawn_job(work), sid, WEB_WAIT)
        };
        Ok(match req.cmd.as_str() {
            "web.goto" => {
                let url = arg("url")?;
                job(self, Box::new(move || page.navigate(&url)))
            }
            "web.reload" => job(self, Box::new(move || page.reload())),
            "web.back" => job(self, Box::new(move || page.back())),
            "web.click" => {
                let target = arg("target")?;
                job(self, Box::new(move || page.click(&target)))
            }
            "web.hover" => {
                let target = arg("target")?;
                job(self, Box::new(move || page.hover(&target)))
            }
            "web.type" => {
                let (selector, typed) = (arg("selector")?, arg("text")?);
                let clear = args.get_bool("clear").unwrap_or(false);
                job(
                    self,
                    Box::new(move || page.type_text(&selector, &typed, clear)),
                )
            }
            "web.press" => {
                let key = arg("key")?;
                job(self, Box::new(move || page.press(&key)))
            }
            "web.scroll" => {
                let what = arg("to")?;
                job(self, Box::new(move || page.scroll(&what, None)))
            }
            "web.viewport" => {
                let web = self.sources[i]
                    .web
                    .as_ref()
                    .unwrap_or_else(|| unreachable!());
                let device = match args.get_str("device") {
                    Some(d) => Device::parse(d).ok_or_else(|| bad_device(d))?,
                    None => web.device.clone(),
                };
                let dark = args.get_bool("dark").unwrap_or(web.dark);
                let ua = self
                    .host
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|h| h.user_agent().to_owned())
                    .unwrap_or_default();
                job(
                    self,
                    Box::new(move || page.set_viewport(&device, dark, &ua)),
                )
            }
            "web.wait-for" => {
                let what = arg("what")?;
                let wait = args.get_num("timeout").unwrap_or(10.0);
                if !(wait > 0.0 && wait <= MAX_WAIT_FOR) {
                    return Err(Error::new("BAD_ARGS", "--timeout: seconds, up to 120"));
                }
                let wait = Duration::from_secs_f64(wait);
                Started::Job(
                    self.spawn_job(move || page.wait_for(&what, wait)),
                    sid,
                    wait + Duration::from_secs(10),
                )
            }
            "web.shot" => {
                let files = self.files();
                let full = args.get_bool("full_page").unwrap_or(false);
                let selector = args.get_str("selector").map(str::to_owned);
                job(
                    self,
                    Box::new(move || {
                        page.shot(&files, full, selector.as_deref())
                            .map(|v| v.with("kind", "web"))
                    }),
                )
            }
            "web.console" => {
                let since = args.get_num("since").unwrap_or(0.0) as u64;
                let levels: Vec<String> = args
                    .get("levels")
                    .and_then(Value::arr)
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(|l| l.str().map(str::to_owned))
                    .collect();
                Started::Now(
                    page.console_since(since, &levels)
                        .with("source", name.as_str()),
                )
            }
            "web.errors" => {
                let since = args.get_num("since").unwrap_or(0.0) as u64;
                Started::Now(page.errors_since(since).with("source", name.as_str()))
            }
            "web.dom" => {
                let selector = arg("selector")?;
                let max = args
                    .get_num("max")
                    .unwrap_or(20_000.0)
                    .clamp(100.0, 500_000.0) as usize;
                job(self, Box::new(move || page.dom(&selector, max)))
            }
            "web.a11y" => {
                let selector = args.get_str("selector").map(str::to_owned);
                let max = args.get_num("max").unwrap_or(400.0).clamp(10.0, 5000.0) as usize;
                job(self, Box::new(move || page.a11y(selector.as_deref(), max)))
            }
            "web.eval" => {
                let js = arg("js")?;
                job(self, Box::new(move || page.eval(&js)))
            }
            _ => return Err(Error::new("UNSUPPORTED", "unknown web command")),
        })
    }

    /// With no page left, the browser goes too.
    pub(super) fn close_browser_if_unused(&mut self) {
        if self.sources.iter().any(|s| s.web.is_some()) {
            return;
        }
        let host = self.host.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(host) = host {
            std::thread::spawn(move || host.close());
        }
    }

    /// The box a source shows in, in pixels of the pane.
    pub(super) fn box_px(&self, at: Cells) -> (f64, f64, f64, f64) {
        let (cw, ch) = self.cell();
        (
            at.col as f64 * cw,
            at.row as f64 * ch,
            at.cols as f64 * cw,
            at.rows as f64 * ch,
        )
    }
}

enum Started {
    Now(Value),
    Job(u64, u64, Duration),
}

fn bad_device(name: &str) -> Error {
    let names: Vec<&str> = web::DEVICES.iter().map(|d| d.0).collect();
    Error::new(
        "BAD_ARGS",
        format!("{:?} isn't a device", text::inert(name, 40)),
    )
    .hint(format!(
        "One of {}, or WIDTHxHEIGHT@DPR (CSS pixels, like 1280x800@2).",
        names.join(", ")
    ))
}
