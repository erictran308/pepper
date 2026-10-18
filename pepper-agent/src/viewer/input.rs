//! The human's keys, from the bytes the terminal sends: plain characters,
//! the few escape sequences the viewer acts on, and pastes (bracketed paste
//! is on, so a paste never runs as keys). What the terminal sends of its
//! own accord is never keys: its replies to queries printed on the pane
//! (control strings: DCS, OSC, APC, PM, SOS) and mouse reports in the old
//! forms are taken whole.

use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Enter,
    Tab,
    BackTab,
    Backspace,
    Esc,
    Up,
    Down,
    Left,
    Right,
    ShiftUp,
    ShiftDown,
    ShiftLeft,
    ShiftRight,
    PageUp,
    PageDown,
    Mouse(Mouse),
    Paste(String),
    /// A sequence the viewer doesn't use.
    Other,
}

/// A mouse report (SGR, `CSI < b ; x ; y M` or `m`): in pixels from the
/// top left of the terminal's cells when SGR pixels (1016) are on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mouse {
    /// 0 left, 1 middle, 2 right, 3 none (a move with no button).
    pub button: u8,
    /// The wheel: -1 up, 1 down (0 when it's not the wheel).
    pub wheel: i8,
    pub motion: bool,
    pub release: bool,
    pub x: i32,
    pub y: i32,
}

impl Mouse {
    /// From the parameters of an SGR report and its final byte.
    fn parse(params: &[u8], last: u8) -> Option<Mouse> {
        let text = std::str::from_utf8(params.strip_prefix(b"<")?).ok()?;
        let mut numbers = text.split(';').map(|n| n.parse::<i32>().ok());
        let (code, x, y) = (numbers.next()??, numbers.next()??, numbers.next()??);
        if numbers.next().is_some() || code < 0 {
            return None;
        }
        let wheel = if code & 64 != 0 {
            match code & 3 {
                0 => -1,
                1 => 1,
                _ => return None,
            }
        } else {
            0
        };
        Some(Mouse {
            button: if wheel != 0 { 3 } else { (code & 3) as u8 },
            wheel,
            motion: code & 32 != 0,
            release: last == b'm',
            x,
            y,
        })
    }
}

/// The most bytes kept of a paste, or of a sequence not finished yet.
const MAX_PASTE: usize = 64 * 1024;
const MAX_SEQUENCE: usize = 64;
/// A control string longer than this, or not ended this long after it
/// began, is let go: what comes after is input again. A terminal's reply
/// comes whole, at once.
const MAX_STRING: usize = 1024 * 1024;
const STRING_WAIT: Duration = Duration::from_secs(1);
/// How long a lone ESC at the end of a read waits for the rest of its
/// sequence (a read can end inside one) before it's the Esc key.
pub const ESC_WAIT: Duration = Duration::from_millis(30);

#[derive(Default)]
pub struct Input {
    /// Bytes of a sequence or character not finished at the end of a read.
    pending: Vec<u8>,
    /// When `pending` was left a lone ESC.
    esc_at: Option<Instant>,
    /// A paste being read, till ESC [ 201 ~.
    paste: Option<Vec<u8>>,
    /// A control string being skipped, till ST or BEL: when it began, and
    /// its bytes so far.
    string: Option<(Instant, usize)>,
}

const PASTE_END: &[u8] = b"\x1b[201~";

impl Input {
    #[cfg(test)]
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        self.feed_at(bytes, Instant::now())
    }

    /// The keys in `bytes`, read at `now`.
    pub fn feed_at(&mut self, bytes: &[u8], now: Instant) -> Vec<Key> {
        let mut keys: Vec<Key> = self.flush(now).into_iter().collect();
        let mut data = std::mem::take(&mut self.pending);
        self.esc_at = None;
        data.extend_from_slice(bytes);
        let mut at = 0;
        while at < data.len() {
            if self.string.is_some() {
                match self.skip_string(&data[at..]) {
                    Some(used) => at += used,
                    None => {
                        // An ESC last, which may begin ST.
                        self.pending = data[at..].to_vec();
                        return keys;
                    }
                }
                continue;
            }
            if let Some(paste) = &mut self.paste {
                match find(&data[at..], PASTE_END) {
                    Some(end) => {
                        if paste.len() < MAX_PASTE {
                            paste.extend_from_slice(&data[at..at + end]);
                        }
                        let text = String::from_utf8_lossy(paste).into_owned();
                        keys.push(Key::Paste(text));
                        self.paste = None;
                        at += end + PASTE_END.len();
                    }
                    None => {
                        // Keep what could be the start of the end marker.
                        let keep = (1..PASTE_END.len())
                            .rev()
                            .find(|&n| data[at..].ends_with(&PASTE_END[..n]))
                            .unwrap_or(0);
                        let take = data.len() - at - keep;
                        if paste.len() < MAX_PASTE {
                            paste.extend_from_slice(&data[at..at + take]);
                        }
                        self.pending = data[at + take..].to_vec();
                        return keys;
                    }
                }
                continue;
            }
            match parse_one(&data[at..]) {
                Parsed::Key(key, used) => {
                    keys.push(key);
                    at += used;
                }
                Parsed::PasteStart(used) => {
                    self.paste = Some(Vec::new());
                    at += used;
                }
                Parsed::StringStart(used) => {
                    self.string = Some((now, 0));
                    at += used;
                }
                Parsed::Incomplete => {
                    if data.len() - at <= MAX_SEQUENCE {
                        self.pending = data[at..].to_vec();
                        if self.pending == [0x1b] {
                            self.esc_at = Some(now);
                        }
                    }
                    return keys;
                }
            }
        }
        keys
    }

    /// What waiting has made of what's pending, at `now`: a lone ESC that
    /// nothing followed is the Esc key; a control string that never ended
    /// is let go.
    pub fn flush(&mut self, now: Instant) -> Option<Key> {
        if let Some((began, _)) = self.string
            && now.saturating_duration_since(began) >= STRING_WAIT
        {
            self.string = None;
            self.pending.clear();
        }
        let at = self.esc_at?;
        if now.saturating_duration_since(at) < ESC_WAIT {
            return None;
        }
        self.esc_at = None;
        self.pending.clear();
        Some(Key::Esc)
    }

    /// Whether a lone ESC waits to be the Esc key (Input::flush).
    pub fn waiting(&self) -> bool {
        self.esc_at.is_some()
    }

    /// Skips a control string's bytes at the start of `data`: how many, up
    /// to and with its end if it's there (the string is then over). None
    /// if `data` is an ESC alone, which may begin ST: it waits.
    fn skip_string(&mut self, data: &[u8]) -> Option<usize> {
        let (began, mut len) = self.string?;
        for (n, &b) in data.iter().enumerate() {
            let over = match b {
                0x07 => Some(n + 1),
                0x1b => match data.get(n + 1) {
                    None if n == 0 => return None,
                    // The ESC waits, on its own.
                    None => {
                        self.string = Some((began, len));
                        return Some(n);
                    }
                    Some(b'\\') => Some(n + 2),
                    // Not ST: the string was cut short, and a sequence
                    // begins at this ESC.
                    Some(_) => Some(n),
                },
                _ => {
                    len += 1;
                    (len > MAX_STRING).then_some(n + 1)
                }
            };
            if let Some(used) = over {
                self.string = None;
                return Some(used);
            }
        }
        self.string = Some((began, len));
        Some(data.len())
    }
}

/// Whether keys that came in one read came as no one types them: more than
/// one, not counting the mouse's reports and pastes. A terminal's replies
/// and tmux's control-mode commands come so; a person's keys one by one.
pub fn burst(keys: &[Key]) -> bool {
    keys.iter()
        .filter(|k| !matches!(k, Key::Mouse(_) | Key::Paste(_)))
        .count()
        > 1
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

enum Parsed {
    Key(Key, usize),
    PasteStart(usize),
    StringStart(usize),
    Incomplete,
}

fn parse_one(data: &[u8]) -> Parsed {
    match data[0] {
        0x1b => escape(data),
        b'\r' | b'\n' => Parsed::Key(Key::Enter, 1),
        b'\t' => Parsed::Key(Key::Tab, 1),
        0x7f | 0x08 => Parsed::Key(Key::Backspace, 1),
        b @ 0..0x20 => Parsed::Key(Key::Ctrl((b + b'a' - 1) as char), 1),
        b => {
            let len = match b {
                0xc0..0xe0 => 2,
                0xe0..0xf0 => 3,
                0xf0..0xf8 => 4,
                _ => 1,
            };
            if data.len() < len {
                return Parsed::Incomplete;
            }
            match std::str::from_utf8(&data[..len])
                .ok()
                .and_then(|s| s.chars().next())
            {
                Some(c) => Parsed::Key(Key::Char(c), len),
                None => Parsed::Key(Key::Other, 1),
            }
        }
    }
}

/// ESC, alone or starting a sequence. A lone ESC at the end of a read
/// waits for what follows (a read may end inside a sequence); if nothing
/// does, it's the Esc key (Input::flush).
fn escape(data: &[u8]) -> Parsed {
    match data.get(1) {
        None => Parsed::Incomplete,
        Some(b'[') => {
            // CSI: parameters 0x30-0x3F, intermediates 0x20-0x2F, a final.
            let mut i = 2;
            while let Some(&b) = data.get(i) {
                if (0x40..=0x7e).contains(&b) {
                    let params = &data[2..i];
                    let key = match (b, params) {
                        (b'A', b"" | b"1") => Key::Up,
                        (b'B', b"" | b"1") => Key::Down,
                        (b'C', b"" | b"1") => Key::Right,
                        (b'D', b"" | b"1") => Key::Left,
                        (b'A', b"1;2") => Key::ShiftUp,
                        (b'B', b"1;2") => Key::ShiftDown,
                        (b'C', b"1;2") => Key::ShiftRight,
                        (b'D', b"1;2") => Key::ShiftLeft,
                        (b'Z', _) => Key::BackTab,
                        (b'~', b"5") => Key::PageUp,
                        (b'~', b"6") => Key::PageDown,
                        (b'M' | b'm', [b'<', ..]) => {
                            Mouse::parse(params, b).map_or(Key::Other, Key::Mouse)
                        }
                        (b'M', b"") => return legacy_mouse(data, i + 1),
                        (b'~', b"200") => return Parsed::PasteStart(i + 1),
                        _ => Key::Other,
                    };
                    return Parsed::Key(key, i + 1);
                }
                if !(0x20..=0x3f).contains(&b) {
                    return Parsed::Key(Key::Other, i);
                }
                i += 1;
            }
            Parsed::Incomplete
        }
        Some(b'O') => match data.get(2) {
            None => Parsed::Incomplete,
            Some(b'A') => Parsed::Key(Key::Up, 3),
            Some(b'B') => Parsed::Key(Key::Down, 3),
            Some(b'C') => Parsed::Key(Key::Right, 3),
            Some(b'D') => Parsed::Key(Key::Left, 3),
            Some(_) => Parsed::Key(Key::Other, 3),
        },
        // A control string: DCS, OSC, APC, PM or SOS, till ST or BEL.
        Some(b'P' | b']' | b'_' | b'^' | b'X') => Parsed::StringStart(2),
        // ESC ESC: the first is Esc.
        Some(0x1b) => Parsed::Key(Key::Esc, 1),
        // Alt and a key.
        Some(_) => Parsed::Key(Key::Other, 2),
    }
}

/// A mouse report in the old forms the viewer never asks for, which the
/// terminal sends once something printed on the pane turns SGR off: CSI M
/// and three characters, X10's bytes or, in mode 1005, UTF-8. Taken whole,
/// so its characters aren't keys.
fn legacy_mouse(data: &[u8], mut at: usize) -> Parsed {
    for _ in 0..3 {
        match data.get(at) {
            None => return Parsed::Incomplete,
            // Never in a report: it ends here, short. (0x7f is: X10's
            // column or row 94.)
            Some(&b) if b < 0x20 => break,
            Some(0xc2..=0xdf) => match data.get(at + 1) {
                None => return Parsed::Incomplete,
                Some(0x80..=0xbf) => at += 2,
                Some(_) => at += 1,
            },
            Some(_) => at += 1,
        }
    }
    Parsed::Key(Key::Other, at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        let mut input = Input::default();
        assert_eq!(
            input.feed(b"m \t\r\x7f\x03q\x1b[A\x1bOD\x1b[1;2C\x1b[Z"),
            vec![
                Key::Char('m'),
                Key::Char(' '),
                Key::Tab,
                Key::Enter,
                Key::Backspace,
                Key::Ctrl('c'),
                Key::Char('q'),
                Key::Up,
                Key::Left,
                Key::ShiftRight,
                Key::BackTab
            ]
        );
        // A lone ESC waits for what may follow it; nothing does: Esc.
        let t = Instant::now();
        assert_eq!(input.feed_at(b"\x1b", t), vec![]);
        assert!(input.waiting());
        assert_eq!(input.flush(t + ESC_WAIT / 2), None);
        assert_eq!(input.flush(t + ESC_WAIT), Some(Key::Esc));
        assert!(!input.waiting());
        // A key long after it: both.
        assert_eq!(input.feed_at(b"\x1b", t), vec![]);
        assert_eq!(
            input.feed_at(b"q", t + ESC_WAIT),
            vec![Key::Esc, Key::Char('q')]
        );
        assert_eq!(
            input.feed("é✓".as_bytes()),
            vec![Key::Char('é'), Key::Char('✓')]
        );
        assert_eq!(input.feed(b"\x1b[I\x1b[5~"), vec![Key::Other, Key::PageUp]);
    }

    #[test]
    fn mouse() {
        let mut input = Input::default();
        let at = |button, wheel, motion, release, x, y| {
            Key::Mouse(Mouse {
                button,
                wheel,
                motion,
                release,
                x,
                y,
            })
        };
        assert_eq!(
            input.feed(b"\x1b[<0;812;403M\x1b[<32;900;450M\x1b[<0;901;451m"),
            vec![
                at(0, 0, false, false, 812, 403),
                at(0, 0, true, false, 900, 450),
                at(0, 0, false, true, 901, 451)
            ]
        );
        assert_eq!(
            input.feed(b"\x1b[<64;10;20M\x1b[<65;10;20M\x1b[<2;0;0M"),
            vec![
                at(3, -1, false, false, 10, 20),
                at(3, 1, false, false, 10, 20),
                at(2, 0, false, false, 0, 0)
            ]
        );
        // Split across reads, and nonsense.
        assert_eq!(input.feed(b"\x1b[<0;1"), vec![]);
        assert_eq!(input.feed(b"2;3M"), vec![at(0, 0, false, false, 12, 3)]);
        assert_eq!(
            input.feed(b"\x1b[<0;;3M\x1b[<0;1;2;3M\x1b[<66;1;1M"),
            vec![Key::Other, Key::Other, Key::Other]
        );
    }

    #[test]
    fn split_reads() {
        let mut input = Input::default();
        assert_eq!(input.feed(b"\x1b[1;"), vec![]);
        assert_eq!(input.feed(b"2D"), vec![Key::ShiftLeft]);
        let bytes = "é".as_bytes();
        assert_eq!(input.feed(&bytes[..1]), vec![]);
        assert_eq!(input.feed(&bytes[1..]), vec![Key::Char('é')]);
    }

    #[test]
    fn pastes_are_not_keys() {
        let mut input = Input::default();
        assert_eq!(
            input.feed(b"a\x1b[200~mmq\x1b[2J\r\x1b[201~b"),
            vec![
                Key::Char('a'),
                Key::Paste("mmq\x1b[2J\r".into()),
                Key::Char('b')
            ]
        );
        assert_eq!(input.feed(b"\x1b[200~one "), vec![]);
        assert_eq!(input.feed(b"two\x1b[20"), vec![]);
        assert_eq!(input.feed(b"1~"), vec![Key::Paste("one two".into())]);
    }

    /// The keys from `bytes` read in two parts, cut at `cut`, then an m.
    fn in_two(bytes: &[u8], cut: usize) -> Vec<Key> {
        let mut input = Input::default();
        let t = Instant::now();
        let mut keys = input.feed_at(&bytes[..cut], t);
        keys.extend(input.feed_at(&bytes[cut..], t));
        keys.extend(input.feed_at(b"m", t));
        keys
    }

    #[test]
    fn a_terminals_replies_are_not_keys() {
        let replies: &[&[u8]] = &[
            // XTVERSION, DECRQSS (SGR, cursor style), OSC 11 both ways, a
            // kitty graphics reply, PM and SOS.
            b"\x1bP>|ghostty 1.3.1\x1b\\",
            b"\x1bP1$r0m\x1b\\",
            b"\x1bP1$r2 q\x1b\\",
            b"\x1b]11;rgb:1d1d/1d1d/1d1d\x1b\\",
            b"\x1b]11;rgb:1d1d/1d1d/1d1d\x07",
            b"\x1b_Gi=1;ENOENT:image not found\x1b\\",
            b"\x1b^yes\r\nno\x1b\\",
            b"\x1bXy\x07",
        ];
        for reply in replies {
            for cut in 0..=reply.len() {
                assert_eq!(
                    in_two(reply, cut),
                    vec![Key::Char('m')],
                    "{reply:?} cut at {cut}"
                );
            }
        }
        // Cut short by another sequence: that one counts.
        let mut input = Input::default();
        assert_eq!(input.feed(b"\x1bPyes\x1b[A"), vec![Key::Up]);
        // Never ended: let go after a while, and keys count again.
        let t = Instant::now();
        assert_eq!(input.feed_at(b"\x1b]11;y", t), vec![]);
        assert_eq!(input.feed_at(b"y", t), vec![]);
        assert_eq!(input.feed_at(b"y", t + STRING_WAIT), vec![Key::Char('y')]);
    }

    #[test]
    fn old_mouse_reports_are_not_keys() {
        // A click and its release at column 89, row 1: X10, then UTF-8
        // (mode 1005) at column 96.
        let x10: &[u8] = b"\x1b[M y!\x1b[M#y!";
        let utf8: &[u8] = b"\x1b[M \xc2\x80!\x1b[M#\xc2\x80!";
        // Column 94 is 0x7f in X10, and the row after it a `y`.
        let col94: &[u8] = b"\x1b[M \x7fy\x1b[M#\x7fy";
        for bytes in [x10, utf8, col94] {
            for cut in 0..=bytes.len() {
                let keys = in_two(bytes, cut);
                let typed: Vec<&Key> = keys
                    .iter()
                    .filter(|k| matches!(k, Key::Char(_) | Key::Enter))
                    .collect();
                assert_eq!(typed, [&Key::Char('m')], "{bytes:?} cut at {cut}: {keys:?}");
            }
        }
        // A report cut short by a control character: it ends there.
        let mut input = Input::default();
        assert_eq!(input.feed(b"\x1b[M \r"), vec![Key::Other, Key::Enter]);
    }

    #[test]
    fn bursts_are_not_typed() {
        let mut input = Input::default();
        assert!(!burst(&input.feed(b"y")));
        assert!(!burst(&input.feed(b"\x1b[A")));
        assert!(burst(&input.feed(b"yx")));
        assert!(burst(&input.feed(b"display-message -p 'y'\n")));
        assert!(!burst(&input.feed(b"\x1b[<0;1;1M\x1b[<0;1;1my")));
        assert!(!burst(&input.feed(b"\x1b[200~yy\r\x1b[201~")));
    }
}
