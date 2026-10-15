//! The few system calls std doesn't give: the user's own temporary folder,
//! a socket's peer, the terminal's mode and size, signals, a random token.

use std::ffi::{c_char, c_int, c_ulong, c_void};
use std::io::Read;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

unsafe extern "C" {
    fn getuid() -> u32;
    fn getpeereid(fd: c_int, euid: *mut u32, egid: *mut u32) -> c_int;
    fn confstr(name: c_int, buf: *mut c_char, len: usize) -> usize;
    fn kill(pid: c_int, sig: c_int) -> c_int;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn tcgetattr(fd: c_int, termios: *mut Termios) -> c_int;
    fn tcsetattr(fd: c_int, action: c_int, termios: *const Termios) -> c_int;
    fn cfmakeraw(termios: *mut Termios);
    fn signal(sig: c_int, handler: usize) -> usize;
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    fn dup2(from: c_int, to: c_int) -> c_int;
    fn gettimeofday(tv: *mut c_void, tz: *mut c_void) -> c_int;
}

pub fn uid() -> u32 {
    // SAFETY: no preconditions.
    unsafe { getuid() }
}

/// The user that holds the other end of a Unix socket.
pub fn peer_uid(fd: RawFd) -> Option<u32> {
    let (mut uid, mut gid) = (0u32, 0u32);
    // SAFETY: fd is a socket; both out pointers are valid.
    (unsafe { getpeereid(fd, &mut uid, &mut gid) } == 0).then_some(uid)
}

/// The user's own temporary folder (`_CS_DARWIN_USER_TEMP_DIR`), which is
/// what `TMPDIR` says in a login shell. Asked of the system rather than read
/// from `TMPDIR`, which an agent's tool may change for its commands: the
/// viewer and the agent must find the same folder.
pub fn user_temp_dir() -> PathBuf {
    const CS_DARWIN_USER_TEMP_DIR: c_int = 65537;
    let mut buf = vec![0 as c_char; 1024];
    // SAFETY: the buffer is as long as said.
    let len = unsafe { confstr(CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr(), buf.len()) };
    if len > 1 && len <= buf.len() {
        let bytes: Vec<u8> = buf[..len - 1].iter().map(|&c| c as u8).collect();
        if let Ok(path) = String::from_utf8(bytes) {
            return PathBuf::from(path);
        }
    }
    std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Whether a process with this pid runs (one of another user's counts).
pub fn alive(pid: u32) -> bool {
    const EPERM: i32 = 1;
    let Ok(pid) = c_int::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks.
    pid > 0
        && (unsafe { kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(EPERM))
}

/// 32 hex digits from /dev/urandom.
pub fn random_hex() -> std::io::Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Seconds since 1970, with microseconds.
pub fn now() -> f64 {
    #[repr(C)]
    struct Timeval {
        sec: i64,
        usec: i32,
    }
    let mut tv = Timeval { sec: 0, usec: 0 };
    // SAFETY: tv is a timeval.
    unsafe { gettimeofday((&raw mut tv).cast(), std::ptr::null_mut()) };
    tv.sec as f64 + tv.usec as f64 / 1e6
}

/// A Unix time as `2026-10-15T12:34:56.789Z`.
pub fn iso_time(unix: f64) -> String {
    let ms = (unix.max(0.0) * 1000.0) as i64;
    let (days, rest) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600_000,
        rest / 60_000 % 60,
        rest / 1000 % 60,
        rest % 1000
    )
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Termios {
    iflag: c_ulong,
    oflag: c_ulong,
    cflag: c_ulong,
    lflag: c_ulong,
    cc: [u8; 20],
    ispeed: c_ulong,
    ospeed: c_ulong,
}

/// Puts the terminal in raw mode (keys one at a time, no echo, no signals
/// from ^C); returns the mode to put back.
pub fn raw_mode(fd: RawFd) -> std::io::Result<Termios> {
    // SAFETY: an all-zero termios is valid to be filled.
    let mut old: Termios = unsafe { std::mem::zeroed() };
    // SAFETY: fd is the terminal; old is a termios.
    if unsafe { tcgetattr(fd, &mut old) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut raw = old;
    // SAFETY: raw is a termios.
    unsafe { cfmakeraw(&mut raw) };
    set_mode(fd, &raw)?;
    Ok(old)
}

pub fn set_mode(fd: RawFd, mode: &Termios) -> std::io::Result<()> {
    const TCSANOW: c_int = 0;
    // SAFETY: fd is the terminal; mode a termios.
    if unsafe { tcsetattr(fd, TCSANOW, mode) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The terminal's size: columns, rows, and its pixels (0 if not said).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WinSize {
    pub cols: u16,
    pub rows: u16,
    pub width: u16,
    pub height: u16,
}

pub fn win_size(fd: RawFd) -> Option<WinSize> {
    #[repr(C)]
    #[derive(Default)]
    struct Winsize {
        rows: u16,
        cols: u16,
        width: u16,
        height: u16,
    }
    const TIOCGWINSZ: c_ulong = 0x4008_7468;
    let mut size = Winsize::default();
    // SAFETY: TIOCGWINSZ fills a winsize.
    let ok = unsafe { ioctl(fd, TIOCGWINSZ, &raw mut size) } == 0;
    (ok && size.cols > 0 && size.rows > 0).then_some(WinSize {
        cols: size.cols,
        rows: size.rows,
        width: size.width,
        height: size.height,
    })
}

/// Set by SIGTERM, SIGHUP and SIGINT: the viewer ends.
pub static STOP: AtomicBool = AtomicBool::new(false);
/// Set by SIGWINCH: the pane changed size.
pub static RESIZED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop(_: c_int) {
    STOP.store(true, Ordering::Relaxed);
}

extern "C" fn on_resize(_: c_int) {
    RESIZED.store(true, Ordering::Relaxed);
}

pub fn catch_signals() {
    const SIGHUP: c_int = 1;
    const SIGINT: c_int = 2;
    const SIGPIPE: c_int = 13;
    const SIGTERM: c_int = 15;
    const SIGWINCH: c_int = 28;
    const SIG_IGN: usize = 1;
    // SAFETY: the handlers only store to atomics.
    unsafe {
        for sig in [SIGHUP, SIGINT, SIGTERM] {
            signal(sig, on_stop as *const () as usize);
        }
        signal(SIGWINCH, on_resize as *const () as usize);
        signal(SIGPIPE, SIG_IGN);
    }
}

/// In a child before exec: `fd` as file descriptor 3, kept open across exec.
///
/// # Safety
/// Only between fork and exec (async-signal-safe calls only).
pub unsafe fn as_fd3(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: as the caller promises.
    unsafe { as_fd(fd, 3) }
}

/// In a child before exec: `fd` as file descriptor `to`, kept open across
/// exec.
///
/// # Safety
/// Only between fork and exec (async-signal-safe calls only).
pub unsafe fn as_fd(fd: RawFd, to: RawFd) -> std::io::Result<()> {
    const F_SETFD: c_int = 2;
    // SAFETY: plain calls on file descriptors.
    unsafe {
        if fd == to {
            // dup2 onto itself leaves close-on-exec set.
            if fcntl(to, F_SETFD, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        } else if dup2(fd, to) != to {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// In a child before exec: `a` as file descriptor 3 and `b` as 4, whatever
/// numbers they have now.
///
/// # Safety
/// Only between fork and exec (async-signal-safe calls only).
pub unsafe fn as_fd3_and_4(a: RawFd, b: RawFd) -> std::io::Result<()> {
    const F_DUPFD_CLOEXEC: c_int = 67;
    // SAFETY: plain calls on file descriptors.
    unsafe {
        // Out of the way first, so that putting one in place can't close
        // the other.
        let a = fcntl(a, F_DUPFD_CLOEXEC, 10);
        let b = fcntl(b, F_DUPFD_CLOEXEC, 10);
        if a < 0 || b < 0 {
            return Err(std::io::Error::last_os_error());
        }
        as_fd(a, 3)?;
        as_fd(b, 4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_times() {
        assert_eq!(iso_time(0.0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_time(1792067696.789), "2026-10-15T12:34:56.789Z");
        assert_eq!(iso_time(951_782_400.0), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn the_temp_dir_is_the_users() {
        let dir = user_temp_dir();
        assert!(dir.is_absolute());
        assert!(dir.is_dir());
        assert!(alive(std::process::id()));
        assert!(!alive(0));
        assert_eq!(random_hex().unwrap().len(), 32);
    }
}
