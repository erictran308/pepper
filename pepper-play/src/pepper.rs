//! Pepper's Mach port: hands it IOSurfaces once, then says which one to show.
//! Pepper answers on this program's own port with the size of the box the
//! frames show in. The messages match Pepper's
//! macos/Sources/Features/Pepper/PepperServer.swift.
//!
//! The port's name ends in Pepper's pid (`<bundle id>.<pid>`). Before any
//! frame goes, that pid must be a running Pepper, so a name left in an
//! environment after Pepper quit, which another program could take, gets
//! nothing; and the box's sizes are taken only from that pid.

use std::ffi::{CString, c_char, c_int, c_void};
use std::io::Read;

use crate::surface::Surface;

pub const TOKEN_SIZE: usize = 64;
/// The most surfaces Pepper takes in one attach.
pub const MAX_SURFACES: usize = 4;

const MSG_ATTACH: i32 = 0x7070_0001;
const MSG_PRESENT: i32 = 0x7070_0002;
const MSG_DETACH: i32 = 0x7070_0003;
/// From Pepper: the box's size in pixels.
const MSG_SIZE: i32 = 0x7070_0101;

const MACH_SEND_MSG: i32 = 0x1;
const MACH_RCV_MSG: i32 = 0x2;
const MACH_SEND_TIMEOUT: i32 = 0x10;
const MACH_MSGH_BITS_COMPLEX: u32 = 0x8000_0000;
const MACH_MSG_TYPE_MOVE_SEND: u8 = 17;
const MACH_MSG_TYPE_COPY_SEND: u32 = 19;
const MACH_MSG_TYPE_MAKE_SEND: u32 = 20;
const MACH_MSG_PORT_DESCRIPTOR: u8 = 0;
const MACH_PORT_RIGHT_RECEIVE: u32 = 1;
const MACH_RCV_TOO_LARGE: i32 = 0x1000_4004;
const MACH_SEND_TIMED_OUT: i32 = 0x1000_0004;
const MACH_SEND_INTERRUPTED: i32 = 0x1000_0007;
/// MACH_RCV_TRAILER_TYPE(MACH_MSG_TRAILER_FORMAT_0) |
/// MACH_RCV_TRAILER_ELEMENTS(MACH_RCV_TRAILER_AUDIT): the sender's audit
/// token after each message received.
const MACH_RCV_TRAILER_AUDIT: i32 = 3 << 24;
/// mach_msg_audit_trailer_t: type, size, seqno, security token (2 words),
/// then the audit token, whose sixth word is the pid.
const AUDIT_TRAILER_SIZE: usize = 52;
const AUDIT_TRAILER_PID: usize = 40;
const PROC_PIDPATHINFO_MAXSIZE: usize = 4096;
/// Pepper's program, inside its app.
const PEPPER_EXECUTABLE: &str = ".app/Contents/MacOS/ghostty";

/// How long a send may wait for room in Pepper's queue. A frame that can't
/// go is dropped rather than holding up the next one.
const PRESENT_TIMEOUT_MS: u32 = 10;
const ATTACH_TIMEOUT_MS: u32 = 500;

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

#[repr(C)]
struct Detach {
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
    fn proc_pidpath(pid: c_int, buffer: *mut c_void, size: u32) -> c_int;
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
}

pub struct Client {
    server: u32,
    /// Our receive right: Pepper watches it to learn when this program ends,
    /// and sends the box's size to it.
    port: u32,
    /// Pepper's pid, from the port's name.
    pepper: i32,
}

/// The pid at the end of a port name (`<bundle id>.<pid>`), if it's a
/// running Pepper.
fn pepper_pid(name: &str) -> Result<i32, String> {
    let pid: i32 = name
        .rsplit_once('.')
        .and_then(|(_, pid)| pid.parse().ok())
        .filter(|&pid| pid > 0)
        .ok_or_else(|| format!("PEPPER_PORT ({name}) doesn't end in Pepper's pid"))?;
    let mut path = vec![0u8; PROC_PIDPATHINFO_MAXSIZE];
    // SAFETY: the buffer is PROC_PIDPATHINFO_MAXSIZE bytes.
    let n = unsafe { proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
    let path = &path[..n.max(0) as usize];
    if n <= 0 || !path.ends_with(PEPPER_EXECUTABLE.as_bytes()) {
        return Err(format!(
            "PEPPER_PORT ({name}) is left from a Pepper that has quit: \
             no Pepper runs as pid {pid}"
        ));
    }
    Ok(pid)
}

impl Client {
    /// Looks up Pepper's port by the name in `PEPPER_PORT`.
    pub fn connect(name: &str) -> Result<Client, String> {
        let pepper = pepper_pid(name)?;
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
                return Err(format!("mach_port_allocate failed: {kr:#x}"));
            }
        }
        Ok(Client {
            server,
            port,
            pepper,
        })
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
        let mut message = Detach {
            header: Header {
                bits: MACH_MSG_TYPE_COPY_SEND,
                id: MSG_DETACH,
                ..Default::default()
            },
            token: padded(token),
        };
        let _ = self.send(&mut message.header, size_of::<Detach>(), PRESENT_TIMEOUT_MS);
    }

    /// What Pepper's size reports arrive on, for a thread of its own.
    pub fn reports(&self) -> Reports {
        Reports {
            port: self.port,
            pepper: self.pepper,
        }
    }
}

/// Pepper's reports of the box's size, on this program's port.
pub struct Reports {
    port: u32,
    /// Only Pepper's pid's reports are taken.
    pepper: i32,
}

impl Reports {
    /// Waits for Pepper's next report: the token and its box. None when the
    /// port can't be read any more.
    pub fn next(&self) -> Option<(String, BoxSize)> {
        // Room for the message and the trailer the kernel adds.
        let mut buffer = [0u64; 32];
        loop {
            let header = buffer.as_mut_ptr().cast::<Header>();
            // SAFETY: the buffer is 256 bytes, aligned for a header; the
            // kernel writes at most that.
            let kr = unsafe {
                mach_msg(
                    header,
                    MACH_RCV_MSG | MACH_RCV_TRAILER_AUDIT,
                    0,
                    size_of_val(&buffer) as u32,
                    self.port,
                    0,
                    0,
                )
            };
            match kr {
                0 => {}
                // Without MACH_RCV_LARGE the kernel has dropped it already.
                MACH_RCV_TOO_LARGE => continue,
                _ => return None,
            }
            // SAFETY: the kernel wrote a whole message of msgh_size bytes,
            // then the audit trailer.
            let header = unsafe { &mut *header };
            let simple = header.bits & MACH_MSGH_BITS_COMPLEX == 0;
            let size = header.size as usize;
            let bytes = size_of_val(&buffer);
            let from_pepper = size <= bytes - AUDIT_TRAILER_SIZE
                && sender_pid(&buffer, size) == Some(self.pepper);
            let message = (header.id == MSG_SIZE
                && simple
                && from_pepper
                && size >= size_of::<SizeMessage>())
            .then(|| {
                // SAFETY: checked to be a simple message this long.
                unsafe { &*(header as *const Header).cast::<SizeMessage>() }
            });
            let report = message.and_then(|message| {
                Some((
                    token_from(&message.token)?,
                    BoxSize {
                        width: message.width,
                        height: message.height,
                        visible: message.visible != 0,
                    },
                ))
            });
            // SAFETY: a received message, read already; frees any rights
            // it carries (a report carries none).
            unsafe { mach_msg_destroy(header) };
            let Some((token, size)) = report else {
                continue;
            };
            return Some((token, size));
        }
    }
}

/// The sender's pid, from the audit trailer after a message of `size` bytes.
fn sender_pid(buffer: &[u64; 32], size: usize) -> Option<i32> {
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
    (word(0)? == 0 && word(4)? as usize >= AUDIT_TRAILER_SIZE)
        .then(|| word(AUDIT_TRAILER_PID).map(|pid| pid as i32))
        .flatten()
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
    fn pepper_is_the_pid_in_the_name() {
        assert!(pepper_pid("io.github.erictran308.pepper").is_err());
        assert!(pepper_pid("io.github.erictran308.pepper.0").is_err());
        // pid 1 runs, but isn't Pepper.
        let err = pepper_pid("io.github.erictran308.pepper.1").unwrap_err();
        assert!(err.contains("no Pepper runs as pid 1"), "{err}");
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
        assert_eq!(sender_pid(&buffer, size), Some(4321));
        words[at + 1] = 20; // a trailer without the audit token
        assert_eq!(sender_pid(&buffer, size), None);
    }

    #[test]
    fn messages_are_laid_out_as_pepper_reads_them() {
        assert_eq!(size_of::<Header>(), 24);
        assert_eq!(size_of::<Present>(), 24 + 64 + 4);
        assert_eq!(size_of::<SizeMessage>(), 24 + 64 + 12);
        // Header, descriptor count, a 12-byte descriptor each, the token:
        // where Pepper looks for it.
        assert_eq!(size_of::<Attach<3>>(), 24 + 4 + 3 * 12 + 64);
        assert_eq!(size_of::<Attach<4>>(), 24 + 4 + 4 * 12 + 64);
    }
}
