# Contributing to ztail

Discuss substantial behavior or format changes in an issue before implementation.
Small fixes should include a focused regression test. Explain the observable
behavior, not just an internal refactor, and update CHANGELOG.md when it changes.
Review all contributed/generated code yourself; do not submit private log data.

## Development environment

Use Linux, or Linux inside WSL, with Rust 1.85 or newer and a C toolchain for the
optional zstd feature. Native Windows is not supported. CI checks both the minimal
and zstd configurations. A newer developer tool must not silently raise the crate's
MSRV. Keep Cargo.lock checked in and review dependency changes separately.

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
cargo test --locked --all-targets --all-features
cargo test --locked --doc --all-features
cargo +1.85.0 check --locked --all-targets --all-features
python3 -m unittest discover -s scripts -p 'test_*.py'
```

Python scripts use only the standard library and require Python 3.11+. Install
rustfmt and Clippy for your toolchain. `cargo audit` is a separate development
tool, not a crate dependency: CI pins its version and uses current RustSec data.
Review advisory warnings as well as errors; do not add ignores just to make CI pass.

## Tests and performance

- Decoder tests belong next to the decoder; exercise arbitrary chunk boundaries,
  concatenated members, partial headers, corruption, and bounded output.
- Black-box CLI tests belong in tests/cli.rs. Assert exact stdout bytes, stderr,
  exit status, cancellation, pipes, and terminal behavior. Set locale/color only
  on subprocesses; avoid mutating the parallel test process environment.
- Follow tests must arrange readiness before append/rename and always reap child
  processes. Prefer a bounded observable wait over arbitrary long sleeps.
- Use synthetic data, not production logs or secrets. See docs/performance.md for
  the baseline and benches/io_scenarios.py for reproducible I/O experiments.
  Include output hashes and RSS as well as timing; a faster wrong result is not
  an optimization. Do not put hardware-dependent timing thresholds in unit tests.
- Manual/completion options must come from the same clap definition. Keep
  generated assets out of source control; use `make assets` for local inspection.

## Installation and release checks

`make install PREFIX=/usr DESTDIR=/absolute/staging/root` stages the executable,
manual, and completions without writing to /usr. `make uninstall` with the exact
same variables removes only managed files, leaving directories and unrelated files.
Do not run either with elevated privileges for a test. See docs/releasing.md for
archive verification, license requirements, and the separate approval to publish.

The repository uses LF line endings. The .gitattributes and .editorconfig files
keep shell scripts and source packages stable between Linux and Windows/WSL.
Do not normalize unrelated files or replace existing user changes in a patch.

For vulnerabilities, follow SECURITY.md instead of opening a public bug report.
