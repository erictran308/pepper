//! pepper-play: plays video inside Pepper, a Ghostty build that shows video
//! inside the terminal, through libmpv. It ships inside Pepper.app, which
//! gives every shell its path as `PEPPER_PLAY`.
//!
//! It takes mpv's options (`--name=value`, `--name`, `--no-name`) and files
//! or URLs to play, and is used two ways (README.md):
//! - From a shell, `pepper-play FILE|URL`: the video takes the terminal (the
//!   alternate screen) but for mpv's status line at the bottom, mpv's keys
//!   work, and the box goes when it ends.
//! - By an app, which says so with `PEPPER_TOKEN` or an IPC option
//!   (`--input-ipc-client`, `--input-ipc-server`): the app places the box
//!   (`OSC 777;pepper;place;TOKEN;…`, or `script-message-to main
//!   pepper-place COL ROW COLS ROWS`) and drives mpv over its JSON IPC.
//!
//! libmpv draws each frame with OpenGL into one of `pepper::MAX_SURFACES`
//! IOSurfaces, and Pepper (its Mach port named in `PEPPER_PORT`) is told
//! which one to show; nothing is copied. Pepper says the box's size in
//! pixels, and the frames are drawn at it.
//!
//! libmpv is loaded at run time (`mpv.rs`). As Homebrew builds it, it's GPL,
//! which is why this is a program of its own: an app that drives it over
//! IPC isn't linked with it.

#[cfg(not(target_os = "macos"))]
fn main() -> std::process::ExitCode {
    eprintln!("pepper-play runs only on macOS");
    std::process::ExitCode::FAILURE
}

#[cfg(target_os = "macos")]
mod gl;
#[cfg(target_os = "macos")]
mod mpv;
#[cfg(target_os = "macos")]
mod nowplaying;
#[cfg(target_os = "macos")]
mod objc;
#[cfg(target_os = "macos")]
mod pepper;
#[cfg(target_os = "macos")]
mod runloop;
#[cfg(target_os = "macos")]
mod surface;
#[cfg(target_os = "macos")]
mod terminal;

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    match macos::run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("pepper-play: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{CStr, CString, c_char, c_int, c_void};
    use std::process::ExitCode;
    use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
    use std::time::{Duration, Instant};

    use crate::nowplaying::NowPlaying;
    use crate::terminal::{self, Cells, Screen, Tty};
    use crate::{gl, mpv, pepper, runloop};

    const USAGE: &str = "\
usage: pepper-play [mpv options] [--] FILE|URL ...

Plays video inside Pepper's terminal, through libmpv (brew install mpv).
URLs go through yt-dlp, as in mpv. mpv's keys work: q quits, space pauses,
arrows seek, f fills the whole tab (Cmd-Enter: Pepper's window, full screen).
All after -- is a file or URL, even one that starts with --: put it before
names you didn't write yourself.

By an app: set PEPPER_TOKEN, or give --input-ipc-client=fd://N or
--input-ipc-server=PATH, and place the box yourself. See
https://github.com/erictran308/pepper/tree/pepper/pepper-play
";

    /// The surfaces' size until Pepper says the box's.
    const DEFAULT_SIZE: (u32, u32) = (640, 360);
    const MAX_SIDE: u32 = 8192;
    /// The most pixels a surface has (a 6K screen's worth): a bigger box
    /// gets frames this size, scaled up.
    const MAX_PIXELS: u64 = 6144 * 3456;
    /// The most of mpv's errors kept, run from a shell, to show at the end.
    const MAX_ERRORS: usize = 20;
    /// The most of one of mpv's messages printed, in bytes.
    const MAX_LINE: usize = 1024;
    /// How long after Pepper didn't take the surfaces they're offered again.
    const ATTACH_AGAIN: Duration = Duration::from_secs(1);

    /// mpv's options that start its IPC: set once frames can be drawn.
    const IPC_OPTIONS: [&str; 2] = ["input-ipc-client", "input-ipc-server"];

    /// libmpv's handle, which its API lets any thread use.
    #[derive(Clone, Copy)]
    struct Mpv(*mut mpv::Handle);
    // SAFETY: libmpv's client API is thread-safe.
    unsafe impl Send for Mpv {}

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Mode {
        /// Run from a shell: the video takes the terminal.
        Shell,
        /// Run by an app, which places the box and drives mpv.
        App,
    }

    /// The command line: mpv's options, then files and URLs.
    #[derive(Debug, Default, PartialEq)]
    struct Args {
        options: Vec<(String, String)>,
        files: Vec<String>,
    }

    struct Setup {
        mode: Mode,
        args: Args,
        port: Option<String>,
        debug: bool,
    }

    /// What the event loop tells the render thread.
    #[derive(Default)]
    struct State {
        token: Option<String>,
        size: Option<(u32, u32)>,
        /// libmpv has a frame to draw.
        frame: bool,
        /// The token or size changed.
        changed: bool,
        stop: bool,
    }

    #[derive(Default)]
    struct Shared {
        state: Mutex<State>,
        wake: Condvar,
    }

    impl Shared {
        fn lock(&self) -> MutexGuard<'_, State> {
            self.state.lock().unwrap_or_else(|e| e.into_inner())
        }

        fn update(&self, f: impl FnOnce(&mut State)) {
            f(&mut self.lock());
            self.wake.notify_one();
        }
    }

    /// What mpv's thread keeps besides mpv: the terminal and what to say at
    /// the end.
    struct Session {
        mode: Mode,
        shared: Arc<Shared>,
        tty: Option<Arc<Tty>>,
        /// From a shell: the screen the video took.
        screen: Option<Screen>,
        /// By an app: the box was placed for it (`pepper-place`), so it's
        /// taken away at the end.
        placed: bool,
        /// From a shell: mpv's errors, shown once the screen is back if
        /// something couldn't play.
        errors: Vec<String>,
        /// A file or URL couldn't be played.
        failed: bool,
    }

    impl Session {
        fn tty(&mut self) -> Option<Arc<Tty>> {
            if self.tty.is_none() {
                match Tty::open() {
                    Ok(tty) => self.tty = Some(Arc::new(tty)),
                    Err(err) => eprintln!("pepper-play: {err}"),
                }
            }
            self.tty.clone()
        }

        /// Gives the screen back, or takes away the box placed for the app,
        /// and, if something couldn't play, says why. After mpv has let go
        /// of the terminal.
        fn end(self, failed: bool) {
            let token = self.shared.lock().token.clone();
            if let Some(screen) = self.screen {
                screen.leave();
            } else if self.placed
                && let (Some(tty), Some(token)) = (&self.tty, &token)
            {
                tty.write(&terminal::remove(token));
            }
            // Only then: mpv's errors include some that don't stop it
            // (CoreAudio's "unable to set the input channel layout").
            if failed {
                for error in &self.errors {
                    eprint!("{error}");
                }
            }
        }
    }

    /// As the mpv program: the main thread is the app's (its events, the
    /// main queue), and mpv runs on another, from start to end. mpv's macOS
    /// side waits on the main queue while it starts, so it can't start on
    /// the main thread.
    pub fn run() -> Result<ExitCode, String> {
        from_its_bundle();
        let args: Vec<String> = std::env::args().skip(1).collect();
        if args.iter().any(|arg| arg == "--version") {
            println!("pepper-play {}", env!("CARGO_PKG_VERSION"));
            return Ok(ExitCode::SUCCESS);
        }
        if args.is_empty() || args.iter().any(|arg| arg == "--help" || arg == "-h") {
            print!("{USAGE}");
            return Ok(if args.is_empty() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            });
        }
        let args = parse_args(&args)?;

        // The app's token, taken out of the environment so that nothing mpv
        // starts (yt-dlp) has it.
        let token = std::env::var("PEPPER_TOKEN").ok().filter(|t| !t.is_empty());
        // SAFETY: no other thread runs yet.
        unsafe { std::env::remove_var("PEPPER_TOKEN") };
        if token.as_deref().is_some_and(|t| !pepper::valid_token(t)) {
            return Err("PEPPER_TOKEN must be 1-64 of A-Z a-z 0-9 _ -".into());
        }
        let ipc = args
            .options
            .iter()
            .filter(|(name, _)| IPC_OPTIONS.contains(&name.as_str()))
            .count();
        if ipc > 1 {
            // Set at run time, a second one would start the first's again
            // (mpv's manual, --input-ipc-client).
            return Err("give one IPC option, --input-ipc-client or --input-ipc-server".into());
        }
        let mode = if token.is_some() || ipc > 0 {
            Mode::App
        } else {
            Mode::Shell
        };
        let port = std::env::var("PEPPER_PORT").ok().filter(|p| !p.is_empty());
        if mode == Mode::Shell {
            if args.files.is_empty() {
                return Err("nothing to play (pepper-play --help)".into());
            }
            if port.is_none() {
                return Err("plays only inside Pepper: PEPPER_PORT isn't set".into());
            }
            if std::env::var_os("TMUX").is_some() {
                return Err("doesn't play inside tmux, which keeps Pepper's escape \
                            sequences from the terminal"
                    .into());
            }
        }
        mpv::load()?;

        let setup = Setup {
            mode,
            args,
            port,
            debug: std::env::var_os("PEPPER_PLAY_DEBUG").is_some(),
        };
        let shared = Arc::new(Shared::default());
        shared.lock().token = token;

        let app = runloop::become_app();
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done_tx.send(run_mpv(setup, shared));
            runloop::stop();
        });
        runloop::run_main(app);
        done_rx
            .recv()
            .unwrap_or_else(|_| Err("mpv's thread ended".into()))
    }

    /// Run by a link (Pepper.app/Contents/MacOS/pepper-play, which puts it
    /// on every shell's PATH), runs again by the program's own path, inside
    /// Contents/Helpers/Pepper.app: macOS knows a program by the bundle of
    /// the path it was run by, and Now Playing shows it by that.
    fn from_its_bundle() {
        use std::os::unix::process::CommandExt;
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let Ok(real) = std::fs::canonicalize(&exe) else {
            return;
        };
        if real == exe {
            return;
        }
        let err = std::process::Command::new(&real)
            .args(std::env::args_os().skip(1))
            .exec();
        eprintln!("pepper-play: cannot run {}: {err}", real.display());
    }

    /// mpv, from its start to its end, while the main thread runs the app.
    fn run_mpv(setup: Setup, shared: Arc<Shared>) -> Result<ExitCode, String> {
        // SAFETY: mpv_create has no preconditions.
        let mpv = Mpv(unsafe { mpv::mpv_create() });
        if mpv.0.is_null() {
            return Err("mpv_create failed".into());
        }
        let mut session = Session {
            mode: setup.mode,
            shared,
            tty: None,
            screen: None,
            placed: false,
            errors: Vec::new(),
            failed: false,
        };
        let result =
            start(mpv, &setup, &mut session).and_then(|ipc| play(mpv, &setup, ipc, &mut session));
        // SAFETY: the render thread has freed its render context by now.
        // While mpv winds down, the main run loop still runs.
        unsafe { mpv::mpv_terminate_destroy(mpv.0) };
        let failed = session.failed;
        session.end(failed || result.is_err());
        result.map(|()| {
            if failed {
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            }
        })
    }

    /// Sets mpv's options and starts it; from a shell, takes the screen
    /// first. Returns the IPC options, to set once frames can be drawn.
    fn start(
        mpv: Mpv,
        setup: &Setup,
        session: &mut Session,
    ) -> Result<Vec<(String, String)>, String> {
        set_option(mpv, "vo", "libmpv")?;
        // Now Playing and the media keys, as the mpv program has them (libmpv
        // turns them off); `--input-media-keys=no` turns them off.
        set_option(mpv, "input-media-keys", "yes")?;
        // And mpv's bindings for them (PLAYPAUSE cycles pause, …), which the
        // mpv program has and libmpv doesn't.
        set_option(mpv, "input-default-bindings", "yes")?;
        if setup.mode == Mode::Shell {
            // mpv's status line, and its keys from the terminal.
            set_option(mpv, "terminal", "yes")?;
            set_option(mpv, "input-terminal", "yes")?;
            // But none of mpv's messages: they hold the video's tags, file
            // names and URLs, which mpv prints with any escape sequences in
            // them. Its errors are shown, made safe, once the screen is back.
            set_option(mpv, "msg-level", "all=no,statusline=status")?;
        }
        let mut ipc = Vec::new();
        for (name, value) in &setup.args.options {
            match name.as_str() {
                name if IPC_OPTIONS.contains(&name) => ipc.push((name.to_owned(), value.clone())),
                // There's no window here to force.
                "force-window" => {}
                // Only to keep from mistakes: the options are the user's or
                // the app's, who could still change the output with
                // --vo-add, --profile or --include.
                "vo" => return Err("--vo is this program's own".into()),
                _ => set_option(mpv, name, value)?,
            }
        }

        if setup.mode == Mode::Shell {
            let tty = session.tty().ok_or("no terminal to play in")?;
            let token = pepper::new_token()?;
            session.shared.lock().token = Some(token.clone());
            // Before mpv starts, so all it prints goes to that screen.
            session.screen = Some(Screen::enter(tty, token));
        }

        // SAFETY: mpv is a fresh handle.
        let err = unsafe { mpv::mpv_initialize(mpv.0) };
        if err < 0 {
            return Err(format!("mpv_initialize: {}", mpv::error(err)));
        }
        // Errors go to stderr; from a shell, they're kept to show once the
        // screen is back (mpv prints them on it too, under the video).
        // PEPPER_PLAY_DEBUG keeps more, from verbose up.
        let level = match (setup.mode, setup.debug) {
            (Mode::Shell, _) => c"error",
            (Mode::App, true) => c"v",
            (Mode::App, false) => c"warn",
        };
        // SAFETY: mpv is initialized; the level is a C string.
        unsafe { mpv::mpv_request_log_messages(mpv.0, level.as_ptr()) };
        Ok(ipc)
    }

    /// Draws and plays until mpv shuts down.
    fn play(
        mpv: Mpv,
        setup: &Setup,
        ipc: Vec<(String, String)>,
        session: &mut Session,
    ) -> Result<(), String> {
        let shared = session.shared.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let render = std::thread::spawn({
            let shared = shared.clone();
            let port = setup.port.clone();
            let debug = setup.debug;
            move || render(mpv, shared, port, debug, ready_tx)
        });
        let ready = ready_rx
            .recv()
            .unwrap_or_else(|_| Err("the render thread ended".into()));

        // Only now may commands come: a `loadfile` before the render context
        // exists would find no video output.
        let started = ready
            .and_then(|()| {
                ipc.iter()
                    .try_for_each(|(name, value)| set_property(mpv, name, value))
            })
            .and_then(|()| {
                setup
                    .args
                    .files
                    .iter()
                    .try_for_each(|file| command(mpv, &["loadfile", file, "append-play"]))
            })
            .and_then(|()| {
                // With files, it ends once they've played, as mpv does. Only
                // now: mpv with nothing to play and no idle would end at once.
                let idle = setup.args.options.iter().any(|(name, _)| name == "idle");
                if setup.args.files.is_empty() || idle {
                    Ok(())
                } else {
                    set_property(mpv, "idle", "no")
                }
            });
        if started.is_ok() {
            events(mpv, session);
        }

        shared.update(|state| state.stop = true);
        let _ = render.join();
        started
    }

    /// The command line: `--name=value`, `--name` and `--no-name` are mpv's
    /// options; anything else, and all after `--`, is a file or URL.
    fn parse_args(args: &[String]) -> Result<Args, String> {
        let mut out = Args::default();
        let mut only_files = false;
        for arg in args {
            if only_files || !arg.starts_with("--") {
                out.files.push(arg.clone());
            } else if arg == "--" {
                only_files = true;
            } else {
                let (name, value) = parse_option(arg)?;
                out.options.push((name.to_owned(), value.to_owned()));
            }
        }
        Ok(out)
    }

    /// `--name=value`, `--name` (yes) or `--no-name` (no).
    fn parse_option(arg: &str) -> Result<(&str, &str), String> {
        let option = arg
            .strip_prefix("--")
            .filter(|o| !o.is_empty() && !o.starts_with('='));
        let Some(option) = option else {
            return Err(format!("not an option: {arg}"));
        };
        Ok(match option.split_once('=') {
            Some((name, value)) => (name, value),
            None => match option.strip_prefix("no-") {
                Some(name) => (name, "no"),
                None => (option, "yes"),
            },
        })
    }

    fn set_option(mpv: Mpv, name: &str, value: &str) -> Result<(), String> {
        let (c_name, c_value) = c_pair(name, value)?;
        // SAFETY: both are C strings; mpv is a live handle.
        let err = unsafe { mpv::mpv_set_option_string(mpv.0, c_name.as_ptr(), c_value.as_ptr()) };
        if err < 0 {
            Err(format!("--{name}: {}", mpv::error(err)))
        } else {
            Ok(())
        }
    }

    fn set_property(mpv: Mpv, name: &str, value: &str) -> Result<(), String> {
        let (c_name, c_value) = c_pair(name, value)?;
        // SAFETY: both are C strings; mpv is a live handle.
        let err = unsafe { mpv::mpv_set_property_string(mpv.0, c_name.as_ptr(), c_value.as_ptr()) };
        if err < 0 {
            Err(format!("{name}: {}", mpv::error(err)))
        } else {
            Ok(())
        }
    }

    fn command(mpv: Mpv, args: &[&str]) -> Result<(), String> {
        let args: Vec<CString> = args
            .iter()
            .map(|arg| CString::new(*arg).map_err(|_| format!("{arg:?} holds a NUL")))
            .collect::<Result<_, _>>()?;
        let mut argv: Vec<*const c_char> = args.iter().map(|arg| arg.as_ptr()).collect();
        argv.push(std::ptr::null());
        // SAFETY: a NULL-terminated array of C strings; mpv is a live handle.
        let err = unsafe { mpv::mpv_command(mpv.0, argv.as_mut_ptr()) };
        if err < 0 {
            Err(format!(
                "{}: {}",
                args[0].to_string_lossy(),
                mpv::error(err)
            ))
        } else {
            Ok(())
        }
    }

    fn c_pair(name: &str, value: &str) -> Result<(CString, CString), String> {
        let nul = |_| format!("--{name} holds a NUL");
        Ok((
            CString::new(name).map_err(nul)?,
            CString::new(value).map_err(nul)?,
        ))
    }

    /// mpv's events, until it shuts down.
    fn events(mpv: Mpv, session: &mut Session) {
        /// How often Now Playing is told it all again, with no event.
        const REFRESH: f64 = 5.0;
        /// The reply id of mpv's `fullscreen`, apart from Now Playing's.
        const FULLSCREEN: u64 = 1000;
        let mut now_playing = NowPlaying::start(mpv.0, session.mode == Mode::App);
        if session.screen.is_some() {
            // From a shell, mpv's fullscreen (`f`) fills the whole window,
            // over its status line: there's no window of mpv's to fill.
            // SAFETY: a live handle and a C string.
            unsafe {
                mpv::mpv_observe_property(
                    mpv.0,
                    FULLSCREEN,
                    c"fullscreen".as_ptr(),
                    mpv::FORMAT_FLAG,
                )
            };
        }
        loop {
            // SAFETY: mpv is a live handle; the event stays valid until the
            // next mpv_wait_event.
            let event = unsafe { &*mpv::mpv_wait_event(mpv.0, REFRESH) };
            match event.event_id {
                mpv::EVENT_SHUTDOWN => return,
                mpv::EVENT_NONE => now_playing.refresh(),
                mpv::EVENT_PROPERTY_CHANGE => {
                    // SAFETY: a property event's data is an mpv_event_property.
                    let property = unsafe { &*event.data.cast::<mpv::EventProperty>() };
                    if event.reply_userdata == FULLSCREEN {
                        // SAFETY: a flag property's data is an int.
                        let on = property.format == mpv::FORMAT_FLAG
                            && !property.data.is_null()
                            && unsafe { *property.data.cast::<c_int>() } != 0;
                        if let Some(screen) = &session.screen {
                            screen.set_fill(on);
                        }
                    } else {
                        now_playing.on_property(mpv.0, property);
                    }
                }
                mpv::EVENT_END_FILE => {
                    // SAFETY: an end-file event's data is an mpv_event_end_file.
                    let end = unsafe { &*event.data.cast::<mpv::EndFile>() };
                    session.failed |= end.reason == mpv::END_FILE_ERROR;
                }
                mpv::EVENT_LOG_MESSAGE => {
                    // SAFETY: a log message event's data is an
                    // mpv_event_log_message of C strings.
                    let (prefix, text) = unsafe {
                        let message = &*event.data.cast::<mpv::LogMessage>();
                        (CStr::from_ptr(message.prefix), CStr::from_ptr(message.text))
                    };
                    let (prefix, text) = (
                        printable(&prefix.to_string_lossy()),
                        printable(&text.to_string_lossy()),
                    );
                    match session.mode {
                        Mode::App => eprint!("mpv [{prefix}] {text}"),
                        Mode::Shell if session.errors.len() < MAX_ERRORS => {
                            session.errors.push(format!("pepper-play: {text}"))
                        }
                        Mode::Shell => {}
                    }
                }
                mpv::EVENT_CLIENT_MESSAGE => {
                    // SAFETY: a client message event's data is an
                    // mpv_event_client_message.
                    let args = unsafe { client_args(event.data.cast()) };
                    on_message(session, &args);
                }
                _ => {}
            }
        }
    }

    /// mpv's text made safe to print to a terminal: it can hold a playlist
    /// entry's name, a URL or yt-dlp's output, so its control characters (C0,
    /// DEL, C1), which would start escape sequences, are shown escaped
    /// (`\u{1b}`). Cut at MAX_LINE bytes; a newline at the end stays.
    fn printable(text: &str) -> String {
        let (body, newline) = match text.strip_suffix('\n') {
            Some(body) => (body, "\n"),
            None => (text, ""),
        };
        let mut out = String::new();
        for c in body.chars() {
            if out.len() >= MAX_LINE {
                out.push('…');
                break;
            }
            if c.is_control() {
                out.extend(c.escape_default());
            } else {
                out.push(c);
            }
        }
        out + newline
    }

    unsafe fn client_args(message: *const mpv::ClientMessage) -> Vec<String> {
        // SAFETY: the caller passes a live mpv_event_client_message, whose
        // args are num_args C strings.
        unsafe {
            let message = &*message;
            (0..message.num_args.max(0) as usize)
                .map(|i| {
                    CStr::from_ptr(*message.args.add(i))
                        .to_string_lossy()
                        .into_owned()
                })
                .collect()
        }
    }

    /// What an app says with `script-message-to main …`.
    #[derive(Debug, PartialEq)]
    enum Message {
        /// The token the app places the box with.
        Token(String),
        /// Place the box here (cells), with the token, or one of this
        /// program's own.
        Place(Cells),
        Remove,
    }

    fn parse_message(args: &[String]) -> Option<Message> {
        let cell = |s: &String| s.parse::<u16>().ok();
        match args {
            [kind, token] if kind == "pepper-token" && pepper::valid_token(token) => {
                Some(Message::Token(token.clone()))
            }
            [kind, col, row, cols, rows] if kind == "pepper-place" => {
                let at = Cells {
                    col: cell(col)?,
                    row: cell(row)?,
                    cols: cell(cols)?,
                    rows: cell(rows)?,
                };
                (at.cols > 0 && at.rows > 0).then_some(Message::Place(at))
            }
            [kind] if kind == "pepper-remove" => Some(Message::Remove),
            _ => None,
        }
    }

    fn on_message(session: &mut Session, args: &[String]) {
        let Some(message) = parse_message(args) else {
            return;
        };
        match message {
            Message::Token(token) => {
                let old = session.shared.lock().token.clone();
                if old.as_ref() == Some(&token) {
                    return;
                }
                // The box placed with the old token goes.
                if session.placed
                    && let (Some(tty), Some(old)) = (&session.tty, &old)
                {
                    tty.write(&terminal::remove(old));
                    session.placed = false;
                }
                session.shared.update(|state| {
                    state.token = Some(token);
                    state.changed = true;
                });
            }
            Message::Place(at) => {
                let Some(tty) = session.tty() else {
                    return;
                };
                let token = session.shared.lock().token.clone();
                let token = match token {
                    Some(token) => token,
                    None => match pepper::new_token() {
                        Ok(token) => {
                            session.shared.update(|state| {
                                state.token = Some(token.clone());
                                state.changed = true;
                            });
                            token
                        }
                        Err(err) => {
                            eprintln!("pepper-play: {err}");
                            return;
                        }
                    },
                };
                tty.write(&terminal::place(&token, at));
                session.placed = true;
            }
            Message::Remove => {
                let token = session.shared.lock().token.clone();
                if let (Some(tty), Some(token)) = (session.tty(), token) {
                    tty.write(&terminal::remove(&token));
                }
                session.placed = false;
            }
        }
    }

    /// Called by libmpv, from its own threads, when there's a frame to draw.
    unsafe extern "C" fn on_update(ctx: *mut c_void) {
        // SAFETY: ctx is the render thread's Shared, alive until after the
        // render context is freed.
        let shared = unsafe { &*ctx.cast::<Shared>() };
        shared.update(|state| state.frame = true);
    }

    /// A size Pepper reported, as surfaces can be made: none for a box
    /// that's gone (the last size stays) or has no pixels. At most MAX_SIDE
    /// a side and MAX_PIXELS in all, the same shape.
    fn surface_size(size: pepper::BoxSize) -> Option<(u32, u32)> {
        if !(size.visible && size.width > 0 && size.height > 0) {
            return None;
        }
        let (w, h) = (size.width.min(MAX_SIDE), size.height.min(MAX_SIDE));
        let pixels = w as u64 * h as u64;
        if pixels <= MAX_PIXELS {
            return Some((w, h));
        }
        let scale = (MAX_PIXELS as f64 / pixels as f64).sqrt();
        let shrink = |side: u32| ((side as f64 * scale) as u32).max(1);
        Some((shrink(w), shrink(h)))
    }

    /// Takes Pepper's reports of the box's size, for as long as the program
    /// runs.
    fn follow_sizes(reports: pepper::Reports, shared: Arc<Shared>, debug: bool) {
        while let Some((token, size)) = reports.next() {
            if debug {
                eprintln!("pepper-play: Pepper says the box is {size:?}");
            }
            let Some(size) = surface_size(size) else {
                continue;
            };
            shared.update(|state| {
                if state.token.as_ref() == Some(&token) && state.size != Some(size) {
                    state.size = Some(size);
                    state.changed = true;
                }
            });
        }
    }

    /// Owns the OpenGL context, the render context and the surfaces: libmpv
    /// wants them all on one thread.
    fn render(
        mpv: Mpv,
        shared: Arc<Shared>,
        port: Option<String>,
        debug: bool,
        ready: mpsc::Sender<Result<(), String>>,
    ) {
        let context = match gl::Context::new() {
            Ok(context) => context,
            Err(err) => {
                let _ = ready.send(Err(err));
                return;
            }
        };
        let mut init = mpv::OpenGlInitParams {
            get_proc_address: Some(gl::get_proc_address),
            get_proc_address_ctx: std::ptr::null_mut(),
        };
        let mut params = [
            mpv::RenderParam {
                kind: mpv::RENDER_PARAM_API_TYPE,
                data: c"opengl".as_ptr().cast_mut().cast(),
            },
            mpv::RenderParam {
                kind: mpv::RENDER_PARAM_OPENGL_INIT_PARAMS,
                data: (&raw mut init).cast(),
            },
            mpv::RenderParam {
                kind: mpv::RENDER_PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        let mut rc = std::ptr::null_mut();
        // SAFETY: the OpenGL context is current on this thread; params ends
        // in INVALID.
        let err = unsafe { mpv::mpv_render_context_create(&mut rc, mpv.0, params.as_mut_ptr()) };
        if err < 0 {
            let _ = ready.send(Err(format!(
                "mpv_render_context_create: {}",
                mpv::error(err)
            )));
            return;
        }
        // SAFETY: this thread holds `shared` until after rc is freed.
        unsafe {
            mpv::mpv_render_context_set_update_callback(
                rc,
                Some(on_update),
                Arc::as_ptr(&shared).cast_mut().cast(),
            )
        };
        let _ = ready.send(Ok(()));

        let client = match port {
            Some(name) => pepper::Client::connect(&name)
                .inspect_err(|err| eprintln!("pepper-play: {err}"))
                .ok(),
            None => {
                eprintln!("pepper-play: PEPPER_PORT is not set; drawing to nowhere");
                None
            }
        };
        if let Some(client) = &client {
            let reports = client.reports();
            let shared = shared.clone();
            std::thread::spawn(move || follow_sizes(reports, shared, debug));
        }
        let mut ring: Vec<gl::Target> = Vec::new();
        let mut attached: Option<String> = None;
        // The token Pepper last didn't take the surfaces for, and when: they
        // go again once that's ATTACH_AGAIN ago, or new surfaces or a new
        // token come, not with every frame.
        let mut refused: Option<(String, Instant)> = None;
        let mut last = 0;
        let mut stats = Stats::default();
        // New surfaces, or surfaces Pepper hasn't seen: draw the frame again
        // even if libmpv has no new one (it's paused, say).
        let mut redraw = false;

        loop {
            let (token, size) = {
                let mut state = shared.lock();
                while !(state.frame || state.changed || state.stop) {
                    state = shared.wake.wait(state).unwrap_or_else(|e| e.into_inner());
                }
                if state.stop {
                    break;
                }
                state.frame = false;
                state.changed = false;
                (state.token.clone(), state.size.unwrap_or(DEFAULT_SIZE))
            };

            if ring.first().map(|t| (t.surface.width, t.surface.height)) != Some(size) {
                ring.clear();
                attached = None;
                refused = None;
                redraw = true;
                match (0..pepper::MAX_SURFACES)
                    .map(|_| gl::Target::new(&context, size.0, size.1))
                    .collect()
                {
                    Ok(targets) => ring = targets,
                    Err(err) => eprintln!("pepper-play: {err}"),
                }
            }

            if let Some(client) = &client
                && ring.len() == pepper::MAX_SURFACES
                && let Some(token) = &token
                && Some(token) != attached.as_ref()
                && !refused
                    .as_ref()
                    .is_some_and(|(t, when)| t == token && when.elapsed() < ATTACH_AGAIN)
            {
                if let Some(old) = attached.take() {
                    client.detach(&old);
                }
                let surfaces = std::array::from_fn(|i| &ring[i].surface);
                match client.attach(token, surfaces) {
                    Ok(()) => {
                        attached = Some(token.clone());
                        refused = None;
                        redraw = true;
                    }
                    Err(err) => {
                        if refused.as_ref().is_none_or(|(t, _)| t != token) {
                            eprintln!("pepper-play: {err}");
                        }
                        refused = Some((token.clone(), Instant::now()));
                    }
                }
            }

            // SAFETY: rc is live and its OpenGL context is current.
            let flags = unsafe { mpv::mpv_render_context_update(rc) };
            if (flags & mpv::RENDER_UPDATE_FRAME == 0 && !redraw) || ring.is_empty() {
                continue;
            }
            redraw = false;
            let slot = free_slot(&ring, last);
            stats.on_screen += ring[slot].surface.in_use() as u64;
            let target = &ring[slot];
            let mut fbo = mpv::OpenGlFbo {
                fbo: target.framebuffer as c_int,
                w: target.surface.width as c_int,
                h: target.surface.height as c_int,
                internal_format: gl::GL_RGBA8,
            };
            let mut flip: c_int = 0;
            let mut params = [
                mpv::RenderParam {
                    kind: mpv::RENDER_PARAM_OPENGL_FBO,
                    data: (&raw mut fbo).cast(),
                },
                mpv::RenderParam {
                    kind: mpv::RENDER_PARAM_FLIP_Y,
                    data: (&raw mut flip).cast(),
                },
                mpv::RenderParam {
                    kind: mpv::RENDER_PARAM_INVALID,
                    data: std::ptr::null_mut(),
                },
            ];
            // SAFETY: as above; the framebuffer is the target's, alive here.
            unsafe { mpv::mpv_render_context_render(rc, params.as_mut_ptr()) };
            context.finish();
            stats.drawn += 1;
            if let (Some(client), Some(token)) = (&client, &attached) {
                stats.presented += client.present(token, slot) as u64;
            }
            // SAFETY: rc is live.
            unsafe { mpv::mpv_render_context_report_swap(rc) };
            last = slot;
        }

        if let (Some(client), Some(token)) = (&client, &attached) {
            client.detach(token);
        }
        if debug {
            eprintln!(
                "pepper-play: {} frames drawn, {} shown, {} into a surface still on screen",
                stats.drawn, stats.presented, stats.on_screen
            );
        }
        // SAFETY: rc is live and its OpenGL context current; freed before the
        // targets and the context, and before mpv_terminate_destroy.
        unsafe { mpv::mpv_render_context_free(rc) };
        drop(ring);
        drop(context);
    }

    #[derive(Default)]
    struct Stats {
        drawn: u64,
        presented: u64,
        /// Every surface was on screen, so one was drawn over anyway.
        on_screen: u64,
    }

    /// The next surface after `last` that the window server isn't showing,
    /// so a frame is never drawn into one on screen.
    fn free_slot(ring: &[gl::Target], last: usize) -> usize {
        let next = |i| (last + i) % ring.len();
        (1..ring.len())
            .map(next)
            .find(|&slot| !ring[slot].surface.in_use())
            .unwrap_or(next(1))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn strings(args: &[&str]) -> Vec<String> {
            args.iter().map(|s| s.to_string()).collect()
        }

        #[test]
        fn options() {
            assert_eq!(
                parse_option("--hwdec=auto-safe"),
                Ok(("hwdec", "auto-safe"))
            );
            assert_eq!(parse_option("--no-config"), Ok(("config", "no")));
            assert_eq!(parse_option("--idle"), Ok(("idle", "yes")));
            assert_eq!(parse_option("--title=a=b"), Ok(("title", "a=b")));
            assert!(parse_option("--").is_err());
            assert!(parse_option("--=x").is_err());
        }

        #[test]
        fn options_then_files() {
            let args = parse_args(&strings(&[
                "--mute",
                "a.mp4",
                "https://example.com/v",
                "-",
                "--",
                "--odd-name.mp4",
            ]))
            .unwrap();
            assert_eq!(args.options, vec![("mute".into(), "yes".into())]);
            assert_eq!(
                args.files,
                strings(&["a.mp4", "https://example.com/v", "-", "--odd-name.mp4"])
            );
            assert!(parse_args(&strings(&["--=x"])).is_err());
        }

        #[test]
        fn messages() {
            let msg = |args: &[&str]| parse_message(&strings(args));
            assert_eq!(
                msg(&["pepper-token", "ab_12-x"]),
                Some(Message::Token("ab_12-x".into()))
            );
            assert_eq!(msg(&["pepper-token", "a b"]), None);
            assert_eq!(
                msg(&["pepper-place", "2", "3", "40", "11"]),
                Some(Message::Place(Cells {
                    col: 2,
                    row: 3,
                    cols: 40,
                    rows: 11
                }))
            );
            assert_eq!(msg(&["pepper-place", "2", "3", "0", "11"]), None);
            assert_eq!(msg(&["pepper-place", "2", "3", "40"]), None);
            assert_eq!(msg(&["pepper-place", "-1", "3", "40", "11"]), None);
            assert_eq!(msg(&["pepper-remove"]), Some(Message::Remove));
            assert_eq!(msg(&["pepper-size", "960", "540"]), None);
        }

        #[test]
        fn sizes_from_pepper() {
            let size = |width, height, visible| {
                surface_size(pepper::BoxSize {
                    width,
                    height,
                    visible,
                })
            };
            assert_eq!(size(1920, 1080, true), Some((1920, 1080)));
            assert_eq!(size(1920, 1080, false), None);
            assert_eq!(size(0, 1080, true), None);
            assert_eq!(size(20000, 1080, true), Some((MAX_SIDE, 1080)));
            assert_eq!(size(6016, 3384, true), Some((6016, 3384)));
            let (w, h) = size(8192, 8192, true).unwrap();
            assert!(w as u64 * h as u64 <= MAX_PIXELS && w == h && w > 4000);
        }

        #[test]
        fn mpv_text_is_printed_inert() {
            assert_eq!(
                printable("Cannot open file '/a\x1b]2;X\x07.mp4'\n"),
                "Cannot open file '/a\\u{1b}]2;X\\u{7}.mp4'\n"
            );
            assert_eq!(
                printable("a\u{9b}31m\r\tb\x7f"),
                "a\\u{9b}31m\\r\\tb\\u{7f}"
            );
            assert_eq!(printable("Failed to open é ✓.\n"), "Failed to open é ✓.\n");
            let long = printable(&format!("{}\n", "x".repeat(5000)));
            assert_eq!(long, format!("{}…\n", "x".repeat(MAX_LINE)));
            let printed = printable("\x1b[2J\x1bP\u{90}\u{85}\n");
            assert!(
                printed
                    .trim_end_matches('\n')
                    .chars()
                    .all(|c| !c.is_control())
            );
        }

        #[test]
        fn a_token_or_an_ipc_option_means_an_app() {
            let ipc = |args: &[&str]| {
                parse_args(&strings(args))
                    .unwrap()
                    .options
                    .iter()
                    .any(|(name, _)| IPC_OPTIONS.contains(&name.as_str()))
            };
            assert!(ipc(&["--input-ipc-client=fd://3"]));
            assert!(ipc(&["--input-ipc-server=/tmp/s", "a.mp4"]));
            assert!(!ipc(&["a.mp4"]));
        }
    }
}
