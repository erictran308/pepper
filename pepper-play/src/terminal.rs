//! The terminal: where the box is placed, by escape sequences written to
//! `/dev/tty`, and, run from a shell, the whole screen the video plays on.
//!
//! The box is placed with `ESC ] 777 ; pepper ; place ; TOKEN ; COL ; ROW ;
//! COLS ; ROWS ESC \` (cells, from the top left, from 0) and taken away
//! with `ESC ] 777 ; pepper ; remove ; TOKEN ESC \`. Other terminals ignore
//! both.

use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often, run from a shell, the window is looked at for a new size.
const LOOK_EVERY: Duration = Duration::from_millis(200);

/// A box of cells: column and row of its top left (from 0), columns, rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cells {
    pub col: u16,
    pub row: u16,
    pub cols: u16,
    pub rows: u16,
}

pub fn place(token: &str, at: Cells) -> String {
    format!(
        "\x1b]777;pepper;place;{token};{};{};{};{}\x1b\\",
        at.col, at.row, at.cols, at.rows
    )
}

pub fn remove(token: &str) -> String {
    format!("\x1b]777;pepper;remove;{token}\x1b\\")
}

#[repr(C)]
#[derive(Default)]
struct Winsize {
    rows: u16,
    cols: u16,
    width: u16,
    height: u16,
}

const TIOCGWINSZ: std::ffi::c_ulong = 0x4008_7468;

unsafe extern "C" {
    fn ioctl(fd: std::ffi::c_int, request: std::ffi::c_ulong, ...) -> std::ffi::c_int;
}

/// The controlling terminal, opened for writing.
pub struct Tty {
    file: Mutex<File>,
}

impl Tty {
    pub fn open() -> Result<Tty, String> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .map_err(|e| format!("cannot open the terminal (/dev/tty): {e}"))?;
        Ok(Tty {
            file: Mutex::new(file),
        })
    }

    /// Writes `text` in one go.
    pub fn write(&self, text: &str) {
        let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        let _ = file.write_all(text.as_bytes());
    }

    /// Columns and rows.
    pub fn size(&self) -> Option<(u16, u16)> {
        let file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        let mut size = Winsize::default();
        // SAFETY: TIOCGWINSZ fills a winsize.
        let ok = unsafe { ioctl(file.as_raw_fd(), TIOCGWINSZ, &raw mut size) } == 0;
        (ok && size.cols > 0 && size.rows > 0).then_some((size.cols, size.rows))
    }
}

/// The box run from a shell: the window but its last row, the status line;
/// or, filling it, the whole window.
pub fn screen_box(cols: u16, rows: u16, fill: bool) -> Cells {
    Cells {
        col: 0,
        row: 0,
        cols,
        rows: if fill {
            rows
        } else {
            rows.saturating_sub(1).max(1)
        },
    }
}

/// Where the box was last placed: the window's size, and whether it fills
/// it; and the status line under it.
struct Placed {
    size: (u16, u16),
    fill: bool,
    status: String,
}

/// Draws `status` on the last row, cut to the window's width. The text is
/// this program's own (numbers and fixed words, main.rs `status_text`).
fn status_line((cols, rows): (u16, u16), status: &str) -> String {
    let text: String = status
        .chars()
        .filter(|c| !c.is_control())
        .take(cols as usize)
        .collect();
    format!("\x1b[{rows};1H\x1b[2K{text}")
}

/// Run from a shell: the video takes the screen (the alternate one, as a
/// full-screen program does) until [`Screen::leave`], following the window
/// as it resizes.
pub struct Screen {
    tty: Arc<Tty>,
    token: String,
    placed: Arc<Mutex<Placed>>,
    stop: Arc<AtomicBool>,
    watch: Option<std::thread::JoinHandle<()>>,
}

impl Screen {
    /// Switches to the alternate screen and hides the cursor, then places the
    /// box over all but the last row, where [`Screen::status`] goes.
    pub fn enter(tty: Arc<Tty>, token: String) -> Screen {
        let (cols, rows) = tty.size().unwrap_or((80, 24));
        tty.write(&format!(
            "\x1b[?1049h\x1b[?25l\x1b[{rows};1H{}",
            place(&token, screen_box(cols, rows, false))
        ));
        let placed = Arc::new(Mutex::new(Placed {
            size: (cols, rows),
            fill: false,
            status: String::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let watch = std::thread::spawn({
            let (tty, token) = (tty.clone(), token.clone());
            let (placed, stop) = (placed.clone(), stop.clone());
            move || {
                while !stop.load(Ordering::Acquire) {
                    std::thread::sleep(LOOK_EVERY);
                    let Some(now) = tty.size() else {
                        continue;
                    };
                    let mut placed = placed.lock().unwrap_or_else(|e| e.into_inner());
                    if now != placed.size && !stop.load(Ordering::Acquire) {
                        placed.size = now;
                        let mut out = place(&token, screen_box(now.0, now.1, placed.fill));
                        if !placed.fill {
                            out += &status_line(now, &placed.status);
                        }
                        tty.write(&out);
                    }
                }
            }
        });
        Screen {
            tty,
            token,
            placed,
            stop,
            watch: Some(watch),
        }
    }

    /// Makes the video fill the whole window, over the status line, or
    /// leave it the last row again: mpv's fullscreen (`f`).
    pub fn set_fill(&self, fill: bool) {
        let mut placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
        if placed.fill != fill {
            placed.fill = fill;
            let (cols, rows) = placed.size;
            let mut out = place(&self.token, screen_box(cols, rows, fill));
            if !fill {
                out += &status_line(placed.size, &placed.status);
            }
            self.tty.write(&out);
        }
    }

    /// Shows `status` on the last row, if it changed (and isn't covered).
    pub fn status(&self, status: &str) {
        let mut placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
        if placed.status != status {
            placed.status = status.to_owned();
            if !placed.fill {
                self.tty.write(&status_line(placed.size, status));
            }
        }
    }

    /// Takes the box away and gives the screen back as it was.
    pub fn leave(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(watch) = self.watch.take() {
            let _ = watch.join();
        }
        self.tty
            .write(&format!("{}\x1b[?25h\x1b[?1049l", remove(&self.token)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_sequences() {
        let at = Cells {
            col: 2,
            row: 3,
            cols: 40,
            rows: 11,
        };
        assert_eq!(place("ab", at), "\x1b]777;pepper;place;ab;2;3;40;11\x1b\\");
        assert_eq!(remove("ab"), "\x1b]777;pepper;remove;ab\x1b\\");
    }

    #[test]
    fn the_status_line_is_inert_and_fits() {
        assert_eq!(
            status_line((80, 24), "(Paused) 00:00:03 / 00:01:10 (4%)"),
            "\x1b[24;1H\x1b[2K(Paused) 00:00:03 / 00:01:10 (4%)"
        );
        assert_eq!(status_line((5, 2), "0123456789"), "\x1b[2;1H\x1b[2K01234");
        assert_eq!(
            status_line((80, 2), "a\x1b]2;x\x07b"),
            "\x1b[2;1H\x1b[2Ka]2;xb"
        );
    }

    #[test]
    fn the_screen_but_the_status_line() {
        assert_eq!(
            screen_box(120, 40, false),
            Cells {
                col: 0,
                row: 0,
                cols: 120,
                rows: 39
            }
        );
        assert_eq!(screen_box(10, 1, false).rows, 1);
        assert_eq!(screen_box(120, 40, true).rows, 40);
    }
}
