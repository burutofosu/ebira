//! A JSON value with a parser and a writer, for what `ask` sends to the API and reads back. The
//! corpus has its own field reader (`jsonl.rs`); this one keeps whole trees.

use std::fmt::{self, Write as _};

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    /// The number as written, so token counts and byte offsets stay exact.
    Number(String),
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

#[derive(Debug)]
pub struct ParseError {
    pub message: &'static str,
    pub offset: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} at byte {}", self.message, self.offset)
    }
}

const MAX_DEPTH: usize = 256;

pub fn parse(text: &str) -> Result<Value, ParseError> {
    let mut parser = Parser {
        text,
        bytes: text.as_bytes(),
        position: 0,
        depth: 0,
    };
    parser.skip_whitespace();
    let value = parser.value()?;
    parser.skip_whitespace();
    if parser.position != parser.bytes.len() {
        return Err(parser.error("trailing characters after the JSON value"));
    }
    Ok(value)
}

struct Parser<'a> {
    text: &'a str,
    bytes: &'a [u8],
    position: usize,
    depth: usize,
}

impl Parser<'_> {
    fn error(&self, message: &'static str) -> ParseError {
        ParseError {
            message,
            offset: self.position,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    fn skip_whitespace(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.peek() {
            self.position += 1;
        }
    }

    fn value(&mut self) -> Result<Value, ParseError> {
        match self.peek() {
            None => Err(self.error("expected a JSON value")),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Value::String(self.string()?)),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'n') => self.literal("null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("unexpected character")),
        }
    }

    fn literal(&mut self, word: &'static str, value: Value) -> Result<Value, ParseError> {
        if self.bytes[self.position..].starts_with(word.as_bytes()) {
            self.position += word.len();
            Ok(value)
        } else {
            Err(self.error("invalid literal"))
        }
    }

    fn digits(&mut self) -> bool {
        let start = self.position;
        while let Some(b'0'..=b'9') = self.peek() {
            self.position += 1;
        }
        self.position > start
    }

    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.position;
        if self.peek() == Some(b'-') {
            self.position += 1;
        }
        if !self.digits() {
            return Err(self.error("invalid number"));
        }
        if self.peek() == Some(b'.') {
            self.position += 1;
            if !self.digits() {
                return Err(self.error("invalid number"));
            }
        }
        if let Some(b'e' | b'E') = self.peek() {
            self.position += 1;
            if let Some(b'+' | b'-') = self.peek() {
                self.position += 1;
            }
            if !self.digits() {
                return Err(self.error("invalid number"));
            }
        }
        Ok(Value::Number(self.text[start..self.position].to_string()))
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let Some(digits) = self.bytes.get(self.position..self.position + 4) else {
            return Err(self.error("short unicode escape"));
        };
        let mut value = 0u32;
        for &digit in digits {
            let nibble = match digit {
                b'0'..=b'9' => digit - b'0',
                b'a'..=b'f' => digit - b'a' + 10,
                b'A'..=b'F' => digit - b'A' + 10,
                _ => return Err(self.error("invalid unicode escape")),
            };
            value = value * 16 + u32::from(nibble);
        }
        self.position += 4;
        Ok(value)
    }

    /// A string; a lone surrogate (a log cut in the middle of an emoji) becomes U+FFFD.
    fn string(&mut self) -> Result<String, ParseError> {
        self.position += 1;
        let mut out = String::new();
        let mut run_start = self.position;
        loop {
            match self.peek() {
                None => return Err(self.error("unterminated string")),
                Some(b'"') => {
                    out.push_str(&self.text[run_start..self.position]);
                    self.position += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    out.push_str(&self.text[run_start..self.position]);
                    self.position += 1;
                    let Some(escaped) = self.peek() else {
                        return Err(self.error("unterminated escape"));
                    };
                    self.position += 1;
                    match escaped {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let first = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&first)
                                && self.bytes.get(self.position..self.position + 2) == Some(b"\\u")
                            {
                                let saved = self.position;
                                self.position += 2;
                                let second = self.hex4()?;
                                if (0xDC00..0xE000).contains(&second) {
                                    0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00)
                                } else {
                                    self.position = saved;
                                    0xFFFD
                                }
                            } else {
                                first
                            };
                            out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                        }
                        _ => return Err(self.error("invalid escape")),
                    }
                    run_start = self.position;
                }
                Some(byte) if byte < 0x20 => return Err(self.error("control character in string")),
                Some(_) => self.position += 1,
            }
        }
    }

    fn enter(&mut self) -> Result<(), ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.error("JSON nested too deeply"));
        }
        self.position += 1;
        self.skip_whitespace();
        Ok(())
    }

    fn array(&mut self) -> Result<Value, ParseError> {
        self.enter()?;
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.position += 1;
        } else {
            loop {
                self.skip_whitespace();
                items.push(self.value()?);
                self.skip_whitespace();
                match self.peek() {
                    Some(b',') => self.position += 1,
                    Some(b']') => {
                        self.position += 1;
                        break;
                    }
                    _ => return Err(self.error("expected , or ]")),
                }
            }
        }
        self.depth -= 1;
        Ok(Value::Array(items))
    }

    fn object(&mut self) -> Result<Value, ParseError> {
        self.enter()?;
        let mut fields = Vec::new();
        if self.peek() == Some(b'}') {
            self.position += 1;
        } else {
            loop {
                self.skip_whitespace();
                if self.peek() != Some(b'"') {
                    return Err(self.error("expected a key"));
                }
                let key = self.string()?;
                self.skip_whitespace();
                if self.peek() != Some(b':') {
                    return Err(self.error("expected :"));
                }
                self.position += 1;
                self.skip_whitespace();
                let value = self.value()?;
                fields.push((key, value));
                self.skip_whitespace();
                match self.peek() {
                    Some(b',') => self.position += 1,
                    Some(b'}') => {
                        self.position += 1;
                        break;
                    }
                    _ => return Err(self.error("expected , or }")),
                }
            }
        }
        self.depth -= 1;
        Ok(Value::Object(fields))
    }
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(fields) => fields
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_array(&self) -> &[Value] {
        match self {
            Value::Array(items) => items,
            _ => &[],
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Number(number) => number.parse().ok(),
            _ => None,
        }
    }

    /// The string under `key`, or "" when it is missing or not a string.
    pub fn str_at(&self, key: &str) -> &str {
        self.get(key).and_then(Value::as_str).unwrap_or("")
    }

    /// The unsigned number under `key`, or 0.
    pub fn u64_at(&self, key: &str) -> u64 {
        self.get(key).and_then(Value::as_u64).unwrap_or(0)
    }

    /// The array under `key`, or an empty slice.
    pub fn array_at(&self, key: &str) -> &[Value] {
        self.get(key).map(Value::as_array).unwrap_or(&[])
    }

    pub fn to_json(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
            Value::Number(number) => out.push_str(number),
            Value::String(text) => write_string(text, out),
            Value::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Value::Object(fields) => {
                out.push('{');
                for (index, (key, value)) in fields.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    write_string(key, out);
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }
}

fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

pub fn string(text: impl Into<String>) -> Value {
    Value::String(text.into())
}

pub fn number(value: u64) -> Value {
    Value::Number(value.to_string())
}

/// A JSON array of strings, as text.
pub fn strings_json(items: &[&str]) -> String {
    Value::Array(items.iter().map(|item| string(*item)).collect()).to_json()
}

pub fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_values_and_keeps_numbers_exact() {
        let value = parse(r#"{"a":[1,2.5,-3e2,true,null],"b":{"c":"d"},"n":423975693}"#).unwrap();
        assert_eq!(value.array_at("a").len(), 5);
        assert_eq!(value.get("b").unwrap().str_at("c"), "d");
        assert_eq!(value.u64_at("n"), 423_975_693);
    }

    #[test]
    fn reads_escapes_and_surrogate_pairs() {
        let value = parse(r#""木が\n\"\\ 🌲""#).unwrap();
        assert_eq!(value.as_str().unwrap(), "木が\n\"\\ 🌲");
    }

    #[test]
    fn replaces_lone_surrogates() {
        let value = parse(r#""a\ud83cb""#).unwrap();
        assert_eq!(value.as_str().unwrap(), "a\u{FFFD}b");
    }

    #[test]
    fn rejects_broken_json() {
        assert!(parse("{\"a\":}").is_err());
        assert!(parse("[1,2").is_err());
        assert!(parse("\"abc").is_err());
        assert!(parse("{} x").is_err());
    }

    #[test]
    fn writes_what_it_reads() {
        let text = r#"{"k":"line\nbreak \"q\" \\ 木","list":[1,{"x":null}],"empty":[],"none":{}}"#;
        let value = parse(text).unwrap();
        assert_eq!(value.to_json(), text);
    }
}
