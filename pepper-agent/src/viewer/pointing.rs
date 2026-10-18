//! The human points: a click on a source is a `point`, a drag a `region`,
//! in the source's own units (a video's pixels, a page's CSS pixels), the
//! letterboxing taken off. Each gets pictures from the source (the whole
//! frame and the part around it), for a page what's there (selector, role,
//! styles), and the note the human writes in the prompt that opens. The
//! event goes once both are in.

use std::path::Path;
use std::time::{Duration, Instant};

use super::input::Mouse;
use super::layout;
use super::{Prompt, Viewer, pause_for_note};
use crate::error::{Error, Result};
use crate::json::Value;
use crate::{files, media};

/// Further than this (pixels of the pane, 5 points on a Retina display)
/// between press and release is a drag: less is a hand that moved as it
/// clicked.
const DRAG_FROM: f64 = 10.0;
/// How long the pictures may take.
const CAPTURE_WAIT: Duration = Duration::from_secs(20);
/// The part around a point of a video: this many of its pixels.
const AROUND: (f64, f64) = (480.0, 270.0);

pub struct Drag {
    pub sid: u64,
    pub start: (f64, f64),
    pub now: (f64, f64),
}

pub struct Pending {
    pub id: u64,
    pub sid: u64,
    event: Value,
    what: String,
    /// The human's note: None while they write; then Some (with or
    /// without text).
    note: Option<Option<String>>,
    capture: Option<Result<Value>>,
    /// A video's snapshot asked of its player: the request and the file.
    shot: Option<(u64, std::path::PathBuf, [u32; 4])>,
    started: Instant,
}

impl Pending {
    /// What it is, for the prompt: "the point at 412,233 on app".
    pub fn what(&self) -> &str {
        &self.what
    }
}

fn distance(a: (f64, f64), b: (f64, f64)) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

impl Viewer {
    /// The source under a point of the pane, and its box in pixels.
    fn hit(&self, p: (f64, f64)) -> Option<(usize, (f64, f64, f64, f64))> {
        self.tiles.iter().find_map(|&(i, _, at)| {
            let b = self.box_px(at);
            (p.0 >= b.0 && p.1 >= b.1 && p.0 < b.0 + b.2 && p.1 < b.1 + b.3).then_some((i, b))
        })
    }

    /// A source's size in its own units: a video's pixels, a page's CSS
    /// pixels.
    fn source_size(&self, i: usize) -> Option<(f64, f64)> {
        match self.sources[i].size {
            (Some(w), Some(h)) if w > 0.0 && h > 0.0 => Some((w, h)),
            _ => None,
        }
    }

    pub(super) fn on_mouse(&mut self, m: Mouse) {
        if self.prompt.is_some() || self.confirm_quit {
            return;
        }
        let p = (m.x as f64, m.y as f64);
        if m.wheel != 0 {
            // A page scrolls; a video steps a frame a notch.
            match self.hit(p) {
                Some((i, at)) if self.sources[i].web.is_some() => {
                    if let Some(size) = self.source_size(i) {
                        let point = layout::to_source(p, at, size, true);
                        self.web_key_scroll(m.wheel as f64 * 120.0, point.map(|pt| (i, pt)));
                    }
                }
                Some((i, _)) => self.wheel_step(i, m.wheel as i32),
                None => {
                    if let Some((i, _)) = self.bar_at(p, None) {
                        self.wheel_step(i, m.wheel as i32);
                    }
                }
            }
            return;
        }
        if m.button != 0 {
            return;
        }
        if m.release {
            if self.bar_release(p) {
                return;
            }
            if let Some(drag) = self.drag.take() {
                self.pointed(drag, p);
            }
            return;
        }
        if m.motion {
            if self.bar_drag(p) {
                return;
            }
            if let Some(drag) = &mut self.drag {
                drag.now = p;
            }
            return;
        }
        if self.bar_press(p) {
            return;
        }
        if let Some((i, _)) = self.hit(p) {
            self.drag = Some(Drag {
                sid: self.sources[i].sid,
                start: p,
                now: p,
            });
            if self.focus != Some(i) {
                self.focus = Some(i);
                self.relayout();
            }
        }
    }

    /// What a drag under way shows on the keys row.
    pub(super) fn drag_text(&self) -> Option<String> {
        let drag = self.drag.as_ref()?;
        if distance(drag.start, drag.now) < DRAG_FROM {
            return None;
        }
        let i = self.sources.iter().position(|s| s.sid == drag.sid)?;
        let &(_, _, at) = self.tiles.iter().find(|t| t.0 == i)?;
        let size = self.source_size(i)?;
        let b = self.box_px(at);
        let a = layout::to_source(drag.start, b, size, true)?;
        let z = layout::to_source(drag.now, b, size, true)?;
        Some(format!(
            "region {:.0}×{:.0} on {}: let go to send it",
            (z.0 - a.0).abs(),
            (z.1 - a.1).abs(),
            self.sources[i].name
        ))
    }

    fn pointed(&mut self, drag: Drag, end: (f64, f64)) {
        let Some(i) = self.sources.iter().position(|s| s.sid == drag.sid) else {
            return;
        };
        let Some(&(_, _, at)) = self.tiles.iter().find(|t| t.0 == i) else {
            return;
        };
        let source = &self.sources[i];
        let name = source.name.clone();
        let ready = source.state == super::State::Ready;
        let Some(size) = self.source_size(i).filter(|_| ready) else {
            self.flash(format!("{name} isn't showing yet: point at it once it is."));
            return;
        };
        let b = self.box_px(at);
        let Some(start) = layout::to_source(drag.start, b, size, false) else {
            self.flash("That's beside the picture: click on it.");
            return;
        };
        let mut region = None;
        if distance(drag.start, end) >= DRAG_FROM
            && let Some(stop) = layout::to_source(end, b, size, true)
        {
            let (x, y) = (start.0.min(stop.0), start.1.min(stop.1));
            let (w, h) = ((start.0 - stop.0).abs(), (start.1 - stop.1).abs());
            if w >= 2.0 && h >= 2.0 {
                region = Some([x, y, w, h].map(|n| n.round()));
            }
        }
        let (x, y) = match region {
            Some([x, y, _, _]) => (x, y),
            None => (start.0.round(), start.1.round()),
        };
        let id = self.next_point;
        self.next_point += 1;
        let source = &self.sources[i];
        let mut event = Value::obj()
            .with("type", if region.is_some() { "region" } else { "point" })
            .with("source", name.as_str())
            .with("kind", source.kind);
        source.origin(&mut event);
        event.set("x", x);
        event.set("y", y);
        if let Some([_, _, w, h]) = region {
            event.set("w", w);
            event.set("h", h);
        }
        let what = match region {
            Some([x, y, w, h]) => format!("the region {w}×{h} at {x},{y} on {name}"),
            None => format!("the point at {x},{y} on {name}"),
        };
        let stamp: String = crate::sys::iso_time(crate::sys::now())
            .chars()
            .filter(char::is_ascii_digit)
            .take(14)
            .collect();
        let stem = format!("{}-{stamp}-{id}", files::name_part(&name));
        let mut pending = Pending {
            id,
            sid: source.sid,
            event,
            what,
            note: None,
            capture: None,
            shot: None,
            started: Instant::now(),
        };
        if let Some(web) = &source.web {
            pending.event.set("viewport", web.device.to_json());
            if let Some(page) = web.page.clone() {
                let points = self.points_dir.clone();
                let job = self.spawn_job(move || {
                    let region = region.map(|r| r.map(|n| n.max(0.0)));
                    // The page writes its pictures by path: into a folder
                    // made in the held points folder, whose path leads
                    // nowhere if `.pepper` became a link; then moved in.
                    let fail = |e: std::io::Error| Error::new("FAILED", format!("points: {e}"));
                    let (name, stage) = points.stage().map_err(fail)?;
                    let made = page
                        .capture(x, y, region, stage.path(), &stem)
                        .and_then(|got| keep(&points, &stage, got).map_err(fail));
                    for part in ["full", "crop"] {
                        let _ = stage.remove(&format!("{stem}-{part}.png"));
                        let _ = stage.remove(&format!("{stem}-{part}.small.png"));
                    }
                    let _ = points.remove_dir(&name);
                    made
                });
                self.point_jobs.insert(job, id);
            }
        } else {
            pending.event.set("width", size.0);
            pending.event.set("height", size.1);
            if let Some(t) = source.time_pos {
                source.at_time(t, &mut pending.event);
            }
            // The part around it, in the video's pixels.
            let near = match region {
                Some([x, y, w, h]) => {
                    let pad = (w.max(h) * 0.1).max(16.0);
                    [x - pad, y - pad, w + 2.0 * pad, h + 2.0 * pad]
                }
                None => [x - AROUND.0 / 2.0, y - AROUND.1 / 2.0, AROUND.0, AROUND.1],
            };
            let crop = clamp_crop(near, size);
            let sid = source.sid;
            match self.output(&format!("point-{sid}"), "png") {
                Ok(raw) => {
                    let path = raw.display().to_string();
                    let request = self.sources[i]
                        .player
                        .as_mut()
                        .map(|p| p.command(&["screenshot-to-file", &path, "video"]));
                    match request {
                        Some(request) => pending.shot = Some((request, raw, crop)),
                        None => {
                            pending.capture = Some(Err(Error::new("FAILED", "its player ended")))
                        }
                    }
                }
                Err(e) => pending.capture = Some(Err(e)),
            }
        }
        let resume = pause_for_note(&mut self.sources[i]);
        let time = self.sources[i]
            .time_pos
            .filter(|_| self.sources[i].web.is_none());
        self.points.push(pending);
        self.prompt = Some(Prompt {
            text: String::new(),
            sid: Some(drag.sid),
            time,
            resume,
            point: Some(id),
            range: None,
        });
        self.dirty = true;
    }

    /// A video's snapshot came (or didn't): crop and shrink it on a thread.
    /// Whether it was a point's.
    pub(super) fn point_shot_taken(
        &mut self,
        sid: u64,
        request: u64,
        ok: bool,
        error: &str,
    ) -> bool {
        let Some(k) = self
            .points
            .iter()
            .position(|p| p.sid == sid && p.shot.as_ref().is_some_and(|s| s.0 == request))
        else {
            return false;
        };
        let (_, raw, crop) = self.points[k].shot.take().unwrap_or_else(|| unreachable!());
        let id = self.points[k].id;
        if !ok {
            self.points[k].capture = Some(Err(Error::new(
                "FAILED",
                format!("the player couldn't take the snapshot: {error}"),
            )));
            self.finish_point(id);
            return true;
        }
        // Only the file made for the player, nothing put in its place.
        if !crate::proto::lone_file(&raw) {
            self.points[k].capture = Some(Err(Error::new(
                "FAILED",
                "the snapshot isn't the viewer's own file",
            )));
            self.finish_point(id);
            return true;
        }
        let stem = raw.with_extension("");
        let name = stem
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("point")
            .to_owned();
        let points = self.points_dir.clone();
        let job = self.spawn_job(move || {
            let (full, near) = (format!("{name}-full.png"), format!("{name}-crop.png"));
            // Made as bytes, written through the held points folder: never
            // where its path may lead now.
            let made = media::shrunk_png(&raw, None, media::MAX_SIDE).and_then(|(whole, _)| {
                let (part, shrunk) = media::shrunk_png(&raw, Some(crop), media::MAX_SIDE)?;
                points
                    .write(&full, &whole)
                    .and_then(|_| points.write(&near, &part))
                    .map_err(|e| Error::new("FAILED", format!("points: {e}")))?;
                Ok(shrunk)
            });
            let _ = std::fs::remove_file(&raw);
            let shrunk = made?;
            Ok(Value::obj()
                .with("snapshot", points.path().join(&full).display().to_string())
                .with("crop", points.path().join(&near).display().to_string())
                .with(
                    "crop_rect",
                    shrunk
                        .region
                        .unwrap_or(crop)
                        .iter()
                        .map(|&n| Value::from(n))
                        .collect::<Vec<_>>(),
                ))
        });
        self.point_jobs.insert(job, id);
        true
    }

    pub(super) fn noted(&mut self, id: u64, note: Option<String>) {
        if let Some(p) = self.points.iter_mut().find(|p| p.id == id) {
            p.note = Some(note);
        }
        self.finish_point(id);
    }

    pub(super) fn captured(&mut self, id: u64, result: Result<Value>) {
        if let Some(p) = self.points.iter_mut().find(|p| p.id == id) {
            p.capture = Some(result);
        }
        self.finish_point(id);
    }

    /// Sends the event once both the note and the pictures are in.
    fn finish_point(&mut self, id: u64) {
        let Some(k) = self.points.iter().position(|p| p.id == id) else {
            return;
        };
        let ready = self.points[k].note.is_some() && self.points[k].capture.is_some();
        if !ready {
            return;
        }
        let pending = self.points.remove(k);
        let mut event = pending.event;
        match pending.capture {
            Some(Ok(Value::Obj(members))) => {
                for (key, value) in members {
                    event.set(&key, value);
                }
            }
            Some(Err(e)) => event.set("capture_error", e.message),
            _ => {}
        }
        if let Some(Some(note)) = pending.note {
            event.set("note", note);
        }
        let kind = event.get_str("type").unwrap_or("point").to_owned();
        self.add_event(event);
        self.flash(format!("Sent the {kind}. More, or Enter when you're done."));
    }

    /// Pictures that take too long go without.
    pub(super) fn expire_points(&mut self) {
        let late: Vec<u64> = self
            .points
            .iter()
            .filter(|p| p.capture.is_none() && p.started.elapsed() >= CAPTURE_WAIT)
            .map(|p| p.id)
            .collect();
        for id in late {
            self.point_jobs.retain(|_, p| *p != id);
            self.captured(
                id,
                Err(Error::new("TIMEOUT", "the pictures didn't come in time")),
            );
        }
    }

    /// A source closed: its points go as they are.
    pub(super) fn drop_points(&mut self, sid: u64) {
        let ids: Vec<u64> = self
            .points
            .iter()
            .filter(|p| p.sid == sid && p.capture.is_none())
            .map(|p| p.id)
            .collect();
        for id in ids {
            self.point_jobs.retain(|_, p| *p != id);
            self.captured(id, Err(Error::new("NO_SOURCE", "the source was closed")));
        }
    }
}

/// A web point's capture, its pictures made in `stage`: moved into
/// `points`, and named there.
fn keep(points: &files::Dir, stage: &files::Dir, mut got: Value) -> std::io::Result<Value> {
    for key in ["snapshot", "crop"] {
        let Some(name) = got
            .get_str(key)
            .and_then(|p| Path::new(p).file_name())
            .and_then(|n| n.to_str())
            .map(str::to_owned)
        else {
            continue;
        };
        points.adopt(stage, &name, &name)?;
        got.set(key, points.path().join(&name).display().to_string());
    }
    Ok(got)
}

/// A crop inside a source of `size`, at least 16 pixels each way, in whole
/// pixels.
fn clamp_crop([x, y, w, h]: [f64; 4], size: (f64, f64)) -> [u32; 4] {
    let w = w.clamp(16.0_f64.min(size.0), size.0);
    let h = h.clamp(16.0_f64.min(size.1), size.1);
    let x = x.clamp(0.0, size.0 - w);
    let y = y.clamp(0.0, size.1 - h);
    [x, y, w, h].map(|n| n.round() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crops() {
        assert_eq!(
            clamp_crop([-100.0, 900.0, 480.0, 270.0], (1920.0, 1080.0)),
            [0, 810, 480, 270]
        );
        assert_eq!(
            clamp_crop([10.0, 10.0, 4.0, 4.0], (1920.0, 1080.0)),
            [10, 10, 16, 16]
        );
        assert_eq!(
            clamp_crop([0.0, 0.0, 4000.0, 4000.0], (640.0, 360.0)),
            [0, 0, 640, 360]
        );
    }

    #[test]
    fn drags_are_long_enough() {
        assert!(distance((0.0, 0.0), (8.0, 4.0)) < DRAG_FROM);
        assert!(distance((0.0, 0.0), (10.0, 0.0)) >= DRAG_FROM);
    }
}
