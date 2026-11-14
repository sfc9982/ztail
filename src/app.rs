//! Command-line driver: initial ring of N lines, then an optional follow loop.
//!
//! Without `-f`/`-F` the process prints that ring and exits. With follow, it
//! sleeps on inotify (or the `-s` interval) and only decodes bytes appended
//! since the last read.

use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::presentation::{self, text, warn};

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
    /// Last operand slot that printed a header. Tracked by slot index so the
    /// same path passed twice still gets a separator, like GNU tail.
    current: Option<usize>,
}

impl Headers {
    fn prefix(&mut self, slot: usize, path: &Path) -> Vec<u8> {
        if !self.enabled || self.current == Some(slot) {
            return Vec::new();
        }
        let mut buf = Vec::new();
        if self.current.is_some() {
            buf.push(b'\n');
        }
        if presentation::settings().stdout_color {
            buf.extend_from_slice(b"\x1b[1;36m");
        }
        write!(buf, "==> {} <==", crate::display_path(path)).expect("header fits in memory");
        if presentation::settings().stdout_color {
            buf.extend_from_slice(b"\x1b[0m");
        }
        buf.push(b'\n');
        self.current = Some(slot);
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
    let cli = match Cli::parse_localized() {
        Ok(cli) => cli,
        Err(error) => {
            if error.use_stderr() {
                if matches!(
                    presentation::settings().language,
                    presentation::Language::Chinese
                ) {
                    warn("ztail: 命令行参数错误；使用 --help 查看帮助。以下为解析器详情：");
                }
                // clap retains its structured, English validation detail.
                let detail: String = error
                    .to_string()
                    .chars()
                    .flat_map(|ch| {
                        if ch.is_control() && ch != '\n' && ch != '\t' {
                            ch.escape_debug().collect::<Vec<_>>()
                        } else {
                            vec![ch]
                        }
                    })
                    .collect();
                warn(detail.trim_end());
                return Ok(error.exit_code() as u8);
            }
            let rendered = error.render();
            let output = if presentation::settings().stdout_color {
                rendered.ansi().to_string()
            } else {
                rendered.to_string()
            };
            write_stdout(output.as_bytes())?;
            return Ok(0);
        }
    };
    if cli.build_info {
        write_stdout(crate::build_info::json().as_bytes())?;
        return Ok(0);
    }
    if cli.manpage {
        write_stdout(&crate::manual::render(presentation::settings().language))?;
        return Ok(0);
    }
    if let Some(shell) = cli.completions {
        let mut command = Cli::localized_command(presentation::settings().language)
            .color(clap::ColorChoice::Never);
        // Generators use infallible writes internally. Generate into memory so
        // a closed pipe cannot panic; only the small script is buffered.
        let mut script = Vec::new();
        clap_complete::generate(shell, &mut command, "ztail", &mut script);
        write_stdout(&script)?;
        return Ok(0);
    }
    if cli.files.is_empty() {
        warn(&text("ztail: missing file operand", "ztail: 缺少文件参数"));
        warn(&text(
            "Try 'ztail --help' for more information.",
            "请使用 ztail --help 查看帮助。",
        ));
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
            warn(&format!(
                "ztail: {}: {err}",
                text("cannot install signal handler", "无法安装信号处理器")
            ));
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
        let slot_idx = slots.len();
        match TailFile::open(path, follow_name) {
            Ok(mut tail) => match tail
                .read_initial_interruptible(n, flush, &|| stop.as_ref().is_some_and(Stop::tripped))
            {
                Ok(lines) => {
                    write_stdout(&headers.prefix(slot_idx, tail.path()))?;
                    write_lines(&lines)?;
                    slots.push(Slot { tail, dead: false });
                }
                Err(err) => {
                    if let Some(code) = stop.as_ref().and_then(Stop::code) {
                        return Ok(code);
                    }
                    warn(&format!("ztail: {}", describe_error(&err)));
                    failed = true;
                }
            },
            Err(err) if follow_name && err.kind() == Some(io::ErrorKind::NotFound) => {
                warn(&format!(
                    "ztail: {}; {}",
                    describe_error(&err),
                    text("waiting for it to appear", "等待文件出现")
                ));
                slots.push(Slot {
                    tail: TailFile::pending(path, n, true),
                    dead: false,
                });
            }
            Err(err) => {
                warn(&format!("ztail: {}", describe_error(&err)));
                failed = true;
            }
        }
    }

    if !follow {
        return Ok(if failed { 1 } else { 0 });
    }
    if slots.is_empty() || slots.iter().all(|slot| slot.dead) {
        return Ok(1);
    }

    let stop = stop.expect("follow installs signal handlers");
    let interval = Duration::try_from_secs_f64(cli.sleep_interval).map_err(|_| {
        warn(&text(
            "ztail: sleep interval is too large",
            "ztail: 轮询间隔过大",
        ));
        Flow::Code(1)
    })?;
    if interval.is_zero() {
        warn(&text(
            "ztail: sleep interval is too small",
            "ztail: 轮询间隔过小",
        ));
        return Ok(1);
    }
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
        for (slot_idx, slot) in slots.iter_mut().enumerate() {
            if let Some(code) = stop.code() {
                return Ok(code);
            }
            if slot.dead {
                continue;
            }
            match slot.tail.poll_batched(&|| stop.tripped()) {
                Ok(update) => {
                    progressed |= slot.tail.has_pending_work();
                    if let Some(notice) = update.notice {
                        note(slot.tail.path(), notice);
                    }
                    if !update.lines.is_empty() {
                        progressed = true;
                        write_stdout(&headers.prefix(slot_idx, slot.tail.path()))?;
                        write_lines(&update.lines)?;
                    }
                    if let Some(err) = update.error {
                        warn(&format!("ztail: {}", describe_error(&err)));
                        slot.dead = true;
                        failed = true;
                    }
                }
                Err(err) => {
                    if let Some(code) = stop.code() {
                        return Ok(code);
                    }
                    warn(&format!("ztail: {}", describe_error(&err)));
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
    warn(&format!(
        "ztail: {}: {}",
        crate::display_path(path),
        presentation::settings().language.text(
            notice.message(),
            match notice {
                Notice::Truncated => "文件已截断",
                Notice::Replaced => "文件已替换；正在跟随新文件",
                Notice::Appeared => "文件已出现；正在跟随",
                Notice::Inaccessible => "文件已无法访问",
            }
        )
    ));
}

fn describe_error(error: &crate::tailfile::TailError) -> String {
    use crate::tailfile::TailError;
    if matches!(
        presentation::settings().language,
        presentation::Language::English
    ) {
        return error.to_string();
    }
    match error {
        TailError::Io { path, source } => format!(
            "{}: 无法访问或读取文件：{source}",
            crate::display_path(path)
        ),
        TailError::Format { path, message } => {
            format!("{}: 文件格式错误：{message}", crate::display_path(path))
        }
    }
}

fn write_lines(lines: &[Vec<u8>]) -> Result<(), Flow> {
    // Stdout is line-buffered: without an explicit batch buffer every short
    // log line can become a separate syscall, even when redirected to a pipe.
    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(64 * 1024, stdout.lock());
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

fn write_stdout_locked(out: &mut impl Write, bytes: &[u8]) -> Result<(), Flow> {
    match out.write_all(bytes) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Err(Flow::BrokenPipe),
        Err(err) => {
            warn(&format!(
                "ztail: {}: {err}",
                text("write error", "写入失败")
            ));
            Err(Flow::Code(1))
        }
    }
}

fn flush_stdout(out: &mut impl Write) -> Result<(), Flow> {
    match out.flush() {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Err(Flow::BrokenPipe),
        Err(err) => {
            warn(&format!(
                "ztail: {}: {err}",
                text("write error", "写入失败")
            ));
            Err(Flow::Code(1))
        }
    }
}
