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
//! libmpv draws each frame with OpenGL into one of a ring of `RING`
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
mod vsync;

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
    use crate::vsync::Vsync;
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
    /// The surfaces frames are drawn into in turn, handed to Pepper once.
    const RING: usize = pepper::MAX_SURFACES;
    /// While the box's size keeps changing (a window drag), new surfaces
    /// are made once it has stayed the same this long, or at least this
    /// often; until then frames go into the old ones, which Pepper scales.
    const SETTLE: Duration = Duration::from_millis(80);
    const REMAKE_EVERY: Duration = Duration::from_millis(500);
    /// Refreshes with nothing to show before the display link stops.
    const LINK_IDLE: u32 = 30;
    /// How long a frame waits for the display's refresh before it's shown
    /// anyway (the displays asleep, the link not running).
    const REFRESH_LATE: Duration = Duration::from_millis(100);
    /// How long after surfaces couldn't be made they're tried again.
    const ALLOC_AGAIN: Duration = Duration::from_secs(1);
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

    /// mpv's options, by name and value.
    type Options = Vec<(String, String)>;

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
        /// A frame waits for the display's next refresh to be shown.
        awaiting: bool,
        /// The display refreshed while one waited.
        refresh: bool,
        /// Refreshes since one was waited for; at LINK_IDLE, `link_idle`
        /// tells the render thread to stop the link.
        idle_refreshes: u32,
        link_idle: bool,
        /// mpv blends frames (`interpolation`), so no frame repeats another.
        interpolation: bool,
        /// The render thread's messages, for mpv's thread to print: under
        /// ADVANCED_CONTROL the render thread must never wait on a thread
        /// that calls libmpv, and stderr's lock (or a full pipe) could make
        /// it.
        notes: Vec<String>,
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

        /// From the render thread: a message for mpv's thread to print.
        fn note(&self, mpv: Mpv, text: String) {
            let mut state = self.lock();
            if state.notes.len() < MAX_ERRORS {
                state.notes.push(text);
            }
            drop(state);
            // SAFETY: a live handle; safe from a render thread.
            unsafe { mpv::mpv_wakeup(mpv.0) };
        }

        /// At each of the display's refreshes, while the link runs.
        fn on_refresh(&self) {
            let mut state = self.lock();
            if state.awaiting {
                state.refresh = true;
                state.idle_refreshes = 0;
            } else {
                state.idle_refreshes = state.idle_refreshes.saturating_add(1);
                if state.idle_refreshes != LINK_IDLE {
                    return;
                }
                state.link_idle = true;
            }
            drop(state);
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
        /// From a shell: the render thread's messages, shown once the screen
        /// is back.
        notes: Vec<String>,
        /// PEPPER_PLAY_DEBUG: mpv's hardware decoder, as last said
        /// (`hwdec-current`), and the render thread's counts, said at the end.
        hwdec: Option<String>,
        stats: Option<Stats>,
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
            for note in &self.notes {
                eprintln!("pepper-play: {note}");
            }
            if let Some(stats) = &self.stats {
                eprintln!(
                    "pepper-play: {} frames drawn, {} shown, {} into a surface still on screen; \
                     {} surfaces made in {} rings; {} repeats not drawn; \
                     {:.3} s waiting on the graphics chip; hwdec {}",
                    stats.drawn,
                    stats.presented,
                    stats.on_screen,
                    stats.surfaces,
                    stats.rings,
                    stats.repeated,
                    stats.gpu_wait.as_secs_f64(),
                    self.hwdec.as_deref().unwrap_or("-"),
                );
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
        if asks(&args, &["--version"]) {
            println!("pepper-play {}", env!("CARGO_PKG_VERSION"));
            return Ok(ExitCode::SUCCESS);
        }
        if args.is_empty() || asks(&args, &["--help", "-h"]) {
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
            notes: Vec::new(),
            hwdec: None,
            stats: None,
        };
        // The display's refresh, to show frames at and time video to; without
        // it, each frame is shown as soon as it's drawn.
        let vsync = Vsync::new(Box::new({
            let shared = Arc::downgrade(&session.shared);
            move || {
                if let Some(shared) = shared.upgrade() {
                    shared.on_refresh();
                }
            }
        }))
        .inspect_err(|err| eprintln!("pepper-play: {err}"))
        .ok()
        .map(Arc::new);
        let rate = vsync.as_deref().and_then(Vsync::rate);
        if setup.debug {
            eprintln!("pepper-play: the display refreshes {rate:?} times a second");
        }
        let result = start(mpv, &setup, rate, &mut session)
            .and_then(|ipc| play(mpv, &setup, ipc, vsync, &mut session));
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
        rate: Option<f64>,
        session: &mut Session,
    ) -> Result<Vec<(String, String)>, String> {
        let (options, ipc) = mpv_options(setup.mode, &setup.args, rate)?;
        for (name, value) in &options {
            set_option(mpv, name, value)?;
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

    /// mpv's options in the order they're set, and apart, the IPC options,
    /// to set once frames can be drawn. This program's own come first, so
    /// that the user's or the app's, set after them, win.
    fn mpv_options(
        mode: Mode,
        args: &Args,
        rate: Option<f64>,
    ) -> Result<(Options, Options), String> {
        let mut own = vec![
            ("vo", "libmpv"),
            // Now Playing and the media keys, as the mpv program has them
            // (libmpv turns them off); `--input-media-keys=no` turns them off.
            ("input-media-keys", "yes"),
            // And mpv's bindings for them (PLAYPAUSE cycles pause, …), which
            // the mpv program has and libmpv doesn't.
            ("input-default-bindings", "yes"),
            // Decoding on the graphics chip (VideoToolbox) for what it can
            // decode, the CPU for the rest. mpv hands VideoToolbox's frames to
            // OpenGL with no copy (CGLTexImageIOSurface2D). libmpv's default
            // is no, and it doesn't read the user's mpv.conf: `--hwdec=no`
            // turns it off.
            ("hwdec", "auto-safe"),
        ];
        if mode == Mode::Shell {
            own.extend([
                // mpv's keys from the terminal.
                ("terminal", "yes"),
                ("input-terminal", "yes"),
                // But nothing of mpv's on it: its messages hold the video's
                // tags, file names and URLs, and its status line also carries
                // subtitles and OSD text whenever there's no video output,
                // all printed with any escape sequences in them (a subtitle
                // could set the clipboard). This program draws the status
                // line itself, from numbers (`status_text`); mpv's errors
                // are shown, made safe, once the screen is back.
                ("msg-level", "all=no"),
                // And a video output always, even for sound alone, so that
                // subtitles and OSD text are drawn in the box, never on the
                // terminal.
                ("force-window", "yes"),
                ("term-osd", "no"),
            ]);
        }
        let mut options: Vec<(String, String)> = own
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect();
        if let Some(rate) = rate {
            // Frames are shown at the display's refreshes (vsync.rs), so mpv
            // can time video to the display, as in its own window: it plays
            // the sound a hair faster or slower to match, and a 24 fps film
            // keeps an even cadence. libmpv can't ask the display's rate, so
            // it's told. `--video-sync=audio` goes back to timing by sound.
            options.push(("display-fps-override".into(), format!("{rate:.6}")));
            options.push(("video-sync".into(), "display-resample".into()));
        }
        let mut ipc = Vec::new();
        for (name, value) in &args.options {
            match name.as_str() {
                name if IPC_OPTIONS.contains(&name) => ipc.push((name.to_owned(), value.clone())),
                // This program's own: forced from a shell (above), and an
                // app's box shows nothing until there's video.
                "force-window" => {}
                // Only to keep from mistakes: the options are the user's or
                // the app's, who could still change the output with
                // --vo-add, --profile or --include.
                "vo" => return Err("--vo is this program's own".into()),
                _ => options.push((name.clone(), value.clone())),
            }
        }
        Ok((options, ipc))
    }

    /// Draws and plays until mpv shuts down.
    fn play(
        mpv: Mpv,
        setup: &Setup,
        ipc: Vec<(String, String)>,
        vsync: Option<Arc<Vsync>>,
        session: &mut Session,
    ) -> Result<(), String> {
        let shared = session.shared.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        // mpv is told the display's rate (mpv_options) unless the user said
        // one: it's told again when the box goes to another display.
        let retime = vsync.as_deref().and_then(Vsync::rate).is_some()
            && !setup
                .args
                .options
                .iter()
                .any(|(name, _)| name == "display-fps-override");
        let render = std::thread::spawn({
            let shared = shared.clone();
            let port = setup.port.clone();
            let debug = setup.debug;
            move || render(mpv, shared, port, debug, vsync, retime, ready_tx)
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
            events(mpv, setup.debug, session);
        }

        shared.update(|state| state.stop = true);
        let stats = render.join().ok();
        take_notes(session);
        if setup.debug {
            session.stats = stats;
        }
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

    /// Whether the command line says one of `flags` before `--`: after it,
    /// everything is a file or an address (`pepper-play -- --help` plays a
    /// file called `--help`).
    fn asks(args: &[String], flags: &[&str]) -> bool {
        args.iter()
            .take_while(|arg| *arg != "--")
            .any(|arg| flags.contains(&arg.as_str()))
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

    /// What the status line shows, run from a shell: numbers and fixed
    /// words only, from mpv's properties (mpv's own status line is off).
    #[derive(Debug, Default, Clone, Copy, PartialEq)]
    struct Status {
        position: Option<f64>,
        duration: Option<f64>,
        paused: bool,
        buffering: bool,
    }

    /// `HH:MM:SS`, as mpv shows times.
    fn clock(seconds: f64) -> String {
        let s = seconds.max(0.0) as u64;
        format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    }

    fn status_text(status: Status) -> String {
        let state = if status.buffering {
            "(Buffering) "
        } else if status.paused {
            "(Paused) "
        } else {
            ""
        };
        let finite = |v: Option<f64>| v.filter(|v| v.is_finite());
        match (finite(status.position), finite(status.duration)) {
            (Some(position), Some(duration)) if duration > 0.0 => format!(
                "{state}{} / {} ({}%)",
                clock(position),
                clock(duration),
                (position / duration * 100.0).clamp(0.0, 100.0) as u32
            ),
            (Some(position), _) => format!("{state}{}", clock(position)),
            _ => state.trim_end().to_owned(),
        }
    }

    /// mpv's events, until it shuts down.
    fn events(mpv: Mpv, debug: bool, session: &mut Session) {
        /// How often Now Playing is told it all again, with no event.
        const REFRESH: f64 = 5.0;
        /// The reply ids of mpv's `fullscreen` and `hwdec-current`, apart
        /// from Now Playing's.
        const FULLSCREEN: u64 = 1000;
        const HWDEC: u64 = 1001;
        const INTERPOLATION: u64 = 1002;
        /// The status line's properties, run from a shell.
        const POSITION: u64 = 1003;
        const DURATION: u64 = 1004;
        const PAUSED: u64 = 1005;
        const BUFFERING: u64 = 1006;
        let mut status = Status::default();
        let mut now_playing = NowPlaying::start(mpv.0, session.mode == Mode::App);
        // With interpolation, a frame shown at several refreshes is drawn
        // anew at each; without, the render thread skips its repeats.
        // SAFETY: a live handle and a C string.
        unsafe {
            mpv::mpv_observe_property(
                mpv.0,
                INTERPOLATION,
                c"interpolation".as_ptr(),
                mpv::FORMAT_FLAG,
            )
        };
        if debug {
            // SAFETY: a live handle and a C string.
            unsafe {
                mpv::mpv_observe_property(
                    mpv.0,
                    HWDEC,
                    c"hwdec-current".as_ptr(),
                    mpv::FORMAT_STRING,
                )
            };
        }
        if session.screen.is_some() {
            // From a shell, mpv's fullscreen (`f`) fills the whole window,
            // over the status line: there's no window of mpv's to fill.
            let observe = [
                (FULLSCREEN, c"fullscreen", mpv::FORMAT_FLAG),
                (POSITION, c"time-pos", mpv::FORMAT_DOUBLE),
                (DURATION, c"duration", mpv::FORMAT_DOUBLE),
                (PAUSED, c"pause", mpv::FORMAT_FLAG),
                (BUFFERING, c"paused-for-cache", mpv::FORMAT_FLAG),
            ];
            for (id, name, format) in observe {
                // SAFETY: a live handle and a C string.
                unsafe { mpv::mpv_observe_property(mpv.0, id, name.as_ptr(), format) };
            }
        }
        loop {
            // SAFETY: mpv is a live handle; the event stays valid until the
            // next mpv_wait_event.
            let event = unsafe { &*mpv::mpv_wait_event(mpv.0, REFRESH) };
            take_notes(session);
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
                    } else if (POSITION..=BUFFERING).contains(&event.reply_userdata) {
                        // SAFETY: as the format says: a double or an int,
                        // or nothing (no file, no duration).
                        let double = (property.format == mpv::FORMAT_DOUBLE
                            && !property.data.is_null())
                        .then(|| unsafe { *property.data.cast::<f64>() });
                        let flag = property.format == mpv::FORMAT_FLAG
                            && !property.data.is_null()
                            && unsafe { *property.data.cast::<c_int>() } != 0;
                        match event.reply_userdata {
                            POSITION => status.position = double,
                            DURATION => status.duration = double,
                            PAUSED => status.paused = flag,
                            _ => status.buffering = flag,
                        }
                        if let Some(screen) = &session.screen {
                            screen.status(&status_text(status));
                        }
                    } else if event.reply_userdata == INTERPOLATION {
                        // SAFETY: a flag property's data is an int.
                        let on = property.format == mpv::FORMAT_FLAG
                            && !property.data.is_null()
                            && unsafe { *property.data.cast::<c_int>() } != 0;
                        session.shared.update(|state| state.interpolation = on);
                    } else if event.reply_userdata == HWDEC {
                        // SAFETY: a string property's data is a char *; the
                        // last one with a file loaded stays.
                        if property.format == mpv::FORMAT_STRING && !property.data.is_null() {
                            let value =
                                unsafe { CStr::from_ptr(*property.data.cast::<*const c_char>()) };
                            let value = printable(&value.to_string_lossy());
                            if !value.is_empty() {
                                session.hwdec = Some(value);
                            }
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

    /// Prints the render thread's messages (App mode) or keeps them for the
    /// end (from a shell, where the video has the screen).
    fn take_notes(session: &mut Session) {
        let notes = std::mem::take(&mut session.shared.lock().notes);
        for note in notes.iter().map(|note| printable(note)) {
            match session.mode {
                Mode::App => eprintln!("pepper-play: {note}"),
                Mode::Shell => session.notes.push(note),
            }
        }
    }

    /// mpv's text made safe to print to a terminal: it can hold a playlist
    /// entry's name, a URL or yt-dlp's output, so its control characters (C0,
    /// DEL, C1), which would start escape sequences, are shown escaped
    /// (`\u{1b}`), and so are the invisible format characters that would
    /// reorder or hide part of the line (bidi controls, zero-width ones).
    /// Cut at MAX_LINE bytes; a newline at the end stays.
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
            } else if invisible(c) {
                out.extend(c.escape_unicode());
            } else {
                out.push(c);
            }
        }
        out + newline
    }

    /// Unicode format characters that change how a line reads without being
    /// seen: bidi embeddings, overrides and isolates, zero-width ones, the
    /// soft hyphen, the byte-order mark.
    fn invisible(c: char) -> bool {
        matches!(c,
            '\u{00AD}' | '\u{061C}' | '\u{180E}' | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206F}' | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}')
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
    /// runs. With a display link, it follows the display the box is on, and
    /// tells mpv that display's rate if it's timing video to it (`retime`).
    fn follow_sizes(
        reports: pepper::Reports,
        shared: Arc<Shared>,
        debug: bool,
        link: Option<(Arc<Vsync>, Mpv, bool)>,
    ) {
        let mut on = None;
        while let Some((token, report)) = reports.next() {
            if debug {
                eprintln!("pepper-play: Pepper says the box is {report:?}");
            }
            let ours = shared.lock().token.as_ref() == Some(&token);
            if ours
                && let (Some(display), Some((vsync, mpv, retime))) = (report.display, &link)
                && on != Some(display)
                && vsync.follow(display)
            {
                on = Some(display);
                let rate = vsync.rate();
                if debug {
                    eprintln!("pepper-play: on display {display}, {rate:?} refreshes a second");
                }
                // Not under the state's lock: libmpv may wait on the render
                // thread, which takes it.
                if *retime && let Some(rate) = rate {
                    let _ = set_property(*mpv, "display-fps-override", &format!("{rate:.6}"));
                }
            }
            let Some(size) = surface_size(report) else {
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
        vsync: Option<Arc<Vsync>>,
        retime: bool,
        ready: mpsc::Sender<Result<(), String>>,
    ) -> Stats {
        // Frames go out at the display's refreshes: this thread wakes for
        // each, and is as prompt as a window's would be.
        // SAFETY: sets this thread's own class.
        unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
        let mut stats = Stats::default();
        let context = match gl::Context::new() {
            Ok(context) => context,
            Err(err) => {
                let _ = ready.send(Err(err));
                return stats;
            }
        };
        let mut init = mpv::OpenGlInitParams {
            get_proc_address: Some(gl::get_proc_address),
            get_proc_address_ctx: std::ptr::null_mut(),
        };
        let mut advanced: c_int = 1;
        let mut params = [
            mpv::RenderParam {
                kind: mpv::RENDER_PARAM_API_TYPE,
                data: c"opengl".as_ptr().cast_mut().cast(),
            },
            mpv::RenderParam {
                kind: mpv::RENDER_PARAM_OPENGL_INIT_PARAMS,
                data: (&raw mut init).cast(),
            },
            // This thread never waits for mpv's core: it calls only the
            // render API, and mpv_render_context_update at each callback.
            // Lets mpv decode straight into its textures when it decodes on
            // the CPU (vd-lavc-dr).
            mpv::RenderParam {
                kind: mpv::RENDER_PARAM_ADVANCED_CONTROL,
                data: (&raw mut advanced).cast(),
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
            return stats;
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
                .inspect_err(|err| shared.note(mpv, err.clone()))
                .ok(),
            None => {
                shared.note(mpv, "PEPPER_PORT is not set; drawing to nowhere".into());
                None
            }
        };
        if let Some(client) = &client {
            let reports = client.reports();
            let shared = shared.clone();
            let link = vsync.clone().map(|vsync| (vsync, mpv, retime));
            std::thread::spawn(move || follow_sizes(reports, shared, debug, link));
        }
        let mut ring: Vec<gl::Target> = Vec::new();
        let mut attached: Option<String> = None;
        // The token Pepper last didn't take the surfaces for, and when: they
        // go again once that's ATTACH_AGAIN ago, or new surfaces or a new
        // token come, not with every frame.
        let mut refused: Option<(String, Instant)> = None;
        let mut last = 0;
        // New surfaces, or surfaces Pepper hasn't seen: draw the frame again
        // even if libmpv has no new one (it's paused, say).
        let mut redraw = false;
        // The size surfaces are wanted at and since when; since when the
        // ring hasn't fitted it; whether the ring was made at a size Pepper
        // reported; and when to make new ones once the size has settled.
        let mut wanted = DEFAULT_SIZE;
        let mut wanted_since = Instant::now();
        let mut stale_since: Option<Instant> = None;
        let mut ring_reported = false;
        let mut remake: Option<Instant> = None;
        // With a display link: what to show at the display's next refresh,
        // a surface drawn last, or none for a frame that repeats the one on
        // screen; and since when it has waited. No link, or one that wouldn't
        // start: frames are shown as soon as they're drawn.
        let mut waiting: Option<Option<usize>> = None;
        let mut waiting_since = Instant::now();
        let mut synced = vsync.is_some();
        // When surfaces couldn't be made: not again before ALLOC_AGAIN.
        let mut alloc_failed: Option<Instant> = None;

        loop {
            let (token, size, reported, work, refresh, idle, interpolation) = {
                let mut state = shared.lock();
                let (mut due, mut late) = (false, false);
                while !(state.frame
                    || state.changed
                    || state.stop
                    || state.refresh
                    || state.link_idle)
                {
                    // Surfaces to make at a size that has settled, or a frame
                    // that has waited too long for a refresh.
                    let late_at = waiting.is_some().then(|| waiting_since + REFRESH_LATE);
                    let Some(at) = [remake, late_at].into_iter().flatten().min() else {
                        state = shared.wake.wait(state).unwrap_or_else(|e| e.into_inner());
                        continue;
                    };
                    let Some(left) = at.checked_duration_since(Instant::now()) else {
                        due = remake.is_some_and(|r| r <= at);
                        late = late_at.is_some_and(|l| l <= at);
                        break;
                    };
                    state = shared
                        .wake
                        .wait_timeout(state, left)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
                if state.stop {
                    break;
                }
                // A refresh due by now that didn't come counts as one.
                state.refresh |= late;
                let work = state.frame || state.changed || due;
                state.frame = false;
                state.changed = false;
                (
                    state.token.clone(),
                    state.size.unwrap_or(DEFAULT_SIZE),
                    state.size.is_some(),
                    work,
                    std::mem::take(&mut state.refresh),
                    std::mem::take(&mut state.link_idle),
                    state.interpolation,
                )
            };
            if refresh && let Some(slot) = waiting.take() {
                shared.lock().awaiting = false;
                show(client.as_ref(), attached.as_deref(), slot, &mut stats);
                // At the refresh, as a window's swap: what mpv times the
                // display by.
                // SAFETY: rc is live.
                unsafe { mpv::mpv_render_context_report_swap(rc) };
            }
            // Nothing was drawn for a while (paused): the link stops until
            // the next frame.
            if idle
                && let Some(vsync) = &vsync
                && !shared.lock().awaiting
            {
                vsync.stop();
            }
            if !work {
                continue;
            }

            let have = ring.first().map(|t| (t.surface.width, t.surface.height));
            remake = None;
            let now = Instant::now();
            if wanted != size {
                wanted = size;
                wanted_since = now;
            }
            stale_since = if have == Some(size) {
                None
            } else {
                Some(stale_since.unwrap_or(now))
            };
            let retry = alloc_failed.map(|failed| failed + ALLOC_AGAIN);
            if let Some(stale) = stale_since
                && !ring.is_empty()
                && ring_reported
                && let Err(at) = remake_due(now, wanted_since, stale)
            {
                // Still changing: the old surfaces for now.
                remake = Some(at);
            } else if have != Some(size)
                && let Some(retry) = retry.filter(|&retry| now < retry)
            {
                // Surfaces couldn't be made a moment ago: not at every frame.
                remake = Some(retry);
            } else if have != Some(size) {
                stale_since = None;
                ring_reported = reported;
                if waiting.take().is_some() {
                    // That frame's surfaces go; mpv waits for its swap.
                    shared.lock().awaiting = false;
                    // SAFETY: rc is live.
                    unsafe { mpv::mpv_render_context_report_swap(rc) };
                }
                ring.clear();
                // The new surfaces go to Pepper again, under the token; a
                // stream under one that changed meanwhile ends first, or
                // Pepper would keep it.
                if let (Some(client), Some(old)) = (&client, attached.take())
                    && token.as_ref() != Some(&old)
                {
                    client.detach(&old);
                }
                refused = None;
                redraw = true;
                stats.rings += 1;
                stats.surfaces += RING as u64;
                match (0..RING)
                    .map(|_| gl::Target::new(&context, size.0, size.1))
                    .collect()
                {
                    Ok(targets) => {
                        ring = targets;
                        alloc_failed = None;
                    }
                    Err(err) => {
                        if alloc_failed.is_none() {
                            shared.note(mpv, err);
                        }
                        alloc_failed = Some(Instant::now());
                        remake = Some(Instant::now() + ALLOC_AGAIN);
                    }
                }
            }

            if let Some(client) = &client
                && ring.len() == RING
                && let Some(token) = &token
                && Some(token) != attached.as_ref()
                && !refused
                    .as_ref()
                    .is_some_and(|(t, when)| t == token && when.elapsed() < ATTACH_AGAIN)
            {
                if let Some(old) = attached.take() {
                    client.detach(&old);
                }
                let surfaces: [_; RING] = std::array::from_fn(|i| &ring[i].surface);
                match client.attach(token, surfaces) {
                    Ok(()) => {
                        attached = Some(token.clone());
                        refused = None;
                        redraw = true;
                    }
                    Err(err) => {
                        if refused.as_ref().is_none_or(|(t, _)| t != token) {
                            shared.note(mpv, err);
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
            // Another frame before the refresh: only it is told shown. mpv
            // waits for a frame's swap before the next, so this happens only
            // when it was told of one it didn't wait for; telling it of this
            // one too would keep it a frame ahead of the display for good.
            // A surface drawn and not shown yet is shown at the refresh if
            // what comes repeats it, and isn't drawn over if not.
            let held = waiting.take();
            if held.is_some() {
                // Waited for again below if the new frame is display-synced.
                shared.lock().awaiting = false;
            }
            let unshown = held.flatten();
            if let Some(slot) = unshown {
                last = slot;
            }
            let mut info = mpv::FrameInfo::default();
            // SAFETY: rc is live; info is the param's type.
            let got = unsafe {
                mpv::mpv_render_context_get_info(
                    rc,
                    mpv::RenderParam {
                        kind: mpv::RENDER_PARAM_NEXT_FRAME_INFO,
                        data: (&raw mut info).cast(),
                    },
                )
            };
            if got < 0 {
                // Nothing known about it: shown at once, as without a link.
                info = mpv::FrameInfo::default();
            }
            let at_refresh = synced && info.flags & mpv::FRAME_INFO_BLOCK_VSYNC != 0;
            if at_refresh
                && info.flags & mpv::FRAME_INFO_REPEAT != 0
                && !interpolation
                && !redraw
                && attached.is_some()
            {
                // The same picture again, at the next refresh: what's on
                // screen stays, and mpv draws nothing.
                let mut skip: c_int = 1;
                let mut params = [
                    mpv::RenderParam {
                        kind: mpv::RENDER_PARAM_SKIP_RENDERING,
                        data: (&raw mut skip).cast(),
                    },
                    mpv::RenderParam {
                        kind: mpv::RENDER_PARAM_INVALID,
                        data: std::ptr::null_mut(),
                    },
                ];
                // SAFETY: rc is live and its OpenGL context current.
                unsafe { mpv::mpv_render_context_render(rc, params.as_mut_ptr()) };
                stats.repeated += 1;
                if wait_for_refresh(&shared, vsync.as_deref(), &mut waiting, unshown) {
                    waiting_since = Instant::now();
                } else {
                    // The link wouldn't start: no more waiting for it.
                    synced = false;
                    show(client.as_ref(), attached.as_deref(), unshown, &mut stats);
                    // SAFETY: rc is live.
                    unsafe { mpv::mpv_render_context_report_swap(rc) };
                }
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
            let waited = Instant::now();
            context.finish();
            stats.gpu_wait += waited.elapsed();
            stats.drawn += 1;
            last = slot;
            if at_refresh && wait_for_refresh(&shared, vsync.as_deref(), &mut waiting, Some(slot)) {
                waiting_since = Instant::now();
            } else {
                // The link wouldn't start: no more waiting for it.
                synced &= !at_refresh;
                show(client.as_ref(), attached.as_deref(), Some(slot), &mut stats);
                // Only a frame of mpv's that isn't a redraw is waited for.
                if info.flags & mpv::FRAME_INFO_PRESENT != 0
                    && info.flags & mpv::FRAME_INFO_REDRAW == 0
                {
                    // SAFETY: rc is live.
                    unsafe { mpv::mpv_render_context_report_swap(rc) };
                }
            }
        }

        if let (Some(client), Some(token)) = (&client, &attached) {
            client.detach(token);
        }
        // No more refreshes (the size reports' thread holds the link too).
        if let Some(vsync) = &vsync {
            vsync.stop();
        }
        drop(vsync);
        // SAFETY: rc is live and its OpenGL context current; freed before the
        // targets and the context, and before mpv_terminate_destroy.
        unsafe { mpv::mpv_render_context_free(rc) };
        drop(ring);
        drop(context);
        stats
    }

    /// What the render thread did, said at the end under PEPPER_PLAY_DEBUG.
    #[derive(Default)]
    struct Stats {
        drawn: u64,
        presented: u64,
        /// Every surface was on screen, so one was drawn over anyway.
        on_screen: u64,
        /// IOSurfaces made, and rings of them: one at each new size.
        surfaces: u64,
        rings: u64,
        /// Frames that repeated the one on screen, not drawn again.
        repeated: u64,
        /// Spent waiting for the graphics chip to finish frames.
        gpu_wait: Duration,
    }

    const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;

    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(class: u32, relative_priority: c_int) -> c_int;
    }

    /// Shows surface `slot` (none: the one on screen stays).
    fn show(
        client: Option<&pepper::Client>,
        token: Option<&str>,
        slot: Option<usize>,
        stats: &mut Stats,
    ) {
        if let (Some(client), Some(token), Some(slot)) = (client, token, slot) {
            stats.presented += client.present(token, slot) as u64;
        }
    }

    /// Whether to make surfaces at a new size now: once it has stayed the
    /// same SETTLE, or REMAKE_EVERY after the surfaces stopped fitting the
    /// box, whichever is first. If not, when.
    fn remake_due(
        now: Instant,
        wanted_since: Instant,
        stale_since: Instant,
    ) -> Result<(), Instant> {
        let at = (wanted_since + SETTLE).min(stale_since + REMAKE_EVERY);
        if now >= at { Ok(()) } else { Err(at) }
    }

    /// Keeps `slot` to show at the display's next refresh. False if the
    /// display link isn't there or won't start; then nothing waits.
    fn wait_for_refresh(
        shared: &Shared,
        vsync: Option<&Vsync>,
        waiting: &mut Option<Option<usize>>,
        slot: Option<usize>,
    ) -> bool {
        let Some(vsync) = vsync else {
            return false;
        };
        {
            let mut state = shared.lock();
            state.awaiting = true;
            state.idle_refreshes = 0;
        }
        if !vsync.start() {
            shared.lock().awaiting = false;
            return false;
        }
        *waiting = Some(slot);
        true
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
        fn flags_only_before_the_files() {
            assert!(asks(&strings(&["--help"]), &["--help", "-h"]));
            assert!(asks(&strings(&["--fs", "-h", "a.mp4"]), &["--help", "-h"]));
            assert!(!asks(&strings(&["--", "--help"]), &["--help", "-h"]));
            assert!(!asks(
                &strings(&["a.mp4", "--", "--version"]),
                &["--version"]
            ));
        }

        #[test]
        fn own_options_first_so_the_users_win() {
            let last = |options: &[(String, String)], name: &str| {
                options
                    .iter()
                    .rev()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v.clone())
            };
            let args = parse_args(&strings(&["a.mp4"])).unwrap();
            let (options, ipc) = mpv_options(Mode::Shell, &args, Some(60.0)).unwrap();
            assert_eq!(options[0], ("vo".into(), "libmpv".into()));
            assert_eq!(last(&options, "hwdec").as_deref(), Some("auto-safe"));
            assert_eq!(
                last(&options, "video-sync").as_deref(),
                Some("display-resample")
            );
            assert_eq!(
                last(&options, "display-fps-override").as_deref(),
                Some("60.000000")
            );
            assert!(ipc.is_empty());
            // From a shell, nothing of mpv's reaches the terminal: no module
            // (not even the status line, which carries subtitles and OSD
            // text without a video output), and a video output always.
            assert_eq!(last(&options, "msg-level").as_deref(), Some("all=no"));
            assert_eq!(last(&options, "force-window").as_deref(), Some("yes"));
            assert_eq!(last(&options, "term-osd").as_deref(), Some("no"));
            assert!(!options.iter().any(|(_, v)| v.contains("statusline")));

            // The user's own timing wins; with no display link, mpv's stays.
            let args = parse_args(&strings(&["--video-sync=audio", "a.mp4"])).unwrap();
            let (options, _) = mpv_options(Mode::Shell, &args, Some(60.0)).unwrap();
            assert_eq!(last(&options, "video-sync").as_deref(), Some("audio"));
            let (options, _) = mpv_options(Mode::Shell, &args, None).unwrap();
            assert!(last(&options, "display-fps-override").is_none());

            let args = parse_args(&strings(&[
                "--hwdec=no",
                "--input-ipc-client=fd://3",
                "--force-window",
            ]))
            .unwrap();
            let (options, ipc) = mpv_options(Mode::App, &args, None).unwrap();
            assert_eq!(last(&options, "hwdec").as_deref(), Some("no"));
            assert_eq!(ipc, vec![("input-ipc-client".into(), "fd://3".into())]);
            assert!(last(&options, "force-window").is_none());
            assert!(last(&options, "terminal").is_none());
            assert!(last(&options, "msg-level").is_none());

            let args = parse_args(&strings(&["--vo=gpu"])).unwrap();
            assert!(mpv_options(Mode::App, &args, None).is_err());
        }

        #[test]
        fn the_status_line_is_numbers() {
            let status = |position, duration, paused, buffering| {
                status_text(Status {
                    position,
                    duration,
                    paused,
                    buffering,
                })
            };
            assert_eq!(
                status(Some(3.7), Some(70.0), false, false),
                "00:00:03 / 00:01:10 (5%)"
            );
            assert_eq!(
                status(Some(3725.0), Some(7200.0), true, false),
                "(Paused) 01:02:05 / 02:00:00 (51%)"
            );
            assert_eq!(
                status(Some(12.0), None, false, true),
                "(Buffering) 00:00:12"
            );
            assert_eq!(status(None, None, true, false), "(Paused)");
            assert_eq!(status(None, None, false, false), "");
            assert_eq!(status(Some(f64::NAN), Some(0.0), false, false), "");
            assert_eq!(
                status(Some(-4.0), Some(10.0), false, false),
                "00:00:00 / 00:00:10 (0%)"
            );
        }

        #[test]
        fn a_ring_of_four() {
            // At 60 fps shown at each refresh, three surfaces are busy at a
            // time: with a ring of three, 40% of frames were drawn into one
            // still on screen (notes/player-perf.md, 10.5).
            assert_eq!(RING, 4);
            const { assert!(RING <= pepper::MAX_SURFACES) };
        }

        #[test]
        fn surfaces_wait_for_the_size_to_settle() {
            let t0 = Instant::now();
            let ms = Duration::from_millis;
            // The size changed 10 ms ago, after a long time the same: wait
            // till it's 80 ms old.
            let now = t0 + ms(1000);
            let changed = now - ms(10);
            assert_eq!(remake_due(now, changed, changed), Err(now + ms(70)));
            assert_eq!(remake_due(now + ms(70), changed, changed), Ok(()));
            // A drag since t0 that never stops: new surfaces every 500 ms all
            // the same.
            let made = t0;
            let now = t0 + ms(400);
            assert_eq!(remake_due(now, now, made), Err(t0 + ms(480)));
            let now = t0 + ms(490);
            assert_eq!(remake_due(now, now, made), Err(t0 + ms(500)));
            assert_eq!(remake_due(t0 + ms(500), t0 + ms(500), made), Ok(()));
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
                    display: None,
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
            assert_eq!(
                printable("evil\u{202e}txt.exe\u{200b}x"),
                "evil\\u{202e}txt.exe\\u{200b}x"
            );
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
