use std::path::PathBuf;

use crate::presentation::{self, Color, Language};
use clap::{CommandFactory, FromArgMatches, Parser};

/// Print the last part of gzip, zstd, and plain text files.
///
/// By default ztail prints the last `-n` lines and exits, like GNU tail.
/// Pass `-f` to follow the open file descriptor, or `-F` to follow the name
/// across rotation and truncation. Following is off unless you ask for it.
#[derive(Parser, Debug)]
#[command(
    name = "ztail",
    version,
    about,
    after_help = "A live `gzip` process only writes compressed bytes when it flushes.\n\
                  ztail can show data only after those bytes hit the file, so a writer\n\
                  that never flushes looks silent until the member is finished.\n\n\
                  -f follows the same file descriptor (appends and in-place truncation).\n\
                  -F follows the path: it also reopens when the inode changes."
)]
pub struct Cli {
    /// Output the last N lines. N=0 prints nothing from the existing file.
    #[arg(short = 'n', long = "lines", default_value_t = 10, value_name = "N")]
    pub lines: usize,

    /// Follow the file descriptor as it grows. Does not switch to a new inode
    /// after rotation. In-place truncation (size shrink) still restarts the decoder.
    #[arg(short = 'f', long = "follow")]
    pub follow: bool,

    /// Follow the file name. Reopen when the path's device/inode changes, when
    /// the file is recreated, or when it is truncated. Implies --follow.
    /// If both -f and -F are set, -F wins.
    #[arg(short = 'F', long = "follow-name")]
    pub follow_name: bool,

    /// Fallback poll interval in seconds, and the longest pause between checks
    /// when a filesystem event is missed. On Linux, inotify wakes ztail sooner.
    #[arg(
        short = 's',
        long = "sleep-interval",
        default_value_t = 1.0,
        value_name = "SECS",
        value_parser = parse_sleep
    )]
    pub sleep_interval: f64,

    /// Never print file name headers.
    #[arg(
        short = 'q',
        long = "quiet",
        visible_alias = "silent",
        conflicts_with = "verbose"
    )]
    pub quiet: bool,

    /// Always print file name headers, even for a single file.
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,

    /// Generate a completion script and exit (no file operands required).
    #[arg(long, value_enum, value_name = "SHELL")]
    pub completions: Option<clap_complete::Shell>,

    /// Generate a man(1) page from this CLI and exit.
    #[arg(long, conflicts_with_all = ["completions", "build_info"])]
    pub manpage: bool,

    /// Print machine-readable build target and enabled formats as JSON.
    #[arg(long, conflicts_with = "completions")]
    pub build_info: bool,

    /// Color headers and diagnostics, never log data.
    #[arg(long, value_enum, default_value = "auto", value_name = "WHEN")]
    pub color: Color,

    /// UI language (default: LC_ALL, LC_MESSAGES, LANG, then English).
    #[arg(long, value_enum, value_name = "LANG")]
    pub language: Option<Language>,

    /// Files to read. With more than one, each group is introduced with a
    /// `==> name <==` header, as in GNU tail.
    #[arg(value_name = "FILE")]
    pub files: Vec<PathBuf>,
}

fn parse_sleep(raw: &str) -> Result<f64, String> {
    let value: f64 = raw
        .parse()
        .map_err(|_| format!("invalid interval '{raw}'"))?;
    if !value.is_finite() || value <= 0.0 {
        return Err("sleep interval must be a positive number of seconds".into());
    }
    // Values smaller than 1 ns round to a zero Duration and would busy-spin.
    let duration = std::time::Duration::try_from_secs_f64(value)
        .map_err(|_| "sleep interval is too large".to_string())?;
    if duration.is_zero() {
        return Err("sleep interval is too small (rounds to zero)".into());
    }
    Ok(value)
}

impl Cli {
    pub fn follow_enabled(&self) -> bool {
        self.follow || self.follow_name
    }

    pub fn show_headers(&self) -> bool {
        !self.quiet && (self.verbose || self.files.len() > 1)
    }
}

impl Cli {
    pub fn parse_localized() -> Result<Self, clap::Error> {
        let args: Vec<_> = std::env::args_os().collect();
        let (language, color, help) = bootstrap(&args);
        let settings = presentation::initialize(language, color);
        let colored = if help {
            settings.stdout_color
        } else {
            settings.stderr_color
        };
        let mut command = Self::localized_command(language).color(if colored {
            clap::ColorChoice::Always
        } else {
            clap::ColorChoice::Never
        });
        let matches = command.try_get_matches_from_mut(args)?;
        Self::from_arg_matches(&matches)
    }

    pub fn localized_command(language: Language) -> clap::Command {
        let mut command = Self::command();
        if matches!(language, Language::Chinese) {
            command = command
                .about("增量读取 gzip、zstd 和纯文本文件的末尾内容")
                .long_about("默认输出最后 N 行并退出；使用 -f 跟随文件描述符，或 -F 在轮转后跟随文件名。")
                .after_help("gzip 写入端必须刷新压缩数据，ztail 才能读取新内容。\n-f 跟随原文件；-F 在文件替换、重建或截断后重新读取。\n颜色仅用于文件标题和诊断，不修改日志正文。")
                .help_template("{about-with-newline}\n用法：{usage}\n\n{all-args}{after-help}");
            for (id, help) in [
                ("lines", "输出最后 N 行；N=0 时不输出已有内容。"),
                ("follow", "跟随文件描述符；轮转后不切换到新文件。"),
                (
                    "follow_name",
                    "跟随文件名，自动重新打开替换或重建的文件。优先于 -f。",
                ),
                (
                    "sleep_interval",
                    "轮询间隔（秒）；文件系统通知可以提前唤醒。",
                ),
                ("quiet", "不输出文件名标题。"),
                ("verbose", "始终输出文件名标题。"),
                ("files", "要读取的文件；多个文件之间使用标题分隔。"),
                ("completions", "生成指定 Shell 的补全脚本并退出。"),
                ("manpage", "生成 man(1) 手册并退出。"),
                ("build_info", "输出 JSON 格式的构建目标和可用格式信息。"),
                (
                    "color",
                    "标题和诊断的颜色：auto 自动、always 始终、never 禁用。",
                ),
                (
                    "language",
                    "界面语言；默认按 LC_ALL、LC_MESSAGES、LANG 检测。",
                ),
            ] {
                command = command.mut_arg(id, |arg| {
                    arg.help(help)
                        .long_help(help)
                        .help_heading(if id == "files" { "参数" } else { "选项" })
                });
            }
            command = command
                .disable_help_flag(true)
                .disable_version_flag(true)
                .arg(
                    clap::Arg::new("help")
                        .short('h')
                        .long("help")
                        .action(clap::ArgAction::Help)
                        .help("显示帮助信息")
                        .help_heading("选项"),
                )
                .arg(
                    clap::Arg::new("version")
                        .short('V')
                        .long("version")
                        .action(clap::ArgAction::Version)
                        .help("显示版本信息")
                        .help_heading("选项"),
                );
        }
        command
    }
}

// Bootstrap presentation before clap handles --help or reports a parse error.
// Consume option values and respect --, so file names cannot change the UI.
fn bootstrap(args: &[std::ffi::OsString]) -> (Language, Color, bool) {
    let mut language = Language::detect();
    let mut color = Color::Auto;
    let mut help = false;
    let mut iter = args.iter().skip(1);
    while let Some(arg) = iter.next() {
        let raw = arg.to_string_lossy();
        if raw == "--" {
            break;
        }
        if raw == "--language" || raw.starts_with("--language=") {
            let value = raw
                .strip_prefix("--language=")
                .map(str::to_owned)
                .or_else(|| iter.next().map(|v| v.to_string_lossy().into_owned()));
            if let Some(parsed) =
                value.and_then(|v| <Language as clap::ValueEnum>::from_str(&v, false).ok())
            {
                language = parsed;
            }
        } else if raw == "--color" || raw.starts_with("--color=") {
            let value = raw
                .strip_prefix("--color=")
                .map(str::to_owned)
                .or_else(|| iter.next().map(|v| v.to_string_lossy().into_owned()));
            if let Some(parsed) =
                value.and_then(|v| <Color as clap::ValueEnum>::from_str(&v, false).ok())
            {
                color = parsed;
            }
        } else if ["--lines", "--sleep-interval", "--completions"].contains(&raw.as_ref()) {
            iter.next();
        } else if raw.starts_with('-') && !raw.starts_with("--") {
            for (i, flag) in raw.char_indices().skip(1) {
                if flag == 'n' || flag == 's' {
                    if i + 1 == raw.len() {
                        iter.next();
                    }
                    break;
                }
                help |= flag == 'h' || flag == 'V';
            }
        } else {
            help |= raw == "--help" || raw == "--version";
        }
    }
    (language, color, help)
}
