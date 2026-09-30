use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScalarKind {
    String,
    Number,
    Bool,
    Null,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub path: String,
    pub value: String,
    pub raw_start: u64,
    pub raw_len: u64,
    pub kind: ScalarKind,
}

impl crate::core::FieldView for Field {
    fn path(&self) -> &str {
        &self.path
    }

    fn value(&self) -> &str {
        &self.value
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub offset: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} at byte {}", self.message, self.offset)
    }
}

impl std::error::Error for ParseError {}

pub fn parse_record(bytes: &[u8], base: u64) -> Result<Vec<Field>, ParseError> {
    let mut scanner = Scanner {
        bytes,
        base,
        position: 0,
        depth: 0,
        fields: Vec::new(),
    };
    scanner.skip_whitespace();
    scanner.parse_value(String::new())?;
    scanner.skip_whitespace();
    if scanner.position != bytes.len() {
        return Err(scanner.error("trailing bytes after JSON value"));
    }
    Ok(scanner.fields)
}

const MAX_NESTING_DEPTH: usize = 256;

struct Scanner<'a> {
    bytes: &'a [u8],
    base: u64,
    position: usize,
    depth: usize,
    fields: Vec<Field>,
}

impl<'a> Scanner<'a> {
    fn parse_value(&mut self, path: String) -> Result<(), ParseError> {
        self.skip_whitespace();
        let Some(&byte) = self.bytes.get(self.position) else {
            return Err(self.error("expected JSON value"));
        };
        match byte {
            b'{' | b'[' => {
                if self.depth >= MAX_NESTING_DEPTH {
                    return Err(self.error("JSON nesting is deeper than the supported bound"));
                }
                self.depth += 1;
                let nested = if byte == b'{' {
                    self.parse_object(path)
                } else {
                    self.parse_array(path)
                };
                self.depth -= 1;
                nested
            }
            b'"' => {
                let start = self.position;
                let value = self.parse_string()?;
                self.fields.push(Field {
                    path,
                    value,
                    raw_start: self.base + start as u64,
                    raw_len: (self.position - start) as u64,
                    kind: ScalarKind::String,
                });
                Ok(())
            }
            b'-' | b'0'..=b'9' => self.parse_number(path),
            b't' => self.parse_literal(path, b"true", "true", ScalarKind::Bool),
            b'f' => self.parse_literal(path, b"false", "false", ScalarKind::Bool),
            b'n' => self.parse_literal(path, b"null", "null", ScalarKind::Null),
            _ => Err(self.error("unexpected byte while reading JSON value")),
        }
    }

    fn parse_object(&mut self, path: String) -> Result<(), ParseError> {
        self.position += 1;
        self.skip_whitespace();
        if self.take_if(b'}') {
            return Ok(());
        }
        loop {
            self.skip_whitespace();
            if self.bytes.get(self.position) != Some(&b'"') {
                return Err(self.error("expected object key"));
            }
            let key = self.parse_string()?;
            self.skip_whitespace();
            if !self.take_if(b':') {
                return Err(self.error("expected ':' after object key"));
            }
            let child_path = pointer_child(&path, &key);
            self.parse_value(child_path)?;
            self.skip_whitespace();
            if self.take_if(b'}') {
                return Ok(());
            }
            if !self.take_if(b',') {
                return Err(self.error("expected ',' or '}' in object"));
            }
        }
    }

    fn parse_array(&mut self, path: String) -> Result<(), ParseError> {
        self.position += 1;
        self.skip_whitespace();
        if self.take_if(b']') {
            return Ok(());
        }
        let mut index = 0usize;
        loop {
            let child_path = pointer_child(&path, &index.to_string());
            self.parse_value(child_path)?;
            index += 1;
            self.skip_whitespace();
            if self.take_if(b']') {
                return Ok(());
            }
            if !self.take_if(b',') {
                return Err(self.error("expected ',' or ']' in array"));
            }
            self.skip_whitespace();
        }
    }

    fn parse_number(&mut self, path: String) -> Result<(), ParseError> {
        let start = self.position;
        self.take_if(b'-');
        if self.bytes.get(self.position) == Some(&b'0') {
            self.position += 1;
        } else {
            if !matches!(self.bytes.get(self.position), Some(b'1'..=b'9')) {
                return Err(self.error("invalid JSON number"));
            }
            while matches!(self.bytes.get(self.position), Some(b'0'..=b'9')) {
                self.position += 1;
            }
        }
        if self.take_if(b'.') {
            let fraction_start = self.position;
            while matches!(self.bytes.get(self.position), Some(b'0'..=b'9')) {
                self.position += 1;
            }
            if fraction_start == self.position {
                return Err(self.error("invalid JSON number fraction"));
            }
        }
        if matches!(self.bytes.get(self.position), Some(b'e' | b'E')) {
            self.position += 1;
            if matches!(self.bytes.get(self.position), Some(b'+' | b'-')) {
                self.position += 1;
            }
            let exponent_start = self.position;
            while matches!(self.bytes.get(self.position), Some(b'0'..=b'9')) {
                self.position += 1;
            }
            if exponent_start == self.position {
                return Err(self.error("invalid JSON number exponent"));
            }
        }
        self.fields.push(Field {
            path,
            value: String::from_utf8_lossy(&self.bytes[start..self.position]).into_owned(),
            raw_start: self.base + start as u64,
            raw_len: (self.position - start) as u64,
            kind: ScalarKind::Number,
        });
        Ok(())
    }

    fn parse_literal(
        &mut self,
        path: String,
        expected: &[u8],
        value: &str,
        kind: ScalarKind,
    ) -> Result<(), ParseError> {
        let start = self.position;
        let end = start.saturating_add(expected.len());
        if self.bytes.get(start..end) != Some(expected) {
            return Err(self.error("invalid JSON literal"));
        }
        self.position = end;
        self.fields.push(Field {
            path,
            value: value.to_string(),
            raw_start: self.base + start as u64,
            raw_len: expected.len() as u64,
            kind,
        });
        Ok(())
    }

    fn parse_string(&mut self) -> Result<String, ParseError> {
        if !self.take_if(b'"') {
            return Err(self.error("expected JSON string"));
        }
        let mut output = String::new();
        loop {
            let Some(&byte) = self.bytes.get(self.position) else {
                return Err(self.error("unterminated JSON string"));
            };
            self.position += 1;
            match byte {
                b'"' => return Ok(output),
                b'\\' => {
                    let Some(&escape) = self.bytes.get(self.position) else {
                        return Err(self.error("unterminated JSON escape"));
                    };
                    self.position += 1;
                    match escape {
                        b'"' => output.push('"'),
                        b'\\' => output.push('\\'),
                        b'/' => output.push('/'),
                        b'b' => output.push('\u{0008}'),
                        b'f' => output.push('\u{000c}'),
                        b'n' => output.push('\n'),
                        b'r' => output.push('\r'),
                        b't' => output.push('\t'),
                        b'u' => {
                            let first = self.parse_hex_quad()?;
                            if (0xD800..=0xDBFF).contains(&first) {
                                if self.bytes.get(self.position..self.position + 2) != Some(b"\\u")
                                {
                                    return Err(self.error("missing low surrogate"));
                                }
                                self.position += 2;
                                let second = self.parse_hex_quad()?;
                                if !(0xDC00..=0xDFFF).contains(&second) {
                                    return Err(self.error("invalid low surrogate"));
                                }
                                let codepoint = 0x1_0000
                                    + (((first - 0xD800) as u32) << 10)
                                    + (second - 0xDC00) as u32;
                                let Some(character) = char::from_u32(codepoint) else {
                                    return Err(self.error("invalid Unicode codepoint"));
                                };
                                output.push(character);
                            } else if (0xDC00..=0xDFFF).contains(&first) {
                                return Err(self.error("unexpected low surrogate"));
                            } else {
                                let Some(character) = char::from_u32(first as u32) else {
                                    return Err(self.error("invalid Unicode codepoint"));
                                };
                                output.push(character);
                            }
                        }
                        _ => return Err(self.error("invalid JSON escape")),
                    }
                }
                0x00..=0x1f => return Err(self.error("control byte in JSON string")),
                _ => {
                    let width =
                        utf8_width(byte).ok_or_else(|| self.error("invalid UTF-8 leading byte"))?;
                    let end = self.position + width - 1;
                    if end > self.bytes.len() {
                        return Err(self.error("truncated UTF-8 sequence"));
                    }
                    let sequence = &self.bytes[self.position - 1..end];
                    let text = std::str::from_utf8(sequence)
                        .map_err(|_| self.error("invalid UTF-8 sequence"))?;
                    output.push_str(text);
                    self.position = end;
                }
            }
        }
    }

    fn parse_hex_quad(&mut self) -> Result<u16, ParseError> {
        let end = self.position + 4;
        if end > self.bytes.len() {
            return Err(self.error("truncated Unicode escape"));
        }
        let mut value = 0u16;
        for byte in &self.bytes[self.position..end] {
            let digit = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(self.error("invalid Unicode escape")),
            };
            value = (value << 4) | digit as u16;
        }
        self.position = end;
        Ok(value)
    }

    fn skip_whitespace(&mut self) {
        while matches!(
            self.bytes.get(self.position),
            Some(b' ' | b'\n' | b'\r' | b'\t')
        ) {
            self.position += 1;
        }
    }

    fn take_if(&mut self, expected: u8) -> bool {
        if self.bytes.get(self.position) == Some(&expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn error(&self, message: &str) -> ParseError {
        ParseError {
            message: message.to_string(),
            offset: self.position,
        }
    }
}

fn utf8_width(byte: u8) -> Option<usize> {
    match byte {
        0x00..=0x7f => Some(1),
        0xc2..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf4 => Some(4),
        _ => None,
    }
}

fn pointer_child(parent: &str, child: &str) -> String {
    let escaped = child.replace('~', "~0").replace('/', "~1");
    format!("{}/{}", parent, escaped)
}

#[cfg(test)]
mod tests {
    use super::{parse_record, ScalarKind};

    #[test]
    fn parses_scalars_and_json_pointers() {
        let record = br#"{"message":{"text":"\u65e5\u672c\u8a9e\uD83D\uDE80"},"items":[true,null,3.5,-0.5],"a/b":1}"#;
        let fields = parse_record(record, 10).expect("record parses");
        assert!(fields.iter().any(|field| {
            field.path == "/message/text"
                && field.value == "日本語🚀"
                && field.kind == ScalarKind::String
        }));
        assert!(fields
            .iter()
            .any(|field| field.path == "/items/0" && field.value == "true"));
        assert!(fields
            .iter()
            .any(|field| field.path == "/items/1" && field.value == "null"));
        assert!(fields
            .iter()
            .any(|field| field.path == "/items/3" && field.value == "-0.5"));
        assert!(fields
            .iter()
            .any(|field| field.path == "/a~1b" && field.value == "1"));
    }

    #[test]
    fn rejects_nesting_deeper_than_the_bound_without_exhausting_the_stack() {
        let depth = super::MAX_NESTING_DEPTH + 1;
        let mut record = "[".repeat(depth);
        record.push('1');
        record.push_str(&"]".repeat(depth));
        assert!(parse_record(record.as_bytes(), 0).is_err());

        let depth = super::MAX_NESTING_DEPTH;
        let mut record = "[".repeat(depth);
        record.push('1');
        record.push_str(&"]".repeat(depth));
        assert!(parse_record(record.as_bytes(), 0).is_ok());
    }

    #[test]
    fn rejects_malformed_records() {
        assert!(parse_record(br#"{"x":"unterminated}"#, 0).is_err());
        assert!(parse_record(br#"{"x":01}"#, 0).is_err());
    }
}
