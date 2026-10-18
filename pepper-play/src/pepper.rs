//! Pepper's Mach port: hands it IOSurfaces once, then says which one to show.
//! Pepper answers on this program's own port with the size of the box the
//! frames show in. The messages match Pepper's
//! macos/Sources/Features/Pepper/PepperServer.swift.
//!
//! The port's name is Pepper's bundle id and pid (`<bundle id>.<pid>`).
//! Before any frame goes, that pid must run Pepper's app, and the port's
//! holder must be that very process: it answers a hello, and the kernel
//! says who sent the answer. So a name left in an environment after Pepper
//! quit, which another program could take (and its pid too), gets nothing;
//! and the box's sizes are taken only from that process.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io::Read;
use std::time::{Duration, Instant};

use crate::surface::Surface;

pub const TOKEN_SIZE: usize = 64;
/// The most surfaces Pepper takes in one attach.
pub const MAX_SURFACES: usize = 4;

const MSG_ATTACH: i32 = 0x7070_0001;
const MSG_PRESENT: i32 = 0x7070_0002;
const MSG_DETACH: i32 = 0x7070_0003;
const MSG_HELLO: i32 = 0x7070_0004;
/// From Pepper: the box's size in pixels.
const MSG_SIZE: i32 = 0x7070_0101;
/// From Pepper: an attach it didn't take.
const MSG_REFUSED: i32 = 0x7070_0102;
/// From Pepper: its answer to a hello.
const MSG_HELLO_ANSWER: i32 = 0x7070_0103;

const MACH_SEND_MSG: i32 = 0x1;
const MACH_RCV_MSG: i32 = 0x2;
const MACH_SEND_TIMEOUT: i32 = 0x10;
const MACH_RCV_TIMEOUT: i32 = 0x100;
const MACH_MSGH_BITS_COMPLEX: u32 = 0x8000_0000;
const MACH_MSG_TYPE_MOVE_SEND: u8 = 17;
const MACH_MSG_TYPE_COPY_SEND: u32 = 19;
const MACH_MSG_TYPE_MAKE_SEND: u32 = 20;
const MACH_MSG_TYPE_MAKE_SEND_ONCE: u32 = 21;
const MACH_MSG_PORT_DESCRIPTOR: u8 = 0;
const MACH_PORT_RIGHT_RECEIVE: u32 = 1;
const MACH_RCV_TIMED_OUT: i32 = 0x1000_4003;
const MACH_RCV_TOO_LARGE: i32 = 0x1000_4004;
const MACH_SEND_TIMED_OUT: i32 = 0x1000_0004;
const MACH_SEND_INTERRUPTED: i32 = 0x1000_0007;
/// The kernel's send-once notice (mach/notify.h): a reply right was let go
/// unanswered, as by a Pepper that doesn't know hellos.
const MACH_NOTIFY_SEND_ONCE: i32 = 0o107;
/// MACH_RCV_TRAILER_TYPE(MACH_MSG_TRAILER_FORMAT_0) |
/// MACH_RCV_TRAILER_ELEMENTS(MACH_RCV_TRAILER_AUDIT): the sender's audit
/// token after each message received.
const MACH_RCV_TRAILER_AUDIT: i32 = 3 << 24;
/// mach_msg_audit_trailer_t: type, size, seqno, security token (2 words),
/// then the audit token, whose sixth word is the pid and eighth its version.
const AUDIT_TRAILER_SIZE: usize = 52;
const AUDIT_TRAILER_PID: usize = 40;
const AUDIT_TRAILER_VERSION: usize = 48;
/// task_info's flavor for a task's audit token (8 words).
const TASK_AUDIT_TOKEN: u32 = 15;
const PROC_PIDPATHINFO_MAXSIZE: usize = 4096;
/// Pepper's program, inside its app.
const PEPPER_EXECUTABLE: &str = "/Contents/MacOS/ghostty";
/// Pepper's bundle ids, which start its port's name: the app's, and a debug
/// build's.
const PEPPER_IDS: [&str; 2] = [
    "io.github.erictran308.pepper",
    "io.github.erictran308.pepper.debug",
];
/// kCFStringEncodingUTF8.
const CF_STRING_UTF8: u32 = 0x0800_0100;

/// How long a send may wait for room in Pepper's queue. A frame that can't
/// go is dropped rather than holding up the next one.
const PRESENT_TIMEOUT_MS: u32 = 10;
/// An attach, a detach and a hello wait longer: a detach that couldn't go
/// would leave Pepper a stream, one of the few a program may have.
const ATTACH_TIMEOUT_MS: u32 = 500;
/// How long Pepper has to answer a hello (its main thread answers it).
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

#[repr(C)]
#[derive(Default)]
struct Header {
    bits: u32,
    size: u32,
    remote_port: u32,
    local_port: u32,
    voucher_port: u32,
    id: i32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct PortDescriptor {
    name: u32,
    pad1: u32,
    pad2: u16,
    disposition: u8,
    kind: u8,
}

/// An attach with `N` surfaces: Pepper reads the token after however many
/// there are.
#[repr(C)]
struct Attach<const N: usize> {
    header: Header,
    descriptor_count: u32,
    surfaces: [PortDescriptor; N],
    token: [u8; TOKEN_SIZE],
}

#[repr(C)]
struct Present {
    header: Header,
    token: [u8; TOKEN_SIZE],
    slot: u32,
}

/// The token alone: a detach, a hello, a hello's answer, a refusal.
#[repr(C)]
struct TokenMessage {
    header: Header,
    token: [u8; TOKEN_SIZE],
}

#[repr(C)]
struct SizeMessage {
    header: Header,
    token: [u8; TOKEN_SIZE],
    width: u32,
    height: u32,
    visible: u32,
    /// The display the box is on (a CGDirectDisplayID, 0 if not known).
    /// An older Pepper sends the message without it.
    display: u32,
}

unsafe extern "C" {
    static bootstrap_port: u32;
    static mach_task_self_: u32;
    fn bootstrap_look_up(bp: u32, name: *const c_char, sp: *mut u32) -> i32;
    fn mach_msg(
        msg: *mut Header,
        option: i32,
        send_size: u32,
        rcv_size: u32,
        rcv_name: u32,
        timeout: u32,
        notify: u32,
    ) -> i32;
    fn mach_msg_destroy(msg: *mut Header);
    fn mach_port_allocate(task: u32, right: u32, name: *mut u32) -> i32;
    fn mach_port_deallocate(task: u32, name: u32) -> i32;
    fn mach_port_insert_right(task: u32, name: u32, right: u32, kind: u32) -> i32;
    fn mach_port_mod_refs(task: u32, name: u32, right: u32, delta: i32) -> i32;
    fn task_name_for_pid(target: u32, pid: c_int, task: *mut u32) -> i32;
    fn task_info(task: u32, flavor: u32, info: *mut u32, count: *mut u32) -> i32;
    fn proc_pidpath(pid: c_int, buffer: *mut c_void, size: u32) -> c_int;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFURLCreateFromFileSystemRepresentation(
        alloc: *const c_void,
        path: *const u8,
        length: isize,
        directory: u8,
    ) -> *const c_void;
    fn CFBundleCreate(alloc: *const c_void, url: *const c_void) -> *const c_void;
    fn CFBundleGetIdentifier(bundle: *const c_void) -> *const c_void;
    fn CFStringGetCString(
        string: *const c_void,
        buffer: *mut c_char,
        size: isize,
        encoding: u32,
    ) -> u8;
    fn CFRelease(cf: *const c_void);
}

/// A token as Pepper takes it: 1–64 of `[A-Za-z0-9_-]`.
pub fn valid_token(token: &str) -> bool {
    (1..=TOKEN_SIZE).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A token of this program's own: 32 random hex digits.
pub fn new_token() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| format!("cannot read /dev/urandom: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn padded(token: &str) -> [u8; TOKEN_SIZE] {
    let mut out = [0; TOKEN_SIZE];
    out[..token.len()].copy_from_slice(token.as_bytes());
    out
}

/// The box a stream shows in, as Pepper last said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoxSize {
    pub width: u32,
    pub height: u32,
    /// Whether the box is placed at all: Pepper says so, with the last size,
    /// when it's taken away.
    pub visible: bool,
    /// The display it's on, if Pepper says.
    pub display: Option<u32>,
}

/// What Pepper says on this program's port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Report {
    /// The box of the token's stream.
    Size(String, BoxSize),
    /// Pepper didn't take the token's surfaces (it has too many streams, or
    /// can't show them): they may be offered again later.
    Refused(String),
}

/// A process as the kernel names a message's sender: its pid, and which
/// program that pid ran (the pid's version, new at each exec).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Peer {
    pid: i32,
    version: u32,
}

pub struct Client {
    server: u32,
    /// Our receive right: Pepper watches it to learn when this program ends,
    /// and sends the box's size to it.
    port: u32,
    /// Pepper: the process that answered the hello.
    pepper: Peer,
}

/// The bundle id and the pid in a port's name (`<bundle id>.<pid>`), if
/// it's one of Pepper's names.
fn name_parts(name: &str) -> Option<(&str, i32)> {
    let (id, pid) = name.rsplit_once('.')?;
    let digits = pid.bytes().all(|b| b.is_ascii_digit()) && !pid.starts_with('0');
    let pid: i32 = pid.parse().ok().filter(|&pid| digits && pid > 0)?;
    PEPPER_IDS.contains(&id).then_some((id, pid))
}

/// The process that runs as `pid` now, by its audit token: none if there's
/// no such process of this user's.
fn peer(pid: i32) -> Option<Peer> {
    let mut task = 0;
    let mut token = [0u32; 8];
    let mut count = token.len() as u32;
    // SAFETY: valid out-pointers; the task's name port is ours, let go after.
    unsafe {
        if task_name_for_pid(mach_task_self_, pid, &mut task) != 0 {
            return None;
        }
        let kr = task_info(task, TASK_AUDIT_TOKEN, token.as_mut_ptr(), &mut count);
        mach_port_deallocate(mach_task_self_, task);
        if kr != 0 || count as usize != token.len() {
            return None;
        }
    }
    Some(Peer {
        pid: token[5] as i32,
        version: token[7],
    })
}

/// The program `pid` runs, by its path.
fn program(pid: i32) -> Option<Vec<u8>> {
    let mut path = vec![0u8; PROC_PIDPATHINFO_MAXSIZE];
    // SAFETY: the buffer is PROC_PIDPATHINFO_MAXSIZE bytes.
    let n = unsafe { proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
    path.truncate(n.max(0) as usize);
    (n > 0).then_some(path)
}

/// The CFBundleIdentifier of the bundle at `path`, from its Info.plist.
fn bundle_id(path: &[u8]) -> Option<String> {
    // SAFETY: CoreFoundation calls; what's made here is released, and the
    // identifier (the bundle's) is copied out before the bundle goes.
    unsafe {
        let url = CFURLCreateFromFileSystemRepresentation(
            std::ptr::null(),
            path.as_ptr(),
            path.len() as isize,
            1,
        );
        if url.is_null() {
            return None;
        }
        let bundle = CFBundleCreate(std::ptr::null(), url);
        CFRelease(url);
        if bundle.is_null() {
            return None;
        }
        let id = CFBundleGetIdentifier(bundle);
        let mut buffer = [0 as c_char; 256];
        let copied = !id.is_null()
            && CFStringGetCString(
                id,
                buffer.as_mut_ptr(),
                buffer.len() as isize,
                CF_STRING_UTF8,
            ) != 0;
        CFRelease(bundle);
        copied.then(|| {
            CStr::from_ptr(buffer.as_ptr())
                .to_string_lossy()
                .into_owned()
        })
    }
}

/// The process a port's name says is Pepper: the pid at its end must run
/// Pepper's app, whose bundle id the name starts with.
fn pepper_at(name: &str) -> Result<Peer, String> {
    let (id, pid) = name_parts(name).ok_or_else(|| {
        format!("PEPPER_PORT ({name}) isn't the name of a Pepper's port (<bundle id>.<pid>)")
    })?;
    let gone = || {
        format!(
            "PEPPER_PORT ({name}) is left from a Pepper that has quit: \
             no Pepper runs as pid {pid}"
        )
    };
    let process = peer(pid).ok_or_else(gone)?;
    let app = program(pid).and_then(|path| {
        path.strip_suffix(PEPPER_EXECUTABLE.as_bytes())
            .filter(|app| app.ends_with(b".app"))
            .map(<[u8]>::to_vec)
    });
    let pepper = app.and_then(|app| bundle_id(&app)).as_deref() == Some(id);
    // The program and its bundle were that process's: it hasn't run another
    // since, nor has another process taken the pid.
    if !pepper || peer(pid) != Some(process) {
        return Err(gone());
    }
    Ok(process)
}

impl Client {
    /// Looks up Pepper's port by the name in `PEPPER_PORT`, and makes sure
    /// Pepper holds it.
    pub fn connect(name: &str) -> Result<Client, String> {
        let pepper = pepper_at(name)?;
        let c_name = CString::new(name).map_err(|_| "PEPPER_PORT holds a NUL")?;
        let mut server = 0;
        let mut port = 0;
        // SAFETY: plain Mach calls with valid out-pointers.
        unsafe {
            let kr = bootstrap_look_up(bootstrap_port, c_name.as_ptr(), &mut server);
            if kr != 0 {
                return Err(format!("bootstrap_look_up({name}) failed: {kr:#x}"));
            }
            let kr = mach_port_allocate(mach_task_self_, MACH_PORT_RIGHT_RECEIVE, &mut port);
            if kr != 0 {
                mach_port_deallocate(mach_task_self_, server);
                return Err(format!("mach_port_allocate failed: {kr:#x}"));
            }
        }
        // Dropped on an error below, which lets both rights go.
        let client = Client {
            server,
            port,
            pepper,
        };
        // A program that took a quit Pepper's name (and its pid) can't
        // answer as that Pepper: the kernel names who sent the answer.
        let answered = client.hello()?;
        if answered != pepper {
            return Err(format!(
                "PEPPER_PORT ({name}) isn't held by the Pepper that runs as pid {}",
                pepper.pid
            ));
        }
        Ok(client)
    }

    fn send(&self, header: &mut Header, size: usize, timeout_ms: u32) -> Result<(), i32> {
        header.remote_port = self.server;
        header.size = size as u32;
        // SAFETY: header starts a message of `size` bytes, laid out as Pepper
        // reads it.
        let kr = unsafe {
            mach_msg(
                header,
                MACH_SEND_MSG | MACH_SEND_TIMEOUT,
                size as u32,
                0,
                0,
                timeout_ms,
                0,
            )
        };
        if kr == 0 { Ok(()) } else { Err(kr) }
    }

    /// Says hello on Pepper's port, with a reply right to this program's,
    /// and waits for the answer: who sent it.
    fn hello(&self) -> Result<Peer, String> {
        let mut message = TokenMessage {
            header: Header {
                bits: MACH_MSG_TYPE_COPY_SEND | (MACH_MSG_TYPE_MAKE_SEND_ONCE << 8),
                local_port: self.port,
                id: MSG_HELLO,
                ..Default::default()
            },
            token: padded("hello"),
        };
        self.send(
            &mut message.header,
            size_of::<TokenMessage>(),
            ATTACH_TIMEOUT_MS,
        )
        .map_err(|kr| format!("hello to Pepper failed: {kr:#x}"))?;
        let no_answer = || "Pepper didn't answer (a Pepper older than this program?)".to_string();
        let deadline = Instant::now() + HELLO_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let message = Received::on(self.port, Some(left)).map_err(|kr| match kr {
                MACH_RCV_TIMED_OUT => no_answer(),
                kr => format!("waiting for Pepper failed: {kr:#x}"),
            })?;
            match message.header().id {
                MSG_HELLO_ANSWER if message.simple() => {
                    return message.sender().ok_or_else(no_answer);
                }
                // The reply right came back unanswered.
                MACH_NOTIFY_SEND_ONCE => return Err(no_answer()),
                _ => {}
            }
        }
    }

    /// Hands Pepper the surfaces of `token`'s stream, replacing any before:
    /// 1 to MAX_SURFACES of them.
    pub fn attach<const N: usize>(
        &self,
        token: &str,
        surfaces: [&Surface; N],
    ) -> Result<(), String> {
        const { assert!(N >= 1 && N <= MAX_SURFACES) };
        let ports = surfaces.map(|surface| surface.mach_port());
        let free = |ports: &[u32]| {
            for &port in ports.iter().filter(|&&port| port != 0) {
                // SAFETY: a send right of ours, from IOSurfaceCreateMachPort.
                unsafe { mach_port_deallocate(mach_task_self_, port) };
            }
        };
        if ports.contains(&0) {
            free(&ports);
            return Err("attach failed: a surface has no Mach port".into());
        }
        let mut message = Attach {
            header: Header {
                bits: MACH_MSG_TYPE_COPY_SEND
                    | (MACH_MSG_TYPE_MAKE_SEND << 8)
                    | MACH_MSGH_BITS_COMPLEX,
                local_port: self.port,
                id: MSG_ATTACH,
                ..Default::default()
            },
            descriptor_count: N as u32,
            surfaces: ports.map(|name| PortDescriptor {
                name,
                disposition: MACH_MSG_TYPE_MOVE_SEND,
                kind: MACH_MSG_PORT_DESCRIPTOR,
                ..Default::default()
            }),
            token: padded(token),
        };
        let sent = self.send(
            &mut message.header,
            size_of::<Attach<N>>(),
            ATTACH_TIMEOUT_MS,
        );
        if let Err(kr) = sent {
            // The surfaces' rights didn't go to Pepper: free them.
            if kr == MACH_SEND_TIMED_OUT || kr == MACH_SEND_INTERRUPTED {
                // The message came back to us, as if received, with them.
                // SAFETY: a pseudo-received message; frees its rights.
                unsafe { mach_msg_destroy(&mut message.header) };
            } else {
                free(&ports);
            }
        }
        sent.map_err(|kr| format!("attach failed: {kr:#x}"))
    }

    /// Shows surface `slot` of `token`'s stream. False if it couldn't go.
    pub fn present(&self, token: &str, slot: usize) -> bool {
        let mut message = Present {
            header: Header {
                bits: MACH_MSG_TYPE_COPY_SEND,
                id: MSG_PRESENT,
                ..Default::default()
            },
            token: padded(token),
            slot: slot as u32,
        };
        self.send(
            &mut message.header,
            size_of::<Present>(),
            PRESENT_TIMEOUT_MS,
        )
        .is_ok()
    }

    pub fn detach(&self, token: &str) {
        let mut message = TokenMessage {
            header: Header {
                bits: MACH_MSG_TYPE_COPY_SEND,
                id: MSG_DETACH,
                ..Default::default()
            },
            token: padded(token),
        };
        let _ = self.send(
            &mut message.header,
            size_of::<TokenMessage>(),
            ATTACH_TIMEOUT_MS,
        );
    }

    /// What Pepper's reports arrive on, for a thread of its own. Their wait
    /// ends once this client is dropped.
    pub fn reports(&self) -> Reports {
        // A send right of the reports' own keeps the port's name theirs (a
        // dead name) after the client lets the receive right go: a wait on
        // it then ends, and never reads another port given that name.
        // SAFETY: made from our own receive right.
        let held = unsafe {
            mach_port_insert_right(
                mach_task_self_,
                self.port,
                self.port,
                MACH_MSG_TYPE_MAKE_SEND,
            )
        } == 0;
        Reports {
            port: self.port,
            pepper: self.pepper,
            held,
        }
    }
}

impl Drop for Client {
    /// Lets this program's port go, which ends its streams in Pepper and a
    /// wait for reports, and Pepper's port.
    fn drop(&mut self) {
        // SAFETY: rights of ours, used no more.
        unsafe {
            mach_port_mod_refs(mach_task_self_, self.port, MACH_PORT_RIGHT_RECEIVE, -1);
            mach_port_deallocate(mach_task_self_, self.server);
        }
    }
}

/// A message received on this program's port, with the audit trailer the
/// kernel adds. Dropping it frees any rights it carries.
struct Received {
    /// Room for the message and the trailer.
    buffer: [u64; 32],
}

impl Received {
    /// Waits for the next message on `port`, at most `timeout` if one is
    /// given. Err is mach_msg's code.
    fn on(port: u32, timeout: Option<Duration>) -> Result<Received, i32> {
        let (option, timeout_ms) = match timeout {
            Some(t) => (MACH_RCV_TIMEOUT, t.as_millis().min(u32::MAX as u128) as u32),
            None => (0, 0),
        };
        loop {
            let mut buffer = [0u64; 32];
            // SAFETY: the buffer is 256 bytes, aligned for a header; the
            // kernel writes at most that.
            let kr = unsafe {
                mach_msg(
                    buffer.as_mut_ptr().cast(),
                    MACH_RCV_MSG | MACH_RCV_TRAILER_AUDIT | option,
                    0,
                    size_of_val(&buffer) as u32,
                    port,
                    timeout_ms,
                    0,
                )
            };
            match kr {
                0 => return Ok(Received { buffer }),
                // Without MACH_RCV_LARGE the kernel has dropped it already.
                MACH_RCV_TOO_LARGE => continue,
                _ => return Err(kr),
            }
        }
    }

    fn header(&self) -> &Header {
        // SAFETY: the kernel wrote a header at the start of the buffer.
        unsafe { &*self.buffer.as_ptr().cast::<Header>() }
    }

    /// Whether it carries no rights or memory, as Pepper's messages don't.
    fn simple(&self) -> bool {
        self.header().bits & MACH_MSGH_BITS_COMPLEX == 0
    }

    /// Who sent it, from the audit trailer.
    fn sender(&self) -> Option<Peer> {
        let size = self.header().size as usize;
        (size <= size_of_val(&self.buffer) - AUDIT_TRAILER_SIZE)
            .then(|| sender(&self.buffer, size))
            .flatten()
    }

    /// The token it starts with, if it's long enough to hold one.
    fn token(&self) -> Option<String> {
        if (self.header().size as usize) < size_of::<TokenMessage>() {
            return None;
        }
        // SAFETY: a message this long starts with a header and a token; the
        // buffer holds a TokenMessage whatever the message's size.
        token_from(&unsafe { &*self.buffer.as_ptr().cast::<TokenMessage>() }.token)
    }
}

impl Drop for Received {
    fn drop(&mut self) {
        // SAFETY: a message the kernel wrote, read already.
        unsafe { mach_msg_destroy(self.buffer.as_mut_ptr().cast()) };
    }
}

/// Pepper's reports, on this program's port.
pub struct Reports {
    port: u32,
    /// Only Pepper's are taken: the process that answered the hello.
    pepper: Peer,
    /// A send right to the port is ours (`Client::reports`).
    held: bool,
}

impl Reports {
    /// Waits for Pepper's next report. None once the port can't be read any
    /// more (the client is dropped).
    pub fn next_report(&self) -> Option<Report> {
        loop {
            let message = Received::on(self.port, None).ok()?;
            if !message.simple() || message.sender() != Some(self.pepper) {
                continue;
            }
            let size = message.header().size as usize;
            let Some(token) = message.token() else {
                continue;
            };
            match message.header().id {
                MSG_SIZE if size >= std::mem::offset_of!(SizeMessage, display) => {
                    // SAFETY: as in `Received::token`; its display is read
                    // only if the message is longer still.
                    let report = unsafe { &*message.buffer.as_ptr().cast::<SizeMessage>() };
                    return Some(Report::Size(
                        token,
                        BoxSize {
                            width: report.width,
                            height: report.height,
                            visible: report.visible != 0,
                            display: (size >= size_of::<SizeMessage>())
                                .then_some(report.display)
                                .filter(|&d| d != 0),
                        },
                    ));
                }
                MSG_REFUSED => return Some(Report::Refused(token)),
                _ => {}
            }
        }
    }

    /// Waits for Pepper's next report of a box: the token and its box. None
    /// once the port can't be read any more. (pepper-agent's viewer takes
    /// boxes alone.)
    #[allow(dead_code)]
    pub fn next(&self) -> Option<(String, BoxSize)> {
        loop {
            if let Report::Size(token, size) = self.next_report()? {
                return Some((token, size));
            }
        }
    }
}

impl Drop for Reports {
    fn drop(&mut self) {
        if self.held {
            // SAFETY: the send right `Client::reports` made, or the dead name
            // it became.
            unsafe { mach_port_deallocate(mach_task_self_, self.port) };
        }
    }
}

/// The sender, from the audit trailer after a message of `size` bytes.
fn sender(buffer: &[u64; 32], size: usize) -> Option<Peer> {
    // SAFETY: plain bytes.
    let bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast(), size_of_val(buffer)) };
    let trailer = bytes.get(size.next_multiple_of(4)..)?;
    let word = |at: usize| {
        Some(u32::from_ne_bytes(
            trailer.get(at..at + 4)?.try_into().ok()?,
        ))
    };
    // A FORMAT_0 trailer this long holds the audit token.
    if word(0)? != 0 || (word(4)? as usize) < AUDIT_TRAILER_SIZE {
        return None;
    }
    Some(Peer {
        pid: word(AUDIT_TRAILER_PID)? as i32,
        version: word(AUDIT_TRAILER_VERSION)?,
    })
}

// SAFETY: a port name, which any thread may receive on.
unsafe impl Send for Reports {}

fn token_from(bytes: &[u8; TOKEN_SIZE]) -> Option<String> {
    let length = bytes.iter().position(|&b| b == 0).unwrap_or(TOKEN_SIZE);
    let token = std::str::from_utf8(&bytes[..length]).ok()?;
    valid_token(token).then(|| token.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command};

    const MACH_MSG_TYPE_MOVE_SEND_ONCE: u32 = 18;

    #[test]
    fn tokens() {
        assert!(valid_token("ab_12-x"));
        assert!(!valid_token(""));
        assert!(!valid_token("a b"));
        assert!(!valid_token(&"a".repeat(65)));
        let token = new_token().unwrap();
        assert_eq!(token.len(), 32);
        assert!(valid_token(&token));
        assert_ne!(token, new_token().unwrap());
        assert_eq!(token_from(&padded("ab_1")), Some("ab_1".into()));
        assert_eq!(token_from(&padded(&"z".repeat(64))), Some("z".repeat(64)));
        assert_eq!(token_from(&[0; TOKEN_SIZE]), None);
        let mut bad = padded("ab");
        bad[1] = b' ';
        assert_eq!(token_from(&bad), None);
    }

    #[test]
    fn only_peppers_port_names() {
        assert_eq!(
            name_parts("io.github.erictran308.pepper.42"),
            Some(("io.github.erictran308.pepper", 42))
        );
        assert_eq!(
            name_parts("io.github.erictran308.pepper.debug.42"),
            Some(("io.github.erictran308.pepper.debug", 42))
        );
        for name in [
            "io.github.erictran308.pepper",
            "io.github.erictran308.pepper.",
            "io.github.erictran308.pepper.0",
            "io.github.erictran308.pepper.042",
            "io.github.erictran308.pepper.+42",
            "io.github.erictran308.pepper.42 ",
            "io.github.erictran308.pepper.99999999999",
            "io.github.erictran308.peppers.42",
            "com.example.io.github.erictran308.pepper.42",
            "io.github.erictran308.42",
            "x.42",
        ] {
            assert_eq!(name_parts(name), None, "{name}");
        }
    }

    #[test]
    fn pepper_is_the_pid_in_the_name() {
        assert!(pepper_at("io.github.erictran308.pepper").is_err());
        assert!(pepper_at("io.github.erictran308.pepper.0").is_err());
        // pid 1 runs, but isn't Pepper (nor this user's).
        let err = pepper_at("io.github.erictran308.pepper.1").unwrap_err();
        assert!(err.contains("no Pepper runs as pid 1"), "{err}");
        // This process runs, as this user, but not from Pepper's app.
        let me = std::process::id();
        let err = pepper_at(&format!("io.github.erictran308.pepper.{me}")).unwrap_err();
        assert!(
            err.contains(&format!("no Pepper runs as pid {me}")),
            "{err}"
        );
    }

    /// Not a test: what `fake_app` runs, a program that waits to be killed.
    #[test]
    #[ignore]
    fn waiting() {
        if std::env::var_os("PEPPER_TEST_WAIT").is_some() {
            std::thread::sleep(Duration::from_secs(30));
        }
    }

    /// `<dir>/<name>.app/Contents/MacOS/ghostty` (this test program, copied)
    /// in a bundle with the id `id`, running.
    fn fake_app(dir: &Path, name: &str, id: &str) -> Child {
        let contents = dir.join(format!("{name}.app/Contents"));
        std::fs::create_dir_all(contents.join("MacOS")).unwrap();
        std::fs::write(
            contents.join("Info.plist"),
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                 <plist version=\"1.0\"><dict>\
                 <key>CFBundleIdentifier</key><string>{id}</string>\
                 <key>CFBundleExecutable</key><string>ghostty</string>\
                 </dict></plist>\n"
            ),
        )
        .unwrap();
        let program = contents.join("MacOS/ghostty");
        std::fs::copy(std::env::current_exe().unwrap(), &program).unwrap();
        Command::new(program)
            .args(["--exact", "pepper::tests::waiting", "--ignored", "-q"])
            .env("PEPPER_TEST_WAIT", "1")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    fn scratch(what: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pepper-test-{what}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn pepper_is_peppers_app_not_any_ghostty() {
        let dir = scratch("apps");
        // Another app whose program is called ghostty, as the real Ghostty's
        // is: a path that ends as Pepper's isn't enough.
        let mut other = fake_app(&dir, "Ghostty", "com.mitchellh.ghostty");
        let mut pepper = fake_app(&dir, "Pepper", "io.github.erictran308.pepper");
        let name = |child: &Child| format!("io.github.erictran308.pepper.{}", child.id());
        let refused = pepper_at(&name(&other));
        let taken = pepper_at(&name(&pepper));
        // Pepper's name says which build: a debug build's id isn't the app's.
        let debug = pepper_at(&format!(
            "io.github.erictran308.pepper.debug.{}",
            pepper.id()
        ));
        for child in [&mut other, &mut pepper] {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(refused.unwrap_err().contains("no Pepper runs as pid"));
        assert_eq!(taken.map(|peer| peer.pid), Ok(pepper.id() as i32));
        assert!(debug.is_err());
        // Gone now: the pid is no one's, or another program's.
        assert!(pepper_at(&name(&pepper)).is_err());
    }

    /// A port of this process's, with a send right to it.
    fn own_port() -> u32 {
        let mut port = 0;
        // SAFETY: valid out-pointer; a right made from our own.
        unsafe {
            assert_eq!(
                mach_port_allocate(mach_task_self_, MACH_PORT_RIGHT_RECEIVE, &mut port),
                0
            );
            assert_eq!(
                mach_port_insert_right(mach_task_self_, port, port, MACH_MSG_TYPE_MAKE_SEND),
                0
            );
        }
        port
    }

    /// A client of `server` that takes `pepper` for Pepper.
    fn client(server: u32, pepper: Peer) -> Client {
        let mut port = 0;
        // SAFETY: valid out-pointer.
        assert_eq!(
            unsafe { mach_port_allocate(mach_task_self_, MACH_PORT_RIGHT_RECEIVE, &mut port) },
            0
        );
        Client {
            server,
            port,
            pepper,
        }
    }

    fn me() -> Peer {
        peer(std::process::id() as i32).unwrap()
    }

    /// Sends `message`, the first `size` bytes, to `port`, whose receive
    /// right is ours.
    fn send_to<T>(port: u32, message: &mut T, id: i32, size: usize) {
        let header = (message as *mut T).cast::<Header>();
        // SAFETY: T starts with a header, and is `size` bytes or more.
        let kr = unsafe {
            *header = Header {
                bits: MACH_MSG_TYPE_MAKE_SEND,
                size: size as u32,
                remote_port: port,
                id,
                ..Default::default()
            };
            mach_msg(header, MACH_SEND_MSG, size as u32, 0, 0, 0, 0)
        };
        assert_eq!(kr, 0);
    }

    fn size_message(token: &str, width: u32, height: u32, display: u32) -> SizeMessage {
        SizeMessage {
            header: Header::default(),
            token: padded(token),
            width,
            height,
            visible: 1,
            display,
        }
    }

    impl Client {
        /// A client that takes this process for Pepper, of no server: its
        /// reports are what `tell` sends it.
        pub fn own() -> Client {
            client(0, me())
        }

        /// Sends this client `report`, as Pepper would.
        pub fn tell(&self, report: &Report) {
            match report {
                Report::Size(token, size) => {
                    let display = size.display.unwrap_or(0);
                    let mut message = size_message(token, size.width, size.height, display);
                    message.visible = size.visible as u32;
                    send_to(self.port, &mut message, MSG_SIZE, size_of::<SizeMessage>());
                }
                Report::Refused(token) => {
                    let mut message = TokenMessage {
                        header: Header::default(),
                        token: padded(token),
                    };
                    send_to(
                        self.port,
                        &mut message,
                        MSG_REFUSED,
                        size_of::<TokenMessage>(),
                    );
                }
            }
        }
    }

    #[test]
    fn hello_names_who_answered() {
        let server = own_port();
        // A Pepper of our own, which answers one hello.
        let pepper = std::thread::spawn(move || {
            let hello = Received::on(server, Some(Duration::from_secs(5))).unwrap();
            assert_eq!(hello.header().id, MSG_HELLO);
            let reply = hello.header().remote_port;
            let mut answer = TokenMessage {
                header: Header {
                    bits: MACH_MSG_TYPE_MOVE_SEND_ONCE,
                    size: size_of::<TokenMessage>() as u32,
                    remote_port: reply,
                    id: MSG_HELLO_ANSWER,
                    ..Default::default()
                },
                token: padded("hello"),
            };
            // SAFETY: a whole message; the reply right goes with it.
            let kr = unsafe {
                mach_msg(
                    &mut answer.header,
                    MACH_SEND_MSG,
                    size_of::<TokenMessage>() as u32,
                    0,
                    0,
                    0,
                    0,
                )
            };
            assert_eq!(kr, 0);
            // Nothing left in it to free.
            std::mem::forget(hello);
        });
        let client = client(server, me());
        assert_eq!(client.hello(), Ok(me()));
        pepper.join().unwrap();
    }

    #[test]
    fn a_hello_unanswered_fails_at_once() {
        let server = own_port();
        // A server that doesn't know hellos: it lets the message go.
        let old = std::thread::spawn(move || {
            drop(Received::on(server, Some(Duration::from_secs(5))).unwrap());
        });
        let client = client(server, me());
        let started = Instant::now();
        assert!(client.hello().unwrap_err().contains("didn't answer"));
        assert!(started.elapsed() < HELLO_TIMEOUT / 2);
        old.join().unwrap();
    }

    #[test]
    fn reports_say_sizes_and_refusals() {
        let client = client(own_port(), me());
        let mut size = size_message("t", 640, 360, 7);
        send_to(client.port, &mut size, MSG_SIZE, size_of::<SizeMessage>());
        let mut refused = TokenMessage {
            header: Header::default(),
            token: padded("t"),
        };
        send_to(
            client.port,
            &mut refused,
            MSG_REFUSED,
            size_of::<TokenMessage>(),
        );
        // An older Pepper's size, without the display.
        let mut old = size_message("t", 320, 180, 9);
        let before_display = std::mem::offset_of!(SizeMessage, display);
        send_to(client.port, &mut old, MSG_SIZE, before_display);
        let reports = client.reports();
        let box_of = |width, height, display| BoxSize {
            width,
            height,
            visible: true,
            display,
        };
        assert_eq!(
            reports.next_report(),
            Some(Report::Size("t".into(), box_of(640, 360, Some(7))))
        );
        assert_eq!(reports.next_report(), Some(Report::Refused("t".into())));
        // `next` passes refusals by: only boxes.
        send_to(
            client.port,
            &mut refused,
            MSG_REFUSED,
            size_of::<TokenMessage>(),
        );
        let mut last = size_message("u", 1, 2, 0);
        send_to(client.port, &mut last, MSG_SIZE, size_of::<SizeMessage>());
        assert_eq!(reports.next(), Some(("t".into(), box_of(320, 180, None))));
        assert_eq!(reports.next(), Some(("u".into(), box_of(1, 2, None))));
    }

    #[test]
    fn reports_are_the_pepper_that_answered() {
        // This process, but as if it ran another program: not that Pepper.
        let other = Peer {
            version: me().version.wrapping_add(1),
            ..me()
        };
        let client = client(own_port(), other);
        let mut size = size_message("t", 640, 360, 0);
        send_to(client.port, &mut size, MSG_SIZE, size_of::<SizeMessage>());
        let reports = client.reports();
        let waiting = std::thread::spawn(move || reports.next_report());
        std::thread::sleep(Duration::from_millis(100));
        assert!(!waiting.is_finished());
        // Dropping the client ends the wait.
        drop(client);
        assert_eq!(waiting.join().unwrap(), None);
    }

    #[test]
    fn the_sender_from_the_audit_trailer() {
        let mut buffer = [0u64; 32];
        let size = size_of::<SizeMessage>();
        let words: &mut [u32] =
            unsafe { std::slice::from_raw_parts_mut(buffer.as_mut_ptr().cast(), 64) };
        let at = size / 4;
        words[at] = 0; // FORMAT_0
        words[at + 1] = AUDIT_TRAILER_SIZE as u32;
        words[at + AUDIT_TRAILER_PID / 4] = 4321;
        words[at + AUDIT_TRAILER_VERSION / 4] = 77;
        assert_eq!(
            sender(&buffer, size),
            Some(Peer {
                pid: 4321,
                version: 77
            })
        );
        words[at + 1] = 20; // a trailer without the audit token
        assert_eq!(sender(&buffer, size), None);
    }

    #[test]
    fn messages_are_laid_out_as_pepper_reads_them() {
        assert_eq!(size_of::<Header>(), 24);
        assert_eq!(size_of::<Present>(), 24 + 64 + 4);
        assert_eq!(size_of::<TokenMessage>(), 24 + 64);
        // An older Pepper's ends before the display.
        assert_eq!(std::mem::offset_of!(SizeMessage, display), 24 + 64 + 12);
        assert_eq!(size_of::<SizeMessage>(), 24 + 64 + 16);
        // Header, descriptor count, a 12-byte descriptor each, the token:
        // where Pepper looks for it.
        assert_eq!(size_of::<Attach<3>>(), 24 + 4 + 3 * 12 + 64);
        assert_eq!(size_of::<Attach<4>>(), 24 + 4 + 4 * 12 + 64);
    }

    #[test]
    fn a_box_prints_as_the_bench_reads_it() {
        // pepper-play/bench/measure.py (BOX_LINE) reads the box from this,
        // printed under PEPPER_PLAY_DEBUG.
        let size = BoxSize {
            width: 1280,
            height: 720,
            visible: true,
            display: Some(1),
        };
        assert_eq!(
            format!("{size:?}"),
            "BoxSize { width: 1280, height: 720, visible: true, display: Some(1) }"
        );
    }
}
