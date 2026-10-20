//! The agent's words around the person's messages: for each message, the agent's last reply
//! before it in its log, and the agent's replies in the turn the message opened, up to the
//! person's next message. A short reply ("go with that", "fixed!") means nothing without them.
//! `said --context` shows them; `ask` can give them to its screen.

use crate::core::{human_text_from_body, Sender, IMPORTED_TURN_PREFIX};
use crate::corpus;
use crate::format::{self, EventHeader};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

const IO_BUFFER_SIZE: usize = 1024 * 1024;

/// One of the agent's replies: its visible text and where it is.
#[derive(Clone, Debug)]
pub struct Reply {
    pub header: EventHeader,
    pub text: String,
}

/// The agent's words around one of the person's messages.
#[derive(Clone, Debug, Default)]
pub struct Turn {
    /// The agent's last reply before the message in its log.
    pub before: Option<Reply>,
    /// The agent's replies in the turn the message opened, the last ones, oldest first.
    pub replies: Vec<Reply>,
    /// How many replies the turn held in all.
    pub reply_count: usize,
}

/// For each of the person's messages, named by source id and byte offset, the agent's words
/// around it, keeping the last `max_replies` replies of its turn. Each log that holds one is
/// read once, up to the end of the turn of the last message named in it.
pub fn read(
    root: &Path,
    messages: &[(String, u64)],
    max_replies: usize,
) -> io::Result<HashMap<(String, u64), Turn>> {
    let mut wanted: BTreeMap<&str, HashSet<u64>> = BTreeMap::new();
    for (source_id, byte_start) in messages {
        wanted
            .entry(source_id.as_str())
            .or_default()
            .insert(*byte_start);
    }
    let mut out = HashMap::new();
    for (source_id, starts) in wanted {
        read_source(root, source_id, &starts, max_replies.max(1), &mut out)?;
    }
    Ok(out)
}

fn read_source(
    root: &Path,
    source_id: &str,
    starts: &HashSet<u64>,
    max_replies: usize,
    out: &mut HashMap<(String, u64), Turn>,
) -> io::Result<()> {
    let mut last_reply: Option<Reply> = None;
    // The message whose turn is being read, with its time and text: Codex writes the person's
    // message twice at the same time, as an item and as an event, and the copy does not end
    // the turn.
    let mut current: Option<(u64, String, String)> = None;
    let mut remaining = starts.len();
    'files: for path in corpus::source_files(root, source_id)? {
        let mut reader = BufReader::with_capacity(IO_BUFFER_SIZE, File::open(&path)?);
        let mut header_line = Vec::new();
        loop {
            header_line.clear();
            if reader.read_until(b'\n', &mut header_line)? == 0 {
                break;
            }
            let header = format::parse_event_header(&header_line).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {}", path.display(), error),
                )
            })?;
            let human = header.sender == Sender::Human.as_str();
            let agent = header.sender == Sender::Assistant.as_str();
            if header.source_id != source_id
                || !(human || agent)
                || header.turn.starts_with(IMPORTED_TURN_PREFIX)
            {
                skip(&mut reader, header.body_len.saturating_add(1))?;
                continue;
            }
            let mut body = vec![0; header.body_len as usize];
            reader.read_exact(&mut body)?;
            skip(&mut reader, 1)?;
            let body = String::from_utf8_lossy(&body);
            if human {
                let said = human_text_from_body(&body).0.trim().to_string();
                if current
                    .as_ref()
                    .is_some_and(|(_, time, text)| *time == header.timestamp && *text == said)
                {
                    continue;
                }
                current = None;
                let key = (source_id.to_string(), header.byte_start);
                if starts.contains(&header.byte_start) && !out.contains_key(&key) {
                    remaining -= 1;
                    out.insert(
                        key,
                        Turn {
                            before: last_reply.clone(),
                            ..Turn::default()
                        },
                    );
                    current = Some((header.byte_start, header.timestamp.clone(), said));
                } else if remaining == 0 {
                    // Every message named is behind, and the turn of the last one has ended.
                    break 'files;
                }
                continue;
            }
            let Some(text) = reply_text(&header, &body) else {
                continue;
            };
            let reply = Reply { header, text };
            if let Some((start, ..)) = &current {
                if let Some(turn) = out.get_mut(&(source_id.to_string(), *start)) {
                    // Codex writes a reply twice too, as an item and as an event.
                    if turn
                        .replies
                        .last()
                        .is_none_or(|last| last.text != reply.text)
                    {
                        turn.reply_count += 1;
                        turn.replies.push(reply.clone());
                        if turn.replies.len() > max_replies {
                            turn.replies.remove(0);
                        }
                    }
                }
            }
            last_reply = Some(reply);
        }
    }
    Ok(())
}

/// The visible text of one of the agent's records: Claude's `text` blocks, and Codex's
/// `output_text` blocks and `agent_message` events. Thinking and tool calls are not replies.
pub fn reply_text(header: &EventHeader, body: &str) -> Option<String> {
    let fields = format::body_fields(body);
    let value = |path: &str| {
        fields
            .iter()
            .find(|(key, _)| key == path)
            .map(|(_, value)| value.as_str())
    };
    let mut texts = Vec::new();
    for (path, kind) in &fields {
        let Some(block) = path.strip_suffix("/type") else {
            continue;
        };
        if !(block.starts_with("/message/content/") || block.starts_with("/payload/content/")) {
            continue;
        }
        if matches!(kind.as_str(), "text" | "output_text") {
            if let Some(text) = value(&format!("{block}/text")) {
                texts.push(without_citations(text));
            }
        }
    }
    if texts.is_empty() && header.event_type == "agent_message" {
        texts.extend(value("/payload/message").map(without_citations));
    }
    let text = texts.join("\n");
    (!text.trim().is_empty()).then(|| text.trim().to_string())
}

/// A reply without the blocks Codex appends to name the memories it drew on: they are not
/// part of what the reply says, and the end of a reply is where its question to the person is.
fn without_citations(text: &str) -> String {
    const OPEN: &str = "<oai-mem-citation>";
    const CLOSE: &str = "</oai-mem-citation>";
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        rest = match rest[start..].find(CLOSE) {
            Some(end) => &rest[start + end + CLOSE.len()..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// The last `max` characters of a text on one line, marked with … when cut.
pub fn tail(text: &str, max: usize) -> String {
    let line = one_line(text);
    let count = line.chars().count();
    if count <= max {
        return line;
    }
    let kept: String = line.chars().skip(count - max).collect();
    format!("…{kept}")
}

/// A text on one line, cut in the middle to `max` characters: two thirds from its head and
/// one third from its tail.
pub fn middle(text: &str, max: usize) -> String {
    let line = one_line(text);
    let count = line.chars().count();
    if count <= max {
        return line;
    }
    let head = max * 2 / 3;
    let tail = max - head;
    let mut out: String = line.chars().take(head).collect();
    out.push_str(" … ");
    out.extend(line.chars().skip(count - tail));
    out
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn skip<R: Read>(reader: &mut R, length: u64) -> io::Result<()> {
    let mut limited = reader.take(length);
    io::copy(&mut limited, &mut io::sink())?;
    if limited.limit() != 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "corpus body ended before body_len",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(event_type: &str) -> EventHeader {
        EventHeader {
            event_type: event_type.to_string(),
            sender: "assistant".to_string(),
            ..EventHeader::default()
        }
    }

    fn body(fields: &[(&str, &str)]) -> String {
        let mut body = String::new();
        for (path, value) in fields {
            format::push_field(&mut body, path, value);
        }
        body
    }

    #[test]
    fn a_reply_is_its_visible_text_not_thinking_or_tool_calls() {
        let claude = body(&[
            ("/message/content/0/type", "thinking"),
            ("/message/content/0/thinking", "hidden"),
            ("/message/content/1/type", "text"),
            ("/message/content/1/text", "Leaflet をおすすめします。"),
        ]);
        assert_eq!(
            reply_text(&header("assistant"), &claude).as_deref(),
            Some("Leaflet をおすすめします。")
        );
        let codex = body(&[
            ("/payload/content/0/type", "output_text"),
            ("/payload/content/0/text", "MapLibre で実装しました。"),
        ]);
        assert_eq!(
            reply_text(&header("message"), &codex).as_deref(),
            Some("MapLibre で実装しました。")
        );
        let cited = body(&[
            ("/payload/content/0/type", "output_text"),
            (
                "/payload/content/0/text",
                "コミットしますか？\n<oai-mem-citation>\n<rollout_ids>\n01a0\n</rollout_ids>\n</oai-mem-citation>",
            ),
        ]);
        assert_eq!(
            reply_text(&header("message"), &cited).as_deref(),
            Some("コミットしますか？")
        );
        let event = body(&[("/payload/message", "直しました。")]);
        assert_eq!(
            reply_text(&header("agent_message"), &event).as_deref(),
            Some("直しました。")
        );
        let call = body(&[
            ("/message/content/0/type", "tool_use"),
            ("/message/content/0/name", "Bash"),
            ("/message/content/0/input/command", "ls"),
        ]);
        assert_eq!(reply_text(&header("assistant"), &call), None);
    }

    #[test]
    fn texts_are_cut_to_their_tail_or_around_their_middle_on_one_line() {
        assert_eq!(tail("abc\n def", 20), "abc def");
        assert_eq!(tail("abcdefghij", 4), "…ghij");
        assert_eq!(middle("abcdefghij", 6), "abcd … ij");
        assert_eq!(middle("short", 6), "short");
    }
}
