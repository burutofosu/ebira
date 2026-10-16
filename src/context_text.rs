//! A readable view of original records. The corpus supplies the established sender and
//! question-answer classification; values are selected again from the original bytes with
//! no tool-output preview limit. The JSON view remains the lossless raw-record interface.

use crate::core::{self, EventKind, Sender};
use crate::{corpus, format, jsonl};
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

pub fn render(root: &Path, source_id: &str, start: u64, bytes: &[u8]) -> io::Result<String> {
    let end = start + bytes.len() as u64;
    let mut text = String::new();
    let mut covered = start;
    let mut records = 0;
    let mut shown = 0;
    for path in corpus::source_files(root, source_id)? {
        let mut reader = BufReader::new(File::open(&path)?);
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            let header = format::parse_event_header(&line)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if header.source_id == source_id && header.byte_start >= end {
                break;
            }
            let record_end = header.byte_start.saturating_add(header.byte_len);
            let selected =
                header.source_id == source_id && header.byte_start >= start && record_end <= end;
            if !selected {
                let mut body = reader.by_ref().take(header.body_len + 1);
                io::copy(&mut body, &mut io::sink())?;
                if body.limit() != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "corpus body ended early",
                    ));
                }
                continue;
            }
            if header.byte_start != covered {
                return Err(incomplete_range());
            }
            let mut projected = vec![0; header.body_len as usize];
            reader.read_exact(&mut projected)?;
            let mut separator = [0];
            reader.read_exact(&mut separator)?;
            if separator != *b"\n" {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "missing event separator",
                ));
            }
            let original =
                &bytes[(header.byte_start - start) as usize..(record_end - start) as usize];
            let fields = jsonl::parse_record(original, header.byte_start).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "cannot read record at {}: {error}; use --format json for its raw bytes",
                        header.byte_start
                    ),
                )
            })?;
            let mut meta = core::record_meta(&fields);
            meta.event_kind = EventKind::from_str(&header.kind);
            meta.sender = Sender::from_str(&header.sender);
            meta.via = header.via.clone();
            // Native question answers need the earlier question's state, which sync already
            // resolved. A raw selected answer alone cannot reconstruct that association.
            let body = if header.via == "question_reply" {
                String::from_utf8(projected)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
            } else {
                core::select_body(&fields, &meta, 0).body
            };
            let values = format::body_fields(&body);
            covered = record_end;
            records += 1;
            if values.is_empty() || header.kind == "unknown" {
                continue;
            }
            shown += 1;
            writeln!(
                text,
                "\n[{}] {} ({}, via: {})",
                header.timestamp,
                header.sender,
                header.kind,
                if header.via.is_empty() {
                    "unspecified"
                } else {
                    &header.via
                }
            )
            .unwrap();
            writeln!(
                text,
                "# source_ref: {source_id} @{}+{}",
                header.byte_start, header.byte_len
            )
            .unwrap();
            if meta.sender == Sender::Human {
                let (message, images) = core::human_text_from_body(&body);
                writeln!(text, "{message}").unwrap();
                if images > 0 {
                    writeln!(text, "[{images} images]").unwrap();
                }
            } else {
                for (path, value) in values {
                    writeln!(text, "{path}:\n{value}").unwrap();
                }
            }
        }
    }
    if records == 0 || covered != end {
        return Err(incomplete_range());
    }
    writeln!(text, "\n# {shown} readable records from {records} original records; reasoning and non-message metadata omitted").unwrap();
    Ok(text)
}

fn incomplete_range() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput,
        "--format text needs complete indexed records; use a source_ref's byte range, or --format json for arbitrary bytes")
}
