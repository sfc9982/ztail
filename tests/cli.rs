//! Black-box tests for the `ztail` binary: exit codes, headers, follow, signals.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
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
    if let Some(stdout) = child.stdout.as_ref() {
        set_fd_nonblocking(stdout.as_raw_fd());
    }
    if let Some(stderr) = child.stderr.as_ref() {
        set_fd_nonblocking(stderr.as_raw_fd());
    }
}

fn set_fd_nonblocking(fd: RawFd) {
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

fn wait_for(child: &mut Child, timeout: Duration, mut done: impl FnMut(&[u8]) -> bool) -> Vec<u8> {
    read_until(
        child.stdout.as_mut().expect("stdout is piped"),
        timeout,
        &mut done,
    )
}

fn wait_for_stderr(
    child: &mut Child,
    timeout: Duration,
    mut done: impl FnMut(&[u8]) -> bool,
) -> Vec<u8> {
    read_until(
        child.stderr.as_mut().expect("stderr is piped"),
        timeout,
        &mut done,
    )
}

fn read_until(
    stream: &mut impl Read,
    timeout: Duration,
    done: &mut impl FnMut(&[u8]) -> bool,
) -> Vec<u8> {
    let start = Instant::now();
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while start.elapsed() < timeout {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                got.extend_from_slice(&buf[..n]);
                if done(&got) {
                    break;
                }
            }
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::Interrupted =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => panic!("read stream: {err}"),
        }
    }
    got
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn interrupt(child: &mut ChildGuard) -> std::process::ExitStatus {
    let pid = child.inner().id() as i32;
    unsafe {
        assert_eq!(libc::kill(pid, libc::SIGINT), 0);
    }
    let status = child.inner().wait().unwrap();
    child.0.take();
    status
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

#[test]
fn zero_lines_prints_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zero.log");
    std::fs::write(&path, b"a\nb\n").unwrap();
    let output = Command::new(bin())
        .args(["-n", "0", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
}

#[test]
fn invalid_line_count_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.log");
    std::fs::write(&path, b"a\n").unwrap();
    let output = Command::new(bin())
        .args(["-n", "not-a-number", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("invalid value"), "{err}");
    assert!(err.contains("not-a-number"), "{err}");
    assert!(output.stdout.is_empty());
}

#[test]
fn verbose_prints_header() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("one.log");
    std::fs::write(&path, b"only\n").unwrap();
    let output = Command::new(bin())
        .args(["-v", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.starts_with(&format!("==> {} <==\n", path.display())),
        "{text}"
    );
    assert!(text.ends_with("only\n"), "{text}");
}

#[test]
fn corrupt_gzip_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt.gz");
    let mut blob = gz(b"good\n");
    // A complete second header with an illegal compression method: the first
    // member decodes, then the format error stops the initial read.
    blob.extend_from_slice(&[0x1f, 0x8b, 0, 0, 0, 0, 0, 0, 0, 255]);
    std::fs::write(&path, &blob).unwrap();

    let output = Command::new(bin())
        .arg(path.to_str().unwrap())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        contains(&output.stderr, b"unsupported gzip compression method"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // `read_initial` drops lines decoded before a format error; this pins the
    // current behavior (the unit-level consume() keeps them).
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
}

#[test]
fn follow_gzip_new_member() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.gz");
    std::fs::write(&path, gz(b"one\ntwo\n")).unwrap();

    let mut child = ChildGuard::new(
        Command::new(bin())
            .args(["-f", "-s", "0.2", "-n", "10", path.to_str().unwrap()])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    set_nonblocking(child.inner());
    let initial = wait_for(child.inner(), Duration::from_secs(2), |out| {
        out.ends_with(b"one\ntwo\n")
    });
    assert!(
        contains(&initial, b"one\ntwo\n"),
        "{}",
        String::from_utf8_lossy(&initial)
    );

    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&gz(b"three\nfour\n"))
        .unwrap();
    let more = wait_for(child.inner(), Duration::from_secs(3), |out| {
        out.ends_with(b"three\nfour\n")
    });
    assert!(
        contains(&more, b"three\nfour\n"),
        "follow did not see the new gzip member: {}",
        String::from_utf8_lossy(&more)
    );

    let status = interrupt(&mut child);
    assert_eq!(status.code(), Some(130));
}

#[test]
fn follow_truncation_rewinds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trunc.log");
    std::fs::write(&path, b"old-1\nold-2\n").unwrap();

    let mut child = ChildGuard::new(
        Command::new(bin())
            .args(["-f", "-s", "0.2", "-n", "10", path.to_str().unwrap()])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    set_nonblocking(child.inner());
    let initial = wait_for(child.inner(), Duration::from_secs(2), |out| {
        out.ends_with(b"old-2\n")
    });
    assert!(
        contains(&initial, b"old-1\nold-2\n"),
        "{}",
        String::from_utf8_lossy(&initial)
    );

    std::fs::write(&path, b"new-1\n").unwrap();
    let more = wait_for(child.inner(), Duration::from_secs(3), |out| {
        out.ends_with(b"new-1\n")
    });
    assert!(
        contains(&more, b"new-1\n"),
        "truncation did not restart the decoder: {}",
        String::from_utf8_lossy(&more)
    );
    let err = wait_for_stderr(child.inner(), Duration::from_secs(2), |err| {
        contains(err, b"file truncated")
    });
    assert!(
        contains(&err, b"file truncated"),
        "{}",
        String::from_utf8_lossy(&err)
    );

    let status = interrupt(&mut child);
    assert_eq!(status.code(), Some(130));
}

#[test]
fn follow_name_survives_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rotate.log");
    std::fs::write(&path, b"old\n").unwrap();

    let mut child = ChildGuard::new(
        Command::new(bin())
            .args(["-F", "-s", "0.2", "-n", "10", path.to_str().unwrap()])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    set_nonblocking(child.inner());
    let initial = wait_for(child.inner(), Duration::from_secs(2), |out| {
        out.ends_with(b"old\n")
    });
    assert!(
        contains(&initial, b"old\n"),
        "{}",
        String::from_utf8_lossy(&initial)
    );

    let rotated = dir.path().join("rotate.log.1");
    std::fs::rename(&path, &rotated).unwrap();
    std::fs::write(&path, b"new-a\nnew-b\n").unwrap();

    let more = wait_for(child.inner(), Duration::from_secs(3), |out| {
        out.ends_with(b"new-a\nnew-b\n")
    });
    assert!(
        contains(&more, b"new-a\nnew-b\n"),
        "rotation was not followed: {}",
        String::from_utf8_lossy(&more)
    );
    assert!(
        child.inner().try_wait().unwrap().is_none(),
        "ztail exited after rotation"
    );

    let status = interrupt(&mut child);
    assert_eq!(status.code(), Some(130));
}

#[test]
fn follow_name_waits_for_appearance() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("later.log");

    let mut child = ChildGuard::new(
        Command::new(bin())
            .args(["-F", "-s", "0.2", path.to_str().unwrap()])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    set_nonblocking(child.inner());
    let err = wait_for_stderr(child.inner(), Duration::from_secs(2), |err| {
        contains(err, b"waiting for it to appear")
    });
    assert!(
        contains(&err, b"waiting for it to appear"),
        "{}",
        String::from_utf8_lossy(&err)
    );
    assert!(
        child.inner().try_wait().unwrap().is_none(),
        "ztail exited while waiting for the path"
    );

    std::fs::write(&path, b"one\ntwo\n").unwrap();
    let output = wait_for(child.inner(), Duration::from_secs(3), |out| {
        out.ends_with(b"one\ntwo\n")
    });
    assert!(
        contains(&output, b"one\ntwo\n"),
        "the file that appeared was not read: {}",
        String::from_utf8_lossy(&output)
    );
    assert!(
        child.inner().try_wait().unwrap().is_none(),
        "ztail exited after the file appeared"
    );

    let status = interrupt(&mut child);
    assert_eq!(status.code(), Some(130));
}

#[cfg(feature = "zstd")]
#[test]
fn zstd_file_decodes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.log.zst");
    let frame = zstd::encode_all(&b"z0\nz1\nz2\n"[..], 1).unwrap();
    std::fs::write(&path, frame).unwrap();

    let output = Command::new(bin())
        .args(["-n", "2", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"z1\nz2\n");
}

#[cfg(not(feature = "zstd"))]
#[test]
fn zstd_file_without_feature_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.log.zst");
    std::fs::write(&path, [0x28, 0xb5, 0x2f, 0xfd]).unwrap();

    let output = Command::new(bin())
        .arg(path.to_str().unwrap())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        contains(&output.stderr, b"without zstd support"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn headers_and_diagnostics_escape_hostile_file_names() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unsafe\nforged\x1b[31m.log");
    std::fs::write(&path, b"safe payload\n").unwrap();
    let output = Command::new(bin()).arg("-v").arg(&path).output().unwrap();
    assert!(output.status.success());
    assert!(!output.stdout.contains(&0x1b));
    assert_eq!(output.stdout.iter().filter(|&&b| b == b'\n').count(), 2);
    assert!(contains(&output.stdout, b"\\nforged\\u{1b}[31m.log"));
    std::fs::remove_file(&path).unwrap();
    let output = Command::new(bin()).arg(&path).output().unwrap();
    assert!(!output.status.success());
    assert!(!output.stderr.contains(&0x1b));
    assert_eq!(output.stderr.iter().filter(|&&b| b == b'\n').count(), 1);
}

#[test]
fn non_broken_pipe_output_errors_exit_nonzero() {
    if !std::path::Path::new("/dev/full").exists() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data");
    std::fs::write(&path, b"hello\n").unwrap();
    let output = Command::new(bin())
        .arg(&path)
        .stdout(
            std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .unwrap(),
        )
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(contains(&output.stderr, b"write error"));
}

// Do not modify the test process environment: these subprocesses are isolated.
fn ui_command() -> Command {
    let mut command = Command::new(bin());
    for name in [
        "LC_ALL",
        "LC_MESSAGES",
        "LANG",
        "NO_COLOR",
        "CLICOLOR",
        "CLICOLOR_FORCE",
        "TERM",
    ] {
        command.env_remove(name);
    }
    command.env("LC_ALL", "C");
    command
}

#[test]
fn completions_for_all_supported_shells() {
    for (shell, marker) in [
        ("bash", "complete"),
        ("zsh", "#compdef ztail"),
        ("fish", "complete -c ztail"),
        ("elvish", "edit:completion:arg-completer"),
        ("powershell", "Register-ArgumentCompleter"),
    ] {
        let output = ui_command()
            .args(["--color", "always", "--completions", shell])
            .output()
            .unwrap();
        assert!(output.status.success(), "{shell}: {:?}", output.stderr);
        let script = String::from_utf8(output.stdout).unwrap();
        assert!(script.contains(marker), "{shell}: {script}");
        assert!(script.contains("language"), "{shell}: missing new options");
        assert!(!script.contains('\x1b'));
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn localized_help_and_runtime_messages() {
    let output = ui_command()
        .args(["--language=zh-CN", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("用法："), "{help}");
    assert!(help.contains("补全脚本"), "{help}");
    assert!(help.contains("选项"), "{help}");
    let output = ui_command().args(["--language", "zh-CN"]).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("缺少文件参数"));
    let output = ui_command()
        .args(["--language", "zh-CN", "--unknown"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("命令行参数错误"));
}

#[test]
fn locale_environment_precedence_and_explicit_override() {
    for (all, messages, lang, explicit, chinese) in [
        ("zh_CN.UTF-8", "C", "C", false, true),
        ("C", "zh_CN.UTF-8", "zh_CN.UTF-8", false, false),
        ("", "zh_CN.UTF-8", "C", false, true),
        ("", "", "zh_SG.UTF-8", false, true),
        ("", "", "fr_FR.UTF-8", false, false),
        ("zh_CN.UTF-8", "", "", true, false),
    ] {
        let mut command = ui_command();
        command
            .env("LC_ALL", all)
            .env("LC_MESSAGES", messages)
            .env("LANG", lang);
        if explicit {
            command.args(["--language", "en"]);
        }
        let output = command.arg("--help").output().unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).contains("用法："),
            chinese
        );
    }
}

#[test]
fn explicit_color_and_environment_policy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("raw.log");
    let raw = b"log \x1b[31mraw\x1b[0m\n";
    std::fs::write(&path, raw).unwrap();
    for (mode, no_color, force, expected) in [
        ("auto", "", "", false),
        ("auto", "1", "1", false),
        ("auto", "", "1", true),
        ("always", "1", "", true),
        ("never", "", "1", false),
    ] {
        let output = ui_command()
            .env("NO_COLOR", no_color)
            .env("CLICOLOR_FORCE", force)
            .args(["--color", mode, "-v"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(output.stdout.ends_with(raw), "payload was modified");
        assert_eq!(output.stdout.starts_with(b"\x1b[1;36m"), expected);
        let diagnostic = ui_command()
            .env("NO_COLOR", no_color)
            .env("CLICOLOR_FORCE", force)
            .args(["--color", mode])
            .output()
            .unwrap();
        assert_eq!(diagnostic.stderr.starts_with(b"\x1b[33m"), expected);
    }
    let output = ui_command()
        .args(["--color=always", "-q"])
        .arg(path)
        .output()
        .unwrap();
    assert_eq!(output.stdout, raw);
}

#[test]
fn help_colors_are_explicit_and_disabled_in_pipes() {
    for (mode, expected) in [("always", true), ("never", false), ("auto", false)] {
        let output = ui_command()
            .args(["--color", mode, "--help"])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.contains(&0x1b), expected);
    }
}

#[test]
fn presentation_flags_do_not_become_subcommands_or_consume_filenames() {
    let dir = tempfile::tempdir().unwrap();
    for filename in ["completions", "--language=zh-CN", "--help"] {
        std::fs::write(dir.path().join(filename), b"ok\n").unwrap();
        let output = ui_command()
            .current_dir(dir.path())
            .args(["--", filename])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"ok\n");
    }
    // A value belonging to -n must not change bootstrap presentation.
    let output = ui_command()
        .args(["-vn", "--language=zh-CN"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("命令行参数错误"));
}

#[test]
fn invalid_presentation_values_exit_two() {
    for args in [
        ["--language", "xx"],
        ["--color", "rainbow"],
        ["--completions", "cmd"],
    ] {
        let output = ui_command().args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn parser_diagnostics_cannot_inject_ansi() {
    let output = ui_command()
        .args(["--color=never", "--unknown\x1b[31m"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(!output.stderr.contains(&0x1b));
}

fn pseudo_terminal() -> (std::fs::File, std::fs::File) {
    use std::os::fd::FromRawFd;
    let (mut master, mut slave) = (-1, -1);
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(result, 0, "openpty: {}", std::io::Error::last_os_error());
    // openpty created two fresh, independently owned descriptors.
    unsafe {
        (
            std::fs::File::from_raw_fd(master),
            std::fs::File::from_raw_fd(slave),
        )
    }
}

fn read_terminal(mut master: std::fs::File) -> Vec<u8> {
    let mut data = Vec::new();
    // Linux signals EIO when all slave handles close; bytes read before EIO
    // remain in data. BSD systems may return EOF instead.
    if let Err(err) = master.read_to_end(&mut data) {
        assert_eq!(err.raw_os_error(), Some(libc::EIO));
    }
    data
}

#[test]
fn auto_color_detects_each_terminal_stream_independently() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("terminal.log");
    std::fs::write(&path, b"payload\n").unwrap();
    for (term, no_color, expected) in [
        ("xterm-256color", "", true),
        ("dumb", "", false),
        ("xterm", "1", false),
    ] {
        let (master, slave) = pseudo_terminal();
        let mut command = ui_command();
        command
            .env("TERM", term)
            .env("NO_COLOR", no_color)
            .arg("-v")
            .arg(&path)
            .arg(dir.path().join("missing"))
            .stdout(Stdio::from(slave));
        let output = command.output().unwrap();
        drop(command);
        let terminal = read_terminal(master);
        assert_eq!(terminal.starts_with(b"\x1b[1;36m"), expected);
        assert!(
            !output.stderr.contains(&0x1b),
            "piped stderr must remain plain"
        );
        let (master, slave) = pseudo_terminal();
        let mut command = ui_command();
        command
            .env("TERM", term)
            .env("NO_COLOR", no_color)
            .arg("-v")
            .arg(&path)
            .arg(dir.path().join("missing"))
            .stderr(Stdio::from(slave));
        let output = command.output().unwrap();
        drop(command);
        assert_eq!(read_terminal(master).starts_with(b"\x1b[33m"), expected);
        assert!(
            !output.stdout.contains(&0x1b),
            "piped stdout must remain plain"
        );
    }
}

#[test]
fn help_and_completion_exit_cleanly_when_pipe_is_closed() {
    use std::os::fd::FromRawFd;
    for args in [
        vec!["--help"],
        vec!["--completions", "bash"],
        vec!["--manpage"],
        vec!["--build-info"],
        vec!["--language=zh-CN", "--help"],
    ] {
        let mut fds = [-1, -1];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // Close the read end before spawning, forcing EPIPE on the first write.
        let (read, write) = unsafe {
            (
                std::fs::File::from_raw_fd(fds[0]),
                std::fs::File::from_raw_fd(fds[1]),
            )
        };
        drop(read);
        let output = ui_command()
            .args(args)
            .stdout(Stdio::from(write))
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn build_info_is_machine_readable_and_matches_compiled_features() {
    let output = ui_command()
        .args(["--color=always", "--language=zh-CN", "--build-info"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert!(!output.stdout.contains(&0x1b));
    let info: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(info["schema_version"], 1);
    assert_eq!(info["name"], "ztail");
    assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(info["target"], env!("ZTAIL_BUILD_TARGET"));
    let mut expected = vec!["gzip", "plain"];
    if cfg!(feature = "zstd") {
        expected.push("zstd");
    }
    assert_eq!(info["features"], serde_json::json!(expected));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains(env!("CARGO_MANIFEST_DIR")));
}

#[test]
fn manpage_uses_cli_options_and_is_reproducible_without_colors() {
    for language in ["en", "zh-CN"] {
        let output = ui_command()
            .args(["--color=always", "--language", language, "--manpage"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        assert!(output.stderr.is_empty());
        assert!(!output.stdout.contains(&0x1b));
        let page = String::from_utf8(output.stdout.clone())
            .unwrap()
            .replace("\\-", "-")
            .replace("\\:", "");
        assert!(page.contains(".TH ZTAIL"), "{page}");
        for flag in [
            "--completions",
            "--build-info",
            "--manpage",
            "--language",
            "--follow-name",
        ] {
            assert!(page.contains(flag), "missing {flag}: {page}");
        }
        assert!(page.contains(if language == "en" {
            "EXIT STATUS"
        } else {
            "退出状态"
        }));
        let repeated = ui_command()
            .args(["--language", language, "--manpage"])
            .output()
            .unwrap();
        assert_eq!(output.stdout, repeated.stdout);
    }
}

#[test]
fn output_generators_are_mutually_exclusive() {
    for args in [
        vec!["--manpage", "--build-info"],
        vec!["--manpage", "--completions=bash"],
        vec!["--build-info", "--completions=bash"],
    ] {
        let output = ui_command().args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
}
