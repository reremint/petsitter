//! Incremental reading of append-only JSONL transcripts.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;

use serde_json::Value;

/// How much of an existing file is read the first time it is opened.
///
/// Transcripts can weigh many megabytes; the latest records are all that
/// matter for a live view, and agents repeat metadata records regularly.
pub const INITIAL_WINDOW: u64 = 512 * 1024;

/// Follows a JSONL file, returning only records appended since the last read.
#[derive(Debug)]
pub struct JsonlTail {
    path: PathBuf,
    offset: u64,
    /// Bytes of an incomplete trailing line, kept until it is terminated.
    pending: Vec<u8>,
    opened: bool,
}

impl JsonlTail {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            offset: 0,
            pending: Vec::new(),
            opened: false,
        }
    }

    /// Whether the file was read at least once.
    pub fn is_open(&self) -> bool {
        self.opened
    }

    /// Reads and parses the records appended since the previous call.
    ///
    /// Lines that are not valid JSON are skipped. If the file shrank (rotated
    /// or rewritten), reading restarts from the beginning.
    pub fn read_new(&mut self) -> io::Result<Vec<Value>> {
        let mut file = File::open(&self.path)?;
        let len = file.metadata()?.len();

        let mut skip_partial_line = false;
        if !self.opened {
            self.opened = true;
            if len > INITIAL_WINDOW {
                self.offset = len - INITIAL_WINDOW;
                skip_partial_line = true;
            }
        } else if len < self.offset {
            self.offset = 0;
            self.pending.clear();
        }
        if len == self.offset {
            return Ok(Vec::new());
        }

        file.seek(SeekFrom::Start(self.offset))?;
        let mut buf = std::mem::take(&mut self.pending);
        let start = buf.len();
        file.take(len - self.offset).read_to_end(&mut buf)?;
        self.offset += (buf.len() - start) as u64;

        let mut body: &[u8] = &buf;
        if skip_partial_line {
            body = body
                .iter()
                .position(|&b| b == b'\n')
                .map_or(&[][..], |i| &body[i + 1..]);
        }

        let complete = body.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        let records = parse_lines(&body[..complete]);
        self.pending = body[complete..].to_vec();
        Ok(records)
    }
}

fn parse_lines(bytes: &[u8]) -> Vec<Value> {
    bytes
        .split(|&b| b == b'\n')
        .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
        .filter_map(|line| serde_json::from_slice(line).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_only_appended_complete_lines() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"n":1}}"#).unwrap();
        write!(file, r#"{{"n":"#).unwrap();
        file.flush().unwrap();

        let mut tail = JsonlTail::new(file.path());
        let first = tail.read_new().unwrap();
        assert_eq!(first.len(), 1);

        writeln!(file, "2}}").unwrap();
        writeln!(file, "not json").unwrap();
        file.flush().unwrap();
        let second = tail.read_new().unwrap();
        assert_eq!(second, vec![serde_json::json!({"n": 2})]);
        assert_eq!(tail.read_new().unwrap(), Vec::<serde_json::Value>::new());
    }

    #[test]
    fn large_files_start_near_the_end() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let filler = format!(r#"{{"pad":"{}"}}"#, "x".repeat(1000));
        for _ in 0..1000 {
            writeln!(file, "{filler}").unwrap();
        }
        writeln!(file, r#"{{"last":true}}"#).unwrap();
        file.flush().unwrap();

        let records = JsonlTail::new(file.path()).read_new().unwrap();
        assert!(records.len() < 1000);
        assert_eq!(records.last(), Some(&serde_json::json!({"last": true})));
    }
}
