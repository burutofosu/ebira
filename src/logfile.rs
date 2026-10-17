//! The bytes of a log: a JSONL file, or the JSONL a Zstandard-compressed log (`.jsonl.zst`)
//! holds. Offsets are offsets in the JSONL either way, so a record keeps its source reference
//! when its log is compressed.
//!
//! A compressed log is read from its start: reaching a later offset decompresses everything
//! before it, and going back starts over.

use crate::zstd;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// Whether a file is a log: `.jsonl`, or `.jsonl.zst`.
pub fn is_log(path: &Path) -> bool {
    let compressed = is_compressed(path);
    let name = if compressed {
        path.file_stem().map(Path::new)
    } else {
        Some(path)
    };
    name.and_then(Path::extension)
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
}

/// Whether a log is compressed with Zstandard.
pub fn is_compressed(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zst"))
}

/// The path a compressed log had before it was compressed (`x.jsonl` for `x.jsonl.zst`); any
/// other path as it is.
pub fn jsonl_path(path: &str) -> &str {
    if is_compressed(Path::new(path)) {
        &path[..path.len() - ".zst".len()]
    } else {
        path
    }
}

/// A log's file name without its extensions: `x` for `x.jsonl` and for `x.jsonl.zst`.
pub fn stem(path: &Path) -> Option<&str> {
    let name = if is_compressed(path) {
        Path::new(path.file_stem()?)
    } else {
        path
    };
    name.file_stem()?.to_str()
}

/// The size a compressed log's first frame declares for its content, read from the frame's
/// header alone.
pub fn declared_size(path: &Path) -> Option<u64> {
    zstd::declared_content_size(&mut File::open(path).ok()?)
        .ok()
        .flatten()
}

/// A log opened for reading its JSONL.
pub struct LogFile {
    source: Source,
    /// The offset in the JSONL of the next byte read.
    position: u64,
}

enum Source {
    Plain(File),
    Compressed(Box<zstd::Decoder<File>>),
}

impl LogFile {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let source = if is_compressed(path) {
            Source::Compressed(Box::new(zstd::Decoder::new(file)))
        } else {
            Source::Plain(file)
        };
        Ok(Self {
            source,
            position: 0,
        })
    }

    /// Opens a log to read its JSONL from `offset`.
    pub fn open_at(path: &Path, offset: u64) -> io::Result<Self> {
        let mut log = Self::open(path)?;
        log.seek(SeekFrom::Start(offset))?;
        Ok(log)
    }

    /// The metadata of the file as it is stored.
    pub fn metadata(&self) -> io::Result<fs::Metadata> {
        match &self.source {
            Source::Plain(file) => file.metadata(),
            Source::Compressed(decoder) => decoder.get_ref().metadata(),
        }
    }
}

impl Read for LogFile {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = match &mut self.source {
            Source::Plain(file) => file.read(buffer)?,
            Source::Compressed(decoder) => decoder.read(buffer)?,
        };
        self.position += read as u64;
        Ok(read)
    }
}

impl Seek for LogFile {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let decoder = match &mut self.source {
            Source::Plain(file) => {
                self.position = file.seek(to)?;
                return Ok(self.position);
            }
            Source::Compressed(decoder) => decoder,
        };
        let target = match to {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
            SeekFrom::End(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "a compressed log is not read from its end",
                ))
            }
        }
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek to a negative or overflowing position",
            )
        })?;
        if target < self.position {
            decoder.rewind()?;
            self.position = 0;
        }
        // As with a file, a position past the end is allowed, and reads nothing.
        self.position += decoder.skip(target - self.position)?;
        Ok(target)
    }
}

#[cfg(test)]
mod tests {
    use super::{declared_size, is_compressed, is_log, jsonl_path, stem, LogFile};
    use std::io::{Read, Seek, SeekFrom};
    use std::path::{Path, PathBuf};

    const TEXT_3: &[u8] = include_bytes!("../fixtures/zstd/text-3.zst");

    fn plain_text() -> Vec<u8> {
        let mut text = Vec::new();
        crate::zstd::Decoder::new(TEXT_3)
            .read_to_end(&mut text)
            .expect("fixture");
        text
    }

    /// The fixture's text as a log, plain and compressed, in a directory of its own.
    fn logs(name: &str) -> (PathBuf, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("ebira-logfile-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create root");
        let plain = root.join("rollout.jsonl");
        let compressed = root.join("rollout-copy.jsonl.zst");
        std::fs::write(&plain, plain_text()).expect("write plain log");
        std::fs::write(&compressed, TEXT_3).expect("write compressed log");
        (plain, compressed)
    }

    #[test]
    fn names_of_logs() {
        for (path, log, compressed) in [
            ("a/rollout-1.jsonl", true, false),
            ("a/rollout-1.JSONL", true, false),
            ("a/rollout-1.jsonl.zst", true, true),
            ("a/rollout-1.jsonl.ZST", true, true),
            ("a/rollout-1.zst", false, true),
            ("a/notes.txt", false, false),
        ] {
            assert_eq!(is_log(Path::new(path)), log, "{path}");
            assert_eq!(is_compressed(Path::new(path)), compressed, "{path}");
        }
        assert_eq!(jsonl_path("/x/rollout-1.jsonl.zst"), "/x/rollout-1.jsonl");
        assert_eq!(jsonl_path("/x/rollout-1.jsonl"), "/x/rollout-1.jsonl");
        assert_eq!(stem(Path::new("/x/rollout-1.jsonl.zst")), Some("rollout-1"));
        assert_eq!(stem(Path::new("/x/rollout-1.jsonl")), Some("rollout-1"));
    }

    #[test]
    fn a_compressed_log_reads_as_its_jsonl_at_any_offset() {
        let (plain, compressed) = logs("offsets");
        let text = std::fs::read(&plain).expect("read plain");
        let mut log = LogFile::open(&compressed).expect("open compressed");
        // Forward, back to an earlier offset, and forward again.
        for (start, len) in [
            (0, 64),
            (150_000, 300),
            (10, 5),
            (301_600, 26),
            (299_000, 1_000),
        ] {
            log.seek(SeekFrom::Start(start)).expect("seek");
            let mut bytes = vec![0u8; len];
            log.read_exact(&mut bytes).expect("read");
            assert_eq!(
                bytes,
                text[start as usize..start as usize + len],
                "at {start}"
            );
        }
        log.seek(SeekFrom::Start(1_000)).expect("seek");
        log.seek(SeekFrom::Current(-500))
            .expect("seek back from here");
        let mut bytes = [0u8; 10];
        log.read_exact(&mut bytes).expect("read");
        assert_eq!(bytes, text[500..510]);
        assert!(log.seek(SeekFrom::End(0)).is_err());
        assert_eq!(
            log.metadata().expect("metadata").len(),
            TEXT_3.len() as u64,
            "the metadata is the stored file's"
        );
        let mut tail = Vec::new();
        LogFile::open_at(&compressed, 301_000)
            .expect("open at")
            .read_to_end(&mut tail)
            .expect("read tail");
        assert_eq!(tail, text[301_000..]);
        assert_eq!(declared_size(&compressed), Some(text.len() as u64));
        assert_eq!(declared_size(&plain), None);
        std::fs::remove_dir_all(plain.parent().expect("root")).expect("remove root");
    }
}
