//! The seek bar under a video: where playback is, an edit preview's cuts,
//! and the human's mouse on it. A click or a drag on the bar seeks (to
//! keyframes while dragging, to the exact frame where it's let go); the
//! wheel over a video steps it a frame a notch, paused. The player gets one
//! seek at a time, the next once it has done that one (`playback-restart`),
//! so a fast drag or a trackpad's burst of notches never queues up: notches
//! that come meanwhile are added into the next step.

use std::time::{Duration, Instant};

use super::layout::Cells;
use super::screen::{self, Row};
use super::{State, Viewer};
use crate::json::Value;
use crate::text;

/// How long a seek may take before the next one goes anyway.
const SEEK_WAIT: Duration = Duration::from_secs(1);
/// The narrowest bar drawn.
const MIN_COLS: u16 = 8;
const PLAYED: &str = screen::BOLD;
const HEAD: &str = "\x1b[1;36m";
const MARK: &str = "\x1b[1;35m";
/// The most recent events looked through for a bar's marks.
const MAX_SCANNED: usize = 2000;
/// How near the playhead is to a mark to be on it, in seconds: a frame at
/// 24 fps and some, since going to a mark shows the first frame at or after
/// its time.
const ON_MARK: f64 = 0.05;

/// One of the human's marks, notes, points or ranges, on a bar.
#[derive(Clone, Debug, PartialEq)]
pub struct Mark {
    /// Its event's id.
    pub id: u64,
    pub time: f64,
    /// A range's end.
    pub end: Option<f64>,
    pub kind: String,
    pub text: Option<String>,
}

impl Mark {
    /// "note at 0:02.500", "range 0:03.200–0:05.000".
    fn what(&self) -> String {
        match self.end {
            Some(end) => format!(
                "{} {}–{}",
                self.kind,
                text::clock(self.time),
                text::clock(end)
            ),
            None => format!("{} at {}", self.kind, text::clock(self.time)),
        }
    }
}

/// The human's seeks of a source, one at a time.
#[derive(Debug, Default)]
pub struct Scrub {
    /// A seek sent, until the player has done it (or SEEK_WAIT passes).
    busy: Option<Instant>,
    /// The bar: where to go next, and whether exactly.
    to: Option<(f64, bool)>,
    /// The wheel: frames to step next (back if negative).
    steps: i32,
}

/// A box's bar: the row under it, as wide.
pub fn bar(at: Cells) -> Cells {
    Cells {
        col: at.col,
        row: at.row + at.rows,
        cols: at.cols,
        rows: 1,
    }
}

/// The bar's cells at `t` of `duration`, with ticks at the times in
/// `cuts`, diamonds at the human's `marks` (a range's span drawn in their
/// colour), and `[` at a range's start while it's `open`: each cell's
/// character and style. The playhead on a mark is a diamond too.
pub fn cells(
    cols: u16,
    t: f64,
    duration: f64,
    cuts: &[f64],
    marks: &[(f64, Option<f64>)],
    open: Option<f64>,
) -> Vec<(char, &'static str)> {
    let n = cols as usize;
    if n == 0 || duration.is_nan() || duration <= 0.0 {
        return Vec::new();
    }
    let cell = |t: f64| ((t / duration * n as f64).floor().max(0.0) as usize).min(n - 1);
    let head = cell(t);
    let mut out: Vec<(char, &'static str)> = (0..n)
        .map(|k| {
            if k < head {
                ('━', PLAYED)
            } else {
                ('─', screen::DIM)
            }
        })
        .collect();
    for &c in cuts.iter().filter(|&&c| c > 0.0 && c < duration) {
        let k = cell(c);
        out[k] = ('│', if k < head { PLAYED } else { screen::DIM });
    }
    let marks: Vec<&(f64, Option<f64>)> = marks.iter().filter(|m| m.0.is_finite()).collect();
    for &&(from, end) in &marks {
        if let Some(end) = end.filter(|e| e.is_finite()) {
            let (a, b) = (cell(from), cell(end));
            for c in &mut out[a.min(b)..=a.max(b)] {
                *c = ('━', MARK);
            }
        }
    }
    for &&(m, _) in &marks {
        out[cell(m)] = ('◆', MARK);
    }
    if let Some(from) = open.filter(|f| f.is_finite()) {
        out[cell(from)] = ('[', MARK);
    }
    out[head] = (if out[head].1 == MARK { '◆' } else { '●' }, HEAD);
    out
}

/// The time at `x` (pixels of the pane) on a bar from `left`, `width` wide.
pub fn time_at(x: f64, left: f64, width: f64, duration: f64) -> f64 {
    if width.is_nan() || width <= 0.0 {
        return 0.0;
    }
    ((x - left) / width).clamp(0.0, 1.0) * duration
}

/// Puts cells in a row, a piece for each run of one style.
fn put(row: &mut Row, col: u16, cells: &[(char, &'static str)]) {
    let mut k = 0;
    while k < cells.len() {
        let (start, style) = (k, cells[k].1);
        let mut piece = String::new();
        while k < cells.len() && cells[k].1 == style {
            piece.push(cells[k].0);
            k += 1;
        }
        row.put(col + start as u16, (k - start) as u16, &piece, style);
    }
}

impl Viewer {
    /// Whether a tile has room for its box's bar.
    fn bar_fits(tile: Cells, b: Cells) -> bool {
        b.cols >= MIN_COLS && b.row < tile.row + tile.rows
    }

    /// A video's length, if it has a timeline to seek in.
    fn seekable(&self, i: usize) -> Option<f64> {
        let s = &self.sources[i];
        if s.web.is_some() || s.state != State::Ready {
            return None;
        }
        s.duration.filter(|d| d.is_finite() && *d > 0.0)
    }

    /// The source whose bar is under a point of the pane, and the time
    /// there. While the human holds the bar of `held`, only its column
    /// counts: a drag may wander off the row.
    pub(super) fn bar_at(&self, p: (f64, f64), held: Option<u64>) -> Option<(usize, f64)> {
        let (cw, ch) = self.cell();
        self.tiles.iter().find_map(|&(i, tile, at)| {
            if held.is_some_and(|sid| sid != self.sources[i].sid) {
                return None;
            }
            let b = bar(at);
            let d = self.seekable(i).filter(|_| Self::bar_fits(tile, b))?;
            let (left, top, width) = (b.col as f64 * cw, b.row as f64 * ch, b.cols as f64 * cw);
            let on = p.0 >= left && p.0 < left + width && p.1 >= top && p.1 < top + ch;
            (on || held.is_some()).then(|| (i, time_at(p.0, left, width, d)))
        })
    }

    /// Draws the bars of the videos that show.
    pub(super) fn draw_bars(&self, out: &mut [Row]) {
        for &(i, tile, at) in &self.tiles {
            let b = bar(at);
            let (Some(d), Some(row)) = (
                self.seekable(i).filter(|_| Self::bar_fits(tile, b)),
                out.get_mut(b.row as usize),
            ) else {
                continue;
            };
            let s = &self.sources[i];
            let t = self.held_time(s.sid).or(s.time_pos).unwrap_or(0.0);
            let cuts: Vec<f64> = s
                .edit
                .as_ref()
                .map(|e| e.cuts.iter().map(|c| c.at).collect())
                .unwrap_or_default();
            let marks: Vec<(f64, Option<f64>)> =
                self.marks(i).iter().map(|m| (m.time, m.end)).collect();
            let open = self.range_from.filter(|r| r.0 == s.sid).map(|r| r.1);
            put(row, b.col, &cells(b.cols, t, d, &cuts, &marks, open));
        }
    }

    /// The human's marks, notes, points and ranges on what a source shows
    /// now, by time, but those they deleted. On a cut they're found again
    /// by their file and its time, so they keep their place in a revised
    /// cut, and one whose moment was cut out has none (a range, if its
    /// start was).
    pub(super) fn marks(&self, i: usize) -> Vec<Mark> {
        let s = &self.sources[i];
        let target = s.shown_target();
        let at = |e: &crate::json::Value, src: &str, time: &str| match &s.edit {
            Some(edit) => edit.place(e.get_str(src)?, e.get_num(&format!("{src}_time"))?),
            None if e.get_str("file") == Some(target.as_str()) => e.get_num(time),
            None => None,
        };
        let mut out: Vec<Mark> = self
            .events
            .iter()
            .rev()
            .take(MAX_SCANNED)
            .filter(|e| e.get_str("source") == Some(s.name.as_str()))
            .filter_map(|e| {
                let kind = e.get_str("type")?;
                if !matches!(kind, "mark" | "note" | "point" | "region" | "range") {
                    return None;
                }
                let id = e.get_num("id")? as u64;
                if self.deleted.contains(&id) {
                    return None;
                }
                let time = at(e, "src", "time")?;
                let end = (kind == "range")
                    .then(|| at(e, "to_src", "to"))
                    .flatten()
                    .filter(|end| *end > time);
                let text = e.get_str("text").or(e.get_str("note")).map(str::to_owned);
                Some(Mark {
                    id,
                    time,
                    end,
                    kind: kind.to_owned(),
                    text,
                })
            })
            .collect();
        out.sort_by(|a, b| a.time.total_cmp(&b.time));
        out
    }

    /// The marks where the focused video is paused: at its time, or a range
    /// it's inside. With the source.
    fn marks_here(&self) -> Option<(usize, Vec<Mark>)> {
        let i = self.focused().filter(|&i| self.sources[i].web.is_none())?;
        let s = &self.sources[i];
        let t = s
            .time_pos
            .filter(|_| s.paused && self.scrubbing.is_none())?;
        let here: Vec<Mark> = self
            .marks(i)
            .into_iter()
            .filter(|m| match m.end {
                Some(end) => m.time - ON_MARK <= t && t <= end + ON_MARK,
                None => (m.time - t).abs() <= ON_MARK,
            })
            .collect();
        (!here.is_empty()).then_some((i, here))
    }

    /// What the human marked where the focused video is paused: for the
    /// keys row, so they see what they wrote there.
    pub(super) fn on_mark_text(&self) -> Option<String> {
        let (_, here) = self.marks_here()?;
        let first = &here[0];
        let words: Vec<String> = here.iter().filter_map(|m| m.text.clone()).collect();
        let what = if here.len() > 1 {
            format!("{} marks at {}", here.len(), text::clock(first.time))
        } else {
            first.what()
        };
        let mut line = format!("◆ your {what}");
        if !words.is_empty() {
            line += &format!(": {}", words.join(" · "));
        }
        let delete = if here.len() > 1 {
            "d deletes the newest"
        } else {
            "d deletes it"
        };
        Some(format!("{line}  ({delete} · < > other marks)"))
    }

    /// `d`: asks before deleting the newest of the marks where the focused
    /// video is paused.
    pub(super) fn ask_delete(&mut self) {
        match self.marks_here() {
            Some((_, here)) => {
                let newest = here.iter().max_by_key(|m| m.id).map(|m| m.id);
                self.confirm_delete = newest;
                self.dirty = true;
            }
            None => self.flash(
                "Nothing to delete here: < > goes to your marks; paused on one, d deletes it.",
            ),
        }
    }

    /// What the keys row asks before a mark is deleted.
    pub(super) fn delete_text(&self) -> Option<String> {
        let id = self.confirm_delete?;
        let (_, here) = self.marks_here()?;
        let mark = here.iter().find(|m| m.id == id)?;
        let words = mark
            .text
            .as_deref()
            .map(|t| format!(": {t}"))
            .unwrap_or_default();
        Some(format!(
            "d again deletes your {}{words} · any other key keeps it",
            mark.what()
        ))
    }

    /// Deletes one of the human's marks: gone from the bar and from what
    /// `events` answers with from now on, and an event of its own tells the
    /// agent, who may have read it already.
    pub(super) fn delete_mark(&mut self, id: u64) {
        let Some(gone) = self
            .events
            .iter()
            .find(|e| e.get_num("id") == Some(id as f64))
            .cloned()
        else {
            return;
        };
        let what = self
            .marks_here()
            .and_then(|(_, here)| here.into_iter().find(|m| m.id == id))
            .map(|m| m.what())
            .unwrap_or_else(|| gone.get_str("type").unwrap_or("mark").to_owned());
        let mut deleted = Value::obj().with("id", id);
        for key in [
            "type",
            "time",
            "time_text",
            "to",
            "to_text",
            "clip",
            "src_time",
            "text",
            "note",
        ] {
            if let Some(v) = gone.get(key) {
                deleted.set(key, v.clone());
            }
        }
        let mut event = Value::obj()
            .with("type", "delete")
            .with("deletes", vec![Value::from(id)])
            .with("deleted", deleted);
        if let Some(source) = gone.get("source") {
            event.set("source", source.clone());
        }
        self.deleted.insert(id);
        self.add_event(event);
        self.flash(format!("Deleted your {what}. The agent is told."));
    }

    /// `<` or `>`: to the human's mark before or after the playhead.
    pub(super) fn jump_mark(&mut self, forward: bool) {
        let Some(i) = self.focused().filter(|&i| self.seekable(i).is_some()) else {
            return;
        };
        let s = &self.sources[i];
        let t = self.held_time(s.sid).or(s.time_pos).unwrap_or(0.0);
        let marks = self.marks(i);
        let next = if forward {
            marks.iter().find(|m| m.time > t + ON_MARK)
        } else {
            marks.iter().rev().find(|m| m.time < t - ON_MARK)
        };
        match next.map(|m| m.time) {
            Some(to) => {
                if !self.sources[i].paused
                    && let Some(player) = &mut self.sources[i].player
                {
                    player.set("pause", true);
                    self.sources[i].paused = true;
                }
                self.seek_to(i, to, true);
            }
            None if marks.is_empty() => self.flash("No marks yet: m marks this moment."),
            None if forward => self.flash("No mark after this one."),
            None => self.flash("No mark before this one."),
        }
    }

    /// `i`: a range starts at the playhead (`o` ends it).
    pub(super) fn range_in(&mut self) {
        let Some(i) = self.focused().filter(|&i| self.seekable(i).is_some()) else {
            self.flash("Ranges are on a video: open one first.");
            return;
        };
        let s = &self.sources[i];
        let t = self.held_time(s.sid).or(s.time_pos).unwrap_or(0.0);
        self.range_from = Some((s.sid, t));
        self.dirty = true;
    }

    /// `o`: the range from `i` to the playhead goes to the agent, with a
    /// note if the human writes one: in the cut's time, and on a cut the
    /// clip, file and file time of each end.
    pub(super) fn range_out(&mut self) {
        let Some(i) = self.focused().filter(|&i| self.seekable(i).is_some()) else {
            return;
        };
        let sid = self.sources[i].sid;
        let Some(from) = self.range_from.filter(|r| r.0 == sid).map(|r| r.1) else {
            self.flash("i starts a range at the playhead, o ends it.");
            return;
        };
        let s = &self.sources[i];
        let now = self.held_time(sid).or(s.time_pos).unwrap_or(0.0);
        let (from, to) = (from.min(now), from.max(now));
        if to - from < ON_MARK {
            self.flash("The range is empty: move to its end, then o.");
            return;
        }
        self.range_from = None;
        let mut event = Value::obj()
            .with("type", "range")
            .with("source", s.name.as_str())
            .with("kind", s.kind);
        s.origin(&mut event);
        s.at_time(from, &mut event);
        let mut end = Value::obj();
        s.at_time(to, &mut end);
        if let Value::Obj(members) = end {
            for (key, value) in members {
                match key.as_str() {
                    "time" => event.set("to", value),
                    "time_text" => event.set("to_text", value),
                    "project" => {}
                    _ => event.set(&format!("to_{key}"), value),
                }
            }
        }
        event.set("length", ((to - from) * 1000.0).round() / 1000.0);
        let resume = super::pause_for_note(&mut self.sources[i]);
        self.prompt = Some(super::Prompt {
            text: String::new(),
            sid: Some(sid),
            time: Some(from),
            resume,
            point: None,
            range: Some(event),
        });
        self.dirty = true;
    }

    /// What the keys row says while a range is open on the focused video.
    pub(super) fn range_text(&self) -> Option<String> {
        let (sid, from) = self.range_from?;
        let i = self.focused().filter(|&i| self.sources[i].sid == sid)?;
        let s = &self.sources[i];
        let now = self.held_time(sid).or(s.time_pos).unwrap_or(from);
        Some(format!(
            "range from {} to {}: o sends it, Esc drops it",
            text::clock(from.min(now)),
            text::clock(from.max(now))
        ))
    }

    /// Where the human holds a source's bar, while they do.
    pub(super) fn held_time(&self, sid: u64) -> Option<f64> {
        self.scrubbing.filter(|s| s.0 == sid).map(|s| s.1)
    }

    /// What the keys row says while the human holds a bar.
    pub(super) fn scrub_text(&self) -> Option<String> {
        let (sid, t) = self.scrubbing?;
        let s = self.sources.iter().find(|s| s.sid == sid)?;
        let clip = s
            .edit
            .as_ref()
            .and_then(|e| e.say(t))
            .map(|c| format!(", {c}"))
            .unwrap_or_default();
        Some(format!(
            "seek {} {}{clip}: let go to land on the exact frame",
            s.name,
            text::clock(t)
        ))
    }

    /// A press: on a bar, the human takes its playhead. Whether it was.
    pub(super) fn bar_press(&mut self, p: (f64, f64)) -> bool {
        let Some((i, t)) = self.bar_at(p, None) else {
            return false;
        };
        self.scrubbing = Some((self.sources[i].sid, t));
        if self.focus != Some(i) {
            self.focus = Some(i);
            self.relayout();
        }
        self.seek_to(i, t, false);
        true
    }

    /// A drag while the human holds a bar. Whether they do.
    pub(super) fn bar_drag(&mut self, p: (f64, f64)) -> bool {
        let Some((sid, _)) = self.scrubbing else {
            return false;
        };
        if let Some((i, t)) = self.bar_at(p, Some(sid)) {
            self.scrubbing = Some((sid, t));
            self.seek_to(i, t, false);
        }
        true
    }

    /// Let go: to the exact frame there. Whether the human held a bar.
    pub(super) fn bar_release(&mut self, p: (f64, f64)) -> bool {
        let Some((sid, last)) = self.scrubbing.take() else {
            return false;
        };
        let at = self.bar_at(p, Some(sid));
        let t = at.map_or(last, |(_, t)| t);
        if let Some(i) = self.sources.iter().position(|s| s.sid == sid) {
            self.seek_to(i, t, true);
        }
        true
    }

    fn seek_to(&mut self, i: usize, t: f64, exact: bool) {
        let s = &mut self.sources[i];
        s.review_to = None;
        s.scrub.to = Some((t, exact));
        s.scrub.steps = 0;
        self.dirty = true;
        self.pump(i);
    }

    /// The wheel over a video: `by` frames (a notch down is the next one),
    /// paused.
    pub(super) fn wheel_step(&mut self, i: usize, by: i32) {
        if self.seekable(i).is_none() {
            return;
        }
        let s = &mut self.sources[i];
        if !s.paused
            && let Some(player) = &mut s.player
        {
            player.set("pause", true);
            s.paused = true;
        }
        s.review_to = None;
        s.scrub.steps += by;
        self.dirty = true;
        self.pump(i);
    }

    /// Sends the next seek, unless one is under way.
    fn pump(&mut self, i: usize) {
        let s = &mut self.sources[i];
        if s.scrub.busy.is_some_and(|at| at.elapsed() < SEEK_WAIT) {
            return;
        }
        s.scrub.busy = None;
        let Some(player) = &mut s.player else {
            return;
        };
        let sent = if let Some((t, exact)) = s.scrub.to.take() {
            let how = if exact {
                "absolute+exact"
            } else {
                "absolute+keyframes"
            };
            player.send(vec!["seek".into(), t.into(), how.into()]);
            format!("seek {t:.3} {how}")
        } else if s.scrub.steps != 0 {
            let n = std::mem::take(&mut s.scrub.steps);
            player.send(vec!["frame-step".into(), n.into(), "seek".into()]);
            format!("frame-step {n}")
        } else {
            return;
        };
        s.scrub.busy = Some(Instant::now());
        let line = format!("{}: {sent}", s.name);
        self.note(&line);
    }

    /// The player has done a seek: the next, if any.
    pub(super) fn seek_done(&mut self, i: usize) {
        if self.sources[i].scrub.busy.is_some() {
            let line = format!(
                "{}: seek done at {:?}",
                self.sources[i].name, self.sources[i].time_pos
            );
            self.note(&line);
        }
        self.sources[i].scrub.busy = None;
        self.pump(i);
    }

    /// Seeks that never said they were done: what's next goes anyway.
    pub(super) fn pump_scrubs(&mut self) {
        for i in 0..self.sources.len() {
            if self.sources[i]
                .scrub
                .busy
                .is_some_and(|at| at.elapsed() >= SEEK_WAIT)
            {
                self.pump(i);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(cells: &[(char, &str)]) -> String {
        cells.iter().map(|c| c.0).collect()
    }

    #[test]
    fn the_bar() {
        let none: &[(f64, Option<f64>)] = &[];
        assert_eq!(chars(&cells(10, 0.0, 10.0, &[], none, None)), "●─────────");
        assert_eq!(chars(&cells(10, 5.0, 10.0, &[], none, None)), "━━━━━●────");
        // The end is the last cell, not past it.
        assert_eq!(chars(&cells(10, 10.0, 10.0, &[], none, None)), "━━━━━━━━━●");
        // Cuts as ticks; the first clip's start isn't one.
        assert_eq!(
            chars(&cells(10, 5.0, 10.0, &[0.0, 3.0, 8.0], none, None)),
            "━━━│━●──│─"
        );
        // Marks as diamonds, over cuts; the playhead on one is one too.
        let marks = [(1.2, None), (3.4, None), (5.5, None), (9.0, None)];
        let bar = cells(10, 5.0, 10.0, &[3.0], &marks, None);
        assert_eq!(chars(&bar), "━◆━◆━◆───◆");
        assert_eq!(bar[5].1, HEAD);
        assert_eq!(bar[9].1, MARK);
        // A range is a span from its diamond; an open one's start is `[`.
        let bar = cells(10, 0.0, 10.0, &[], &[(2.0, Some(5.0))], Some(7.0));
        assert_eq!(chars(&bar), "●─◆━━━─[──");
        assert!(bar[2..=5].iter().all(|c| c.1 == MARK));
        assert!(cells(10, 1.0, 0.0, &[], none, None).is_empty());
        assert!(cells(0, 1.0, 5.0, &[], none, None).is_empty());
    }

    #[test]
    fn times_on_the_bar() {
        assert_eq!(time_at(100.0, 100.0, 800.0, 16.0), 0.0);
        assert_eq!(time_at(500.0, 100.0, 800.0, 16.0), 8.0);
        // Off either end: the start or the end.
        assert_eq!(time_at(20.0, 100.0, 800.0, 16.0), 0.0);
        assert_eq!(time_at(5000.0, 100.0, 800.0, 16.0), 16.0);
        assert_eq!(time_at(5.0, 0.0, 0.0, 16.0), 0.0);
    }
}
