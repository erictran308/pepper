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
    // No room at all (a pane a few rows high): nothing to fit.
    if area.cols == 0 || area.rows == 0 {
        return area;
    }
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

/// Where a picture of `aspect` (width over height) shows in a box `w`×`h`
/// pixels: centred, its shape kept, as large as fits (as mpv and the web
/// frames draw it). Its left, top, width and height.
pub fn content(w: f64, h: f64, aspect: f64) -> (f64, f64, f64, f64) {
    if !(aspect.is_finite() && aspect > 0.0 && w > 0.0 && h > 0.0) {
        return (0.0, 0.0, w.max(0.0), h.max(0.0));
    }
    let (cw, ch) = if w / h > aspect {
        ((h * aspect).round().clamp(1.0, w), h)
    } else {
        (w, (w / aspect).round().clamp(1.0, h))
    };
    (((w - cw) / 2.0).floor(), ((h - ch) / 2.0).floor(), cw, ch)
}

/// A point of the pane, in pixels (as the mouse is reported), in the
/// source's own units: pixels for a video, CSS pixels for a page, whatever
/// its device pixel ratio. `at` is the box (left, top, width, height in
/// pixels), `size` the source's. None outside the picture, unless `clamp`,
/// which brings it to the nearest edge (for the end of a drag).
pub fn to_source(
    (px, py): (f64, f64),
    at: (f64, f64, f64, f64),
    size: (f64, f64),
    clamp: bool,
) -> Option<(f64, f64)> {
    if !(size.0 > 0.0 && size.1 > 0.0) {
        return None;
    }
    let (x, y, cw, ch) = content(at.2, at.3, size.0 / size.1);
    let (mut lx, mut ly) = (px - at.0 - x, py - at.1 - y);
    if clamp {
        lx = lx.clamp(0.0, cw - 1.0);
        ly = ly.clamp(0.0, ch - 1.0);
    } else if lx < 0.0 || ly < 0.0 || lx >= cw || ly >= ch {
        return None;
    }
    Some((lx * size.0 / cw, ly * size.1 / ch))
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
        // A box with no rows left (the label and seek bar took them).
        assert_eq!(fit(area(160, 0), CELL, Some(16.0 / 9.0)), area(160, 0));
        assert_eq!(fit(area(0, 45), CELL, Some(16.0 / 9.0)), area(0, 45));
    }

    #[test]
    fn pictures_in_boxes() {
        // 16:9 in 1600×1000: bars above and below.
        assert_eq!(
            content(1600.0, 1000.0, 16.0 / 9.0),
            (0.0, 50.0, 1600.0, 900.0)
        );
        // 4:3 in 1600×900: bars at the sides.
        assert_eq!(
            content(1600.0, 900.0, 4.0 / 3.0),
            (200.0, 0.0, 1200.0, 900.0)
        );
        assert_eq!(content(10.0, 10.0, f64::NAN), (0.0, 0.0, 10.0, 10.0));
    }

    #[test]
    fn clicks_in_source_pixels() {
        let at = (100.0, 40.0, 1600.0, 1000.0);
        // A 1920×1080 video, letterboxed: its top edge is 50 px down.
        assert_eq!(
            to_source((900.0, 90.0), at, (1920.0, 1080.0), false),
            Some((960.0, 0.0))
        );
        assert_eq!(to_source((900.0, 80.0), at, (1920.0, 1080.0), false), None);
        assert_eq!(
            to_source((900.0, 80.0), at, (1920.0, 1080.0), true),
            Some((960.0, 0.0))
        );
        // A page 1440×900 CSS pixels at a device pixel ratio of 2 (frames of
        // 2880×1800) fills a 1600×1000 box: the middle is its middle, in CSS
        // pixels.
        assert_eq!(
            to_source((900.0, 540.0), at, (1440.0, 900.0), false),
            Some((720.0, 450.0))
        );
        // A phone, 393×852 at 3, in the same box: bars at the sides.
        let (x, _, w, _) = content(1600.0, 1000.0, 393.0 / 852.0);
        assert_eq!((x, w), (569.0, 461.0));
        let (sx, sy) =
            to_source((100.0 + 569.0 + 230.5, 540.0), at, (393.0, 852.0), false).unwrap();
        assert!(
            (sx - 196.5).abs() < 0.01 && (sy - 426.0).abs() < 0.01,
            "{sx},{sy}"
        );
        assert_eq!(to_source((150.0, 540.0), at, (393.0, 852.0), false), None);
    }
}
