//! The display's refresh, from a CVDisplayLink on the main display (this
//! program has no window, so no display of its own). The render thread
//! shows each frame at a refresh, as a window's swap would, and tells mpv
//! then; with the refresh rate, mpv can time video to the display
//! (`video-sync=display-resample`).
//!
//! The render thread starts the link when a frame waits for a refresh, and
//! stops it once refreshes have come with nothing to show for a while
//! (main.rs, LINK_IDLE), so a paused video costs no wakeups.

use std::ffi::c_void;
use std::sync::Arc;

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
    /// Kept alive for the callback, which has a pointer to it.
    _on_refresh: Arc<OnRefresh>,
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
    // SAFETY: the context is the Vsync's OnRefresh, which outlives the link.
    let on_refresh = unsafe { &*context.cast::<OnRefresh>() };
    on_refresh();
    0
}

impl Vsync {
    /// A link on the displays there are, stopped, calling `on_refresh` at
    /// each refresh while it runs.
    pub fn new(on_refresh: OnRefresh) -> Result<Vsync, String> {
        let on_refresh = Arc::new(on_refresh);
        let mut link = std::ptr::null_mut();
        // SAFETY: valid out-pointer; the callback's context lives as long as
        // the link (released in Drop before it goes).
        unsafe {
            let err = CVDisplayLinkCreateWithActiveCGDisplays(&mut link);
            if err != 0 || link.is_null() {
                return Err(format!(
                    "CVDisplayLinkCreateWithActiveCGDisplays failed: {err}"
                ));
            }
            CVDisplayLinkSetOutputCallback(
                link,
                output,
                Arc::as_ptr(&on_refresh).cast_mut().cast(),
            );
        }
        Ok(Vsync {
            link,
            _on_refresh: on_refresh,
        })
    }

    /// The display's refreshes a second, as it says it runs.
    pub fn rate(&self) -> Option<f64> {
        // SAFETY: a live link.
        let period = unsafe { CVDisplayLinkGetNominalOutputVideoRefreshPeriod(self.link) };
        (period.flags & TIME_INDEFINITE == 0 && period.value > 0)
            .then(|| period.scale as f64 / period.value as f64)
    }

    /// Calls at each refresh from now on (the first at the next one).
    pub fn start(&self) {
        // SAFETY: a live link.
        unsafe {
            if CVDisplayLinkIsRunning(self.link) == 0 {
                CVDisplayLinkStart(self.link);
            }
        }
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
        // SAFETY: stopping waits for a callback under way; then nothing calls
        // it again, and the link is ours to release.
        unsafe {
            CVDisplayLinkStop(self.link);
            CVDisplayLinkRelease(self.link);
        }
    }
}
