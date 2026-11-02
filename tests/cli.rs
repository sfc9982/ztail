//! Black-box tests for the `ztail` binary: exit codes, headers, follow, signals.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use flate2::write::GzEncoder;
use flate2::Compression;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ztail")
}

fn gz(data: &[u8]) -> Vec<u8> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    fn inner(&mut self) -> &mut Child {
        self.0.as_mut().expect("child still running")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn set_nonblocking(child: &mut Child) {
    let fd = child.stdout.as_ref().unwrap().as_raw_fd();
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        assert!(flags >= 0, "F_GETFL failed");
        let rc = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        assert!(rc >= 0, "F_SETFL failed");
    }
}

fn read_for(child: &mut Child, timeout: Duration) -> Vec<u8> {
    let start = Instant::now();
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    let stdout = child.stdout.as_mut().unwrap();
    while start.elapsed() < timeout {
        match stdout.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::Interrupted =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => panic!("read stdout: {err}"),
        }
    }
    got
}

#[test]
fn missing_operand_exits_1() {
    let output = Command::new(bin()).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("missing file operand"), "{err}");
    assert!(!err.to_lowercase().contains("panic"), "{err}");
}

#[test]
fn missing_file_exits_1() {
    let output = Command::new(bin())
        .arg("this-file-does-not-exist.log")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("this-file-does-not-exist.log"), "{err}");
    assert!(output.stdout.is_empty());
}

#[test]
fn last_n_gzip_and_plain_without_follow_exits() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain.log");
    let gzipped = dir.path().join("app.log.gz");
    std::fs::write(&plain, b"p0\np1\np2\np3\np4\n").unwrap();
    let mut body = Vec::new();
    for i in 0..12 {
        writeln!(body, "g{i}").unwrap();
    }
    std::fs::write(&gzipped, gz(&body)).unwrap();

    let plain_out = Command::new(bin())
        .args(["-n", "2", plain.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(plain_out.status.success(), "{plain_out:?}");
    assert_eq!(plain_out.stdout, b"p3\np4\n");

    let gz_out = Command::new(bin())
        .args(["-n", "3", gzipped.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(gz_out.status.success());
    assert_eq!(gz_out.stdout, b"g9\ng10\ng11\n");
}

#[test]
fn fewer_than_n_lines_prints_all_of_them() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("short.log");
    std::fs::write(&path, b"only\ntwo\n").unwrap();
    let output = Command::new(bin())
        .args(["-n", "10", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"only\ntwo\n");
}

#[test]
fn fifteen_lines_is_not_printed_in_full() {
    // Crystal ztail printed the whole file when it held fewer than 20 lines.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fifteen.log");
    let mut body = Vec::new();
    for i in 0..15 {
        writeln!(body, "L{i}").unwrap();
    }
    std::fs::write(&path, &body).unwrap();
    let output = Command::new(bin())
        .arg(path.to_str().unwrap())
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 10);
    assert_eq!(lines[0], "L5");
    assert_eq!(lines[9], "L14");
}

#[test]
fn multiple_files_print_headers() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.log");
    let b = dir.path().join("b.log");
    std::fs::write(&a, b"alpha\n").unwrap();
    std::fs::write(&b, b"beta\n").unwrap();
    let output = Command::new(bin())
        .args([a.to_str().unwrap(), b.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        text,
        format!(
            "==> {} <==\nalpha\n\n==> {} <==\nbeta\n",
            a.display(),
            b.display()
        )
    );
}

#[test]
fn repeated_path_operands_each_get_a_header() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.log");
    std::fs::write(&a, b"alpha\n").unwrap();
    let path = a.to_str().unwrap();
    let output = Command::new(bin())
        .args(["-v", path, path])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        text,
        format!(
            "==> {} <==\nalpha\n\n==> {} <==\nalpha\n",
            a.display(),
            a.display()
        )
    );
}

#[test]
fn tiny_sleep_interval_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.log");
    std::fs::write(&path, b"a\n").unwrap();
    let output = Command::new(bin())
        .args(["-f", "-s", "1e-12", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert_ne!(output.status.code(), Some(0));
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains("sleep interval") || err.contains("too small"),
        "{err}"
    );
}

#[test]
fn quiet_suppresses_headers_and_one_missing_file_still_exits_1() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.log");
    std::fs::write(&a, b"alpha\n").unwrap();
    let output = Command::new(bin())
        .args([
            "-q",
            a.to_str().unwrap(),
            dir.path().join("missing.log").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"alpha\n");
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("missing.log"), "{err}");
}

#[test]
fn partial_trailing_line_is_not_printed_without_follow() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("partial.log");
    std::fs::write(&path, b"full\npartial").unwrap();
    let output = Command::new(bin())
        .arg(path.to_str().unwrap())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"full\n");
}

#[test]
fn sigpipe_does_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wide.log");
    let mut body = Vec::new();
    for i in 0..8_000 {
        writeln!(body, "line {i:04} {}", "y".repeat(64)).unwrap();
    }
    assert!(body.len() > 64 * 1024);
    std::fs::write(&path, &body).unwrap();

    let mut child = Command::new(bin())
        .args(["-n", "8000", path.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Close the read end so the next stdout write gets EPIPE.
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        !err.to_lowercase().contains("panic"),
        "stderr:\n{err}\nstatus: {:?}",
        output.status
    );
    assert!(
        output.status.success(),
        "expected a clean exit on SIGPIPE, got {:?} stderr {err}",
        output.status
    );
}

#[test]
fn follow_sees_append_and_sigint_exits_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("follow.log");
    std::fs::write(&path, b"one\ntwo\n").unwrap();

    let mut child = ChildGuard::new(
        Command::new(bin())
            .args(["-f", "-s", "0.2", "-n", "10", path.to_str().unwrap()])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    set_nonblocking(child.inner());
    let initial = read_for(child.inner(), Duration::from_secs(2));
    assert_eq!(
        initial,
        b"one\ntwo\n",
        "initial follow output was {}",
        String::from_utf8_lossy(&initial)
    );

    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"three\n")
        .unwrap();
    let more = read_for(child.inner(), Duration::from_secs(3));
    assert!(
        more.windows(6).any(|w| w == b"three\n"),
        "follow did not see append: {}",
        String::from_utf8_lossy(&more)
    );

    let pid = child.inner().id() as i32;
    unsafe {
        assert_eq!(libc::kill(pid, libc::SIGINT), 0);
    }
    let status = child.inner().wait().unwrap();
    child.0.take();
    assert_eq!(
        status.code(),
        Some(130),
        "SIGINT should exit 130, got {status:?}"
    );
}

#[test]
fn help_mentions_follow_is_opt_in() {
    let output = Command::new(bin()).arg("--help").output().unwrap();
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("exits") || text.contains("like GNU tail"),
        "{text}"
    );
    assert!(text.contains("-f") || text.contains("--follow"), "{text}");
}
