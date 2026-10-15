//! A small JSON value, reader and writer: what pepper-agent says and hears
//! (its commands' output, the viewer's socket, mpv's IPC, ffprobe) without
//! a dependency.
//!
//! Written text is safe to print to a terminal as it is: besides what JSON
//! must escape, DEL, the C1 controls and the invisible format characters
//! (bidi controls, zero-width ones) are written as `\uXXXX`.

use std::fmt::Write as _;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Value>),
    /// In the order the members were added or read.
    Obj(Vec<(String, Value)>),
}

/// Nesting deeper than this is refused when reading.
const MAX_DEPTH: usize = 64;

impl Value {
    pub fn obj() -> Value {
        Value::Obj(Vec::new())
    }

    /// Adds a member (an object's; nothing for anything else) and returns it.
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Value {
        self.set(key, value);
        self
    }

    /// Sets a member, replacing one of the same name.
    pub fn set(&mut self, key: &str, value: impl Into<Value>) {
        if let Value::Obj(members) = self {
            let value = value.into();
            match members.iter_mut().find(|(k, _)| k == key) {
                Some((_, v)) => *v = value,
                None => members.push((key.to_owned(), value)),
            }
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Takes a member out, leaving the rest.
    pub fn take(&mut self, key: &str) -> Option<Value> {
        match self {
            Value::Obj(members) => {
                let at = members.iter().position(|(k, _)| k == key)?;
                Some(members.remove(at).1)
            }
            _ => None,
        }
    }

    pub fn str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn num(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            _ => None,
        }
    }

    pub fn bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn arr(&self) -> Option<&[Value]> {
        match self {
            Value::Arr(items) => Some(items),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// A member's string, number or flag.
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Value::str)
    }

    pub fn get_num(&self, key: &str) -> Option<f64> {
        self.get(key).and_then(Value::num)
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.get(key).and_then(Value::bool)
    }

    /// On one line.
    pub fn to_line(&self) -> String {
        let mut out = String::new();
        write_value(&mut out, self, None, 0);
        out
    }

    /// Indented by two spaces a level.
    pub fn to_pretty(&self) -> String {
        let mut out = String::new();
        write_value(&mut out, self, Some(2), 0);
        out
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Value {
        Value::Bool(b)
    }
}

impl From<f64> for Value {
    fn from(n: f64) -> Value {
        Value::Num(n)
    }
}

impl From<u32> for Value {
    fn from(n: u32) -> Value {
        Value::Num(n.into())
    }
}

impl From<i32> for Value {
    fn from(n: i32) -> Value {
        Value::Num(n.into())
    }
}

impl From<u64> for Value {
    fn from(n: u64) -> Value {
        Value::Num(n as f64)
    }
}

impl From<usize> for Value {
    fn from(n: usize) -> Value {
        Value::Num(n as f64)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::Str(s.to_owned())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Value {
        Value::Str(s)
    }
}

impl From<&String> for Value {
    fn from(s: &String) -> Value {
        Value::Str(s.clone())
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(o: Option<T>) -> Value {
        o.map_or(Value::Null, Into::into)
    }
}

impl<T: Into<Value>> From<Vec<T>> for Value {
    fn from(items: Vec<T>) -> Value {
        Value::Arr(items.into_iter().map(Into::into).collect())
    }
}

fn write_value(out: &mut String, value: &Value, indent: Option<usize>, level: usize) {
    let newline = |out: &mut String, level: usize| {
        if let Some(step) = indent {
            out.push('\n');
            out.extend(std::iter::repeat_n(' ', step * level));
        }
    };
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Num(n) => write_num(out, *n),
        Value::Str(s) => write_str(out, s),
        Value::Arr(items) if items.is_empty() => out.push_str("[]"),
        Value::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, level + 1);
                write_value(out, item, indent, level + 1);
            }
            newline(out, level);
            out.push(']');
        }
        Value::Obj(members) if members.is_empty() => out.push_str("{}"),
        Value::Obj(members) => {
            out.push('{');
            for (i, (key, item)) in members.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, level + 1);
                write_str(out, key);
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                write_value(out, item, indent, level + 1);
            }
            newline(out, level);
            out.push('}');
        }
    }
}

/// An integer as one, anything else as Rust's shortest form that reads back
/// the same; JSON has no NaN or infinity, so those are null.
fn write_num(out: &mut String, n: f64) {
    if !n.is_finite() {
        out.push_str("null");
    } else if n == n.trunc() && n.abs() < 1e15 {
        let _ = write!(out, "{}", n as i64);
    } else {
        let _ = write!(out, "{n}");
    }
}

fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() || crate::text::invisible(c) => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Reads one JSON value, with nothing but white space after it.
pub fn parse(text: &str) -> Result<Value, String> {
    let mut reader = Reader {
        bytes: text.as_bytes(),
        at: 0,
    };
    let value = reader.value(0)?;
    reader.space();
    if reader.at != reader.bytes.len() {
        return Err(reader.fail("text after the value"));
    }
    Ok(value)
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn fail(&self, what: &str) -> String {
        format!("bad JSON at byte {}: {what}", self.at)
    }

    fn space(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.bytes.get(self.at) {
            self.at += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn eat(&mut self, byte: u8) -> Result<(), String> {
        if self.peek() == Some(byte) {
            self.at += 1;
            Ok(())
        } else {
            Err(self.fail(&format!("expected '{}'", byte as char)))
        }
    }

    fn word(&mut self, word: &str, value: Value) -> Result<Value, String> {
        if self.bytes[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            Ok(value)
        } else {
            Err(self.fail("unknown word"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > MAX_DEPTH {
            return Err(self.fail("nested too deep"));
        }
        self.space();
        match self.peek() {
            Some(b'{') => {
                self.at += 1;
                let mut members = Vec::new();
                self.space();
                if self.peek() == Some(b'}') {
                    self.at += 1;
                    return Ok(Value::Obj(members));
                }
                loop {
                    self.space();
                    let key = self.string()?;
                    self.space();
                    self.eat(b':')?;
                    let value = self.value(depth + 1)?;
                    // The last of a name wins, as most readers do.
                    members.retain(|(k, _)| *k != key);
                    members.push((key, value));
                    self.space();
                    match self.peek() {
                        Some(b',') => self.at += 1,
                        Some(b'}') => {
                            self.at += 1;
                            return Ok(Value::Obj(members));
                        }
                        _ => return Err(self.fail("expected ',' or '}'")),
                    }
                }
            }
            Some(b'[') => {
                self.at += 1;
                let mut items = Vec::new();
                self.space();
                if self.peek() == Some(b']') {
                    self.at += 1;
                    return Ok(Value::Arr(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.space();
                    match self.peek() {
                        Some(b',') => self.at += 1,
                        Some(b']') => {
                            self.at += 1;
                            return Ok(Value::Arr(items));
                        }
                        _ => return Err(self.fail("expected ',' or ']'")),
                    }
                }
            }
            Some(b'"') => self.string().map(Value::Str),
            Some(b't') => self.word("true", Value::Bool(true)),
            Some(b'f') => self.word("false", Value::Bool(false)),
            Some(b'n') => self.word("null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.fail("unexpected character")),
            None => Err(self.fail("unexpected end")),
        }
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.at;
        let digits = |r: &mut Self| {
            let from = r.at;
            while let Some(b'0'..=b'9') = r.peek() {
                r.at += 1;
            }
            r.at > from
        };
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        if !digits(self) {
            return Err(self.fail("expected a digit"));
        }
        if self.peek() == Some(b'.') {
            self.at += 1;
            if !digits(self) {
                return Err(self.fail("expected a digit"));
            }
        }
        if let Some(b'e' | b'E') = self.peek() {
            self.at += 1;
            if let Some(b'+' | b'-') = self.peek() {
                self.at += 1;
            }
            if !digits(self) {
                return Err(self.fail("expected a digit"));
            }
        }
        // ASCII only, checked above.
        let text = std::str::from_utf8(&self.bytes[start..self.at]).unwrap_or("x");
        text.parse::<f64>()
            .map(Value::Num)
            .map_err(|_| self.fail("bad number"))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self
            .bytes
            .get(self.at..self.at + 4)
            .ok_or_else(|| self.fail("short \\u escape"))?;
        let text = std::str::from_utf8(digits).map_err(|_| self.fail("bad \\u escape"))?;
        let unit = u32::from_str_radix(text, 16).map_err(|_| self.fail("bad \\u escape"))?;
        self.at += 4;
        Ok(unit)
    }

    fn string(&mut self) -> Result<String, String> {
        self.eat(b'"')?;
        let mut out = Vec::new();
        loop {
            // A run of plain bytes in one go: Chrome's frames come as
            // megabytes of base64.
            let run = self.bytes[self.at..]
                .iter()
                .position(|&b| b == b'"' || b == b'\\' || b < 0x20)
                .unwrap_or(self.bytes.len() - self.at);
            out.extend_from_slice(&self.bytes[self.at..self.at + run]);
            self.at += run;
            let Some(byte) = self.peek() else {
                return Err(self.fail("unterminated string"));
            };
            self.at += 1;
            match byte {
                b'"' => break,
                b'\\' => {
                    let Some(escape) = self.peek() else {
                        return Err(self.fail("unterminated string"));
                    };
                    self.at += 1;
                    let c = match escape {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let unit = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&unit)
                                && self.bytes[self.at..].starts_with(b"\\u")
                            {
                                self.at += 2;
                                let low = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&low) {
                                    return Err(self.fail("bad surrogate pair"));
                                }
                                0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00)
                            } else {
                                unit
                            };
                            // A lone surrogate isn't a character.
                            char::from_u32(code).unwrap_or('\u{FFFD}')
                        }
                        _ => return Err(self.fail("bad escape")),
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
                0..0x20 => return Err(self.fail("control character in a string")),
                _ => out.push(byte),
            }
        }
        // The input was a &str, and escapes add whole characters.
        String::from_utf8(out).map_err(|_| self.fail("bad UTF-8"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_it_writes() {
        let value = Value::obj()
            .with("name", "a \"clip\"\n")
            .with("time", 83.5)
            .with("count", 3u32)
            .with("paused", true)
            .with("none", Value::Null)
            .with("list", vec![1.0, -2.25e-3])
            .with("empty", Value::obj());
        let line = value.to_line();
        assert_eq!(
            line,
            r#"{"name":"a \"clip\"\n","time":83.5,"count":3,"paused":true,"none":null,"list":[1,-0.00225],"empty":{}}"#
        );
        assert_eq!(parse(&line).unwrap(), value);
        assert_eq!(parse(&value.to_pretty()).unwrap(), value);
    }

    #[test]
    fn text_is_inert_on_a_terminal() {
        let s = Value::from("a\x1b]2;x\x07b\u{9b}c\u{7f}\u{202e}d\u{200b}");
        let line = s.to_line();
        assert_eq!(line, r#""a\u001b]2;x\u0007b\u009bc\u007f\u202ed\u200b""#);
        assert!(line.chars().all(|c| !c.is_control()));
        assert_eq!(parse(&line).unwrap(), s);
    }

    #[test]
    fn reads_json() {
        let v = parse(r#" {"a": [true, false, null, "\u00e9\ud83d\ude00", 1e3, -0.5], "a2": {}} "#)
            .unwrap();
        let a = v.get("a").and_then(Value::arr).unwrap();
        assert_eq!(a[3].str(), Some("é😀"));
        assert_eq!(a[4].num(), Some(1000.0));
        assert_eq!(a[5].num(), Some(-0.5));
        assert_eq!(parse(r#"{"k":1,"k":2}"#).unwrap().get_num("k"), Some(2.0));
        assert_eq!(parse(r#""\ud800""#).unwrap().str(), Some("\u{FFFD}"));
    }

    #[test]
    fn refuses_bad_json() {
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\"}",
            "01x",
            "1.",
            "-",
            "\"a",
            "\"\x01\"",
            "tru",
            "[1] 2",
            "\"\\x\"",
            "{\"a\":1,}",
            "\"\\ud800\\u0041\"",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        let deep = "[".repeat(100) + &"]".repeat(100);
        assert!(parse(&deep).is_err());
    }

    #[test]
    fn numbers() {
        assert_eq!(Value::from(f64::NAN).to_line(), "null");
        assert_eq!(Value::from(1e20).to_line(), "100000000000000000000");
        assert_eq!(Value::from(0.1).to_line(), "0.1");
        assert_eq!(Value::from(-3.0).to_line(), "-3");
    }
}
