//! Text from outside (file names, a video's tags, mpv's and ffmpeg's
//! messages, the human's notes) made inert before it's shown on the
//! viewer's screen or written out, and times as people write them.

/// Unicode format characters that change how a line reads without being
/// seen: bidi embeddings, overrides and isolates, zero-width ones, the soft
/// hyphen, the byte-order mark.
pub fn invisible(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{061C}' | '\u{180E}' | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}' | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}')
}

/// `text` on one line, safe to print to a terminal: control characters (C0,
/// DEL, C1, which would start escape sequences) and invisible ones are shown
/// escaped (`\u{1b}`), and it's cut at `max` characters with an ellipsis.
pub fn inert(text: &str, max: usize) -> String {
    let mut out = String::new();
    for (count, c) in text.chars().enumerate() {
        if count >= max {
            out.push('…');
            break;
        }
        if c.is_control() {
            out.extend(c.escape_default());
        } else if invisible(c) {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    out
}

/// What the human typed, kept: printable characters only (control and
/// invisible ones dropped), at most `max`.
pub fn typed(text: &str, max: usize) -> String {
    text.chars()
        .filter(|&c| !c.is_control() && !invisible(c))
        .take(max)
        .collect()
}

/// A URL without a user and password in it: `rtsp://user:pw@host/x` is
/// shown `rtsp://***@host/x`. Anything else as it is.
pub fn redact_url(url: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://")
        && !scheme.contains('/')
    {
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        if let Some(at) = rest[..end].rfind('@') {
            return format!("{scheme}://***@{}", &rest[at + 1..]);
        }
    }
    url.to_owned()
}

/// A time in seconds, as given: `83.5`, `1:23.5`, `00:01:23.500`; or, with
/// `+` or `-` in front, from where playback is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Time {
    At(f64),
    By(f64),
}

pub fn parse_time(text: &str) -> Result<Time, String> {
    let bad = || {
        format!(
            "not a time: {:?} (83.5, 1:23.5, 00:01:23.500, +5, -2)",
            inert(text, 40)
        )
    };
    let text = text.trim();
    let (sign, rest) = match text.as_bytes().first() {
        Some(b'+') => (Some(1.0), &text[1..]),
        Some(b'-') => (Some(-1.0), &text[1..]),
        _ => (None, text),
    };
    let seconds = clock_seconds(rest).ok_or_else(bad)?;
    Ok(match sign {
        Some(sign) => Time::By(sign * seconds),
        None => Time::At(seconds),
    })
}

/// A time that must be a point, not a step: `83.5`, `1:23.5`.
pub fn parse_at(text: &str) -> Result<f64, String> {
    match parse_time(text)? {
        Time::At(t) => Ok(t),
        Time::By(_) => Err(format!(
            "{:?}: a time from the start, not +/-",
            inert(text, 40)
        )),
    }
}

/// `SS[.f]`, `MM:SS[.f]`, `HH:MM:SS[.f]`; minutes and seconds under 60 after
/// the first part.
fn clock_seconds(text: &str) -> Option<f64> {
    let parts: Vec<&str> = text.split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let mut total = 0.0;
    for (i, part) in parts.iter().enumerate() {
        let last = i == parts.len() - 1;
        let ok = !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_digit() || (last && c == '.'))
            && part.matches('.').count() <= 1
            && !part.starts_with('.')
            && !part.ends_with('.');
        if !ok {
            return None;
        }
        let value: f64 = part.parse().ok()?;
        if i > 0 && value >= 60.0 {
            return None;
        }
        total = total * 60.0 + value;
    }
    (total.is_finite() && total < 1e9).then_some(total)
}

/// `1:23.500`, or `1:02:03.500` from an hour.
pub fn clock(seconds: f64) -> String {
    let ms = (seconds.max(0.0) * 1000.0).round() as u64;
    let (h, m, s, ms) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}.{ms:03}")
    } else {
        format!("{m}:{s:02}.{ms:03}")
    }
}

/// `1:23`, or `1:02:03`: for a label.
pub fn short_clock(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(parse_time("83.5"), Ok(Time::At(83.5)));
        assert_eq!(parse_time("1:23.5"), Ok(Time::At(83.5)));
        assert_eq!(parse_time("00:01:23.500"), Ok(Time::At(83.5)));
        assert_eq!(parse_time("+5"), Ok(Time::By(5.0)));
        assert_eq!(parse_time("-2"), Ok(Time::By(-2.0)));
        assert_eq!(parse_time("-0:02"), Ok(Time::By(-2.0)));
        assert_eq!(parse_time("90:00"), Ok(Time::At(5400.0)));
        assert_eq!(parse_time(" 7 "), Ok(Time::At(7.0)));
        for bad in [
            "", "+", "1:60", "1:2:3:4", "a", "1.2.3", ".5", "5.", "1:-2", "1e3", "inf", "NaN",
            "1::2", "1:2.5:3", "++1", "1 2",
        ] {
            assert!(parse_time(bad).is_err(), "{bad:?}");
        }
        assert_eq!(parse_at("12"), Ok(12.0));
        assert!(parse_at("+12").is_err());
    }

    #[test]
    fn clocks() {
        assert_eq!(clock(83.5), "1:23.500");
        assert_eq!(clock(3723.25), "1:02:03.250");
        assert_eq!(clock(-1.0), "0:00.000");
        assert_eq!(clock(59.9996), "1:00.000");
        assert_eq!(short_clock(83.9), "1:23");
        assert_eq!(short_clock(3723.0), "1:02:03");
        assert_eq!(parse_time(&clock(3723.25)), Ok(Time::At(3723.25)));
    }

    #[test]
    fn passwords_are_not_shown() {
        assert_eq!(
            redact_url("rtsp://me:secret@cam.local:554/s"),
            "rtsp://***@cam.local:554/s"
        );
        assert_eq!(
            redact_url("https://a:b@c@host/p?x=@y"),
            "https://***@host/p?x=@y"
        );
        assert_eq!(redact_url("https://host/p?u=a@b"), "https://host/p?u=a@b");
        assert_eq!(redact_url("/Users/me/a@b.mp4"), "/Users/me/a@b.mp4");
        assert_eq!(redact_url("/a/b://c@d"), "/a/b://c@d");
    }

    #[test]
    fn inert_text() {
        assert_eq!(inert("a\x1b]2;x\x07b", 100), "a\\u{1b}]2;x\\u{7}b");
        assert_eq!(inert("evil\u{202e}txt", 100), "evil\\u{202e}txt");
        assert_eq!(inert("abcdef", 3), "abc…");
        assert!(inert("\u{9b}31m\r\n", 10).chars().all(|c| !c.is_control()));
        assert_eq!(typed("a\x1b[31mb\u{200b}c\td", 100), "a[31mbcd");
        assert_eq!(typed("abcdef", 2), "ab");
    }
}
