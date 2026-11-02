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

A line with no trailing newline is held until the newline arrives, including in the initial view. That way follow mode does not print the tail of a line and then print it again when the rest shows up.

Ctrl+C exits 130. Closing the stdout pipe (for example `ztail file | head`) exits 0 instead of panicking.

## Why a live `gzip` writer looks stuck

`ztail` can only decode bytes that are already in the file. The `gzip` format is a DEFLATE bitstream plus a checksum that sits **after** the compressed data. A `gzip` process keeps that bitstream in its own buffer and writes it out on flush, or when the member is finished. Until then the file often contains only a header, and there is nothing new for an incremental inflater to emit.

That lag is the format, not the tailer. `gzip >> file` (a new member concatenated on the end) is visible as soon as that member is written; ztail resets the inflater at each member boundary and keeps going. A writer that flushes a finished DEFLATE block without ending the member is visible too, because the streaming inflater does not need to rewind.

## Design

1. **One pass for the initial view.** The file is read from offset 0 in chunks. Decompressed bytes are split into lines and stored in a ring (`VecDeque`) of the last N lines. The decompressed text is not kept.
2. **Persistent inflate state.** A `flate2::Decompress` (raw DEFLATE, `zlib_header = false`) stays alive across polls. Gzip headers, optional name/comment/extra fields, and the CRC32/ISIZE trailer are parsed around it. On `StreamEnd`, the inflater is reset and the next member is parsed. Unparsed tail bytes stay in a small buffer so the file offset can advance by everything we have already read.
3. **Follow.** `-f` fstats the open descriptor. `-F` also stats the path and compares device and inode. Truncation is `size < offset`; rotation is a different device/inode. Either one seeks back to the start (or reopens) and resets the decoder.
4. **Wakeups.** The parent directory is watched with the `notify` crate, which uses inotify on Linux. The follow loop also wakes at least every `-s` seconds.
5. **Formats.** Magic bytes select gzip (`1f 8b`), zstd (`28 b5 2f fd`, feature `zstd`), or plain text. An xz magic is reported and refused.

## Tests

```bash
cargo test
cargo test --all-features   # includes the zstd decoder
```

Unit tests cover single-member and multi-member gzip (including one-byte splits and a payload larger than the inflate output buffer), partial lines, truncation, rotation vs follow-by-descriptor, last-N when the file has fewer lines than N, and a large file whose second poll feeds only the appended member. Integration tests cover headers, exit codes, SIGPIPE, follow, and SIGINT.

## Legacy

The original Crystal implementation is preserved on branch
[`cursor/legacy-crystal-1065`](https://github.com/sfc9982/ztail/tree/cursor/legacy-crystal-1065)
(`ztail.cr`). It is not part of this tree.
