//! This program as a macOS app. mpv's macOS side (its media keys,
//! `input-media-keys`) and Now Playing (`nowplaying.rs`) need what the mpv
//! program has and a libmpv program doesn't by itself: an NSApplication
//! whose main thread takes the app's events and serves the main queue. It's
//! an accessory: no Dock icon, no menu bar.
//!
//! Not mpv's own `Application` class: as that, mpv would also send its Now
//! Playing the title and position (`AppHub.isApplication`), but it would
//! take the video output for a window of its own (cocoa-cb) and fail, since
//! this program draws through the render API itself.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::objc::{Id, Sel, class, objc_autoreleasePoolPop, objc_autoreleasePoolPush, sel, send};

/// NSApplicationActivationPolicyAccessory.
const ACCESSORY: isize = 1;
/// NSEventMaskAny.
const ANY_EVENT: u64 = u64::MAX;
/// How long it waits for an event between looks at whether to stop.
const LOOK_EVERY: f64 = 0.25;

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    /// The same string as NSDefaultRunLoopMode (toll-free bridged).
    static kCFRunLoopDefaultMode: Id;
    fn CFRunLoopGetMain() -> *mut c_void;
    fn CFRunLoopWakeUp(run_loop: *mut c_void);
}

static STOP: AtomicBool = AtomicBool::new(false);

/// NSApp, made an accessory app. On the main thread, before mpv starts.
pub fn become_app() -> Id {
    // SAFETY: AppKit calls on the main thread, through the methods' own
    // signatures.
    unsafe {
        let app: Id = send!(unsafe extern "C" fn(Id, Sel) -> Id)(
            class(c"NSApplication"),
            sel(c"sharedApplication"),
        );
        send!(unsafe extern "C" fn(Id, Sel, isize) -> i8)(
            app,
            sel(c"setActivationPolicy:"),
            ACCESSORY,
        );
        send!(unsafe extern "C" fn(Id, Sel))(app, sel(c"finishLaunching"));
        app
    }
}

/// Takes the app's events, which also runs the main run loop and serves the
/// main queue, until [`stop`]. On the main thread.
pub fn run_main(app: Id) {
    // SAFETY: as in `become_app`; each round's autoreleased objects (the
    // date, the event) go with its pool.
    unsafe {
        let next_event = sel(c"nextEventMatchingMask:untilDate:inMode:dequeue:");
        let send_event = sel(c"sendEvent:");
        let date_class = class(c"NSDate");
        let date_after = sel(c"dateWithTimeIntervalSinceNow:");
        while !STOP.load(Ordering::Acquire) {
            let pool = objc_autoreleasePoolPush();
            let until: Id =
                send!(unsafe extern "C" fn(Id, Sel, f64) -> Id)(date_class, date_after, LOOK_EVERY);
            let event: Id = send!(unsafe extern "C" fn(Id, Sel, u64, Id, Id, i8) -> Id)(
                app,
                next_event,
                ANY_EVENT,
                until,
                kCFRunLoopDefaultMode,
                1,
            );
            if !event.is_null() {
                send!(unsafe extern "C" fn(Id, Sel, Id))(app, send_event, event);
            }
            objc_autoreleasePoolPop(pool);
        }
    }
}

/// Makes [`run_main`] return, from any thread.
pub fn stop() {
    STOP.store(true, Ordering::Release);
    // SAFETY: CFRunLoopWakeUp may be called from any thread.
    unsafe { CFRunLoopWakeUp(CFRunLoopGetMain()) };
}
