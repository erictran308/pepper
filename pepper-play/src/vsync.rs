//! The display's refresh, from a CVDisplayLink on the main display (this
//! program has no window, so no display of its own). The render thread
//! shows each frame at a refresh, as a window's swap would, and tells mpv
//! then; with the refresh rate, mpv can time video to the display
//! (`video-sync=display-resample`).
//!
//! The render thread starts the link when a frame waits for a refresh, and
//! stops it once refreshes have come with nothing to show for a while
//! (main.rs, LINK_IDLE), so a paused video costs no wakeups. A frame that
//! waits too long (the displays asleep) is shown anyway (REFRESH_LATE).

use std::ffi::c_void;

type DisplayLink = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CvTime {
    value: i64,
    scale: i32,
    flags: i32,
}

/// kCVTimeIsIndefinite.
const TIME_INDEFINITE: i32 = 1 << 0;

type Output = unsafe extern "C" fn(
    link: DisplayLink,
    now: *const c_void,
    output_time: *const c_void,
    flags_in: u64,
    flags_out: *mut u64,
    context: *mut c_void,
) -> i32;

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVDisplayLinkCreateWithActiveCGDisplays(link: *mut DisplayLink) -> i32;
    fn CVDisplayLinkSetOutputCallback(
        link: DisplayLink,
        callback: Output,
        context: *mut c_void,
    ) -> i32;
    fn CVDisplayLinkStart(link: DisplayLink) -> i32;
    fn CVDisplayLinkStop(link: DisplayLink) -> i32;
    fn CVDisplayLinkIsRunning(link: DisplayLink) -> u8;
    fn CVDisplayLinkGetNominalOutputVideoRefreshPeriod(link: DisplayLink) -> CvTime;
    fn CVDisplayLinkRelease(link: DisplayLink);
}

/// What the link calls at each refresh, with the context it was given.
pub type OnRefresh = Box<dyn Fn() + Send + Sync>;

pub struct Vsync {
    link: DisplayLink,
}

// SAFETY: a CVDisplayLink may be started and stopped from any thread.
unsafe impl Send for Vsync {}
unsafe impl Sync for Vsync {}

unsafe extern "C" fn output(
    _link: DisplayLink,
    _now: *const c_void,
    _output_time: *const c_void,
    _flags_in: u64,
    _flags_out: *mut u64,
    context: *mut c_void,
) -> i32 {
    // SAFETY: the context is an OnRefresh that is never freed (Vsync::new).
    let on_refresh = unsafe { &*context.cast::<OnRefresh>() };
    on_refresh();
    0
}

impl Vsync {
    /// A link on the displays there are, stopped, calling `on_refresh` at
    /// each refresh while it runs.
    ///
    /// `on_refresh` is never freed (one per program): CoreVideo doesn't
    /// document that stopping a link waits for a callback under way, so a
    /// late one must still find it. It should hold only weak references.
    pub fn new(on_refresh: OnRefresh) -> Result<Vsync, String> {
        let mut link = std::ptr::null_mut();
        // SAFETY: valid out-pointer; a link made on failure is released.
        let err = unsafe { CVDisplayLinkCreateWithActiveCGDisplays(&mut link) };
        if err != 0 || link.is_null() {
            if !link.is_null() {
                // SAFETY: ours, never started.
                unsafe { CVDisplayLinkRelease(link) };
            }
            return Err(format!(
                "CVDisplayLinkCreateWithActiveCGDisplays failed: {err}"
            ));
        }
        let context: &'static OnRefresh = Box::leak(Box::new(on_refresh));
        // SAFETY: a live link; the context lives as long as the program.
        let err = unsafe {
            CVDisplayLinkSetOutputCallback(
                link,
                output,
                (context as *const OnRefresh).cast_mut().cast(),
            )
        };
        if err != 0 {
            // SAFETY: ours, never started.
            unsafe { CVDisplayLinkRelease(link) };
            return Err(format!("CVDisplayLinkSetOutputCallback failed: {err}"));
        }
        Ok(Vsync { link })
    }

    /// The display's refreshes a second, as it says it runs.
    pub fn rate(&self) -> Option<f64> {
        // SAFETY: a live link.
        let period = unsafe { CVDisplayLinkGetNominalOutputVideoRefreshPeriod(self.link) };
        (period.flags & TIME_INDEFINITE == 0 && period.value > 0 && period.scale > 0)
            .then(|| period.scale as f64 / period.value as f64)
    }

    /// Calls at each refresh from now on (the first at the next one). False
    /// if the link won't run.
    pub fn start(&self) -> bool {
        // SAFETY: a live link.
        unsafe { CVDisplayLinkIsRunning(self.link) != 0 || CVDisplayLinkStart(self.link) == 0 }
    }

    pub fn stop(&self) {
        // SAFETY: a live link.
        unsafe {
            if CVDisplayLinkIsRunning(self.link) != 0 {
                CVDisplayLinkStop(self.link);
            }
        }
    }
}

impl Drop for Vsync {
    fn drop(&mut self) {
        // SAFETY: after the stop nothing starts it again, and the link is
        // ours to release; a callback still under way finds its context
        // alive (never freed).
        unsafe {
            CVDisplayLinkStop(self.link);
            CVDisplayLinkRelease(self.link);
        }
    }
}
