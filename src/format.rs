use std::fmt::Write as _;

#[derive(Clone, Debug, Default)]
pub struct EventHeader {
    pub event_index: u64,
    pub source_id: String,
    pub line: u64,
    pub byte_start: u64,
    pub byte_len: u64,
    pub session: String,
    pub turn: String,
    pub role: String,
    pub kind: String,
    pub event_type: String,
    pub timestamp: String,
    pub cwd: String,
    pub repository: String,
    pub call_id: String,
    pub sender: String,
    pub via: String,
    /// Some value of the record was shortened when it was projected (tool output beyond
    /// `--tool-output-chars`, or a record too large to project).
    pub body_cut: bool,
    pub body_len: u64,
}

/// v7: every body field is length-delimited, the header says whether the body was cut, the
/// segments live in `segments/`, and managed imports live inside the corpus directory.
pub const FORMAT_VERSION: &str = "7";

pub fn event_header_line(header: &EventHeader) -> String {
    let mut line = String::with_capacity(256);
    writeln!(
        line,
        "@ebira\tv={}\tevent={}\tsource_id={}\tline={}\tbyte={}\tlen={}\tsession={}\tturn={}\trole={}\tkind={}\ttype={}\ttimestamp={}\tcwd={}\trepository={}\tcall_id={}\tsender={}\tvia={}\tcut={}\tbody_len={}",
        FORMAT_VERSION,
        header.event_index,
        encode_token(&header.source_id),
        header.line,
        header.byte_start,
        header.byte_len,
        encode_token(&header.session),
        encode_token(&header.turn),
        encode_token(&header.role),
        encode_token(&header.kind),
        encode_token(&header.event_type),
        encode_token(&header.timestamp),
        encode_token(&header.cwd),
        encode_token(&header.repository),
        encode_token(&header.call_id),
        encode_token(&header.sender),
        encode_token(&header.via),
        u8::from(header.body_cut),
        header.body_len,
    )
    .expect("writing to String cannot fail");
    line
}

pub fn parse_event_header(line: &[u8]) -> Result<EventHeader, String> {
    let text = std::str::from_utf8(line)
        .map_err(|_| "corpus header is not UTF-8".to_string())?
        .trim_end_matches(['\r', '\n']);
    let mut parts = text.split('\t');
    if parts.next() != Some("@ebira") {
        return Err("missing @ebira header".to_string());
    }
    let mut header = EventHeader::default();
    for part in parts {
        let Some((key, value)) = part.split_once('=') else {
            return Err(format!("invalid header field: {}", part));
        };
        match key {
            "event" => header.event_index = parse_u64(value, key)?,
            "source_id" => header.source_id = decode_token(value)?,
            "line" => header.line = parse_u64(value, key)?,
            "byte" => header.byte_start = parse_u64(value, key)?,
            "len" => header.byte_len = parse_u64(value, key)?,
            "session" => header.session = decode_token(value)?,
            "turn" => header.turn = decode_token(value)?,
            "role" => header.role = decode_token(value)?,
            "kind" => header.kind = decode_token(value)?,
            "type" => header.event_type = decode_token(value)?,
            "timestamp" => header.timestamp = decode_token(value)?,
            "cwd" => header.cwd = decode_token(value)?,
            "repository" => header.repository = decode_token(value)?,
            "call_id" => header.call_id = decode_token(value)?,
            "sender" => header.sender = decode_token(value)?,
            "via" => header.via = decode_token(value)?,
            "cut" => header.body_cut = value == "1",
            "body_len" => header.body_len = parse_u64(value, key)?,
            "v" if value != FORMAT_VERSION => {
                return Err(format!(
                "corpus format is v={}, this build reads v={}; run `ebira sync`, which rebuilds it",
                value, FORMAT_VERSION
            ))
            }
            "v" => {}
            _ => {}
        }
    }
    if header.source_id.is_empty() {
        return Err("header has no source_id".to_string());
    }
    Ok(header)
}

fn parse_u64(value: &str, key: &str) -> Result<u64, String> {
    value
        .parse()
        .map_err(|_| format!("invalid {} value: {}", key, value))
}

/// The body of a projected event is a list of fields, each written as `path\tlen\tvalue\n`,
/// where `len` is the byte length of `value`; values may hold any character, newlines included.
pub fn push_field(body: &mut String, path: &str, value: &str) {
    body.push_str(path);
    body.push('\t');
    body.push_str(&value.len().to_string());
    body.push('\t');
    body.push_str(value);
    body.push('\n');
}

/// Reads a body written by `push_field` back into its fields.
pub fn body_fields(body: &str) -> Vec<(String, String)> {
    body_field_slices(body.as_bytes())
        .map(|(path, value)| {
            (
                String::from_utf8_lossy(path).into_owned(),
                String::from_utf8_lossy(value).into_owned(),
            )
        })
        .collect()
}

/// The fields of a body in order, as `(path, value)` slices of it. A body that ends inside a
/// field yields the fields before it.
pub fn body_field_slices(body: &[u8]) -> BodyFields<'_> {
    BodyFields {
        bytes: body,
        cursor: 0,
    }
}

pub struct BodyFields<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> Iterator for BodyFields<'a> {
    type Item = (&'a [u8], &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let bytes = self.bytes;
        let start = self.cursor;
        let path_end = start + bytes.get(start..)?.iter().position(|byte| *byte == b'\t')?;
        let length_start = path_end + 1;
        let length_end = length_start
            + bytes[length_start..]
                .iter()
                .position(|byte| *byte == b'\t')?;
        let value_len = std::str::from_utf8(&bytes[length_start..length_end])
            .ok()?
            .parse::<usize>()
            .ok()?;
        let value_start = length_end + 1;
        let value_end = value_start.checked_add(value_len)?;
        if bytes.get(value_end) != Some(&b'\n') {
            return None;
        }
        self.cursor = value_end + 1;
        Some((&bytes[start..path_end], &bytes[value_start..value_end]))
    }
}

pub fn encode_token(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '%' => encoded.push_str("%25"),
            '|' => encoded.push_str("%7C"),
            '\t' => encoded.push_str("%09"),
            '\n' => encoded.push_str("%0A"),
            '\r' => encoded.push_str("%0D"),
            character => encoded.push(character),
        }
    }
    encoded
}

pub fn decode_token(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let mut decoded = String::with_capacity(value.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err("truncated percent escape".to_string());
            }
            let high = hex(bytes[index + 1])?;
            let low = hex(bytes[index + 2])?;
            decoded.push((high << 4 | low) as char);
            index += 3;
        } else {
            let character = value[index..]
                .chars()
                .next()
                .ok_or_else(|| "invalid UTF-8 token".to_string())?;
            decoded.push(character);
            index += character.len_utf8();
        }
    }
    Ok(decoded)
}

fn hex(value: u8) -> Result<u8, String> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err("invalid percent escape".to_string()),
    }
}

pub fn json_string(value: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::{body_field_slices, body_fields, decode_token, encode_token, push_field};

    #[test]
    fn a_body_cut_inside_a_field_yields_the_fields_before_it() {
        let mut body = String::new();
        push_field(&mut body, "/first", "kept	whole");
        push_field(&mut body, "/second", "cut short");
        let cut = &body.as_bytes()[..body.len() - 4];
        let fields = body_field_slices(cut).collect::<Vec<_>>();
        assert_eq!(fields, [(&b"/first"[..], &b"kept	whole"[..])]);
    }

    #[test]
    fn body_fields_read_back_exactly() {
        let mut body = String::new();
        push_field(&mut body, "/message/content", "two\nlines\twith a tab\n");
        push_field(&mut body, "/payload/output", "");
        push_field(&mut body, "/a", "日本語");
        assert_eq!(
            body_fields(&body),
            vec![
                (
                    "/message/content".to_string(),
                    "two\nlines\twith a tab\n".to_string()
                ),
                ("/payload/output".to_string(), String::new()),
                ("/a".to_string(), "日本語".to_string()),
            ]
        );
    }

    #[test]
    fn token_round_trip_preserves_delimiters() {
        let value = "日本語\tpath%\n";
        assert_eq!(decode_token(&encode_token(value)).expect("decodes"), value);
    }
}
