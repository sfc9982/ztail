# ztail

`tail` for gzip files that does **not** re-decompress the file on every poll.

The original Crystal program (branch [`cursor/legacy-crystal-1065`](https://github.com/sfc9982/ztail/tree/cursor/legacy-crystal-1065)) opened the file, inflated it from the first byte, and repeated that every 500 ms. CPU and I/O were proportional to the whole file, forever. This Rust version keeps the file open, remembers the byte offset, and feeds only newly appended bytes into a persistent streaming inflater (`flate2`'s low-level `Decompress`, on the pure-Rust `zlib-rs` backend). Each poll is O(new bytes).

Following is **off by default**, matching GNU `tail`. The old program always followed. Pass `-f` or `-F` when you want that.

## Build

```bash
cargo build --release
# binary: target/release/ztail
```

Static musl build (what CI produces):

```bash
rustup target add x86_64-unknown-linux-musl
sudo apt-get install musl-tools
cargo build --release --target x86_64-unknown-linux-musl
```

Optional zstd support (links the bundled libzstd; not part of the default static build):

```bash
cargo build --release --features zstd
```

xz is not supported. The usual decoders wrap liblzma, which fights a small static musl binary, and a correct streaming xz decoder is not a trivial addition.

## Usage

```bash
ztail app.log.gz                 # last 10 lines, then exit
ztail -n 50 app.log.gz           # last 50 lines, then exit
ztail -f app.log.gz              # follow the open file descriptor
ztail -F -s 0.5 app.log.gz       # follow the name; poll every 0.5s if inotify is quiet
ztail -f a.gz b.log              # several files, with ==> name <== headers
ztail -q -f a.gz b.log           # same, without headers
```

| Flag | Meaning |
| --- | --- |
| `-n N` | Last N complete lines (default 10). N=0 prints nothing from the existing file. There is no special case for short files: 3 lines with `-n 10` prints 3, and 15 lines with the default prints the last 10, not all 15. |
| `-f` | Follow the file descriptor. Appends are shown. In-place truncation (the file shrinks) restarts the decoder from offset 0. A rename/rotation is **not** followed; the original inode is. |
| `-F` | Follow the path. Implies `-f`, and also reopens when the device/inode changes or the file is recreated. If the path is missing at startup, ztail waits for it. |
| `-s SECS` | Fallback poll interval (default 1.0). On Linux, inotify wakes ztail as soon as the directory changes; `-s` is the backup when an event is missed. |
| `-q` / `-v` | Never / always print `==> name <==` headers. Headers are automatic when more than one file is given. |

A line with no trailing newline is held until the newline arrives, including in the initial view. Records reaching 1 MiB are force-emitted in 1 MiB segments (without inserting newlines), independent of read boundaries; these segments count toward `-n`. That way follow mode does not print the tail of a line and then print it again when the rest shows up.

Ctrl+C exits 130. Closing the stdout pipe (for example `ztail file | head`) exits 0 instead of panicking.

## Why a live `gzip` writer looks stuck

`ztail` can only decode bytes that are already in the file. The `gzip` format is a DEFLATE bitstream plus a checksum that sits **after** the compressed data. A `gzip` process keeps that bitstream in its own buffer and writes it out on flush, or when the member is finished. Until then the file often contains only a header, and there is nothing new for an incremental inflater to emit.

That lag is the format, not the tailer. `gzip >> file` (a new member concatenated on the end) is visible as soon as that member is written; ztail resets the inflater at each member boundary and keeps going. A writer that flushes a finished DEFLATE block without ending the member is visible too, because the streaming inflater does not need to rewind.

## Design

1. **Initial view.** Compressed files are scanned once to a snapshot EOF, regardless of the follow budget. Plain files first search backwards (up to 1 MiB) for the last N complete lines; if that is not sufficient, they use the forward scan. Decompressed bytes are split into records and stored in a ring (`VecDeque`) of the last N records, reusing evicted buffers. Only the requested tail and a bounded decoder output block are retained.
2. **Persistent inflate state.** A `flate2::Decompress` (raw DEFLATE, `zlib_header = false`) stays alive across polls. Gzip headers, optional name/comment/extra fields, and the CRC32/ISIZE trailer are parsed around it. On `StreamEnd`, the inflater is reset and the next member is parsed. Unparsed tail bytes stay in a small buffer so the file offset can advance by everything we have already read.
3. **Follow.** `-f` fstats the open descriptor. `-F` also stats the path and compares device and inode. Truncation is `size < offset`; rotation is a different device/inode. Either one seeks back to the start (or reopens) and resets the decoder. A 64-byte fingerprint behind the cursor also detects many truncate-and-regrow cases. Old descriptors are drained to a frozen EOF across bounded polls before switching to a replacement, so an active old writer cannot postpone the switch forever.
4. **Wakeups.** The parent directory is watched with the `notify` crate, which uses inotify on Linux. The follow loop also wakes at least every `-s` seconds.
5. **Formats.** Magic bytes select gzip (`1f 8b`), zstd (`28 b5 2f fd`, feature `zstd`), or plain text. An xz magic is reported and refused.

## I/O safety and resource limits

- Only regular files are accepted. Both initial open and reopen use
  `O_NONBLOCK | O_NOCTTY` and validate the opened descriptor, so replacing a
  checked path with a FIFO does not leave `open` waiting for a writer. Symlinks
  to regular files remain supported; this is not a path-access sandbox.
- Gzip validates header flags, optional FHCRC, and each member's CRC32/ISIZE.
  Non-following reads reject incomplete archives. Follow mode waits for more
  bytes instead. Output can precede trailer verification: CRC is corruption
  detection, **not authentication**, and output is not transactional.
- Inflaters yield in 64 KiB output steps; a bounded follow consume reads at
  most 8 MiB of input or produces about 1 MiB of decoded bytes. Pending input and inflater
  output are resumed before new input is read. Output batches can also include
  a previously buffered partial record (up to 1 MiB). Budgets preserve fairness
  between files; they are not data-size limits and never mean archive EOF.
  A replacement poll can finish an old-file batch and begin a new-file batch.
  A late-appearing file's initial last-N view, like startup, scans to snapshot
  EOF with cancellation checks rather than applying the follow budget.
- zstd history windows are capped at 64 MiB; frames requiring larger windows
  are rejected rather than allowed to allocate arbitrarily large histories.
- Each incomplete record is capped at 1 MiB. The initial ring still scales
  with the requested `-n` and record lengths; an enormous `-n` can require large
  amounts of memory. There is no fixed whole-process RSS limit. Compressed
  initial views still require work proportional to the decoded history.
- Initial scans check cancellation between decoder steps. The CLI coalesces tiny follow records without allocating one vector per line. Stdout is batched
  in a 64 KiB buffer and flushed after each update; slow readers apply normal
  blocking pipe backpressure and can delay signal handling. Broken pipes
  exit successfully; other output failures exit nonzero.
- File names in headers and diagnostics escape control characters to prevent
  forged lines or terminal escape injection. **Log payloads remain raw bytes**,
  as with `tail`; do not display untrusted logs directly on a sensitive terminal.
- Concurrent replacement/truncation is best-effort, not an atomic snapshot.
  Positional fingerprint reads do not disturb the data cursor, and a racing
  shrink is retried as truncation. Changes outside the fingerprint, or changes
  preserving size/mtime and fingerprint bytes, can go undetected. Intermediate
  rotations and writes to an old inode after it is closed can also be missed.

## Performance checks

```bash
cargo bench --bench scan
# End-to-end release timings and per-process peak RSS (Linux; requires GNU /usr/bin/time):
cargo build --release
python3 benches/io_scenarios.py --bin target/release/ztail
# Optional comparison; invalid baseline output is reported, never timed as a win:
python3 benches/io_scenarios.py --bin target/release/ztail --baseline /path/to/old/ztail
```

Scenarios include small/large plain logs, compressed inputs beyond 8 MiB,
normal compressed logs, high expansion ratios, long records, bulk stdout, and
real follow appends through a pipe (including very short records).
The script validates output before timing and reports medians and peak RSS.
Filesystem/cache/pipe conditions affect results; use the same machine and
build profile for comparisons. Large appends and rotation are additionally
covered by deterministic budget/progress regression tests. See
[local validation results and measurement limits](docs/performance.md).

## Tests

```bash
cargo test
cargo test --all-features   # includes the zstd decoder
```

Additional regressions cover input files larger than the 8 MiB poll budget, bounded high-ratio gzip/zstd decoding, multi-poll rotation draining, cancellation, hostile zstd windows, optional gzip header CRCs, and chunk-independent long records.

Unit tests cover single-member and multi-member gzip (including one-byte splits and a payload larger than the inflate output buffer), partial lines, truncation, rotation vs follow-by-descriptor, last-N when the file has fewer lines than N, and a large file whose second poll feeds only the appended member. Property tests exercise arbitrary chunk boundaries in line splitting, gzip/zstd decoding, and the public `TailFile` append path. Integration tests cover headers, exit codes, SIGPIPE, follow, rotation, truncation, and SIGINT.

Coverage needs `cargo-llvm-cov` and the `llvm-tools-preview` component:

```bash
cargo llvm-cov --all-features --lcov --output-path lcov.info
```

## Legacy

The original Crystal implementation is preserved on branch
[`cursor/legacy-crystal-1065`](https://github.com/sfc9982/ztail/tree/cursor/legacy-crystal-1065)
(`ztail.cr`). It is not part of this tree.


## Shell integration, colors, and language

```sh
ztail --completions bash       # also: zsh, fish, elvish, powershell
ztail --language zh-CN --help  # English: --language en
ztail --color never -v app.log.gz
```

Auto color detects stdout/stderr independently; pipes stay plain by default.
`NO_COLOR` and CLICOLOR conventions are supported. Colors only affect UI text,
never log payloads. The language defaults to `LC_ALL`, `LC_MESSAGES`, then `LANG`.
See [Shell installation and localization](docs/shells-and-localization.md) for
installation instructions and the exact precedence rules / 中文说明。

## Installation and release status

Source install on Linux (Rust 1.85+):

```sh
cargo install --path . --locked                 # gzip + plain
cargo install --path . --locked --features zstd # adds zstd (C toolchain required)
```

Native Windows is not supported; use WSL. PowerShell completions are intended for
`pwsh` on Unix/WSL. Do not assume GitHub binaries or a crates.io package already
exist: this change prepares releases; it does not publish them.

The release workflow builds static x86_64 Linux musl (minimal) and x86_64 Linux
glibc (zstd) archives with all completion scripts, docs, and SHA-256 checksums.
It only creates **draft** releases after tag/version/license checks and tests.
**A maintainer must choose a distribution license before release:** no license
is currently declared. See [release checklist](docs/releasing.md) and
[unreleased changes](CHANGELOG.md).


## Offline manual and Linux integration

```sh
ztail --manpage > ztail.1
man -l ztail.1
ztail --build-info             # JSON: version, target triple, enabled formats
make install                  # binary + manpage + completions in ~/.local
# Package-manager staging (no system paths are written):
make install PREFIX=/usr DESTDIR="$PWD/target/staging"
```

Use `make install CARGO_FEATURES=--all-features` for zstd. `make uninstall` accepts
the same prefix/staging variables and removes only the installed files. Generators
never add colors or read log operands. Exit codes: 0 success/closed downstream
pipe; 1 file/format/output or missing-operand error; 2 argument error; 130/143 for
SIGINT/SIGTERM while following. Only regular files are supported; no stdin or FIFO
mode is implied by the GNU-tail-like options.

See [Contributing](CONTRIBUTING.md), [Security policy](SECURITY.md), and
[comparison with mature Rust CLIs](docs/project-conventions.md). Dependency
advisories and pinned GitHub Actions are maintained separately from core decoding
logic. The absence of known advisories is not a security guarantee.
