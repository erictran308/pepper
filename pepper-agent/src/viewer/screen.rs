//! The viewer's pane: the alternate screen, its rows of text (labels, the
//! agent's message, the keys), and the boxes placed with `OSC 777;pepper`.
//! Only rows that changed are written again, and every row now and then.
//!
//! All text that reaches it is the viewer's own or already made inert
//! (`text::inert`); a row is cut to the pane's width in cells (a wide
//! character takes two), and autowrap is off, so nothing spills onto
//! another row or another tile's label.

use std::fs::File;
use std::io::Write;
use std::time::{Duration, Instant};

use super::layout::Cells;

pub const RESET: &str = "\x1b[0m";
pub const REVERSE: &str = "\x1b[7m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const AGENT: &str = "\x1b[1;33m";

pub fn place(token: &str, at: Cells) -> String {
    format!(
        "\x1b]777;pepper;place;{token};{};{};{};{}\x1b\\",
        at.col, at.row, at.cols, at.rows
    )
}

pub fn remove(token: &str) -> String {
    format!("\x1b]777;pepper;remove;{token}\x1b\\")
}

/// A row's text, from pieces at given columns.
#[derive(Default)]
pub struct Row {
    pieces: Vec<(u16, String, &'static str)>,
}

impl Row {
    /// `text` (inert, one line) at `col`, in `style`, cut to `width` cells.
    pub fn put(&mut self, col: u16, width: u16, text: &str, style: &'static str) {
        let (text, _) = fit(text, width);
        self.pieces.push((col, text, style));
    }

    fn render(mut self, cols: u16) -> String {
        self.pieces.sort_by_key(|p| p.0);
        let mut out = String::new();
        let mut at = 0u16;
        for (col, text, style) in self.pieces {
            if col >= cols || col < at {
                continue;
            }
            out.extend(std::iter::repeat_n(' ', (col - at) as usize));
            let (text, used) = fit(&text, cols - col);
            at = col + used;
            if style.is_empty() {
                out.push_str(&text);
            } else {
                out.push_str(style);
                out.push_str(&text);
                out.push_str(RESET);
            }
        }
        out
    }
}

/// `text`'s first characters that fit in `width` cells, and the cells they
/// take.
fn fit(text: &str, width: u16) -> (String, u16) {
    let mut used = 0;
    let mut out = String::new();
    for c in text.chars() {
        let w = cells(c);
        if used + w > width {
            break;
        }
        used += w;
        out.push(c);
    }
    (out, used)
}

/// The cells a character takes, near enough what the terminal does: two
/// for wide ones (CJK, Hangul, fullwidth forms, most emoji), none for
/// combining marks and variation selectors, one for the rest.
fn cells(c: char) -> u16 {
    match c as u32 {
        0x0300..=0x036F
        | 0x0483..=0x0489
        | 0x0591..=0x05BD
        | 0x0610..=0x061A
        | 0x064B..=0x065F
        | 0x1AB0..=0x1AFF
        | 0x1DC0..=0x1DFF
        | 0x200B..=0x200F
        | 0x20D0..=0x20FF
        | 0xFE00..=0xFE0F
        | 0xFE20..=0xFE2F
        | 0xE0000..=0xE01EF => 0,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD => 2,
        _ => 1,
    }
}

pub struct Screen {
    tty: File,
    drawn: Vec<String>,
    cols: u16,
    /// When every row was last drawn.
    painted: Instant,
}

/// The pane's modes, set when the viewer takes it and again with every
/// draw that writes: no cursor, no autowrap, bracketed paste, and the mouse
/// (presses, releases and drags, in pixels: SGR 1006 with 1016). Anything
/// that can print on the pane can turn them off, and then the human's
/// clicks would come as old reports, pastes bare: they don't stay off.
const MODES: &str = "\x1b[?25l\x1b[?7l\x1b[?2004h\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?1016h";
/// How often every row is drawn again, changed or not, so what something
/// else printed over a label or the keys doesn't stay.
const REPAINT_EVERY: Duration = Duration::from_secs(5);

impl Screen {
    /// Takes the pane: the alternate screen, and its modes (MODES).
    pub fn enter(tty: File) -> Screen {
        let mut screen = Screen {
            tty,
            drawn: Vec::new(),
            cols: 0,
            painted: Instant::now(),
        };
        screen.write(&format!("\x1b[?1049h{MODES}\x1b[2J"));
        screen
    }

    pub fn write(&mut self, text: &str) {
        let _ = self.tty.write_all(text.as_bytes());
    }

    /// Whether every row is due to be drawn again (REPAINT_EVERY).
    pub fn stale(&self) -> bool {
        self.painted.elapsed() >= REPAINT_EVERY
    }

    /// Draws the rows that changed; all of them after a resize, or when
    /// it's stale. In one synchronized update (mode 2026), the modes first.
    pub fn draw(&mut self, rows: Vec<Row>, cols: u16) {
        if cols != self.cols || rows.len() != self.drawn.len() || self.stale() {
            self.cols = cols;
            self.drawn = vec![String::from("\u{0}"); rows.len()];
            self.painted = Instant::now();
        }
        let mut out = String::new();
        for (i, row) in rows.into_iter().enumerate() {
            let text = row.render(cols);
            if self.drawn[i] != text {
                out.push_str(&format!("\x1b[{};1H\x1b[2K{text}", i + 1));
                self.drawn[i] = text;
            }
        }
        if !out.is_empty() {
            self.write(&format!("\x1b[?2026h{MODES}{out}\x1b[?2026l"));
        }
    }

    /// Gives the pane back as it was.
    pub fn leave(&mut self) {
        self.write(LEAVE);
    }
}

/// What gives the pane back: the mouse, paste, wrap and cursor as they
/// were, and the screen the shell had.
pub const LEAVE: &str =
    "\x1b[0m\x1b[?1016l\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?2004l\x1b[?7h\x1b[?25h\x1b[?1049l";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_cut_and_padded() {
        let mut row = Row::default();
        row.put(4, 10, "label", BOLD);
        row.put(0, 3, "abcdef", "");
        assert_eq!(row.render(20), format!("abc {BOLD}label{RESET}"));
        let mut row = Row::default();
        row.put(5, 10, "0123456789", "");
        assert_eq!(row.render(8), "     012");
        let mut row = Row::default();
        row.put(0, 10, "aaaa", "");
        row.put(2, 10, "bb", "");
        assert_eq!(row.render(8), "aaaa");
    }

    #[test]
    fn wide_characters_take_two_cells() {
        let mut row = Row::default();
        row.put(0, 4, "日本語ab", "");
        assert_eq!(row.render(20), "日本");
        // A wide title stays in its tile: the next label is where it was.
        let mut row = Row::default();
        row.put(0, 7, "日本語日本語", "");
        row.put(7, 7, "[after]", "");
        assert_eq!(row.render(20), "日本語 [after]");
        assert_eq!(fit("e\u{301}x", 2), ("e\u{301}x".into(), 2));
    }

    #[test]
    fn every_draw_sets_the_modes_again() {
        let path = std::env::temp_dir().join(format!("pepper-screen-{}", std::process::id()));
        let file = File::create(&path).unwrap();
        let mut screen = Screen::enter(file);
        let row = |text: &str| {
            let mut row = Row::default();
            row.put(0, 20, text, "");
            vec![row]
        };
        screen.draw(row("one"), 20);
        let written = std::fs::read_to_string(&path).unwrap();
        let draw = written.rsplit("\x1b[?1049h").next().unwrap_or("");
        assert!(
            draw.contains(&format!("\x1b[?2026h{MODES}\x1b[1;1H\x1b[2Kone")),
            "{written:?}"
        );
        // Nothing changed: nothing written; until it's stale.
        screen.draw(row("one"), 20);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), written);
        screen.painted = Instant::now() - REPAINT_EVERY;
        assert!(screen.stale());
        screen.draw(row("one"), 20);
        let again = std::fs::read_to_string(&path).unwrap();
        assert!(
            again.len() > written.len() && again.ends_with("one\x1b[?2026l"),
            "{again:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sequences() {
        let at = Cells {
            col: 2,
            row: 3,
            cols: 40,
            rows: 11,
        };
        assert_eq!(place("ab", at), "\x1b]777;pepper;place;ab;2;3;40;11\x1b\\");
        assert_eq!(remove("ab"), "\x1b]777;pepper;remove;ab\x1b\\");
    }
}
