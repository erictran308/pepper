//! The human's keys, from the bytes the terminal sends: plain characters,
//! the few escape sequences the viewer acts on, and pastes (bracketed paste
//! is on, so a paste never runs as keys).

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
    Paste(String),
    /// A sequence the viewer doesn't use.
    Other,
}

/// The most bytes kept of a paste, or of a sequence not finished yet.
const MAX_PASTE: usize = 64 * 1024;
const MAX_SEQUENCE: usize = 64;

#[derive(Default)]
pub struct Input {
    /// Bytes of a sequence or character not finished at the end of a read.
    pending: Vec<u8>,
    /// A paste being read, till ESC [ 201 ~.
    paste: Option<Vec<u8>>,
}

const PASTE_END: &[u8] = b"\x1b[201~";

impl Input {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(bytes);
        let mut keys = Vec::new();
        let mut at = 0;
        while at < data.len() {
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
                Parsed::Incomplete => {
                    if data.len() - at <= MAX_SEQUENCE {
                        self.pending = data[at..].to_vec();
                    }
                    return keys;
                }
            }
        }
        keys
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

enum Parsed {
    Key(Key, usize),
    PasteStart(usize),
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

/// ESC, alone or starting a sequence. A lone ESC at the end of a read is
/// the Esc key: the terminal sends a sequence's bytes together.
fn escape(data: &[u8]) -> Parsed {
    match data.get(1) {
        None => Parsed::Key(Key::Esc, 1),
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
        // ESC ESC: the first is Esc.
        Some(0x1b) => Parsed::Key(Key::Esc, 1),
        // Alt and a key.
        Some(_) => Parsed::Key(Key::Other, 2),
    }
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
        assert_eq!(input.feed(b"\x1b"), vec![Key::Esc]);
        assert_eq!(
            input.feed("é✓".as_bytes()),
            vec![Key::Char('é'), Key::Char('✓')]
        );
        assert_eq!(
            input.feed(b"\x1b[I\x1b[<0;10;5M"),
            vec![Key::Other, Key::Other]
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
}
