//! Command-line driver: initial ring of N lines, then an optional follow loop.
//!
//! Without `-f`/`-F` the process prints that ring and exits. With follow, it
//! sleeps on inotify (or the `-s` interval) and only decodes bytes appended
//! since the last read.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;

use crate::cli::Cli;
use crate::tailfile::{Notice, TailFile};
use crate::watch::Watch;

struct Stop {
    sigint: Arc<AtomicBool>,
    sigterm: Arc<AtomicBool>,
}

impl Stop {
    fn install() -> io::Result<Self> {
        let sigint = Arc::new(AtomicBool::new(false));
        let sigterm = Arc::new(AtomicBool::new(false));
        signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&sigint))?;
        signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&sigterm))?;
        Ok(Self { sigint, sigterm })
    }

    fn code(&self) -> Option<u8> {
        if self.sigint.load(Ordering::Relaxed) {
            Some(130)
        } else if self.sigterm.load(Ordering::Relaxed) {
            Some(143)
        } else {
            None
        }
    }

    fn tripped(&self) -> bool {
        self.code().is_some()
    }
}

struct Headers {
    enabled: bool,
    current: Option<PathBuf>,
}

impl Headers {
    fn prefix(&mut self, path: &Path) -> Vec<u8> {
        if !self.enabled || self.current.as_deref() == Some(path) {
            return Vec::new();
        }
        let mut buf = Vec::new();
        if self.current.is_some() {
            buf.push(b'\n');
        }
        writeln!(buf, "==> {} <==", path.display()).expect("header fits in memory");
        self.current = Some(path.to_path_buf());
        buf
    }
}

struct Slot {
    tail: TailFile,
    dead: bool,
}

enum Flow {
    BrokenPipe,
    Code(u8),
}

/// Run ztail with the process arguments. The returned value is the exit code.
pub fn run() -> u8 {
    match run_inner() {
        Ok(code) => code,
        Err(Flow::BrokenPipe) => 0,
        Err(Flow::Code(code)) => code,
    }
}

fn run_inner() -> Result<u8, Flow> {
    let cli = Cli::parse();
    if cli.files.is_empty() {
        warn("ztail: missing file operand");
        warn("Try 'ztail --help' for more information.");
        return Ok(1);
    }

    let follow = cli.follow_enabled();
    let follow_name = cli.follow_name;
    let show_headers = cli.show_headers();
    let n = cli.lines;
    // Not following: resolve a trailing magic prefix as text. Following: keep
    // it, the next read may complete a gzip or zstd header.
    let flush = !follow;

    let stop = if follow {
        Some(Stop::install().map_err(|err| {
            warn(&format!("ztail: cannot install signal handler: {err}"));
            Flow::Code(1)
        })?)
    } else {
        None
    };

    let mut headers = Headers {
        enabled: show_headers,
        current: None,
    };
    let mut slots = Vec::new();
    let mut failed = false;

    for path in &cli.files {
        match TailFile::open(path, follow_name) {
            Ok(mut tail) => match tail.read_initial(n, flush) {
                Ok(lines) => {
                    write_stdout(&headers.prefix(tail.path()))?;
                    write_lines(&lines)?;
                    slots.push(Slot { tail, dead: false });
                }
                Err(err) => {
                    warn(&format!("ztail: {err}"));
                    failed = true;
                }
            },
            Err(err) if follow_name && err.kind() == Some(io::ErrorKind::NotFound) => {
                warn(&format!("ztail: {err}; waiting for it to appear"));
                slots.push(Slot {
                    tail: TailFile::pending(path, n, true),
                    dead: false,
                });
            }
            Err(err) => {
                warn(&format!("ztail: {err}"));
                failed = true;
            }
        }
    }

    if !follow {
        return Ok(if failed { 1 } else { 0 });
    }
    if slots.iter().all(|slot| slot.dead) {
        return Ok(1);
    }

    let stop = stop.expect("follow installs signal handlers");
    let interval = Duration::try_from_secs_f64(cli.sleep_interval).map_err(|_| {
        warn("ztail: sleep interval is too large");
        Flow::Code(1)
    })?;
    let paths: Vec<PathBuf> = slots
        .iter()
        .map(|slot| slot.tail.path().to_path_buf())
        .collect();
    let watch = Watch::new(&paths);

    loop {
        if let Some(code) = stop.code() {
            return Ok(code);
        }
        let mut progressed = false;
        for slot in slots.iter_mut() {
            if slot.dead {
                continue;
            }
            match slot.tail.poll() {
                Ok(update) => {
                    if let Some(notice) = update.notice {
                        note(slot.tail.path(), notice);
                    }
                    if update.lines.is_empty() {
                        continue;
                    }
                    progressed = true;
                    write_stdout(&headers.prefix(slot.tail.path()))?;
                    write_lines(&update.lines)?;
                }
                Err(err) => {
                    warn(&format!("ztail: {err}"));
                    slot.dead = true;
                    failed = true;
                }
            }
        }
        if slots.iter().all(|slot| slot.dead) {
            return Ok(if failed { 1 } else { 0 });
        }
        if !progressed {
            if let Some(code) = stop.code() {
                return Ok(code);
            }
            watch.wait(interval, &|| stop.tripped());
        }
    }
}

fn note(path: &Path, notice: Notice) {
    warn(&format!("ztail: {}: {}", path.display(), notice.message()));
}

fn warn(msg: &str) {
    let mut err = io::stderr().lock();
    let _ = writeln!(err, "{msg}");
}

fn write_lines(lines: &[Vec<u8>]) -> Result<(), Flow> {
    let mut out = io::stdout().lock();
    for line in lines {
        write_stdout_locked(&mut out, line)?;
    }
    flush_stdout(&mut out)
}

fn write_stdout(bytes: &[u8]) -> Result<(), Flow> {
    if bytes.is_empty() {
        return Ok(());
    }
    let mut out = io::stdout().lock();
    write_stdout_locked(&mut out, bytes)?;
    flush_stdout(&mut out)
}

fn write_stdout_locked(out: &mut io::StdoutLock<'_>, bytes: &[u8]) -> Result<(), Flow> {
    match out.write_all(bytes) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Err(Flow::BrokenPipe),
        Err(err) => {
            warn(&format!("ztail: write error: {err}"));
            Err(Flow::Code(1))
        }
    }
}

fn flush_stdout(out: &mut io::StdoutLock<'_>) -> Result<(), Flow> {
    match out.flush() {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Err(Flow::BrokenPipe),
        Err(err) => {
            warn(&format!("ztail: write error: {err}"));
            Err(Flow::Code(1))
        }
    }
}
