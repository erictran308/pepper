//! What the viewer serves its browser on 127.0.0.1: the folder of a page
//! opened from a file, and the dead end that pages kept to this machine
//! have for a proxy.
//!
//! A page from a file is never a `file:` page in the browser, which may
//! show any other file of the user's (in a frame, as a picture), and
//! pepper-agent's pictures of the page would carry it to the agent. Its
//! folder is served instead, at `http://127.0.0.1:<port>/<token>/<file>`,
//! while the page is open: the page reaches that folder only, and nothing
//! without the token reaches it. Each request is checked: GET or HEAD, the
//! token, no `..` and no dot-name (.git, .env, .pepper) on the way, written
//! or through a link, still inside the folder once links are followed, a
//! plain file (no listings).
//!
//! Both listen on 127.0.0.1 at a port the system picks, without
//! SO_REUSEADDR or SO_REUSEPORT (std's listeners set the first): no other
//! socket shares the address, another user's can't take the port, and one
//! listening on every address (0.0.0.0) doesn't get what comes to 127.0.0.1.

use std::ffi::{OsString, c_int, c_void};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::{sys, text};

unsafe extern "C" {
    fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    fn bind(fd: c_int, addr: *const SockaddrIn, len: u32) -> c_int;
    fn listen(fd: c_int, backlog: c_int) -> c_int;
    fn setsockopt(fd: c_int, level: c_int, name: c_int, value: *const c_void, len: u32) -> c_int;
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
}

const AF_INET: c_int = 2;
const SOCK_STREAM: c_int = 1;
const F_SETFD: c_int = 2;
const FD_CLOEXEC: c_int = 1;
/// The path an open file is at (sys/fcntl.h), into MAXPATHLEN bytes.
const F_GETPATH: c_int = 50;
const MAXPATHLEN: usize = 1024;
const SOL_SOCKET: c_int = 0xffff;
const SO_LINGER: c_int = 0x0080;
const O_NONBLOCK: i32 = 0x0004;
const O_NOFOLLOW: i32 = 0x0100;

/// macOS's `struct sockaddr_in`.
#[repr(C)]
struct SockaddrIn {
    len: u8,
    family: u8,
    port: u16,
    addr: u32,
    zero: [u8; 8],
}

#[repr(C)]
struct Linger {
    on: c_int,
    time: c_int,
}

/// Requests answered at once; more wait in the listener's queue.
const MAX_BUSY: usize = 8;
/// A request's line and headers, at most.
const MAX_HEAD: usize = 8 * 1024;
/// How long a request's head may take to come in, and one write to go.
const HEAD_WAIT: Duration = Duration::from_secs(10);
const WRITE_WAIT: Duration = Duration::from_secs(30);
/// The folders a page's server keeps (the oldest goes).
const MAX_ROOTS: usize = 16;
/// Its threads only read a little and copy a file.
const SMALL_STACK: usize = 128 * 1024;

const OK: &str = "200 OK";
const BAD_REQUEST: &str = "400 Bad Request";
const NOT_FOUND: &str = "404 Not Found";
const NOT_ALLOWED: &str = "405 Method Not Allowed";
const TIMEOUT: &str = "408 Request Timeout";
const TOO_LARGE: &str = "431 Request Header Fields Too Large";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A listener on 127.0.0.1, at a port the system picks.
fn loopback_listener() -> std::io::Result<TcpListener> {
    // SAFETY: no preconditions.
    let fd = unsafe { socket(AF_INET, SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fd is a socket of our own, open; the listener closes it.
    let listener = unsafe { TcpListener::from_raw_fd(fd) };
    let size = std::mem::size_of::<SockaddrIn>();
    let addr = SockaddrIn {
        len: size as u8,
        family: AF_INET as u8,
        port: 0,
        addr: u32::from(Ipv4Addr::LOCALHOST).to_be(),
        zero: [0; 8],
    };
    // SAFETY: plain calls on that socket; addr is a sockaddr_in, as long
    // as said.
    let made = unsafe {
        fcntl(fd, F_SETFD, FD_CLOEXEC) == 0
            && bind(fd, &addr, size as u32) == 0
            && listen(fd, 64) == 0
    };
    if !made {
        return Err(std::io::Error::last_os_error());
    }
    Ok(listener)
}

fn small_thread(work: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
    std::thread::Builder::new()
        .stack_size(SMALL_STACK)
        .spawn(work)
        .map(|_| ())
}

/// Ends a listener's thread: it's told, then woken by a connection.
fn stop_listening(stop: &AtomicBool, port: u16) {
    stop.store(true, Ordering::Relaxed);
    let _ = TcpStream::connect_timeout(&(Ipv4Addr::LOCALHOST, port).into(), Duration::from_secs(1));
}

/// Closes a connection with a reset: what came on it isn't read.
fn reset(stream: TcpStream) {
    let linger = Linger { on: 1, time: 0 };
    // SAFETY: a socket of our own; linger is a struct linger, as long as
    // said.
    unsafe {
        setsockopt(
            stream.as_raw_fd(),
            SOL_SOCKET,
            SO_LINGER,
            (&raw const linger).cast(),
            std::mem::size_of::<Linger>() as u32,
        );
    }
}

/// What pages kept to this machine have for a proxy: what they load from
/// elsewhere comes here and is cut off at once (a reset), so the browser
/// says it failed. The viewer holds the port for the browser's life, so
/// nothing else can answer in its place. (A port bound and not listening
/// would hold it too, but macOS drops what comes to one: each load would
/// wait seconds to fail.)
pub struct DeadEnd {
    port: u16,
    stop: Arc<AtomicBool>,
}

impl DeadEnd {
    pub fn start() -> std::io::Result<DeadEnd> {
        let listener = loopback_listener()?;
        let port = listener.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        small_thread(move || {
            for stream in listener.incoming() {
                if stopped.load(Ordering::Relaxed) {
                    return;
                }
                match stream {
                    Ok(stream) => reset(stream),
                    // Out of descriptors, say: not in a busy loop.
                    Err(_) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
        })?;
        Ok(DeadEnd { port, stop })
    }

    /// The proxy, as a browser context takes it.
    pub fn proxy(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for DeadEnd {
    fn drop(&mut self) {
        stop_listening(&self.stop, self.port);
    }
}

/// Whether `url` is a file: address.
pub fn is_file_url(url: &str) -> bool {
    url.split_once("://")
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("file"))
}

/// A file: address's path, and what follows it (`?…`, `#…`); None if it
/// isn't one, or names no absolute path on this machine.
pub fn file_path(url: &str) -> Option<(PathBuf, String)> {
    let (scheme, rest) = url.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("file") {
        return None;
    }
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let (path, tail) = rest.split_at(rest.find(['?', '#']).unwrap_or(rest.len()));
    Some((
        PathBuf::from(OsString::from_vec(decode(path))),
        tail.to_owned(),
    ))
}

/// What's said of a file: address that names no absolute path here.
pub fn not_a_path() -> Error {
    Error::new(
        "BAD_ARGS",
        "a file: address is file:// and an absolute path on this machine",
    )
}

/// Whether a file: address would be served (`FileServer::url`), asked
/// before a page is made for it.
pub fn check(url: &str) -> Result<()> {
    let (file, _) = file_path(url).ok_or_else(not_a_path)?;
    servable(&file).map(|_| ())
}

/// A page's files: the folder of each file it shows, under a token of its
/// own. One shown before stays (`back` goes there again) until the page
/// closes, which drops it.
pub struct FileServer {
    port: u16,
    shared: Arc<Shared>,
}

struct Shared {
    /// Each folder (its real path), by its token.
    roots: Mutex<Vec<(String, PathBuf)>>,
    stop: AtomicBool,
    /// Requests being answered, MAX_BUSY at most.
    busy: Mutex<usize>,
    freed: Condvar,
}

impl FileServer {
    pub fn start() -> std::io::Result<FileServer> {
        let listener = loopback_listener()?;
        let port = listener.local_addr()?.port();
        let shared = Arc::new(Shared {
            roots: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            busy: Mutex::new(0),
            freed: Condvar::new(),
        });
        let serving = shared.clone();
        small_thread(move || accept(&listener, &serving))?;
        Ok(FileServer { port, shared })
    }

    /// The address `file` (an absolute path) is served at; its folder is
    /// served from now on.
    pub fn url(&self, file: &Path) -> Result<String> {
        let (root, name) = servable(file)?;
        let mut roots = lock(&self.shared.roots);
        let token = match roots.iter().find(|(_, r)| *r == root) {
            Some((token, _)) => token.clone(),
            None => {
                let token =
                    sys::random_hex().map_err(|e| Error::new("FAILED", format!("a token: {e}")))?;
                if roots.len() >= MAX_ROOTS {
                    roots.remove(0);
                }
                roots.push((token.clone(), root));
                token
            }
        };
        Ok(format!(
            "http://127.0.0.1:{}/{token}/{}",
            self.port,
            encode(name.as_bytes())
        ))
    }

    /// The folders it serves (their real paths), all a page may load.
    pub fn roots(&self) -> Vec<PathBuf> {
        lock(&self.shared.roots)
            .iter()
            .map(|(_, root)| root.clone())
            .collect()
    }

    /// A served address as the file's (`file:///…`), for what the agent
    /// and the human read; None if it isn't one of this server's.
    pub fn file_url(&self, url: &str) -> Option<String> {
        let rest = url.strip_prefix(&format!("http://127.0.0.1:{}/", self.port))?;
        let (token, path) = rest.split_once('/')?;
        let roots = lock(&self.shared.roots);
        let (_, root) = roots.iter().find(|(t, _)| same(t, token))?;
        let (path, tail) = path.split_at(path.find(['?', '#']).unwrap_or(path.len()));
        // Only a file's own name and folders: not one the page made
        // (pushState) to read as some other file (`..`, `a%2Fb`).
        let mut file = root.clone();
        for part in path.split('/') {
            let part = decode(part);
            if part.is_empty() || part[0] == b'.' || part.contains(&b'/') || part.contains(&0) {
                return None;
            }
            file.push(OsString::from_vec(part));
        }
        Some(format!("file://{}{tail}", file.display()))
    }
}

impl Drop for FileServer {
    fn drop(&mut self) {
        lock(&self.shared.roots).clear();
        stop_listening(&self.shared.stop, self.port);
    }
}

/// A file's folder (its real path) and name, if it may be served: an
/// absolute path to a plain file, no dot-file, no link out of its folder;
/// and a folder that isn't too wide (`too_wide`).
fn servable(file: &Path) -> Result<(PathBuf, OsString)> {
    let shown = || text::inert(&file.display().to_string(), 300);
    let refuse = |why: &str| Error::new("BAD_ARGS", format!("{} isn't shown: {why}", shown()));
    let (Some(dir), Some(name)) = (file.parent(), file.file_name()) else {
        return Err(refuse("not a file"));
    };
    if !file.is_absolute() {
        return Err(refuse("not an absolute path"));
    }
    if name.as_bytes().starts_with(b".") {
        return Err(refuse("a dot-file isn't served"));
    }
    let missing = || Error::new("NOT_FOUND", format!("no file at {}", shown()));
    let root = std::fs::canonicalize(dir).map_err(|_| missing())?;
    let real = std::fs::canonicalize(file).map_err(|_| missing())?;
    if inside(&root, &real).is_none() || !std::fs::metadata(&real).is_ok_and(|m| m.is_file()) {
        return Err(refuse("a link out of its folder, or not a plain file"));
    }
    let run = std::fs::canonicalize(sys::user_temp_dir())
        .ok()
        .map(|t| t.join(format!("pepper-agent-{}", sys::uid())));
    let home = sys::home_dir().and_then(|h| std::fs::canonicalize(h).ok());
    let outputs = crate::proto::outputs_path().and_then(|o| std::fs::canonicalize(o).ok());
    if too_wide(&root, home.as_deref(), &[run, outputs]) {
        return Err(refuse(
            "its folder holds all your files; put the page in a folder of its own",
        ));
    }
    Ok((root, name.to_owned()))
}

/// A folder too wide to serve: `/`, the home folder or one right in it
/// (Downloads, Desktop, Library: all of a kind of the user's files), or one
/// holding pepper-agent's own folders.
fn too_wide(root: &Path, home: Option<&Path>, own: &[Option<PathBuf>]) -> bool {
    root.parent().is_none()
        || home.is_some_and(|h| h.starts_with(root) || root.parent() == Some(h))
        || own.iter().flatten().any(|p| p.starts_with(root))
}

/// `path` below `root`, if it is, with no dot-name on the way.
fn inside<'a>(root: &Path, path: &'a Path) -> Option<&'a Path> {
    let below = path.strip_prefix(root).ok()?;
    let plain = below
        .components()
        .all(|c| matches!(c, Component::Normal(n) if !n.as_bytes().starts_with(b".")));
    (plain && !below.as_os_str().is_empty()).then_some(below)
}

/// The file a request's path names, if it may be served, and its folder.
fn resolve(roots: &[(String, PathBuf)], target: &str) -> Option<(PathBuf, PathBuf)> {
    let path = &target[..target.find(['?', '#']).unwrap_or(target.len())];
    let (token, rest) = path.strip_prefix('/')?.split_once('/')?;
    let (_, root) = roots.iter().find(|(t, _)| same(t, token))?;
    let mut file = root.clone();
    for part in rest.split('/') {
        let part = decode(part);
        // No `..` or `.git`, no `a%2Fb`, no listing of a folder (`a/`).
        if part.is_empty() || part[0] == b'.' || part.contains(&b'/') || part.contains(&0) {
            return None;
        }
        file.push(OsString::from_vec(part));
    }
    // Links followed: still inside, with no dot-name, and a plain file.
    let real = std::fs::canonicalize(&file).ok()?;
    inside(root, &real)?;
    std::fs::metadata(&real)
        .ok()?
        .is_file()
        .then(|| (root.clone(), real))
}

/// Where an open file is now, links resolved (F_GETPATH).
fn opened_path(file: &std::fs::File) -> Option<PathBuf> {
    let mut buf = vec![0u8; MAXPATHLEN];
    // SAFETY: F_GETPATH writes a NUL-terminated path of at most MAXPATHLEN
    // bytes into `buf`, which holds that many.
    if unsafe { fcntl(file.as_raw_fd(), F_GETPATH, buf.as_mut_ptr()) } == -1 {
        return None;
    }
    buf.truncate(buf.iter().position(|&b| b == 0)?);
    Some(PathBuf::from(OsString::from_vec(buf)))
}

/// Tokens compared in a time that doesn't tell how much matched.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |d, (x, y)| d | (x ^ y)) == 0
}

/// A name in a URL's path: letters, digits and `-._~` as they are, the
/// rest percent-encoded.
fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out += &format!("%{b:02X}");
        }
    }
    out
}

/// Percent-decoded as a browser reads it: a `%` without two hex digits
/// after it stays.
fn decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let hex = |at: usize| bytes.get(at).and_then(|&b| (b as char).to_digit(16));
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match (bytes[i], hex(i + 1), hex(i + 2)) {
            (b'%', Some(high), Some(low)) => {
                out.push((high * 16 + low) as u8);
                i += 3;
            }
            (b, _, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

fn accept(listener: &TcpListener, shared: &Arc<Shared>) {
    for stream in listener.incoming() {
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        let Ok(stream) = stream else {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        {
            let mut busy = lock(&shared.busy);
            while *busy >= MAX_BUSY {
                busy = shared.freed.wait(busy).unwrap_or_else(|e| e.into_inner());
            }
            *busy += 1;
        }
        let serving = shared.clone();
        let spawned = small_thread(move || {
            answer(stream, &serving);
            *lock(&serving.busy) -= 1;
            serving.freed.notify_one();
        });
        if spawned.is_err() {
            *lock(&shared.busy) -= 1;
        }
    }
}

/// Answers one request, then closes the connection.
fn answer(mut stream: TcpStream, shared: &Shared) {
    let _ = stream.set_write_timeout(Some(WRITE_WAIT));
    let status = match request_line(&mut stream) {
        Ok(line) => match serve(&line, shared) {
            Ok((file, len, kind, head_only)) => {
                let sent = stream.write_all(head(OK, kind, len).as_bytes());
                if sent.is_ok() && !head_only {
                    let _ = std::io::copy(&mut file.take(len), &mut stream);
                }
                let _ = stream.shutdown(Shutdown::Write);
                return;
            }
            Err(status) => status,
        },
        Err(Some(status)) => status,
        Err(None) => return,
    };
    let _ = stream.write_all(head(status, "text/plain", 0).as_bytes());
    let _ = stream.shutdown(Shutdown::Write);
}

/// The request's first line, once its whole head is in. Err(None): the
/// connection went; Err(Some(status)): what to answer.
fn request_line(stream: &mut TcpStream) -> std::result::Result<String, Option<&'static str>> {
    let deadline = Instant::now() + HEAD_WAIT;
    let mut head = Vec::new();
    let mut chunk = [0u8; 2048];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        if head.len() > MAX_HEAD {
            return Err(Some(TOO_LARGE));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Some(TIMEOUT));
        }
        let _ = stream.set_read_timeout(Some(left));
        match stream.read(&mut chunk) {
            Ok(0) => return Err(None),
            Ok(n) => head.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(Some(TIMEOUT)),
        }
    }
    let end = head.windows(2).position(|w| w == b"\r\n").unwrap_or(0);
    String::from_utf8(head[..end].to_vec()).map_err(|_| Some(BAD_REQUEST))
}

/// A file to send: it, its size, its type, and whether only its head goes.
type Served = (std::fs::File, u64, &'static str, bool);

/// The file a request line asks for, opened.
fn serve(line: &str, shared: &Shared) -> std::result::Result<Served, &'static str> {
    let mut words = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (words.next(), words.next(), words.next(), words.next())
    else {
        return Err(BAD_REQUEST);
    };
    if !version.starts_with("HTTP/1.") {
        return Err(BAD_REQUEST);
    }
    let head_only = match method {
        "GET" => false,
        "HEAD" => true,
        _ => return Err(NOT_ALLOWED),
    };
    let roots = lock(&shared.roots).clone();
    let (root, path) = resolve(&roots, target).ok_or(NOT_FOUND)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(&path)
        .map_err(|_| NOT_FOUND)?;
    let meta = file.metadata().map_err(|_| NOT_FOUND)?;
    // Opened by path: a folder on the way swapped for a link since
    // `resolve` looked would have led elsewhere. What was opened is where
    // it should be, or nothing is sent.
    if !meta.is_file() || !opened_path(&file).is_some_and(|p| inside(&root, &p).is_some()) {
        return Err(NOT_FOUND);
    }
    Ok((file, meta.len(), content_type(&path), head_only))
}

/// A response's head. Nothing cached (a watched page reloads what's new),
/// no sniffing past the type, and no other origin may use what's sent.
fn head(status: &str, kind: &str, len: u64) -> String {
    let allow = if status == NOT_ALLOWED {
        "Allow: GET, HEAD\r\n"
    } else {
        ""
    };
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {len}\r\n\
         X-Content-Type-Options: nosniff\r\nCache-Control: no-store\r\n\
         Referrer-Policy: no-referrer\r\nCross-Origin-Resource-Policy: same-origin\r\n\
         {allow}Connection: close\r\n\r\n"
    )
}

/// A file's type, by its extension.
fn content_type(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html",
        "xhtml" => "application/xhtml+xml",
        "svg" => "image/svg+xml",
        "css" => "text/css",
        "js" | "mjs" | "cjs" => "text/javascript",
        "json" | "map" => "application/json",
        "wasm" => "application/wasm",
        "txt" | "md" | "log" => "text/plain",
        "csv" => "text/csv",
        "xml" => "application/xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "ogv" => "video/ogg",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pepper-serve-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    /// What 127.0.0.1:`port` answers to `request`: its status line, head
    /// and body.
    fn ask(port: u16, request: &str) -> (String, String, Vec<u8>) {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut answer = Vec::new();
        let _ = stream.read_to_end(&mut answer);
        let end = answer
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap_or(answer.len());
        let head = String::from_utf8_lossy(&answer[..end]).into_owned();
        let body = answer.get(end + 4..).unwrap_or_default().to_vec();
        (head.lines().next().unwrap_or("").to_owned(), head, body)
    }

    fn get(port: u16, path: &str) -> (String, String, Vec<u8>) {
        ask(
            port,
            &format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"),
        )
    }

    #[test]
    fn file_addresses() {
        assert!(is_file_url("FILE:///x") && !is_file_url("http://x/file:"));
        assert_eq!(
            file_path("file:///a/b%20c.html?q=1#top"),
            Some((PathBuf::from("/a/b c.html"), "?q=1#top".into()))
        );
        assert_eq!(
            file_path("file://localhost/a/100%.html"),
            Some((PathBuf::from("/a/100%.html"), String::new()))
        );
        assert_eq!(file_path("file://host/a"), None);
        assert_eq!(file_path("http://localhost/a"), None);
        assert_eq!(encode("a b/é.html".as_bytes()), "a%20b%2F%C3%A9.html");
        assert_eq!(decode("%2e%2E/%zz%4"), b"../%zz%4");
    }

    #[test]
    fn a_page_gets_its_folder_and_nothing_else() {
        let dir = scratch("folder");
        let (site, outside) = (dir.join("site"), dir.join("outside"));
        for d in [site.join("sub"), site.join(".git"), outside.clone()] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(site.join("index.html"), "<p>hi</p>").unwrap();
        std::fs::write(site.join("sub/a.css"), "p{}").unwrap();
        std::fs::write(site.join(".env"), "SECRET").unwrap();
        std::fs::write(site.join(".git/config"), "SECRET").unwrap();
        std::fs::write(outside.join("secret.txt"), "SECRET").unwrap();
        let link = |to: &str, name: &str| std::os::unix::fs::symlink(to, site.join(name)).unwrap();
        link("../outside/secret.txt", "out.txt");
        link(".env", "env.txt");
        link("index.html", "same.html");

        let server = FileServer::start().unwrap();
        let url = server.url(&site.join("index.html")).unwrap();
        let prefix = format!("http://127.0.0.1:{}/", server.port);
        let token = url
            .strip_prefix(&prefix)
            .unwrap()
            .split('/')
            .next()
            .unwrap()
            .to_owned();
        assert_eq!(url, format!("{prefix}{token}/index.html"));
        assert_eq!(
            server.file_url(&format!("{url}#x")),
            Some(format!("file://{}#x", site.join("index.html").display()))
        );
        assert_eq!(
            server.file_url(&format!("{prefix}{token}/%2Fetc%2Fhosts")),
            None
        );
        assert_eq!(server.file_url("http://127.0.0.1:1/x/index.html"), None);
        let port = server.port;
        let at = |p: &str| format!("/{token}/{p}");

        let (status, head, body) = get(port, &at("index.html"));
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert!(
            head.contains("Content-Type: text/html\r\n")
                && head.contains("X-Content-Type-Options: nosniff")
        );
        assert_eq!(body, b"<p>hi</p>");
        assert!(
            get(port, &at("sub/a.css"))
                .1
                .contains("Content-Type: text/css")
        );
        assert_eq!(get(port, &at("same.html")).2, b"<p>hi</p>");
        let (status, head, body) =
            ask(port, &format!("HEAD {} HTTP/1.1\r\n\r\n", at("index.html")));
        assert!(
            status.ends_with("200 OK") && head.contains("Content-Length: 9") && body.is_empty()
        );
        let (status, head, _) = ask(port, &format!("POST {} HTTP/1.1\r\n\r\n", at("index.html")));
        assert!(status.ends_with("405 Method Not Allowed") && head.contains("Allow: GET, HEAD"));

        // Without the token, out of the folder, dot-names, links out,
        // folders: nothing.
        for path in [
            "/index.html".to_owned(),
            "/0123/index.html".to_owned(),
            format!("/{token}x/index.html"),
            at("../outside/secret.txt"),
            at("%2e%2e/outside/secret.txt"),
            at("sub%2F..%2F..%2Foutside%2Fsecret.txt"),
            at("sub/../../outside/secret.txt"),
            at(".env"),
            at("%2Eenv"),
            at(".git/config"),
            at("out.txt"),
            at("env.txt"),
            at(""),
            at("sub"),
            at("sub/"),
            at("missing.html"),
        ] {
            let (status, _, body) = get(port, &path);
            assert_eq!(status, "HTTP/1.1 404 Not Found", "{path}");
            assert!(body.is_empty(), "{path}");
        }
        let long = format!(
            "GET {} HTTP/1.1\r\nX: {}\r\n\r\n",
            at("index.html"),
            "a".repeat(20_000)
        );
        assert_eq!(
            ask(port, &long).0,
            "HTTP/1.1 431 Request Header Fields Too Large"
        );
        assert_eq!(ask(port, "GET\r\n\r\n").0, "HTTP/1.1 400 Bad Request");

        // A file that isn't to be shown isn't, and another folder gets a
        // token of its own.
        assert!(server.url(&site.join(".env")).is_err());
        assert!(server.url(&site.join("out.txt")).is_err());
        assert!(server.url(&site.join("sub")).is_err());
        assert_eq!(
            server.url(&site.join("nope.html")).unwrap_err().code,
            "NOT_FOUND"
        );
        let other = server.url(&site.join("sub/a.css")).unwrap();
        assert!(other.starts_with(&prefix) && !other.contains(&token));
        assert_eq!(get(port, &at("index.html")).0, "HTTP/1.1 200 OK");

        drop(server);
        std::thread::sleep(Duration::from_millis(100));
        assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn not_a_folder_of_all_the_users_files() {
        // A page right in the user's temp folder: its folder holds
        // pepper-agent's run folder (shots, records, browser profiles).
        let wide =
            sys::user_temp_dir().join(format!("pepper-serve-wide-{}.html", std::process::id()));
        std::fs::write(&wide, "x").unwrap();
        let server = FileServer::start().unwrap();
        let refused = server.url(&wide).unwrap_err();
        let _ = std::fs::remove_file(&wide);
        assert!(
            refused.message.contains("all your files"),
            "{}",
            refused.message
        );
        assert!(server.url(Path::new("/page.html")).is_err());
    }

    #[test]
    fn folders_too_wide_to_serve() {
        let home = Path::new("/Users/u");
        let own = [Some(PathBuf::from(
            "/Users/u/Library/Caches/pepper-agent-1",
        ))];
        for wide in [
            "/",
            "/Users",
            "/Users/u",
            "/Users/u/Downloads",
            "/Users/u/Library",
        ] {
            assert!(too_wide(Path::new(wide), Some(home), &own), "{wide}");
        }
        assert!(too_wide(
            Path::new("/Users/u/Library/Caches"),
            Some(home),
            &own
        ));
        for fine in [
            "/Users/u/Downloads/site",
            "/Users/u/code/app/dist",
            "/tmp/x",
        ] {
            assert!(!too_wide(Path::new(fine), Some(home), &own), "{fine}");
        }
    }

    #[test]
    fn the_dead_end_cuts_off_at_once_and_keeps_its_port() {
        let dead = DeadEnd::start().unwrap();
        let port = dead.port;
        assert_eq!(dead.proxy(), format!("http://127.0.0.1:{port}"));
        // A load through it fails at once: reset (even before the connection
        // is said to be made), nothing answered.
        let started = Instant::now();
        if let Ok(mut stream) = TcpStream::connect((Ipv4Addr::LOCALHOST, port)) {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = stream.write_all(b"GET http://example.invalid/ HTTP/1.1\r\n\r\n");
            let mut got = [0u8; 16];
            assert!(!matches!(stream.read(&mut got), Ok(n) if n > 0));
        }
        assert!(started.elapsed() < Duration::from_secs(2));
        // No one listens in its place: not on 127.0.0.1, and one on every
        // address doesn't get what comes to 127.0.0.1.
        assert!(TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_err());
        if let Ok(thief) = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)) {
            thief.set_nonblocking(true).unwrap();
            let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, port));
            std::thread::sleep(Duration::from_millis(100));
            assert!(thief.accept().is_err());
        }
        drop(dead);
    }
}
