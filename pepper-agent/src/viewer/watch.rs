//! `web open --watch DIR`: the page reloads when a file under DIR changes,
//! for servers with no hot reload of their own. FSEvents says what changed;
//! changes in `.git`, `node_modules` and `.pepper` don't count.

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use super::Msg;

const UTF8: u32 = 0x0800_0100;
const SINCE_NOW: u64 = u64::MAX;
const NO_DEFER: u32 = 0x02;
const FILE_EVENTS: u32 = 0x10;
/// Seconds FSEvents gathers changes before it tells.
const LATENCY: f64 = 0.1;

#[repr(C)]
struct StreamContext {
    version: isize,
    info: *mut c_void,
    retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<extern "C" fn(*const c_void)>,
    describe: *const c_void,
}

type Callback =
    extern "C" fn(*const c_void, *mut c_void, usize, *mut c_void, *const u32, *const u64);

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        alloc: *const c_void,
        callback: Callback,
        context: *const StreamContext,
        paths: *const c_void,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> *mut c_void;
    fn FSEventStreamSetDispatchQueue(stream: *mut c_void, queue: *mut c_void);
    fn FSEventStreamStart(stream: *mut c_void) -> u8;
    fn FSEventStreamStop(stream: *mut c_void);
    fn FSEventStreamInvalidate(stream: *mut c_void);
    fn FSEventStreamRelease(stream: *mut c_void);
}

#[repr(C)]
struct CallBacks {
    _private: [u8; 0],
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeArrayCallBacks: CallBacks;
    fn CFStringCreateWithCString(
        alloc: *const c_void,
        text: *const c_char,
        encoding: u32,
    ) -> *const c_void;
    fn CFArrayCreate(
        alloc: *const c_void,
        values: *const *const c_void,
        count: isize,
        callbacks: *const CallBacks,
    ) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
    fn dispatch_release(object: *mut c_void);
}

/// What the callback needs: where to say so. FSEvents holds a reference to
/// it (retain_info, release_info) and lets go only when the stream is gone,
/// after a callback that was running when it was stopped has returned.
struct Info {
    sid: u64,
    tx: Sender<Msg>,
}

pub struct Watch {
    stream: *mut c_void,
    queue: *mut c_void,
    /// Its Info, for a test to see when it goes.
    #[cfg(test)]
    info: std::sync::Weak<Info>,
}

// SAFETY: the stream and queue are only started and torn down, from one
// thread at a time; the callback runs on the queue.
unsafe impl Send for Watch {}

extern "C" fn retain_info(info: *const c_void) -> *const c_void {
    // SAFETY: an Arc<Info>'s pointer (Watch::start), alive while FSEvents
    // holds it.
    unsafe { Arc::increment_strong_count(info.cast::<Info>()) };
    info
}

extern "C" fn release_info(info: *const c_void) {
    // SAFETY: as above; FSEvents gives back the reference it took.
    unsafe { Arc::decrement_strong_count(info.cast::<Info>()) };
}

/// Whether a change at `path` counts: not in a folder of tools' own.
fn counts(path: &str) -> bool {
    !["/.git/", "/node_modules/", "/.pepper/", "/.DS_Store"]
        .iter()
        .any(|part| path.contains(part) || path.ends_with(part.trim_end_matches('/')))
}

extern "C" fn changed(
    _stream: *const c_void,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    _flags: *const u32,
    _ids: *const u64,
) {
    // SAFETY: FSEvents passes the context's info, which it holds a
    // reference to while it may call back, and `count` C strings.
    let (info, paths) = unsafe {
        (
            &*(info as *const Info),
            std::slice::from_raw_parts(paths as *const *const c_char, count),
        )
    };
    #[cfg(test)]
    tests::hold(info.sid);
    let any = paths.iter().any(|&p| {
        // SAFETY: each is a NUL-terminated path.
        !p.is_null() && counts(&unsafe { CStr::from_ptr(p) }.to_string_lossy())
    });
    if any {
        let _ = info.tx.send(Msg::Watch(info.sid));
    }
}

impl Watch {
    pub fn start(dir: &Path, sid: u64, tx: Sender<Msg>) -> Option<Watch> {
        let path = CString::new(dir.as_os_str().as_encoded_bytes()).ok()?;
        // FSEvents takes its own reference to it, if it makes the stream;
        // this one goes at the end of start.
        let info = Arc::new(Info { sid, tx });
        // SAFETY: CoreFoundation and FSEvents calls on what's made here; the
        // array and string are released once the stream holds them.
        unsafe {
            let string = CFStringCreateWithCString(std::ptr::null(), path.as_ptr(), UTF8);
            if string.is_null() {
                return None;
            }
            let paths = CFArrayCreate(
                std::ptr::null(),
                &string,
                1,
                &raw const kCFTypeArrayCallBacks,
            );
            CFRelease(string);
            let context = StreamContext {
                version: 0,
                info: Arc::as_ptr(&info).cast_mut().cast(),
                retain: Some(retain_info),
                release: Some(release_info),
                describe: std::ptr::null(),
            };
            let stream = FSEventStreamCreate(
                std::ptr::null(),
                changed,
                &context,
                paths,
                SINCE_NOW,
                LATENCY,
                NO_DEFER | FILE_EVENTS,
            );
            CFRelease(paths);
            if stream.is_null() {
                return None;
            }
            let queue = dispatch_queue_create(c"pepper-agent.watch".as_ptr(), std::ptr::null());
            FSEventStreamSetDispatchQueue(stream, queue);
            let watch = Watch {
                stream,
                queue,
                #[cfg(test)]
                info: Arc::downgrade(&info),
            };
            (FSEventStreamStart(stream) != 0).then_some(watch)
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        // SAFETY: tears down what start made, in order. A callback running
        // meanwhile goes on with its Info: FSEvents lets go of it only once
        // that has returned (release_info).
        unsafe {
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
            FSEventStreamRelease(self.stream);
            dispatch_release(self.queue);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Barrier, OnceLock};
    use std::time::{Duration, Instant};

    /// The sid whose first callback a test holds: it waits there till let go.
    const HELD: u64 = 4242;
    static HOLD: OnceLock<(Barrier, Barrier)> = OnceLock::new();
    static TAKEN: AtomicBool = AtomicBool::new(false);

    pub(super) fn hold(sid: u64) {
        if sid == HELD && !TAKEN.swap(true, Ordering::SeqCst) {
            let (entered, go) = HOLD.get_or_init(|| (Barrier::new(2), Barrier::new(2)));
            entered.wait();
            go.wait();
        }
    }

    #[test]
    fn what_counts() {
        assert!(counts("/p/src/App.tsx"));
        assert!(!counts("/p/.git/index"));
        assert!(!counts("/p/node_modules/x/y.js"));
        assert!(!counts("/p/.pepper/events.jsonl"));
        assert!(!counts("/p/src/.DS_Store"));
    }

    #[test]
    fn changes_are_told() {
        let dir = std::env::temp_dir().join(format!("pepper-agent-watch-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let dir = dir.canonicalize().unwrap();
        // FSEvents may still tell of the folder's making, just before.
        std::thread::sleep(Duration::from_millis(500));
        let (tx, rx) = std::sync::mpsc::channel();
        let watch = Watch::start(&dir, 7, tx).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        std::fs::write(dir.join(".git/HEAD"), "x").unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(800)).is_err());
        std::fs::write(dir.join("page.html"), "x").unwrap();
        let got = rx.recv_timeout(Duration::from_secs(3));
        assert!(matches!(got, Ok(Msg::Watch(7))));
        drop(watch);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_callback_running_when_its_watch_ends_keeps_its_info() {
        let dir = std::env::temp_dir().join(format!("pepper-agent-held-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let watch = Watch::start(&dir, HELD, tx).unwrap();
        let info = watch.info.clone();
        std::thread::sleep(Duration::from_millis(300));
        std::fs::write(dir.join("page.html"), "x").unwrap();
        let (entered, go) = HOLD.get_or_init(|| (Barrier::new(2), Barrier::new(2)));
        // The callback is running, held: the watch ends meanwhile.
        entered.wait();
        let (done_tx, done) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(watch);
            let _ = done_tx.send(());
        });
        assert!(done.recv_timeout(Duration::from_secs(2)).is_ok());
        assert!(info.upgrade().is_some(), "freed under a running callback");
        // Let go: it says so, then its Info goes with the stream.
        go.wait();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)),
            Ok(Msg::Watch(HELD))
        ));
        let end = Instant::now() + Duration::from_secs(2);
        while info.upgrade().is_some() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(info.upgrade().is_none(), "never let go");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
