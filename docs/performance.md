# I/O hardening and performance validation

Measured on November 14, 2026. Baseline: commit `b0a5d92`; candidate: the I/O
hardening changes in this working tree. Both binaries used Rust 1.85.0, default
features, and the repository's release profile (LTO, one codegen unit).

Environment: Intel Core i7-10870H, x86_64 Linux
`6.18.58.1-microsoft-standard-WSL2`. Executables were in the Windows-mounted
workspace; generated inputs were on Linux `/tmp`. Results are local warm-cache
measurements, not production latency or a promise about other filesystems.

## Method

Run `benches/io_scenarios.py --bin /path/to/candidate --baseline /path/to/baseline --runs 7`.
The script hashes all expected output before timing ordinary reads. Follow
measurements use a flushed header as a readiness handshake, append actual bytes,
hash the bytes received through stdout, then check clean SIGINT exit (130).

- Times below are medians of seven runs.
- Ordinary read times include process/GNU-time startup and output to `/dev/null`.
- Follow times cover append through receipt of all bytes by the pipe consumer,
  excluding startup and shutdown; consumer and writer overhead is included.
- Peak RSS is the maximum of seven independently measured child peaks in MiB.
  GNU `/usr/bin/time` runs from its own small executable image before forking
  ztail. This avoids reporting the Python parent's pre-exec RSS high-water mark
  as the candidate's memory consumption. Its own RSS is not included.
- Invalid baseline output is not timed or counted as a speedup.

## Results

| Scenario | Baseline ms | Candidate ms | Baseline peak MiB | Candidate peak MiB | Speed ratio |
| --- | ---: | ---: | ---: | ---: | ---: |
| plain-small | 11.970 | 12.340 | 2.80 | 2.82 | 0.97x |
| plain-large-tail | invalid output | 12.576 | — | 2.93 | not comparable |
| gzip-normal | 26.689 | 17.624 | 15.18 | 3.09 | 1.51x |
| gzip-stored-over-8MiB | invalid output | 18.057 | — | 3.54 | not comparable |
| gzip-high-expansion | 120.152 | 37.143 | 64.26 | 3.43 | 3.23x |
| bulk-stdout | 69.623 | 32.095 | 22.22 | 22.39 | 2.17x |
| long-record | 18.153 | 15.677 | 10.93 | 7.92 | 1.16x |
| follow-tiny-plain | 40.113 | 3.509 | 8.48 | 3.48 | 11.43x |
| follow-high-expansion-gzip | 397.027 | 49.834 | 66.84 | 4.17 | 7.97x |

Small-file timing increased by 0.37 ms in this run; this is a startup-dominated
case, not evidence that every workload is faster. Bulk initial output retains
250,000 explicitly requested records, so its RSS is still dominated by that
ring rather than by decoder buffering. Neither a huge `-n` nor many zstd files
has a fixed whole-process memory bound.

Fixtures include 100-line small input; 1,000,000-line large plain input;
200,000-line normal gzip; 160,000-line stored gzip (compressed size exceeds
8 MiB); 1,000,000-line high-expansion gzip; 250,000-record bulk stdout;
a 4 MiB newline-terminated record; 100,000 tiny plain follow records; and
1,000,000 compressed follow records. The script defines exact fixture bytes.

The old large-plain case returned success but selected the tail of only the
first 8 MiB. The old stored-gzip case reported a complete archive as truncated.
Both now produce the validated last ten records.

## Correctness and tooling

Executed in the isolated WSL Rust 1.85.0 toolchain:

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets --all-features
cargo test --locked --all-targets
cargo test --locked --doc --all-features
cargo build --locked --release
```

All passed: 65 library + 24 CLI + 1 public-API property tests with all features;
60 + 24 + 1 with default features. Both all-target runs also exercised the six
Criterion cases in smoke-test mode, not a full Criterion timing run.

Regressions cover >8 MiB initial input, `-n 0`, arbitrary compressed chunk and
output boundaries, high expansion ratios, header CRCs, failed decoder scratch
buffers, zstd history-window limits, bounded tiny-line batching, deterministic
1 MiB records, cancellation (including late appearance), nonblocking opens,
stdout failures, hostile file names, and draining rotation/unlink over multiple
polls. An active old writer is capped at a frozen EOF so switching can complete.

This is not a complete security audit or a fuzzing campaign. CRCs do not
authenticate data; payloads remain raw terminal bytes; blocking output can delay
signals; concurrent rewrites cannot be made atomic by the fingerprint scheme.
See the README's I/O safety section for the supported threat boundaries.
