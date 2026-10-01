//! Results, in one way: every command writes one JSON object, its fields in a fixed order. A
//! value that is not there is `null`, never `""`: an empty name, time, path, or id is
//! written as `null`, while text is written as it is, empty or not.

use std::fmt::Write as _;

pub fn string(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            character if character.is_control() => {
                write!(output, "\\u{:04x}", character as u32).expect("String write");
            }
            character => output.push(character),
        }
    }
    output.push('"');
    output
}

/// A name, time, path, or id: `null` when it is empty.
pub fn string_or_null(value: &str) -> String {
    if value.is_empty() {
        "null".to_string()
    } else {
        string(value)
    }
}

/// One JSON object, written field by field.
pub struct Object(String);

impl Default for Object {
    fn default() -> Self {
        Object(String::from("{"))
    }
}

impl Object {
    pub fn new() -> Self {
        Self::default()
    }

    fn key(&mut self, key: &str) -> &mut String {
        if self.0.len() > 1 {
            self.0.push(',');
        }
        self.0.push('"');
        self.0.push_str(key);
        self.0.push_str("\":");
        &mut self.0
    }

    /// Text as it was written, also when it is empty.
    pub fn text(&mut self, key: &str, value: &str) -> &mut Self {
        let value = string(value);
        self.key(key).push_str(&value);
        self
    }

    /// A name, time, path, or id, `null` when empty.
    pub fn name(&mut self, key: &str, value: &str) -> &mut Self {
        let value = string_or_null(value);
        self.key(key).push_str(&value);
        self
    }

    pub fn optional(&mut self, key: &str, value: Option<&str>) -> &mut Self {
        self.name(key, value.unwrap_or(""))
    }

    pub fn number(&mut self, key: &str, value: u64) -> &mut Self {
        let value = value.to_string();
        self.key(key).push_str(&value);
        self
    }

    pub fn optional_number(&mut self, key: &str, value: Option<u64>) -> &mut Self {
        match value {
            Some(value) => self.number(key, value),
            None => {
                self.key(key).push_str("null");
                self
            }
        }
    }

    pub fn boolean(&mut self, key: &str, value: bool) -> &mut Self {
        self.key(key).push_str(if value { "true" } else { "false" });
        self
    }

    /// A value that is already JSON: a nested object or an array.
    pub fn raw(&mut self, key: &str, json: &str) -> &mut Self {
        self.key(key).push_str(json);
        self
    }

    pub fn finish(&mut self) -> String {
        let mut text = std::mem::take(&mut self.0);
        text.push('}');
        text
    }
}

/// A value that is already JSON, or `null`.
pub fn or_null(value: Option<String>) -> String {
    value.unwrap_or_else(|| "null".to_string())
}

/// An array of values that are already JSON.
pub fn array(items: impl IntoIterator<Item = String>) -> String {
    let mut output = String::from("[");
    for (index, item) in items.into_iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        output.push_str(&item);
    }
    output.push(']');
    output
}

pub fn strings<'a>(values: impl IntoIterator<Item = &'a str>) -> String {
    array(values.into_iter().map(string))
}

/// A count per name, as an object.
/// Where a record is in its log: the one shape every command uses. `source_id` is `null` for a
/// transcript the corpus has not read, and `line` for a byte range not known to start a record.
pub fn source_ref(
    source_id: &str,
    source_path: &str,
    line: Option<u64>,
    byte_start: u64,
    byte_len: u64,
) -> String {
    Object::new()
        .name("source_id", source_id)
        .name("source_path", source_path)
        .optional_number("line", line)
        .number("byte_start", byte_start)
        .number("byte_len", byte_len)
        .finish()
}

#[cfg(test)]
mod tests {
    use super::{source_ref, string, Object};

    #[test]
    fn absent_values_are_null_and_text_is_kept() {
        let json = Object::new()
            .text("text", "")
            .name("session", "")
            .optional("cwd", None)
            .name("via", "typed")
            .number("count", 2)
            .boolean("cut", false)
            .finish();
        assert_eq!(
            json,
            r#"{"text":"","session":null,"cwd":null,"via":"typed","count":2,"cut":false}"#
        );
        assert_eq!(string("a\"b\\\n\u{1}"), r#""a\"b\\\n\u0001""#);
        assert_eq!(
            source_ref("", "/logs/a.jsonl", Some(3), 10, 20),
            r#"{"source_id":null,"source_path":"/logs/a.jsonl","line":3,"byte_start":10,"byte_len":20}"#
        );
    }
}
