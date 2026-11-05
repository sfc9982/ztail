//! One followed file: offset, decoder, and incomplete-line buffer.
//!
//! `-f` keeps this file descriptor. Appended bytes are read from the current
//! offset and handed to the decoder; the offset only moves forward.
//!
//! `-F` also stats the path. A new device/inode means the name was rotated
//! onto a different file, so the descriptor is reopened and the decoder reset.
//! A shrink of the open file (copytruncate, or `O_TRUNC`) does the same rewind
//! on both `-f` and `-F`: the bytes we already decoded are gone.
//!
//! Truncate-and-regrow that finishes at or past the saved offset between polls
//! is detected with a short content fingerprint of the bytes just behind the
//! cursor. Each consume snapshots the end offset (and a byte budget) so a
//! writer that outpaces the reader cannot starve other files or Ctrl+C.

use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::decode::Decoder;
use crate::lines::{retain_last, LineBuffer};

const CHUNK: usize = 256 * 1024;
/// Cap how much one poll/consume will read so follow stays responsive.
const MAX_BYTES_PER_CONSUME: u64 = 8 * 1024 * 1024;
/// Bytes kept just behind the cursor to detect truncate-and-regrow.
const FINGERPRINT: usize = 64;

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
    /// Format error after some lines were already decoded. Callers should
    /// print `lines` and then treat the file as failed.
    pub error: Option<TailError>,
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

struct NameChange {
    notice: Notice,
    drained: Vec<Vec<u8>>,
}

pub struct TailFile {
    path: PathBuf,
    file: Option<File>,
    offset: u64,
    /// Last observed size of the open file; used with the fingerprint.
    last_size: u64,
    /// Size and mtime observed when `fingerprint` was last known valid.
    /// Idle polls with the same pair skip the fingerprint seek/read.
    fp_len: u64,
    fp_mtime: Option<SystemTime>,
    /// File offset when `fingerprint` was read. Unchanged offset skips a refresh.
    fp_offset: u64,
    /// Content just behind `offset`, used to catch truncate-and-regrow.
    fingerprint: Vec<u8>,
    /// Reused across `read_chunk` calls.
    read_buf: Vec<u8>,
    /// Length and mtime from the current consume snapshot.
    snap_len: u64,
    snap_mtime: Option<SystemTime>,
    dev: u64,
    ino: u64,
    decoder: Decoder,
    lines: LineBuffer,
    /// Last-N used for the initial view, including a file that appears later.
    n: usize,
    follow_name: bool,
    did_initial: bool,
    missing_announced: bool,
    /// Upper bound for the current `consume` call; `None` means use live size.
    read_until: Option<u64>,
}

impl TailFile {
    pub fn open(path: &Path, follow_name: bool) -> Result<Self, TailError> {
        // Stat first so a FIFO without a writer cannot hang `open`.
        // Stat first so a FIFO without a writer cannot hang `File::open`.
        let meta = std::fs::metadata(path).map_err(|source| TailError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        require_regular(path, &meta)?;
        let file = File::open(path).map_err(|source| TailError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let meta = file.metadata().map_err(|source| TailError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        require_regular(path, &meta)?;
        let (dev, ino) = file_id(&meta);
        Ok(Self {
            path: path.to_path_buf(),
            file: Some(file),
            offset: 0,
            last_size: meta.len(),
            fp_len: 0,
            fp_mtime: None,
            fp_offset: 0,
            fingerprint: Vec::new(),
            read_buf: Vec::new(),
            snap_len: meta.len(),
            snap_mtime: meta.modified().ok(),
            dev,
            ino,
            decoder: Decoder::new(),
            lines: LineBuffer::new(),
            n: 10,
            follow_name,
            did_initial: false,
            missing_announced: false,
            read_until: None,
        })
    }

    /// `-F` placeholder for a path that does not exist yet.
    pub fn pending(path: &Path, n: usize, follow_name: bool) -> Self {
        Self {
            path: path.to_path_buf(),
            file: None,
            offset: 0,
            last_size: 0,
            fp_len: 0,
            fp_mtime: None,
            fp_offset: 0,
            fingerprint: Vec::new(),
            read_buf: Vec::new(),
            snap_len: 0,
            snap_mtime: None,
            dev: 0,
            ino: 0,
            decoder: Decoder::new(),
            lines: LineBuffer::new(),
            n,
            follow_name,
            did_initial: false,
            missing_announced: true,
            read_until: None,
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
    /// treated as text, and an incomplete compressed member is an error.
    /// While following, a short prefix might still grow into a gzip or zstd
    /// header, so it is left buffered.
    pub fn read_initial(&mut self, n: usize, flush: bool) -> Result<Vec<Vec<u8>>, TailError> {
        self.n = n;
        let (lines, error) = self.consume(Some(n), flush)?;
        self.did_initial = true;
        if let Some(err) = error {
            return Err(err);
        }
        Ok(lines)
    }

    /// Read whatever was appended, and apply rotation / truncation policy.
    pub fn poll(&mut self) -> Result<Update, TailError> {
        if self.follow_name {
            if let Some(change) = self.sync_name()? {
                let (mut lines, error) = match change.notice {
                    Notice::Appeared => {
                        let n = self.n;
                        let result = self.consume(Some(n), false)?;
                        self.did_initial = true;
                        result
                    }
                    Notice::Inaccessible => (Vec::new(), None),
                    Notice::Replaced | Notice::Truncated => self.consume(None, false)?,
                };
                if !change.drained.is_empty() {
                    let mut combined = change.drained;
                    combined.append(&mut lines);
                    lines = combined;
                }
                return Ok(Update {
                    lines,
                    notice: Some(change.notice),
                    error,
                });
            }
        }

        if self.file.is_some() && self.truncated()? {
            self.rewind()?;
            let (lines, error) = self.consume(None, false)?;
            return Ok(Update {
                lines,
                notice: Some(Notice::Truncated),
                error,
            });
        }

        let (lines, error) = self.consume(None, false)?;
        Ok(Update {
            lines,
            notice: None,
            error,
        })
    }

    /// Returns `(notice, lines drained from the old descriptor)`.
    fn sync_name(&mut self) -> Result<Option<NameChange>, TailError> {
        match std::fs::metadata(&self.path) {
            Ok(meta) => {
                require_regular(&self.path, &meta)?;
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
                // Drain unread bytes from the old inode before replacing it.
                let drained = if self.file.is_some() {
                    let (lines, error) = self.consume(None, false)?;
                    if let Some(err) = error {
                        return Err(err);
                    }
                    lines
                } else {
                    Vec::new()
                };
                self.reopen()?;
                Ok(Some(NameChange { notice, drained }))
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                if self.file.is_some() && !self.missing_announced {
                    let (drained, error) = self.consume(None, false)?;
                    if let Some(err) = error {
                        return Err(err);
                    }
                    self.file = None;
                    self.offset = 0;
                    self.last_size = 0;
                    self.fingerprint.clear();
                    self.decoder.reset();
                    self.lines.clear();
                    self.missing_announced = true;
                    return Ok(Some(NameChange {
                        notice: Notice::Inaccessible,
                        drained,
                    }));
                }
                Ok(None)
            }
            Err(source) => Err(TailError::Io {
                path: self.path.clone(),
                source,
            }),
        }
    }

    fn reopen(&mut self) -> Result<(), TailError> {
        let file = File::open(&self.path).map_err(|source| TailError::Io {
            path: self.path.clone(),
            source,
        })?;
        // Identity must come from the descriptor we opened, not an earlier
        // path lookup that can race with another rotation.
        let meta = file.metadata().map_err(|source| TailError::Io {
            path: self.path.clone(),
            source,
        })?;
        require_regular(&self.path, &meta)?;
        let (dev, ino) = file_id(&meta);
        self.file = Some(file);
        self.dev = dev;
        self.ino = ino;
        self.offset = 0;
        self.last_size = meta.len();
        self.fp_len = 0;
        self.fp_mtime = None;
        self.fp_offset = 0;
        self.fingerprint.clear();
        self.decoder.reset();
        self.lines.clear();
        self.missing_announced = false;
        Ok(())
    }

    fn truncated(&mut self) -> Result<bool, TailError> {
        let Some(file) = &mut self.file else {
            return Ok(false);
        };
        let meta = file.metadata().map_err(|source| TailError::Io {
            path: self.path.clone(),
            source,
        })?;
        let len = meta.len();
        let mtime = meta.modified().ok();
        if len < self.offset || len < self.last_size {
            self.last_size = len;
            return Ok(true);
        }
        let unchanged = mtime.is_some() && len == self.fp_len && mtime == self.fp_mtime;
        if !unchanged && self.offset > 0 && !self.fingerprint.is_empty() && len >= self.offset {
            let fp_len = self.fingerprint.len() as u64;
            let start = self.offset - fp_len;
            let mut buf = vec![0u8; self.fingerprint.len()];
            file.seek(SeekFrom::Start(start))
                .map_err(|source| TailError::Io {
                    path: self.path.clone(),
                    source,
                })?;
            file.read_exact(&mut buf).map_err(|source| TailError::Io {
                path: self.path.clone(),
                source,
            })?;
            file.seek(SeekFrom::Start(self.offset))
                .map_err(|source| TailError::Io {
                    path: self.path.clone(),
                    source,
                })?;
            if buf != self.fingerprint {
                self.last_size = len;
                return Ok(true);
            }
        }
        self.last_size = len;
        Ok(false)
    }

    fn rewind(&mut self) -> Result<(), TailError> {
        let file = self.file.as_mut().expect("rewind requires an open file");
        file.seek(SeekFrom::Start(0))
            .map_err(|source| TailError::Io {
                path: self.path.clone(),
                source,
            })?;
        self.offset = 0;
        self.fp_offset = 0;
        self.fp_len = 0;
        self.fp_mtime = None;
        self.fingerprint.clear();
        self.decoder.reset();
        self.lines.clear();
        Ok(())
    }

    /// `keep_last = Some(n)` retains only the last n complete lines.
    /// `keep_last = None` returns every complete line produced by this call.
    ///
    /// Format errors return the lines decoded so far alongside the error.
    fn consume(
        &mut self,
        keep_last: Option<usize>,
        flush: bool,
    ) -> Result<(Vec<Vec<u8>>, Option<TailError>), TailError> {
        let mut ring = std::collections::VecDeque::new();
        let mut all = Vec::new();
        let mut decompressed = Vec::new();
        let mut format_error = None;

        // Snapshot the readable end so a racing writer cannot keep this call
        // looping forever, and so memory for `all` stays proportional to one
        // poll's budget rather than an unbounded append stream.
        self.read_until = self.snapshot_end()?;

        loop {
            let n = self.read_chunk()?;
            if n == 0 {
                if flush {
                    decompressed.clear();
                    match self.decoder.finish(&mut decompressed) {
                        Ok(()) => {
                            self.take_lines(&decompressed, keep_last, &mut ring, &mut all);
                        }
                        Err(message) => {
                            self.take_lines(&decompressed, keep_last, &mut ring, &mut all);
                            format_error = Some(TailError::Format {
                                path: self.path.clone(),
                                message,
                            });
                        }
                    }
                }
                break;
            }
            decompressed.clear();
            let chunk = std::mem::take(&mut self.read_buf);
            let fed = self.decoder.feed(&chunk[..n], &mut decompressed);
            self.read_buf = chunk;
            match fed {
                Ok(()) => {
                    self.take_lines(&decompressed, keep_last, &mut ring, &mut all);
                }
                Err(message) => {
                    self.take_lines(&decompressed, keep_last, &mut ring, &mut all);
                    format_error = Some(TailError::Format {
                        path: self.path.clone(),
                        message,
                    });
                    break;
                }
            }
        }

        self.read_until = None;
        if self.offset != self.fp_offset {
            self.refresh_fingerprint()?;
            self.fp_offset = self.offset;
        }
        self.last_size = self.snap_len;
        self.fp_len = self.snap_len;
        self.fp_mtime = self.snap_mtime;

        let lines = match keep_last {
            Some(_) => ring.into_iter().collect(),
            None => all,
        };
        Ok((lines, format_error))
    }

    fn snapshot_end(&mut self) -> Result<Option<u64>, TailError> {
        let Some(file) = &self.file else {
            return Ok(None);
        };
        let meta = file.metadata().map_err(|source| TailError::Io {
            path: self.path.clone(),
            source,
        })?;
        let len = meta.len();
        self.snap_len = len;
        self.snap_mtime = meta.modified().ok();
        let end = len.min(self.offset.saturating_add(MAX_BYTES_PER_CONSUME));
        Ok(Some(end))
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
        self.lines.push_with(data, |line| match keep_last {
            Some(n) => retain_last(ring, n, line),
            None => {
                all.push(line);
                None
            }
        });
    }

    /// Fill `read_buf` up to the snapshot limit. `Ok(0)` means nothing left.
    fn read_chunk(&mut self) -> Result<usize, TailError> {
        let Some(file) = self.file.as_mut() else {
            return Ok(0);
        };
        let limit = self.read_until.unwrap_or(u64::MAX);
        if self.offset >= limit {
            return Ok(0);
        }
        let available = (limit - self.offset) as usize;
        if available == 0 {
            return Ok(0);
        }
        let want = available.min(CHUNK);
        if self.read_buf.len() < want {
            self.read_buf.resize(want, 0);
        }
        let mut filled = 0;
        while filled < want {
            match file.read(&mut self.read_buf[filled..want]) {
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
        self.offset += filled as u64;
        Ok(filled)
    }

    fn refresh_fingerprint(&mut self) -> Result<(), TailError> {
        self.fingerprint.clear();
        if self.offset == 0 {
            return Ok(());
        }
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        let len = FINGERPRINT as u64;
        let start = self.offset.saturating_sub(len);
        let want = (self.offset - start) as usize;
        if want == 0 {
            return Ok(());
        }
        let mut buf = vec![0u8; want];
        file.seek(SeekFrom::Start(start))
            .map_err(|source| TailError::Io {
                path: self.path.clone(),
                source,
            })?;
        file.read_exact(&mut buf).map_err(|source| TailError::Io {
            path: self.path.clone(),
            source,
        })?;
        file.seek(SeekFrom::Start(self.offset))
            .map_err(|source| TailError::Io {
                path: self.path.clone(),
                source,
            })?;
        self.fingerprint = buf;
        Ok(())
    }
}

fn require_regular(path: &Path, meta: &Metadata) -> Result<(), TailError> {
    if meta.is_file() {
        return Ok(());
    }
    if meta.is_dir() {
        return Err(TailError::Format {
            path: path.to_path_buf(),
            message: "is a directory".into(),
        });
    }
    Err(TailError::Format {
        path: path.to_path_buf(),
        message: "is not a regular file".into(),
    })
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
    use std::os::unix::net::UnixListener;

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

    fn deflate_len(member: &[u8]) -> u64 {
        // Standard flate2 members: 10-byte header + 8-byte trailer.
        member.len() as u64 - 18
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
        assert!(update.error.is_none());
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
        let expected = deflate_len(&second) + deflate_len(&third);
        assert_eq!(
            inflate_delta, expected,
            "inflate must count exactly the new DEFLATE payloads"
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
        assert_eq!(delta, deflate_len(&extra));
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
        std::fs::write(&path, &new).unwrap();
        let update = tail.poll().unwrap();
        assert_eq!(update.notice, Some(Notice::Truncated));
        assert_eq!(update.lines, vec![b"new-1\n".to_vec()]);
        assert_eq!(tail.bytes_fed(), fed + new.len() as u64);
    }

    #[test]
    fn truncate_and_regrow_to_same_size_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("regrow.log");
        write(&path, b"AAAAAAAA\n");
        let mut tail = TailFile::open(&path, false).unwrap();
        assert_eq!(
            tail.read_initial(10, false).unwrap(),
            vec![b"AAAAAAAA\n".to_vec()]
        );
        // Same length, different content, same inode. Detection keys off
        // mtime, and some filesystems coalesce back-to-back updates, so
        // move the timestamp forward explicitly.
        write(&path, b"BBBBBBBB\n");
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        let bumped =
            file.metadata().unwrap().modified().unwrap() + std::time::Duration::from_secs(1);
        file.set_modified(bumped).unwrap();
        let update = tail.poll().unwrap();
        assert_eq!(update.notice, Some(Notice::Truncated));
        assert_eq!(update.lines, vec![b"BBBBBBBB\n".to_vec()]);
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

        let fed = by_name.bytes_fed();
        append(&path, b"new-c\n");
        let update = by_name.poll().unwrap();
        assert_eq!(update.lines, vec![b"new-c\n".to_vec()]);
        assert_eq!(by_name.bytes_fed(), fed + 6);
    }

    #[test]
    fn follow_name_drains_old_inode_before_replace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("drain.log");
        write(&path, b"keep-me");
        let mut tail = TailFile::open(&path, true).unwrap();
        assert!(tail.read_initial(10, false).unwrap().is_empty());

        // Append a complete line on the old inode, then rotate before poll.
        append(&path, b"-old\n");
        let rotated = dir.path().join("drain.log.1");
        std::fs::rename(&path, &rotated).unwrap();
        write(&path, b"new\n");

        let update = tail.poll().unwrap();
        assert_eq!(update.notice, Some(Notice::Replaced));
        assert_eq!(
            update.lines,
            vec![b"keep-me-old\n".to_vec(), b"new\n".to_vec()]
        );
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

    #[test]
    fn truncated_gzip_without_follow_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cut.gz");
        let compressed = gz(b"hello\nworld\n");
        write(&path, &compressed[..compressed.len() / 2]);
        let mut tail = TailFile::open(&path, false).unwrap();
        let err = tail.read_initial(10, true).unwrap_err();
        assert!(err.to_string().contains("truncated"), "{err}");
    }

    #[test]
    fn valid_lines_before_format_error_are_returned() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.gz");
        let mut blob = gz(b"good\n");
        // Complete next header with an illegal compression method so feed()
        // fails after the first member has already produced lines.
        blob.extend_from_slice(&[0x1f, 0x8b, 0, 0, 0, 0, 0, 0, 0, 255]);
        write(&path, &blob);
        let mut tail = TailFile::open(&path, false).unwrap();
        let (lines, error) = tail.consume(None, false).unwrap();
        assert_eq!(lines, vec![b"good\n".to_vec()]);
        assert!(error.is_some(), "expected format error after valid lines");
    }

    #[test]
    fn fifo_is_rejected_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pipe");
        nix_mkfifo(&path);
        match TailFile::open(&path, false) {
            Ok(_) => panic!("FIFO should be rejected"),
            Err(err) => assert!(err.to_string().contains("not a regular file"), "{err}"),
        }
    }

    #[test]
    fn socket_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sock");
        let _listener = UnixListener::bind(&path).unwrap();
        match TailFile::open(&path, false) {
            Ok(_) => panic!("socket should be rejected"),
            Err(err) => assert!(err.to_string().contains("not a regular file"), "{err}"),
        }
    }

    fn nix_mkfifo(path: &Path) {
        use std::ffi::CString;
        let c = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo failed");
    }
}
