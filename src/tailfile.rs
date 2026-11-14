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

use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::decode::Decoder;
use crate::lines::{retain_last, LineBuffer};

const CHUNK: usize = 256 * 1024;
/// Cap how much one poll/consume will read so follow stays responsive.
const MAX_BYTES_PER_CONSUME: u64 = 8 * 1024 * 1024;
/// Bytes kept just behind the cursor to detect truncate-and-regrow.
const FINGERPRINT: usize = 64;
/// Bound decompression and result allocation separately from compressed I/O.
const MAX_OUTPUT_PER_CONSUME: usize = 1024 * 1024;
const DECODE_OUTPUT: usize = 64 * 1024;

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
            TailError::Io { path, source } => write!(f, "{}: {source}", crate::display_path(path)),
            TailError::Format { path, message } => {
                write!(f, "{}: {message}", crate::display_path(path))
            }
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
    notice: Option<Notice>,
    drained: Vec<Vec<u8>>,
    error: Option<TailError>,
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
    /// Frozen old-inode EOF once rotation/unlink draining begins.
    drain_end: Option<u64>,
    batch_output: bool,
}

impl TailFile {
    pub fn open(path: &Path, follow_name: bool) -> Result<Self, TailError> {
        let (file, meta) = open_regular(path)?;
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
            drain_end: None,
            batch_output: false,
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
            drain_end: None,
            batch_output: false,
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
        self.read_initial_interruptible(n, flush, &|| false)
    }

    /// Like `read_initial`, but checks cancellation between bounded decoder
    /// steps, including when a small compressed file expands enormously.
    pub fn read_initial_interruptible(
        &mut self,
        n: usize,
        flush: bool,
        stop: &dyn Fn() -> bool,
    ) -> Result<Vec<Vec<u8>>, TailError> {
        self.n = n;
        if !self.did_initial && self.offset == 0 {
            self.seek_plain_tail(n, stop)?;
        }
        let (lines, error) = self.consume_interruptible(Some(n), flush, stop)?;
        self.did_initial = true;
        if let Some(err) = error {
            return Err(err);
        }
        Ok(lines)
    }

    /// For ordinary text, find the last N complete records backwards. Limit
    /// speculative scanning to 1 MiB; large N / long records fall back to the
    /// forward path, preserving its deterministic long-record policy.
    fn seek_plain_tail(&mut self, n: usize, stop: &dyn Fn() -> bool) -> Result<(), TailError> {
        let Some(file) = &mut self.file else {
            return Ok(());
        };
        let io_error = |source| TailError::Io {
            path: self.path.clone(),
            source,
        };
        let len = file.metadata().map_err(io_error)?.len();
        if len <= CHUNK as u64 {
            return Ok(());
        }
        let mut magic = [0; 6];
        if read_exact_at(file, &mut magic, 0).is_err() || !Decoder::is_plain_prefix(&magic) {
            return Ok(());
        }
        let mut end = len;
        let lower = len.saturating_sub(crate::lines::MAX_PARTIAL_LINE as u64);
        let mut seen = 0usize;
        let mut buf = vec![0; DECODE_OUTPUT];
        while end > lower {
            if stop() {
                return Err(io_error(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "read cancelled",
                )));
            }
            let start = end.saturating_sub(buf.len() as u64).max(lower);
            let chunk = &mut buf[..(end - start) as usize];
            if read_exact_at(file, chunk, start).is_err() {
                // Concurrent shrink: leave the forward reader at offset zero.
                return Ok(());
            }
            for i in memchr::memrchr_iter(b'\n', chunk) {
                if seen == n {
                    let offset = start + i as u64 + 1;
                    file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
                    self.offset = offset;
                    self.decoder.select_plain();
                    return Ok(());
                }
                seen += 1;
            }
            end = start;
        }
        Ok(())
    }

    /// Work already observed but not yet consumed because a poll hit a budget.
    /// Callers must not sleep merely because no complete line was emitted.
    pub fn has_pending_work(&self) -> bool {
        self.file.is_some() && (self.offset < self.last_size || self.decoder.needs_drain())
    }

    /// Read whatever was appended, and apply rotation / truncation policy.
    pub fn poll(&mut self) -> Result<Update, TailError> {
        self.poll_interruptible(&|| false)
    }

    /// CLI-only output batching; the public poll API still returns one Vec
    /// per record. This bounds allocation overhead even for newline-only logs.
    pub(crate) fn poll_batched(&mut self, stop: &dyn Fn() -> bool) -> Result<Update, TailError> {
        self.batch_output = true;
        let result = self.poll_interruptible(stop);
        self.batch_output = false;
        result
    }

    /// Includes cancellation during the initial view of a late-appearing file.
    pub fn poll_interruptible(&mut self, stop: &dyn Fn() -> bool) -> Result<Update, TailError> {
        if self.follow_name {
            if let Some(change) = self.sync_name()? {
                let (mut lines, error) = match change.notice {
                    Some(Notice::Appeared) => {
                        let n = self.n;
                        let result = self.consume_interruptible(Some(n), false, stop)?;
                        self.did_initial = true;
                        result
                    }
                    Some(Notice::Inaccessible) => (Vec::new(), None),
                    Some(Notice::Replaced | Notice::Truncated) => self.consume(None, false)?,
                    None => (Vec::new(), change.error),
                };
                if !change.drained.is_empty() {
                    let mut combined = change.drained;
                    combined.append(&mut lines);
                    lines = combined;
                }
                return Ok(Update {
                    lines,
                    notice: change.notice,
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
                    self.drain_end = None;
                    return Ok(None);
                }
                let notice = if self.file.is_none() && !self.did_initial {
                    Notice::Appeared
                } else {
                    Notice::Replaced
                };
                // Drain unread bytes from the old inode before replacing it.
                let drained = if self.file.is_some() {
                    self.begin_drain()?;
                    let (lines, error) = self.consume(None, false)?;
                    if error.is_some() || self.has_pending_work() {
                        return Ok(Some(NameChange {
                            notice: None,
                            drained: lines,
                            error,
                        }));
                    }
                    lines
                } else {
                    Vec::new()
                };
                match self.reopen() {
                    Ok(()) => Ok(Some(NameChange {
                        notice: Some(notice),
                        drained,
                        error: None,
                    })),
                    // The new path vanished between stat and open. Preserve
                    // the old output and retry rather than killing this slot.
                    Err(err) if err.kind() == Some(io::ErrorKind::NotFound) => {
                        Ok(Some(NameChange {
                            notice: None,
                            drained,
                            error: None,
                        }))
                    }
                    Err(err) => Ok(Some(NameChange {
                        notice: None,
                        drained,
                        error: Some(err),
                    })),
                }
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                if self.file.is_some() && !self.missing_announced {
                    self.begin_drain()?;
                    let (drained, error) = self.consume(None, false)?;
                    if error.is_some() || self.has_pending_work() {
                        return Ok(Some(NameChange {
                            notice: None,
                            drained,
                            error,
                        }));
                    }
                    self.file = None;
                    self.offset = 0;
                    self.last_size = 0;
                    self.fingerprint.clear();
                    self.decoder.reset();
                    self.lines.clear();
                    self.missing_announced = true;
                    return Ok(Some(NameChange {
                        notice: Some(Notice::Inaccessible),
                        drained,
                        error: None,
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

    fn begin_drain(&mut self) -> Result<(), TailError> {
        if self.drain_end.is_none() {
            if let Some(file) = &self.file {
                let meta = file.metadata().map_err(|source| TailError::Io {
                    path: self.path.clone(),
                    source,
                })?;
                self.drain_end = Some(meta.len());
            }
        }
        Ok(())
    }

    fn reopen(&mut self) -> Result<(), TailError> {
        // Validate the descriptor actually opened; the path can change at any
        // point, including changing into a FIFO between stat and open.
        let (file, meta) = open_regular(&self.path)?;
        let (dev, ino) = file_id(&meta);
        self.file = Some(file);
        self.dev = dev;
        self.ino = ino;
        self.offset = 0;
        self.drain_end = None;
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
            match read_exact_at(file, &mut buf, start) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(true),
                Err(source) => {
                    return Err(TailError::Io {
                        path: self.path.clone(),
                        source,
                    })
                }
            }
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
        self.drain_end = None;
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
        self.consume_interruptible(keep_last, flush, &|| false)
    }

    fn consume_interruptible(
        &mut self,
        keep_last: Option<usize>,
        flush: bool,
        stop: &dyn Fn() -> bool,
    ) -> Result<(Vec<Vec<u8>>, Option<TailError>), TailError> {
        let mut ring = std::collections::VecDeque::new();
        let mut all = Vec::new();
        let mut decompressed = Vec::new();
        let mut format_error = None;

        // Snapshot the readable end so a racing writer cannot keep this call
        // looping forever, and so memory for `all` stays proportional to one
        // poll's budget rather than an unbounded append stream.
        self.read_until = self.snapshot_end(keep_last.is_none())?;
        let mut produced = 0usize;

        loop {
            if stop() {
                return Err(TailError::Io {
                    path: self.path.clone(),
                    source: io::Error::new(io::ErrorKind::Interrupted, "read cancelled"),
                });
            }
            let draining = self.decoder.needs_drain();
            let n = if draining { 0 } else { self.read_chunk()? };
            if n == 0 && !draining {
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
            let fed = self
                .decoder
                .feed_limited(&chunk[..n], &mut decompressed, DECODE_OUTPUT);
            self.read_buf = chunk;
            produced += decompressed.len();
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
            // Never flush at a scheduling boundary: this is not archive EOF.
            if keep_last.is_none() && produced >= MAX_OUTPUT_PER_CONSUME {
                break;
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

    fn snapshot_end(&mut self, bounded: bool) -> Result<Option<u64>, TailError> {
        let Some(file) = &self.file else {
            return Ok(None);
        };
        let meta = file.metadata().map_err(|source| TailError::Io {
            path: self.path.clone(),
            source,
        })?;
        let len = self.drain_end.map_or(meta.len(), |end| meta.len().min(end));
        self.snap_len = len;
        self.snap_mtime = meta.modified().ok();
        let end = if bounded {
            len.min(self.offset.saturating_add(MAX_BYTES_PER_CONSUME))
        } else {
            // Initial last-N must reach the original snapshot EOF, even for
            // files larger than a follow poll's fairness budget.
            len
        };
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
        if self.batch_output && keep_last.is_none() {
            self.lines.push_slices(data, |line| {
                if let Some(last) = all.last_mut() {
                    if last.len() + line.len() <= DECODE_OUTPUT {
                        last.extend_from_slice(line);
                        return;
                    }
                }
                all.push(line.to_vec());
            });
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
        let available = (limit - self.offset).min(CHUNK as u64) as usize;
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
        match read_exact_at(file, &mut buf, start) {
            Ok(()) => {}
            // A racing truncate is handled on the next poll. Do not kill the
            // file (or disturb its sequential read position) for this race.
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(source) => {
                return Err(TailError::Io {
                    path: self.path.clone(),
                    source,
                })
            }
        }
        self.fingerprint = buf;
        Ok(())
    }
}

/// Nonblocking open closes the stat/open FIFO race; O_NOCTTY prevents a
/// substituted terminal device from becoming the process's controlling tty.
fn open_regular(path: &Path) -> Result<(File, Metadata), TailError> {
    let io_error = |source| TailError::Io {
        path: path.to_path_buf(),
        source,
    };
    let meta = std::fs::metadata(path).map_err(io_error)?;
    require_regular(path, &meta)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .map_err(io_error)?;
    let meta = file.metadata().map_err(io_error)?;
    require_regular(path, &meta)?;
    Ok((file, meta))
}

fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match file.read_at(buf, offset) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            Ok(n) => {
                offset += n as u64;
                buf = &mut buf[n..];
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(())
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

    fn large_records() -> Vec<u8> {
        let mut data = vec![b'x'; 9 * 1024 * 1024];
        for i in (1023..data.len()).step_by(1024) {
            data[i] = b'\n';
        }
        data.extend_from_slice(b"last\npartial");
        data
    }

    #[test]
    fn initial_large_plain_seeks_tail_and_preserves_partial() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.txt");
        write(&path, &large_records());
        let mut tail = TailFile::open(&path, false).unwrap();
        let lines = tail.read_initial(1, true).unwrap();
        assert_eq!(lines, vec![b"last\n".to_vec()]);
        assert!(tail.bytes_fed() < 2 * DECODE_OUTPUT as u64);
        assert!(!tail.has_pending_work());
        append(&path, b" complete\n");
        assert_eq!(
            tail.poll().unwrap().lines,
            vec![b"partial complete\n".to_vec()]
        );
    }

    #[test]
    fn initial_large_stored_gzip_reads_beyond_poll_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.gz");
        let raw = large_records();
        let mut enc = GzEncoder::new(Vec::new(), Compression::none());
        enc.write_all(&raw).unwrap();
        let compressed = enc.finish().unwrap();
        assert!(compressed.len() as u64 > MAX_BYTES_PER_CONSUME);
        write(&path, &compressed);
        let mut tail = TailFile::open(&path, false).unwrap();
        assert_eq!(
            tail.read_initial(1, true).unwrap(),
            vec![b"last\n".to_vec()]
        );
        assert_eq!(tail.bytes_fed(), compressed.len() as u64);
        assert!(!tail.has_pending_work());
    }

    #[test]
    fn zero_lines_skips_all_existing_data_beyond_budget() {
        let dir = tempfile::tempdir().unwrap();
        for compressed in [false, true] {
            let path = dir
                .path()
                .join(if compressed { "zero.gz" } else { "zero.txt" });
            let raw = large_records();
            write(&path, &if compressed { gz(&raw) } else { raw });
            let mut tail = TailFile::open(&path, false).unwrap();
            assert!(tail.read_initial(0, false).unwrap().is_empty());
            assert!(tail.poll().unwrap().lines.is_empty());
            let extra = b" done\n";
            append(
                &path,
                &if compressed {
                    gz(extra)
                } else {
                    extra.to_vec()
                },
            );
            assert_eq!(tail.poll().unwrap().lines, vec![b"partial done\n".to_vec()]);
        }
    }

    #[test]
    fn high_ratio_follow_is_output_bounded_and_resumable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bomb.gz");
        write(&path, b"");
        let mut tail = TailFile::open(&path, false).unwrap();
        tail.read_initial(0, false).unwrap();
        let raw = vec![b'x'; 9 * 1024 * 1024 + 17];
        append(&path, &gz(&raw));
        let mut total = 0;
        let mut polls = 0;
        loop {
            let update = tail.poll().unwrap();
            assert!(update.error.is_none());
            let bytes: usize = update.lines.iter().map(Vec::len).sum();
            assert!(bytes <= MAX_OUTPUT_PER_CONSUME + crate::lines::MAX_PARTIAL_LINE);
            total += bytes;
            polls += 1;
            assert!(polls < 30);
            if !tail.has_pending_work() {
                break;
            }
        }
        assert!(polls >= 9);
        assert_eq!(total, 9 * 1024 * 1024);
        append(&path, &gz(b"\n"));
        let update = tail.poll().unwrap();
        assert_eq!(
            update.lines,
            vec![[vec![b'x'; 17], b"\n".to_vec()].concat()]
        );
    }

    #[test]
    fn rotation_drains_multiple_output_budgets_before_switching() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rotate.gz");
        write(&path, b"");
        let mut tail = TailFile::open(&path, true).unwrap();
        tail.read_initial(0, false).unwrap();
        let mut raw = large_records();
        raw.extend_from_slice(b"\n");
        append(&path, &gz(&raw));
        std::fs::rename(&path, dir.path().join("old.gz")).unwrap();
        write(&path, b"new file\n");
        let mut got = Vec::new();
        let mut replaced = false;
        for _ in 0..30 {
            let update = tail.poll().unwrap();
            assert!(update.error.is_none());
            for line in update.lines {
                got.extend_from_slice(&line);
            }
            if update.notice == Some(Notice::Replaced) {
                replaced = true;
                break;
            }
        }
        assert!(replaced);
        raw.extend_from_slice(b"new file\n");
        assert_eq!(got, raw);
    }

    #[test]
    fn initial_scan_can_be_cancelled() {
        use std::cell::Cell;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cancel.gz");
        write(&path, &gz(&vec![b'x'; 4 * 1024 * 1024]));
        let mut tail = TailFile::open(&path, false).unwrap();
        let steps = Cell::new(0);
        let error = tail
            .read_initial_interruptible(10, true, &|| {
                steps.set(steps.get() + 1);
                steps.get() > 3
            })
            .unwrap_err();
        assert_eq!(error.kind(), Some(io::ErrorKind::Interrupted));
    }

    #[test]
    fn reverse_scan_does_not_sniff_magic_at_a_plain_line_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("magic.txt");
        let mut raw = vec![b'\n'; CHUNK + 1];
        raw.extend_from_slice(b"\x1f\x8bnot gzip\n");
        write(&path, &raw);
        let mut tail = TailFile::open(&path, false).unwrap();
        assert_eq!(
            tail.read_initial(1, true).unwrap(),
            vec![b"\x1f\x8bnot gzip\n".to_vec()]
        );
    }

    #[test]
    fn open_and_reopen_descriptors_are_nonblocking() {
        use std::os::fd::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("regular");
        write(&path, b"hello\n");
        let mut tail = TailFile::open(&path, true).unwrap();
        for _ in 0..2 {
            let fd = tail.file.as_ref().unwrap().as_raw_fd();
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            assert!(flags >= 0 && flags & libc::O_NONBLOCK != 0);
            tail.reopen().unwrap();
        }
    }

    #[test]
    fn unlink_drains_old_descriptor_over_multiple_polls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unlink.gz");
        write(&path, b"");
        let mut tail = TailFile::open(&path, true).unwrap();
        tail.read_initial(0, false).unwrap();
        let raw = b"old data\n".repeat(400_000);
        append(&path, &gz(&raw));
        std::fs::remove_file(&path).unwrap();
        let mut got = Vec::new();
        let mut inaccessible = false;
        for _ in 0..20 {
            let update = tail.poll().unwrap();
            assert!(update.error.is_none());
            for line in update.lines {
                got.extend_from_slice(&line);
            }
            if update.notice == Some(Notice::Inaccessible) {
                inaccessible = true;
                break;
            }
        }
        assert!(inaccessible);
        assert_eq!(got, raw);
    }

    #[test]
    fn reverse_scan_matches_forward_for_large_n_and_unterminated_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records");
        for trailing_newline in [false, true] {
            let mut raw = b"hello world\n".repeat(100_000);
            if !trailing_newline {
                raw.pop();
            }
            write(&path, &raw);
            for n in [0, 1, 10, 100_001, usize::MAX] {
                let mut fast = TailFile::open(&path, false).unwrap();
                let got = fast.read_initial(n, true).unwrap();
                let mut slow = TailFile::open(&path, false).unwrap();
                // Consume directly, bypassing the backwards optimization.
                let (expected, error) = slow.consume(Some(n), true).unwrap();
                assert!(error.is_none());
                assert_eq!(got, expected, "n={n}, newline={trailing_newline}");
                assert_eq!(fast.lines.partial(), slow.lines.partial());
            }
        }
    }

    #[test]
    fn active_old_writer_cannot_prevent_switching_forever() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("active.log");
        write(&path, b"");
        let mut tail = TailFile::open(&path, true).unwrap();
        tail.read_initial(0, false).unwrap();
        let raw = b"before rotation\n".repeat(200_000);
        append(&path, &raw);
        let old = dir.path().join("old.log");
        std::fs::rename(&path, &old).unwrap();
        write(&path, b"new\n");
        let mut got = Vec::new();
        let mut replaced = false;
        for _ in 0..10 {
            let update = tail.poll().unwrap();
            for line in update.lines {
                got.extend_from_slice(&line);
            }
            if update.notice == Some(Notice::Replaced) {
                replaced = true;
                break;
            }
            append(&old, &b"too late\n".repeat(200_000));
        }
        assert!(replaced);
        let mut expected = raw;
        expected.extend_from_slice(b"new\n");
        assert_eq!(got, expected);
    }

    #[test]
    fn late_appearing_initial_scan_can_be_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("late.gz");
        let mut tail = TailFile::pending(&path, 10, true);
        write(&path, &gz(b"data\n"));
        assert_eq!(
            tail.poll_interruptible(&|| true).unwrap_err().kind(),
            Some(io::ErrorKind::Interrupted)
        );
    }
    #[test]
    fn cli_batches_tiny_lines_without_per_line_allocations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiny.gz");
        write(&path, b"");
        let mut tail = TailFile::open(&path, false).unwrap();
        tail.read_initial(0, false).unwrap();
        let raw = vec![b'\n'; 2 * MAX_OUTPUT_PER_CONSUME + 17];
        append(&path, &gz(&raw));
        let mut total = 0;
        loop {
            let update = tail.poll_batched(&|| false).unwrap();
            assert!(update.error.is_none());
            assert!(update.lines.len() <= MAX_OUTPUT_PER_CONSUME / DECODE_OUTPUT + 1);
            for batch in update.lines {
                assert!(batch.iter().all(|&b| b == b'\n'));
                total += batch.len();
            }
            if !tail.has_pending_work() {
                break;
            }
        }
        assert_eq!(total, raw.len());
    }
}
