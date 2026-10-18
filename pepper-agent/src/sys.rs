//! The few system calls std doesn't give: the user's own temporary folder
//! and home, a socket's peer and its sandbox, the terminal's mode and size,
//! signals, a random token.

use std::ffi::{CStr, CString, OsStr, c_char, c_int, c_ulong, c_void};
use std::io::Read;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
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
    fn mkfifo(path: *const c_char, mode: u16) -> c_int;
    fn umask(mask: u16) -> u16;
    fn getsockopt(fd: c_int, level: c_int, name: c_int, value: *mut c_void, len: *mut u32)
    -> c_int;
    fn proc_pidinfo(pid: c_int, flavor: c_int, arg: u64, buffer: *mut c_void, size: c_int)
    -> c_int;
    fn getpwuid_r(
        uid: u32,
        pwd: *mut Passwd,
        buf: *mut c_char,
        len: usize,
        result: *mut *mut Passwd,
    ) -> c_int;
    // libsystem_sandbox's, in libSystem: 0 if the sandbox of the process
    // the token names allows the operation (or it has none), 1 if not.
    fn sandbox_check_by_audit_token(
        token: AuditToken,
        operation: *const c_char,
        filter: c_int,
        ...
    ) -> c_int;
    static SANDBOX_CHECK_NO_REPORT: c_int;
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

/// A process's audit token, as the kernel gives it for a socket's peer:
/// unlike a pid, never another process's.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AuditToken([u32; 8]);

/// The program at the other end of a Unix socket: its user, its pid, and
/// whether a sandbox confines it (an agent's command sandbox, the App
/// Sandbox: Seatbelt's).
#[derive(Clone, Copy, Debug)]
pub struct Peer {
    pub uid: u32,
    pub pid: u32,
    pub sandboxed: bool,
    token: AuditToken,
}

impl Peer {
    /// A peer as a test makes it up.
    #[cfg(test)]
    pub fn of(uid: u32, pid: u32, sandboxed: bool) -> Peer {
        Peer {
            uid,
            pid,
            sandboxed,
            token: AuditToken([0; 8]),
        }
    }

    /// Whether it may read the file at `path`: always, if no sandbox
    /// confines it; else as its sandbox says.
    pub fn may_read(&self, path: &Path) -> bool {
        if !self.sandboxed {
            return true;
        }
        const FILTER_PATH: c_int = 1;
        let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: a token from the kernel, a NUL-terminated operation and
        // path; the flag is libsystem_sandbox's own constant.
        unsafe {
            sandbox_check_by_audit_token(
                self.token,
                c"file-read-data".as_ptr(),
                FILTER_PATH | SANDBOX_CHECK_NO_REPORT,
                path.as_ptr(),
            ) == 0
        }
    }
}

/// Who holds the other end of a Unix socket.
pub fn peer(fd: RawFd) -> Option<Peer> {
    const SOL_LOCAL: c_int = 0;
    const LOCAL_PEERPID: c_int = 0x002;
    const LOCAL_PEERTOKEN: c_int = 0x006;
    let uid = peer_uid(fd)?;
    let mut pid: c_int = 0;
    let mut len = size_of::<c_int>() as u32;
    // SAFETY: fd is a socket; the value is as long as said.
    if unsafe {
        getsockopt(
            fd,
            SOL_LOCAL,
            LOCAL_PEERPID,
            (&raw mut pid).cast(),
            &mut len,
        )
    } != 0
    {
        return None;
    }
    let mut token = AuditToken([0; 8]);
    let mut len = size_of::<AuditToken>() as u32;
    // SAFETY: as above.
    let got = unsafe {
        getsockopt(
            fd,
            SOL_LOCAL,
            LOCAL_PEERTOKEN,
            (&raw mut token).cast(),
            &mut len,
        )
    };
    if got != 0 || len as usize != size_of::<AuditToken>() {
        return None;
    }
    // Anything but 0 (1, or -1 if it couldn't say) counts as sandboxed.
    // SAFETY: a token from the kernel; no operation asks whether there's
    // a sandbox at all.
    let sandboxed = unsafe { sandbox_check_by_audit_token(token, std::ptr::null(), 0) } != 0;
    Some(Peer {
        uid,
        pid: u32::try_from(pid).ok()?,
        sandboxed,
        token,
    })
}

/// The user a process runs as, if the system says.
pub fn owner_uid(pid: u32) -> Option<u32> {
    const PROC_PIDT_SHORTBSDINFO: c_int = 13;
    #[repr(C)]
    struct ShortBsdInfo {
        pid: u32,
        ppid: u32,
        pgid: u32,
        status: u32,
        comm: [u8; 16],
        flags: u32,
        uid: u32,
        gid: u32,
        ruid: u32,
        rgid: u32,
        svuid: u32,
        svgid: u32,
        rfu: u32,
    }
    let pid = c_int::try_from(pid).ok()?;
    // SAFETY: all zeros is a valid value of plain integers.
    let mut info: ShortBsdInfo = unsafe { std::mem::zeroed() };
    let size = size_of::<ShortBsdInfo>() as c_int;
    // SAFETY: the buffer is as long as said.
    let got = unsafe { proc_pidinfo(pid, PROC_PIDT_SHORTBSDINFO, 0, (&raw mut info).cast(), size) };
    (got == size).then_some(info.uid)
}

/// The user's home folder, from the user database rather than HOME, which
/// an agent's tool may change for its commands (as it may TMPDIR).
pub fn home_dir() -> Option<PathBuf> {
    // SAFETY: all zeros is a valid passwd to be filled (null pointers).
    let mut pwd: Passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as c_char; 4096];
    let mut found: *mut Passwd = std::ptr::null_mut();
    // SAFETY: the buffer is as long as said; the strings point into it.
    let rc = unsafe { getpwuid_r(uid(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut found) };
    if rc != 0 || found.is_null() || pwd.dir.is_null() {
        return std::env::var_os("HOME").map(PathBuf::from);
    }
    // SAFETY: a NUL-terminated string in `buf`.
    let dir = unsafe { CStr::from_ptr(pwd.dir) };
    Some(PathBuf::from(OsStr::from_bytes(dir.to_bytes())))
}

#[repr(C)]
struct Passwd {
    name: *mut c_char,
    passwd: *mut c_char,
    uid: u32,
    gid: u32,
    change: i64,
    class: *mut c_char,
    gecos: *mut c_char,
    dir: *mut c_char,
    shell: *mut c_char,
    expire: i64,
}

/// Runs `f` with the file mode creation mask `mask`, so what it makes (a
/// socket) is never more open than that, whatever the user's umask. The
/// mask is the process's: only while no other thread makes files.
pub fn with_umask<T>(mask: u16, f: impl FnOnce() -> T) -> T {
    // SAFETY: no preconditions.
    let old = unsafe { umask(mask) };
    let out = f();
    // SAFETY: as above.
    unsafe { umask(old) };
    out
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

/// Asks a process group to end (SIGTERM).
pub fn terminate_group(pgid: u32) {
    const SIGTERM: c_int = 15;
    if let Ok(pgid) = c_int::try_from(pgid)
        && pgid > 1
    {
        // SAFETY: a plain signal to a group of our own.
        unsafe {
            kill(-pgid, SIGTERM);
        }
    }
}

/// Whether a process with this pid runs as this user. One that can't be
/// signalled (another user's, or out of a sandbox's reach) counts unless
/// the system says it's another user's.
pub fn alive(pid: u32) -> bool {
    const EPERM: i32 = 1;
    let Ok(id) = c_int::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks.
    id > 0
        && (unsafe { kill(id, 0) } == 0
            || (std::io::Error::last_os_error().raw_os_error() == Some(EPERM)
                && owner_uid(pid).is_none_or(|u| u == uid())))
}

/// A FIFO at `path` only this user can open: made there, and still that
/// (not something put in its place) when this answers.
pub fn make_fifo(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: a NUL-terminated path.
    if unsafe { mkfifo(c_path.as_ptr(), 0o600) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.file_type().is_fifo() || meta.uid() != uid() || meta.mode() & 0o077 != 0 {
        return Err(std::io::Error::other("not the FIFO made"));
    }
    Ok(())
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
#[derive(Clone, Copy, PartialEq)]
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

/// The terminal's mode now.
pub fn mode(fd: RawFd) -> std::io::Result<Termios> {
    // SAFETY: an all-zero termios is valid to be filled.
    let mut mode: Termios = unsafe { std::mem::zeroed() };
    // SAFETY: fd is the terminal; mode is a termios.
    if unsafe { tcgetattr(fd, &mut mode) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(mode)
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
        // Another user's (root's): not one of ours.
        assert_eq!(owner_uid(1), Some(0));
        assert!(!alive(1));
        assert_eq!(owner_uid(std::process::id()), Some(uid()));
        let home = home_dir().unwrap();
        assert!(home.is_absolute() && home.is_dir(), "{home:?}");
        assert_eq!(random_hex().unwrap().len(), 32);
    }

    #[test]
    fn a_sockets_peer() {
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        use std::os::fd::AsRawFd;
        let peer = peer(a.as_raw_fd()).unwrap();
        assert_eq!((peer.uid, peer.pid), (uid(), std::process::id()));
        // As sandboxed as this test is; then its reach is the sandbox's.
        let me = peer.sandboxed;
        assert!(peer.may_read(Path::new("/etc/hosts")));
        if !me {
            assert!(peer.may_read(Path::new("/no/such/file")));
        }
        drop(b);
    }
}
