//! Where sources go in the viewer's pane, in cells: a tile each, its label
//! on the tile's first row and the box under it, shaped like the picture.

/// A box of cells: column and row of its top left (from 0), columns, rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cells {
    pub col: u16,
    pub row: u16,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// One source alone, two or more as they fit best.
    Auto,
    /// The focused source alone.
    Single,
    /// Two side by side.
    SplitH,
    /// Two, one above the other.
    SplitV,
    /// Up to four, two by two.
    Grid,
}

impl Mode {
    pub fn parse(text: &str) -> Option<Mode> {
        Some(match text {
            "auto" => Mode::Auto,
            "single" => Mode::Single,
            "split-h" => Mode::SplitH,
            "split-v" => Mode::SplitV,
            "grid" => Mode::Grid,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Single => "single",
            Mode::SplitH => "split-h",
            Mode::SplitV => "split-v",
            Mode::Grid => "grid",
        }
    }

    /// How many sources show at once.
    pub fn capacity(self) -> usize {
        match self {
            Mode::Single => 1,
            Mode::SplitH | Mode::SplitV => 2,
            Mode::Auto | Mode::Grid => 4,
        }
    }
}

/// Which of `count` sources show, in order: all that fit, the focused one
/// always among them.
pub fn visible(mode: Mode, count: usize, focus: Option<usize>) -> Vec<usize> {
    let capacity = mode.capacity().min(count);
    if capacity == 0 {
        return Vec::new();
    }
    if capacity == 1 {
        return vec![focus.unwrap_or(0).min(count - 1)];
    }
    let mut shown: Vec<usize> = (0..capacity).collect();
    if let Some(focus) = focus.filter(|&f| f < count && f >= capacity) {
        shown[capacity - 1] = focus;
    }
    shown
}

/// The tiles for `n` sources in `area`, in reading order. `cell` is a
/// cell's size in pixels, so tiles can be shaped for 16:9 pictures.
pub fn tiles(mode: Mode, n: usize, area: Cells, cell: (f64, f64)) -> Vec<Cells> {
    if n == 0 || area.cols == 0 || area.rows == 0 {
        return Vec::new();
    }
    let (across, down) = match (mode, n) {
        (_, 1) | (Mode::Single, _) => (1, 1),
        (Mode::SplitH, _) => (2, 1),
        (Mode::SplitV, _) => (1, 2),
        (Mode::Grid, _) => (2, n.div_ceil(2).min(2)),
        (Mode::Auto, _) => best_grid(n, area, cell),
    };
    let n = n.min(across * down);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let (x, y) = (i % across, i / across);
        out.push(Cells {
            col: area.col + split(area.cols, across, x, 1).0,
            row: area.row + split(area.rows, down, y, 0).0,
            cols: split(area.cols, across, x, 1).1,
            rows: split(area.rows, down, y, 0).1,
        });
    }
    out
}

/// Part `i` of `parts` of `length` cells, with `gap` cells between parts:
/// its start and length.
fn split(length: u16, parts: usize, i: usize, gap: u16) -> (u16, u16) {
    let parts = parts.max(1) as u32;
    let gaps = gap as u32 * (parts - 1);
    let usable = (length as u32).saturating_sub(gaps);
    let start = usable * i as u32 / parts;
    let end = usable * (i as u32 + 1) / parts;
    (
        (start + gap as u32 * i as u32) as u16,
        (end - start).max(1) as u16,
    )
}

/// The grid that shows `n` 16:9 pictures largest in `area`.
fn best_grid(n: usize, area: Cells, cell: (f64, f64)) -> (usize, usize) {
    let candidates: &[(usize, usize)] = match n {
        2 => &[(2, 1), (1, 2)],
        3 => &[(2, 2), (3, 1), (1, 3)],
        _ => &[(2, 2), (4, 1), (1, 4)],
    };
    let shown = |&(across, down): &(usize, usize)| {
        let w = split(area.cols, across, 0, 1).1 as f64 * cell.0;
        let h = split(area.rows, down, 0, 0).1.saturating_sub(1) as f64 * cell.1;
        let (fw, fh) = if w / h > 16.0 / 9.0 {
            (h * 16.0 / 9.0, h)
        } else {
            (w, w * 9.0 / 16.0)
        };
        fw * fh
    };
    *candidates
        .iter()
        .max_by(|a, b| shown(a).total_cmp(&shown(b)))
        .unwrap_or(&(2, 2))
}

/// The box in `area` with the picture's shape (`aspect`, width over
/// height), centred; all of `area` if the shape isn't known yet.
pub fn fit(area: Cells, cell: (f64, f64), aspect: Option<f64>) -> Cells {
    let Some(aspect) = aspect.filter(|a| a.is_finite() && *a > 0.0) else {
        return area;
    };
    let (w, h) = (area.cols as f64 * cell.0, area.rows as f64 * cell.1);
    let (cols, rows) = if w / h > aspect {
        (((h * aspect) / cell.0).round() as u16, area.rows)
    } else {
        (area.cols, ((w / aspect) / cell.1).round() as u16)
    };
    let (cols, rows) = (cols.clamp(1, area.cols), rows.clamp(1, area.rows));
    Cells {
        col: area.col + (area.cols - cols) / 2,
        row: area.row + (area.rows - rows) / 2,
        cols,
        rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL: (f64, f64) = (10.0, 20.0);

    fn area(cols: u16, rows: u16) -> Cells {
        Cells {
            col: 0,
            row: 0,
            cols,
            rows,
        }
    }

    #[test]
    fn one_source_takes_the_pane() {
        assert_eq!(
            tiles(Mode::Auto, 1, area(120, 40), CELL),
            vec![area(120, 40)]
        );
        assert_eq!(tiles(Mode::Grid, 0, area(120, 40), CELL), vec![]);
    }

    #[test]
    fn two_sources_side_by_side_in_a_wide_pane() {
        let t = tiles(Mode::Auto, 2, area(201, 30), CELL);
        assert_eq!(
            t[0],
            Cells {
                col: 0,
                row: 0,
                cols: 100,
                rows: 30
            }
        );
        assert_eq!(
            t[1],
            Cells {
                col: 101,
                row: 0,
                cols: 100,
                rows: 30
            }
        );
        // A tall pane: one above the other.
        let t = tiles(Mode::Auto, 2, area(80, 60), CELL);
        assert_eq!(
            t[0],
            Cells {
                col: 0,
                row: 0,
                cols: 80,
                rows: 30
            }
        );
        assert_eq!(
            t[1],
            Cells {
                col: 0,
                row: 30,
                cols: 80,
                rows: 30
            }
        );
        // Asked for: as asked.
        let t = tiles(Mode::SplitV, 2, area(201, 30), CELL);
        assert_eq!(t[1].row, 15);
    }

    #[test]
    fn a_grid_of_four() {
        let t = tiles(Mode::Grid, 4, area(161, 40), CELL);
        assert_eq!(t.len(), 4);
        assert_eq!(
            t[3],
            Cells {
                col: 81,
                row: 20,
                cols: 80,
                rows: 20
            }
        );
        let t = tiles(Mode::Grid, 3, area(161, 40), CELL);
        assert_eq!(t.len(), 3);
        assert_eq!(
            t[2],
            Cells {
                col: 0,
                row: 20,
                cols: 80,
                rows: 20
            }
        );
        assert_eq!(tiles(Mode::Grid, 2, area(161, 40), CELL)[1].row, 0);
    }

    #[test]
    fn which_show() {
        assert_eq!(visible(Mode::Single, 3, Some(2)), vec![2]);
        assert_eq!(visible(Mode::SplitH, 3, Some(2)), vec![0, 2]);
        assert_eq!(visible(Mode::SplitH, 3, Some(1)), vec![0, 1]);
        assert_eq!(visible(Mode::Grid, 3, None), vec![0, 1, 2]);
        assert_eq!(visible(Mode::Auto, 0, None), Vec::<usize>::new());
    }

    #[test]
    fn boxes_take_the_pictures_shape() {
        // 160×45 cells of 10×20 px: 1600×900, 16:9 exactly.
        let a = area(160, 45);
        assert_eq!(fit(a, CELL, Some(16.0 / 9.0)), a);
        // 4:3 in it: 1200×900, centred.
        assert_eq!(
            fit(a, CELL, Some(4.0 / 3.0)),
            Cells {
                col: 20,
                row: 0,
                cols: 120,
                rows: 45
            }
        );
        // 2.39:1: 1600×669, centred.
        let wide = fit(a, CELL, Some(2.39));
        assert_eq!((wide.cols, wide.rows), (160, 33));
        assert_eq!(wide.row, 6);
        assert_eq!(fit(a, CELL, None), a);
        assert_eq!(fit(a, CELL, Some(f64::NAN)), a);
        assert_eq!(fit(area(1, 1), CELL, Some(100.0)), area(1, 1));
    }
}
