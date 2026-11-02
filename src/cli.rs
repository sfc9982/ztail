use std::path::PathBuf;

use clap::Parser;

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
