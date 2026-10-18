//! Before and after (spec 4.5): two sources side by side, labelled, for the
//! human to approve (y) or reject (x). With `sync`, two videos play as one:
//! the focused one leads, and the other follows its pause, its seeks and,
//! while both play, its time. Two pictures can show their difference in
//! place of the after one (`d`): black where they're the same.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use super::{Viewer, player};
use crate::error::{Error, Result};
use crate::json::Value;
use crate::{media, text};

/// How far a follower may drift from its leader while both play.
const DRIFT: f64 = 0.08;
/// The least time between two corrections.
const SETTLE: Duration = Duration::from_millis(500);
const PICTURES: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "gif", "bmp", "tif", "tiff", "avif", "heic",
];

pub struct Compare {
    pub a: u64,
    pub b: u64,
    pub sync: bool,
    /// The difference picture, while it shows in b's place.
    diff: Option<PathBuf>,
    diff_job: Option<u64>,
    corrected: Instant,
}

impl Viewer {
    /// `compare`: two sources side by side, or no longer (`off`).
    pub(super) fn set_compare(&mut self, args: &Value) -> Result<Value> {
        if args.get_bool("off") == Some(true) {
            self.end_compare();
            self.relayout();
            return Ok(Value::obj().with("compare", Value::Null));
        }
        let find = |key: &str| -> Result<usize> {
            let name = args
                .get_str(key)
                .ok_or_else(|| Error::new("BAD_ARGS", "compare: two sources"))?;
            self.sources
                .iter()
                .position(|s| s.name == name)
                .ok_or_else(|| {
                    Error::new(
                        "NO_SOURCE",
                        format!("no source is called {:?}", text::inert(name, 64)),
                    )
                })
        };
        let (ia, ib) = (find("a")?, find("b")?);
        // Another agent's sources are paired (and their verdicts so named,
        // their playback locked) only if the user asked (--force).
        self.check_owner(ia, args)?;
        self.check_owner(ib, args)?;
        if ia == ib {
            return Err(Error::new("BAD_ARGS", "compare: two different sources"));
        }
        let sync = args.get_bool("sync") == Some(true);
        let timed = |i: usize| {
            self.sources[i].web.is_none() && self.sources[i].duration.is_some_and(|d| d > 0.0)
        };
        if sync && !(timed(ia) && timed(ib)) {
            return Err(Error::new(
                "NOT_PLAYABLE",
                "--sync locks two videos together: one of these has no timeline",
            ));
        }
        self.end_compare();
        let (a, b) = (self.sources[ia].sid, self.sources[ib].sid);
        self.compare = Some(Compare {
            a,
            b,
            sync,
            diff: None,
            diff_job: None,
            corrected: Instant::now(),
        });
        self.focus = Some(ib);
        self.zoom = false;
        if sync {
            // Both from where the before one is, paused.
            let t = self.sources[ia].time_pos.unwrap_or(0.0);
            for i in [ia, ib] {
                let source = &mut self.sources[i];
                source.review_to = None;
                if let Some(player) = &mut source.player {
                    player.set("pause", true);
                    player.send(vec!["seek".into(), t.into(), "absolute+exact".into()]);
                }
                source.paused = true;
            }
        }
        self.relayout();
        Ok(Value::obj().with(
            "compare",
            Value::obj()
                .with("before", self.sources[ia].name.as_str())
                .with("after", self.sources[ib].name.as_str())
                .with("sync", sync),
        ))
    }

    /// The compared sources' places, while both are open.
    pub(super) fn compare_pair(&self) -> Option<(usize, usize)> {
        let c = self.compare.as_ref()?;
        let at = |sid: u64| self.sources.iter().position(|s| s.sid == sid);
        Some((at(c.a)?, at(c.b)?))
    }

    /// "before" or "after" (or "difference"), for a source's label.
    pub(super) fn compare_role(&self, sid: u64) -> Option<&'static str> {
        let c = self.compare.as_ref()?;
        if sid == c.a {
            Some("before")
        } else if sid == c.b {
            Some(if c.diff.is_some() {
                "difference"
            } else {
                "after"
            })
        } else {
            None
        }
    }

    /// The comparison, for an event about one of its sources.
    pub(super) fn compare_json(&self, sid: u64) -> Option<Value> {
        let (ia, ib) = self.compare_pair()?;
        let c = self.compare.as_ref()?;
        (sid == c.a || sid == c.b).then(|| {
            Value::obj()
                .with("before", self.sources[ia].name.as_str())
                .with("after", self.sources[ib].name.as_str())
        })
    }

    /// No more comparing; the after picture back if its difference showed.
    pub(super) fn end_compare(&mut self) {
        let Some(c) = self.compare.take() else { return };
        if c.diff.is_some()
            && let Some(source) = self.sources.iter_mut().find(|s| s.sid == c.b)
            && let Some(p) = &mut source.player
        {
            p.load(&source.target, player::remote(&source.target), None);
        }
    }

    /// With sync, the leader (the focused one of the two, else the before
    /// one) and its follower, if `sid` is the leader.
    fn leading(&self, sid: u64) -> Option<(usize, usize)> {
        let c = self.compare.as_ref().filter(|c| c.sync)?;
        let (ia, ib) = self.compare_pair()?;
        let leader = if self.focus == Some(ib) { ib } else { ia };
        let follower = if leader == ia { ib } else { ia };
        (self.sources[leader].sid == sid && (c.a == sid || c.b == sid))
            .then_some((leader, follower))
    }

    /// The leader paused or played: so does the follower.
    pub(super) fn sync_pause(&mut self, sid: u64) {
        let Some((leader, follower)) = self.leading(sid) else {
            return;
        };
        let paused = self.sources[leader].paused;
        let source = &mut self.sources[follower];
        if source.paused != paused
            && let Some(player) = &mut source.player
        {
            player.set("pause", paused);
            source.paused = paused;
        }
    }

    /// The leader landed somewhere (a seek, a step): so does the follower.
    pub(super) fn sync_seek(&mut self, sid: u64) {
        let Some((leader, follower)) = self.leading(sid) else {
            return;
        };
        let Some(t) = self.sources[leader].time_pos else {
            return;
        };
        if let Some(player) = &mut self.sources[follower].player {
            player.send(vec!["seek".into(), t.into(), "absolute+exact".into()]);
        }
        if let Some(c) = &mut self.compare {
            c.corrected = Instant::now();
        }
    }

    /// While both play, a follower that drifted is brought back.
    pub(super) fn sync_drift(&mut self) {
        let Some(c) = self.compare.as_ref().filter(|c| c.sync) else {
            return;
        };
        if c.corrected.elapsed() < SETTLE {
            return;
        }
        let Some((ia, ib)) = self.compare_pair() else {
            return;
        };
        let leader = if self.focus == Some(ib) { ib } else { ia };
        let follower = if leader == ia { ib } else { ia };
        let (l, f) = (&self.sources[leader], &self.sources[follower]);
        let (Some(tl), Some(tf)) = (l.time_pos, f.time_pos) else {
            return;
        };
        if l.paused || f.paused || (tl - tf).abs() <= DRIFT {
            return;
        }
        if let Some(player) = &mut self.sources[follower].player {
            player.send(vec!["seek".into(), tl.into(), "absolute+exact".into()]);
        }
        if let Some(c) = &mut self.compare {
            c.corrected = Instant::now();
        }
    }

    /// Whether the two compared are pictures (files of a picture's kind on
    /// this machine: only those go to ffmpeg for their difference).
    pub(super) fn comparing_pictures(&self) -> bool {
        let picture = |i: usize| {
            let s = &self.sources[i];
            s.web.is_none()
                && s.target.starts_with('/')
                && Path::new(&s.target)
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| PICTURES.contains(&e.to_ascii_lowercase().as_str()))
        };
        self.compare_pair()
            .is_some_and(|(ia, ib)| picture(ia) && picture(ib))
    }

    /// `d` on two pictures: their difference in the after one's place, or
    /// the after one again.
    pub(super) fn toggle_difference(&mut self) {
        let Some((ia, ib)) = self.compare_pair() else {
            return;
        };
        let showing = self.compare.as_ref().is_some_and(|c| c.diff.is_some());
        if showing {
            if let Some(c) = &mut self.compare {
                c.diff = None;
            }
            let source = &mut self.sources[ib];
            if let Some(p) = &mut source.player {
                p.load(&source.target, player::remote(&source.target), None);
            }
            self.flash("The after picture again: d for their difference.");
            return;
        }
        let Some(ffmpeg) = media::which("ffmpeg") else {
            self.flash("The difference needs ffmpeg: brew install ffmpeg.");
            return;
        };
        let (a, b) = (
            PathBuf::from(&self.sources[ia].target),
            PathBuf::from(&self.sources[ib].target),
        );
        let out = match self.output("difference", "png") {
            Ok(out) => out,
            Err(e) => {
                self.flash(format!("No difference: {}", text::inert(&e.message, 100)));
                return;
            }
        };
        let job = self.spawn_job(move || difference(&ffmpeg, &a, &b, &out));
        if let Some(c) = &mut self.compare {
            c.diff_job = Some(job);
        }
        self.flash("Working out the difference…");
    }

    /// A difference worked out: shown. Whether the job was one.
    pub(super) fn difference_done(&mut self, job: u64, result: &Result<Value>) -> bool {
        if !self
            .compare
            .as_ref()
            .is_some_and(|c| c.diff_job == Some(job))
        {
            return false;
        }
        let path = match result {
            Ok(v) => v.get_str("path").map(PathBuf::from),
            Err(e) => {
                self.flash(format!("No difference: {}", text::inert(&e.message, 100)));
                None
            }
        };
        if let (Some(path), Some((_, ib))) = (path, self.compare_pair()) {
            if let Some(p) = &mut self.sources[ib].player {
                p.load(&path.display().to_string(), false, None);
            }
            if let Some(c) = &mut self.compare {
                c.diff = Some(path);
            }
            self.flash(
                "Their difference: black where they're the same. d again for the after picture.",
            );
        }
        if let Some(c) = &mut self.compare {
            c.diff_job = None;
        }
        self.dirty = true;
        true
    }
}

/// The difference of two pictures, b scaled to a's size, made brighter so
/// small changes show.
fn difference(ffmpeg: &Path, a: &Path, b: &Path, out: &Path) -> Result<Value> {
    // Each read only as itself (a playlist names others), from `/`.
    let made = media::command(ffmpeg)
        .args(["-nostdin", "-v", "error", "-y"])
        .args(media::input(a))
        .args(media::input(b))
        .args([
            "-filter_complex",
            "[0:v]format=rgb24[a];[1:v]format=rgb24[b];[b][a]scale2ref=flags=bicubic[b2][a2];\
             [a2][b2]blend=all_mode=difference,lutrgb=r='min(val*4,255)':g='min(val*4,255)':b='min(val*4,255)'",
            "-frames:v",
            "1",
            "-update",
            "1",
        ])
        .arg(format!("file:{}", out.display()))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Error::new("FAILED", format!("cannot run ffmpeg: {e}")))?;
    // Made empty beforehand (Viewer::output): written, and still its own.
    if !made.status.success() || !crate::proto::lone_file(out) {
        let err = String::from_utf8_lossy(&made.stderr);
        return Err(Error::new(
            "FAILED",
            text::inert(&text::redact_urls(err.trim()), 200),
        ));
    }
    Ok(Value::obj().with("path", out.display().to_string()))
}
