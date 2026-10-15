//! The viewer's pane: the alternate screen, its rows of text (labels, the
//! agent's message, the keys), and the boxes placed with `OSC 777;pepper`.
//! Only rows that changed are written again.
//!
//! All text that reaches it is the viewer's own or already made inert
//! (`text::inert`); a row is cut to the pane's width, and autowrap is off,
//! so nothing spills onto another row.

use std::fs::File;
use std::io::Write;

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
        let text: String = text.chars().take(width as usize).collect();
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
            let room = (cols - col) as usize;
            let text: String = text.chars().take(room).collect();
            at = col + text.chars().count() as u16;
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

pub struct Screen {
    tty: File,
    drawn: Vec<String>,
    cols: u16,
}

impl Screen {
    /// Takes the pane: the alternate screen, no cursor, no autowrap,
    /// bracketed paste, and the mouse (presses, releases and drags, in
    /// pixels: SGR 1006 with 1016).
    pub fn enter(tty: File) -> Screen {
        let mut screen = Screen {
            tty,
            drawn: Vec::new(),
            cols: 0,
        };
        screen.write(
            "\x1b[?1049h\x1b[?25l\x1b[?7l\x1b[?2004h\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?1016h\x1b[2J",
        );
        screen
    }

    pub fn write(&mut self, text: &str) {
        let _ = self.tty.write_all(text.as_bytes());
    }

    /// Draws the rows that changed; all of them after a resize.
    pub fn draw(&mut self, rows: Vec<Row>, cols: u16) {
        if cols != self.cols || rows.len() != self.drawn.len() {
            self.cols = cols;
            self.drawn = vec![String::from("\u{0}"); rows.len()];
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
            self.write(&out);
        }
    }

    /// Gives the pane back as it was.
    pub fn leave(&mut self) {
        self.write(
            "\x1b[0m\x1b[?1016l\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?2004l\x1b[?7h\x1b[?25h\x1b[?1049l",
        );
    }
}

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
