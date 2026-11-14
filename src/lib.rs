//! Incremental `tail` for gzip, plain text, and (optionally) zstd files.
//!
//! The original Crystal program re-opened and fully re-decompressed the file on
//! every 500 ms poll. This crate keeps the file descriptor open, remembers the
//! byte offset, and feeds only newly appended bytes into a persistent streaming
//! decoder, so each poll is O(new bytes).

mod app;
mod build_info;
mod cli;
mod decode;
mod gzip;
mod lines;
mod manual;
mod presentation;
mod tailfile;
mod watch;

pub use app::run;
pub use tailfile::TailFile;

/// Keep attacker-controlled file names from injecting terminal controls or
/// forged multi-line diagnostics. Log payloads remain byte-for-byte output.
fn display_path(path: &std::path::Path) -> String {
    path.to_string_lossy().escape_debug().to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn file_names_cannot_inject_terminal_controls() {
        let path = std::path::Path::new("normal\nforged\x1b[31m\t.log");
        let displayed = super::display_path(path);
        assert!(!displayed.chars().any(char::is_control));
        assert!(displayed.contains("\\n"));
        assert!(displayed.contains("\\u{1b}"));
        assert_eq!(
            super::display_path(std::path::Path::new("日志.gz")),
            "日志.gz"
        );
    }
}
