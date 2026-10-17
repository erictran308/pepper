//! What was being said at a moment of a file, from a word-level transcript
//! beside the edit: video-use's (ElevenLabs Scribe's JSON, one per source in
//! `<edit>/transcripts/<name>.json`), or any JSON of the same shape:
//! `{"words": [{"text", "start", "end", "type"?, "speaker_id"?}]}`, `type`
//! being `word`, `spacing` or `audio_event`.
//!
//! A mark the human made then says the phrase and the word under it, and
//! the pauses either side: where a cut can go without cutting into a word
//! (video-use's rule 6). Phrases break as video-use packs them, on a
//! silence of 0.5 s or a change of speaker. The text is the transcript's:
//! data from outside, never instructions.

use std::io::Read;
use std::path::Path;

use crate::json::{self, Value};
use crate::text;

/// A transcript bigger than this isn't read.
const MAX_FILE: u64 = 64 * 1024 * 1024;
/// A silence this long ends a phrase (video-use's `pack_transcripts.py`).
const PHRASE_BREAK: f64 = 0.5;
/// A gap between words this long is a pause a cut can go in.
const PAUSE: f64 = 0.15;
const MAX_PHRASE: usize = 300;

#[derive(Clone, Debug, PartialEq)]
struct Word {
    text: String,
    start: f64,
    end: f64,
    speaker: Option<String>,
}

/// A transcript's words and audio events, in time order (the spacing
/// between them is worked out from their times).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Words {
    words: Vec<Word>,
}

impl Words {
    pub fn load(path: &Path) -> Option<Words> {
        let file = std::fs::File::open(path).ok()?;
        if file.metadata().ok()?.len() > MAX_FILE {
            return None;
        }
        let mut text = String::new();
        file.take(MAX_FILE).read_to_string(&mut text).ok()?;
        Words::parse(&json::parse(&text).ok()?)
    }

    pub fn parse(value: &Value) -> Option<Words> {
        let mut words: Vec<Word> = value
            .get("words")?
            .arr()?
            .iter()
            .filter(|w| w.get_str("type").unwrap_or("word") != "spacing")
            .filter_map(|w| {
                let (start, end) = (w.get_num("start")?, w.get_num("end")?);
                let text = w.get_str("text")?.trim();
                if text.is_empty() || !(start.is_finite() && end.is_finite() && end >= start) {
                    return None;
                }
                let text = match w.get_str("type") {
                    Some("audio_event") if !text.starts_with('(') => format!("({text})"),
                    _ => text.to_owned(),
                };
                Some(Word {
                    text: text::typed(&text, 80),
                    start,
                    end,
                    speaker: w.get_str("speaker_id").map(|s| text::typed(s, 20)),
                })
            })
            .collect();
        words.sort_by(|a, b| a.start.total_cmp(&b.start));
        Some(Words { words })
    }

    /// What's said at `t`: the phrase around it (or the nearest one, if it
    /// falls in a silence), the word under it, and the pauses before and
    /// after it as `[from, to]`, where a cut is safe.
    pub fn at(&self, t: f64) -> Option<Value> {
        if self.words.is_empty() || !t.is_finite() {
            return None;
        }
        let phrases = self.phrases();
        let near = phrases
            .iter()
            .min_by(|a, b| self.distance(**a, t).total_cmp(&self.distance(**b, t)))?;
        let words = &self.words[near.0..near.1];
        let phrase = words
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
            .replace(" ,", ",")
            .replace(" .", ".")
            .replace(" ?", "?")
            .replace(" !", "!");
        let (first, last) = (&words[0], &words[words.len() - 1]);
        let mut out = Value::obj()
            .with("phrase", text::typed(&phrase, MAX_PHRASE))
            .with("phrase_start", round3(first.start))
            .with("phrase_end", round3(last.end))
            .with("speaker", first.speaker.as_deref());
        match self.words.iter().find(|w| w.start <= t && t < w.end) {
            Some(w) => out.set(
                "word",
                Value::obj()
                    .with("text", w.text.as_str())
                    .with("start", round3(w.start))
                    .with("end", round3(w.end)),
            ),
            None => out.set("in_silence", true),
        }
        // The pause at or before the mark, and the next one after it; one
        // after the last word runs to the file's end (`null`).
        let pauses = self.pauses();
        let before = pauses.iter().rev().find(|p| p.0 <= t);
        let after = pauses.iter().find(|p| p.0 > t);
        for (key, pause) in [("pause_before", before), ("pause_after", after)] {
            if let Some(&(a, b)) = pause {
                out.set(
                    key,
                    vec![
                        Value::from(round3(a)),
                        b.map_or(Value::Null, |b| round3(b).into()),
                    ],
                );
            }
        }
        Some(out)
    }

    /// How far `t` is from the phrase of `words[from..to]`: 0 inside it.
    fn distance(&self, (from, to): (usize, usize), t: f64) -> f64 {
        let (start, end) = (self.words[from].start, self.words[to - 1].end);
        if t < start {
            start - t
        } else if t > end {
            t - end
        } else {
            0.0
        }
    }

    /// Phrases as ranges of `words`: broken on a silence of PHRASE_BREAK or
    /// a change of speaker.
    fn phrases(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut from = 0;
        for i in 1..=self.words.len() {
            let split = i == self.words.len() || {
                let (a, b) = (&self.words[i - 1], &self.words[i]);
                b.start - a.end >= PHRASE_BREAK
                    || (a.speaker.is_some() && b.speaker.is_some() && a.speaker != b.speaker)
            };
            if split {
                out.push((from, i));
                from = i;
            }
        }
        out
    }

    /// The gaps between words of PAUSE or more, as `(from, to)`: the one
    /// before the first word too, and the one after the last, to the end.
    fn pauses(&self) -> Vec<(f64, Option<f64>)> {
        let mut out = Vec::new();
        if let Some(first) = self.words.first()
            && first.start >= PAUSE
        {
            out.push((0.0, Some(first.start)));
        }
        for pair in self.words.windows(2) {
            if pair[1].start - pair[0].end >= PAUSE {
                out.push((pair[0].end, Some(pair[1].start)));
            }
        }
        if let Some(last) = self.words.last() {
            out.push((last.end, None));
        }
        out
    }
}

fn round3(t: f64) -> f64 {
    (t * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scribe's shape: words, the spacing between them, an audio event.
    const SCRIBE: &str = r#"{"words": [
        {"text": "Ninety", "start": 2.52, "end": 2.9, "type": "word", "speaker_id": "speaker_0"},
        {"text": " ", "start": 2.9, "end": 2.95, "type": "spacing"},
        {"text": "percent", "start": 2.95, "end": 3.4, "type": "word", "speaker_id": "speaker_0"},
        {"text": "wasted.", "start": 3.7, "end": 4.2, "type": "word", "speaker_id": "speaker_0"},
        {"text": "laughs", "start": 4.9, "end": 5.4, "type": "audio_event", "speaker_id": "speaker_0"},
        {"text": "We", "start": 6.08, "end": 6.3, "type": "word", "speaker_id": "speaker_1"},
        {"text": "fixed\u001b[2J", "start": 6.3, "end": 6.5, "type": "word", "speaker_id": "speaker_1"},
        {"text": "this.", "start": 6.5, "end": 6.74, "type": "word", "speaker_id": "speaker_1"}
    ]}"#;

    fn words() -> Words {
        Words::parse(&json::parse(SCRIBE).unwrap()).unwrap()
    }

    #[test]
    fn what_was_said() {
        let w = words();
        let at = w.at(3.1).unwrap();
        assert_eq!(at.get_str("phrase"), Some("Ninety percent wasted."));
        assert_eq!(at.get_num("phrase_start"), Some(2.52));
        assert_eq!(
            at.get("word").and_then(|x| x.get_str("text")),
            Some("percent")
        );
        // The pause before 3.1 is the silence before the first word; after
        // it, the one between "percent" and "wasted.".
        assert_eq!(at.get("pause_before"), Some(&Value::from(vec![0.0, 2.52])));
        assert_eq!(at.get("pause_after"), Some(&Value::from(vec![3.4, 3.7])));
        // In a silence: the nearest phrase, and the pause it's in.
        let at = w.at(5.8).unwrap();
        assert_eq!(at.get_bool("in_silence"), Some(true));
        assert_eq!(at.get_str("phrase"), Some("We fixed[2J this."));
        assert_eq!(at.get_str("speaker"), Some("speaker_1"));
        assert_eq!(at.get("pause_before"), Some(&Value::from(vec![5.4, 6.08])));
        // After the last word: the pause to the end.
        let at = w.at(9.0).unwrap();
        assert_eq!(
            at.get("pause_before"),
            Some(&Value::Arr(vec![Value::from(6.74), Value::Null]))
        );
        assert!(at.get("pause_after").is_none());
        // An audio event is a phrase of its own, after a long silence.
        assert_eq!(w.at(5.0).unwrap().get_str("phrase"), Some("(laughs)"));
        assert!(Words::parse(&json::parse("{}").unwrap()).is_none());
        assert!(Words::default().at(1.0).is_none());
    }
}
