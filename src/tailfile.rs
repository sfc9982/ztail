//! One followed file: offset, decoder, and incomplete-line buffer.
//!
//! `-f` keeps this file descriptor. Appended bytes are read from the current
//! offset and handed to the decoder; the offset only moves forward.
//!
//! `-F` also stats the path. A new device/inode means the name was rotated
//! onto a different file, so the descriptor is reopened and the decoder reset.
//! A shrink of the open file (copytruncate, or `O_TRUNC`) does the same rewind
//! on both `-f` and `-F`: the bytes we already decoded are gone.

use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::decode::Decoder;
use crate::lines::{retain_last, LineBuffer};

const CHUNK: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// Open file shrank behind our offset.
    Truncated,
    /// Path now names a different inode, or came back after disappearing.
    Replaced,
    /// `-F` and the path did not exist when we started.
    Appeared,
    /// `-F` and the path was removed.
    Inaccessible,
}

impl Notice {
    pub fn message(self) -> &'static str {
        match self {
            Notice::Truncated => "file truncated",
            Notice::Replaced => "file replaced; following the new file",
            Notice::Appeared => "has appeared; following",
            Notice::Inaccessible => "has become inaccessible",
        }
    }
}

#[derive(Debug)]
pub struct Update {
    pub lines: Vec<Vec<u8>>,
    pub notice: Option<Notice>,
}

#[derive(Debug)]
pub enum TailError {
    Io { path: PathBuf, source: io::Error },
    Format { path: PathBuf, message: String },
}

impl std::fmt::Display for TailError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TailError::Io { path, source } => write!(f, "{}: {source}", path.display()),
            TailError::Format { path, message } => write!(f, "{}: {message}", path.display()),
        }
    }
}

impl std::error::Error for TailError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TailError::Io { source, .. } => Some(source),
            TailError::Format { .. } => None,
        }
    }
}

impl TailError {
    pub fn kind(&self) -> Option<io::ErrorKind> {
        match self {
            TailError::Io { source, .. } => Some(source.kind()),
            TailError::Format { .. } => None,
        }
    }
}

pub struct TailFile {
    path: PathBuf,
    file: Option<File>,
    offset: u64,
    dev: u64,
    ino: u64,
    decoder: Decoder,
    lines: LineBuffer,
    /// Last-N used for the initial view, including a file that appears later.
    n: usize,
    follow_name: bool,
    did_initial: bool,
    missing_announced: bool,
}

impl TailFile {
    pub fn open(path: &Path, follow_name: bool) -> Result<Self, TailError> {
        let file = File::open(path).map_err(|source| TailError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let meta = file.metadata().map_err(|source| TailError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if meta.is_dir() {
            return Err(TailError::Format {
                path: path.to_path_buf(),
                message: "is a directory".into(),
            });
        }
        let (dev, ino) = file_id(&meta);
        Ok(Self {
            path: path.to_path_buf(),
            file: Some(file),
            offset: 0,
            dev,
            ino,
            decoder: Decoder::new(),
            lines: LineBuffer::new(),
            n: 10,
            follow_name,
            did_initial: false,
            missing_announced: false,
        })
    }

    /// `-F` placeholder for a path that does not exist yet.
    pub fn pending(path: &Path, n: usize, follow_name: bool) -> Self {
        Self {
            path: path.to_path_buf(),
            file: None,
            offset: 0,
            dev: 0,
            ino: 0,
            decoder: Decoder::new(),
            lines: LineBuffer::new(),
            n,
            follow_name,
            did_initial: false,
            missing_announced: true,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Compressed (or plain) bytes passed into the decoder since it was created.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn bytes_fed(&self) -> u64 {
        self.decoder.bytes_fed()
    }

    /// Bytes passed to the raw inflater / zstd decoder, excluding gzip framing.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn inflate_bytes(&self) -> u64 {
        self.decoder.inflate_bytes()
    }

    /// Single forward pass. Keeps the last `n` complete lines.
    ///
    /// `flush` is set when we are about to exit: a leftover sniff prefix is
    /// treated as text. While following, a short prefix might still grow into
    /// a gzip or zstd header, so it is left buffered.
    pub fn read_initial(&mut self, n: usize, flush: bool) -> Result<Vec<Vec<u8>>, TailError> {
        self.n = n;
        let lines = self.consume(Some(n), flush)?;
        self.did_initial = true;
        Ok(lines)
    }

    /// Read whatever was appended, and apply rotation / truncation policy.
    pub fn poll(&mut self) -> Result<Update, TailError> {
        if self.follow_name {
            if let Some(notice) = self.sync_name()? {
                let lines = match notice {
                    // First time we see the file: same initial view as startup.
                    Notice::Appeared => {
                        let n = self.n;
                        let lines = self.consume(Some(n), false)?;
                        self.did_initial = true;
                        lines
                    }
                    Notice::Inaccessible => Vec::new(),
                    // New inode: every complete line currently in the file is
                    // data we have not printed. Do not cap it at N.
                    Notice::Replaced | Notice::Truncated => self.consume(None, false)?,
                };
                return Ok(Update {
                    lines,
                    notice: Some(notice),
                });
            }
        }

        if self.file.is_some() && self.truncated()? {
            self.rewind()?;
            let lines = self.consume(None, false)?;
            return Ok(Update {
                lines,
                notice: Some(Notice::Truncated),
            });
        }

        let lines = self.consume(None, false)?;
        Ok(Update {
            lines,
            notice: None,
        })
    }

    fn sync_name(&mut self) -> Result<Option<Notice>, TailError> {
        match std::fs::metadata(&self.path) {
            Ok(meta) => {
                if meta.is_dir() {
                    return Err(TailError::Format {
                        path: self.path.clone(),
                        message: "is a directory".into(),
                    });
                }
                let (dev, ino) = file_id(&meta);
                let changed = match &self.file {
                    None => true,
                    Some(_) => dev != self.dev || ino != self.ino,
                };
                if !changed {
                    return Ok(None);
                }
                let notice = if self.file.is_none() && !self.did_initial {
                    Notice::Appeared
                } else {
                    Notice::Replaced
                };
                self.reopen_with(meta)?;
                Ok(Some(notice))
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                if self.file.is_some() && !self.missing_announced {
                    self.file = None;
                    self.offset = 0;
                    self.decoder.reset();
                    self.lines.clear();
                    self.missing_announced = true;
                    return Ok(Some(Notice::Inaccessible));
                }
                Ok(None)
            }
            Err(source) => Err(TailError::Io {
                path: self.path.clone(),
                source,
            }),
        }
    }

    fn reopen_with(&mut self, meta: Metadata) -> Result<(), TailError> {
        let file = File::open(&self.path).map_err(|source| TailError::Io {
            path: self.path.clone(),
            source,
        })?;
        let (dev, ino) = file_id(&meta);
        self.file = Some(file);
        self.dev = dev;
        self.ino = ino;
        self.offset = 0;
        self.decoder.reset();
        self.lines.clear();
        self.missing_announced = false;
        Ok(())
    }

    fn truncated(&self) -> Result<bool, TailError> {
        let Some(file) = &self.file else {
            return Ok(false);
        };
        let len = file
            .metadata()
            .map_err(|source| TailError::Io {
                path: self.path.clone(),
                source,
            })?
            .len();
        Ok(len < self.offset)
    }

    fn rewind(&mut self) -> Result<(), TailError> {
        let file = self.file.as_mut().expect("rewind requires an open file");
        file.seek(SeekFrom::Start(0))
            .map_err(|source| TailError::Io {
                path: self.path.clone(),
                source,
            })?;
        self.offset = 0;
        self.decoder.reset();
        self.lines.clear();
        Ok(())
    }

    /// `keep_last = Some(n)` retains only the last n complete lines.
    /// `keep_last = None` returns every complete line produced by this call.
    fn consume(
        &mut self,
        keep_last: Option<usize>,
        flush: bool,
    ) -> Result<Vec<Vec<u8>>, TailError> {
        let mut ring = std::collections::VecDeque::new();
        let mut all = Vec::new();
        let mut decompressed = Vec::new();
        loop {
            let chunk = self.read_chunk()?;
            if chunk.is_empty() {
                if flush {
                    decompressed.clear();
                    self.decoder.finish(&mut decompressed).map_err(|message| {
                        TailError::Format {
                            path: self.path.clone(),
                            message,
                        }
                    })?;
                    self.take_lines(&decompressed, keep_last, &mut ring, &mut all);
                }
                break;
            }
            decompressed.clear();
            self.decoder
                .feed(&chunk, &mut decompressed)
                .map_err(|message| TailError::Format {
                    path: self.path.clone(),
                    message,
                })?;
            self.take_lines(&decompressed, keep_last, &mut ring, &mut all);
        }
        Ok(match keep_last {
            Some(_) => ring.into_iter().collect(),
            None => all,
        })
    }

    fn take_lines(
        &mut self,
        data: &[u8],
        keep_last: Option<usize>,
        ring: &mut std::collections::VecDeque<Vec<u8>>,
        all: &mut Vec<Vec<u8>>,
    ) {
        if data.is_empty() {
            return;
        }
        for line in self.lines.push(data) {
            match keep_last {
                Some(n) => retain_last(ring, n, line),
                None => all.push(line),
            }
        }
    }

    fn read_chunk(&mut self) -> Result<Vec<u8>, TailError> {
        let Some(file) = self.file.as_mut() else {
            return Ok(Vec::new());
        };
        let len = file
            .metadata()
            .map_err(|source| TailError::Io {
                path: self.path.clone(),
                source,
            })?
            .len();
        if len < self.offset {
            // Truncation is handled by the caller before the next consume.
            // Stopping here avoids feeding a torn read into the inflater.
            return Ok(Vec::new());
        }
        let available = (len - self.offset) as usize;
        if available == 0 {
            return Ok(Vec::new());
        }
        let want = available.min(CHUNK);
        let mut buf = vec![0u8; want];
        let mut filled = 0;
        while filled < want {
            match file.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(source) if source.kind() == io::ErrorKind::Interrupted => continue,
                Err(source) => {
                    return Err(TailError::Io {
                        path: self.path.clone(),
                        source,
                    })
                }
            }
        }
        buf.truncate(filled);
        self.offset += filled as u64;
        Ok(buf)
    }
}

fn file_id(meta: &Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    fn gz(data: &[u8]) -> Vec<u8> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
    }

    fn append(path: &Path, bytes: &[u8]) {
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(bytes).unwrap();
    }

    fn text(lines: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for i in 0..lines {
            writeln!(
                out,
                "log line {i:07} the quick brown fox jumps over the lazy dog"
            )
            .unwrap();
        }
        out
    }

    #[test]
    fn plain_last_n_fewer_than_n_and_no_short_file_exception() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain.log");

        write(&path, b"a\nb\nc\n");
        let mut tail = TailFile::open(&path, false).unwrap();
        let lines = tail.read_initial(10, true).unwrap();
        assert_eq!(
            lines,
            vec![b"a\n".to_vec(), b"b\n".to_vec(), b"c\n".to_vec()]
        );

        // The Crystal version printed every line when the file had fewer than
        // 20. Fifteen lines must yield the last 10, not all 15.
        let mut body = Vec::new();
        for i in 0..15 {
            writeln!(body, "L{i}").unwrap();
        }
        write(&path, &body);
        let mut tail = TailFile::open(&path, false).unwrap();
        let lines = tail.read_initial(10, true).unwrap();
        let text: Vec<_> = lines
            .iter()
            .map(|l| String::from_utf8(l.clone()).unwrap())
            .collect();
        assert_eq!(text, (5..15).map(|i| format!("L{i}\n")).collect::<Vec<_>>());
    }

    #[test]
    fn plain_partial_line_completes_on_append() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("partial.log");
        write(&path, b"hello");
        let mut tail = TailFile::open(&path, false).unwrap();
        let lines = tail.read_initial(10, false).unwrap();
        assert!(lines.is_empty(), "partial line must stay buffered");
        append(&path, b" world\nmore");
        let update = tail.poll().unwrap();
        assert_eq!(update.lines, vec![b"hello world\n".to_vec()]);
        assert!(update.notice.is_none());
        append(&path, b" stuff\n");
        let update = tail.poll().unwrap();
        assert_eq!(update.lines, vec![b"more stuff\n".to_vec()]);
    }

    #[test]
    fn single_member_gzip_last_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("one.gz");
        let mut raw = Vec::new();
        for i in 0..30 {
            writeln!(raw, "g{i}").unwrap();
        }
        write(&path, &gz(&raw));
        let mut tail = TailFile::open(&path, false).unwrap();
        let lines = tail.read_initial(10, true).unwrap();
        let text: Vec<_> = lines
            .iter()
            .map(|l| String::from_utf8(l.clone()).unwrap())
            .collect();
        assert_eq!(
            text,
            (20..30).map(|i| format!("g{i}\n")).collect::<Vec<_>>()
        );
        assert_eq!(tail.bytes_fed(), std::fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn multi_member_append_feeds_only_new_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi.gz");
        let first = gz(b"a\nb\n");
        write(&path, &first);
        let mut tail = TailFile::open(&path, false).unwrap();
        let lines = tail.read_initial(10, false).unwrap();
        assert_eq!(lines, vec![b"a\n".to_vec(), b"b\n".to_vec()]);
        let fed = tail.bytes_fed();
        let inflated = tail.inflate_bytes();
        assert_eq!(fed, first.len() as u64);

        // A partial line split across two gzip members (`gzip >> file`).
        let second = gz(b"c");
        let third = gz(b"d\ne\n");
        append(&path, &second);
        let update = tail.poll().unwrap();
        assert!(update.lines.is_empty());
        append(&path, &third);
        let update = tail.poll().unwrap();
        assert_eq!(update.lines, vec![b"cd\n".to_vec(), b"e\n".to_vec()]);

        let added = second.len() + third.len();
        assert_eq!(
            tail.bytes_fed() - fed,
            added as u64,
            "second poll re-fed the original member"
        );
        let inflate_delta = tail.inflate_bytes() - inflated;
        assert!(inflate_delta > 0);
        assert!(
            inflate_delta < added as u64,
            "framing should not be counted as inflate input"
        );
    }

    #[test]
    fn large_file_is_not_redecompressed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.gz");
        let raw = text(20_000);
        let compressed = gz(&raw);
        write(&path, &compressed);
        let compressed_len = compressed.len() as u64;

        let mut tail = TailFile::open(&path, false).unwrap();
        let lines = tail.read_initial(10, true).unwrap();
        assert_eq!(lines.len(), 10);
        let last = String::from_utf8(lines[9].clone()).unwrap();
        assert_eq!(
            last,
            "log line 0019999 the quick brown fox jumps over the lazy dog\n"
        );
        assert_eq!(tail.bytes_fed(), compressed_len);
        let inflated = tail.inflate_bytes();
        assert!(inflated + 18 <= compressed_len);
        assert!(
            inflated > 10_000,
            "expected a real deflate payload, got {inflated}"
        );

        let extra = gz(b"after\n");
        append(&path, &extra);
        let update = tail.poll().unwrap();
        assert_eq!(update.lines, vec![b"after\n".to_vec()]);
        assert_eq!(tail.bytes_fed(), compressed_len + extra.len() as u64);
        let delta = tail.inflate_bytes() - inflated;
        assert!(delta > 0 && delta < extra.len() as u64);
        assert!(
            delta * 20 < inflated,
            "append inflated {delta} bytes, original was {inflated}; file was re-read"
        );
    }

    #[test]
    fn truncation_rewinds_decoder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trunc.gz");
        let old = gz(b"old-1\nold-2\n");
        write(&path, &old);
        let mut tail = TailFile::open(&path, false).unwrap();
        let lines = tail.read_initial(10, false).unwrap();
        assert_eq!(lines.len(), 2);
        let fed = tail.bytes_fed();

        let new = gz(b"new-1\n");
        // Same inode: truncate in place, as logrotate copytruncate does.
        std::fs::write(&path, &new).unwrap();
        let update = tail.poll().unwrap();
        assert_eq!(update.notice, Some(Notice::Truncated));
        assert_eq!(update.lines, vec![b"new-1\n".to_vec()]);
        assert_eq!(tail.bytes_fed(), fed + new.len() as u64);
    }

    #[test]
    fn follow_name_detects_rotation_follow_fd_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rot.log");
        write(&path, b"old\n");

        let mut by_name = TailFile::open(&path, true).unwrap();
        assert_eq!(
            by_name.read_initial(10, false).unwrap(),
            vec![b"old\n".to_vec()]
        );
        let mut by_fd = TailFile::open(&path, false).unwrap();
        assert_eq!(
            by_fd.read_initial(10, false).unwrap(),
            vec![b"old\n".to_vec()]
        );

        let rotated = dir.path().join("rot.log.1");
        std::fs::rename(&path, &rotated).unwrap();
        write(&path, b"new-a\nnew-b\n");

        let update = by_fd.poll().unwrap();
        assert!(update.lines.is_empty());
        assert!(update.notice.is_none());

        let update = by_name.poll().unwrap();
        assert_eq!(update.notice, Some(Notice::Replaced));
        assert_eq!(update.lines, vec![b"new-a\n".to_vec(), b"new-b\n".to_vec()]);

        // Further appends to the new file are incremental.
        let fed = by_name.bytes_fed();
        append(&path, b"new-c\n");
        let update = by_name.poll().unwrap();
        assert_eq!(update.lines, vec![b"new-c\n".to_vec()]);
        assert_eq!(by_name.bytes_fed(), fed + 6);
    }

    #[test]
    fn follow_name_waits_for_appearance() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("later.log");
        let mut tail = TailFile::pending(&path, 10, true);
        let update = tail.poll().unwrap();
        assert!(update.lines.is_empty());
        assert!(update.notice.is_none());

        write(&path, b"one\ntwo\nthree\n");
        let update = tail.poll().unwrap();
        assert_eq!(update.notice, Some(Notice::Appeared));
        assert_eq!(update.lines.len(), 3);
    }
}
