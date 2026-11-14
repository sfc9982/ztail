# Release checklist

## Current blocker: license

This repository has no distribution license yet. The copyright holder must choose
one, add the full `LICENSE` text (or `LICENSE-*` files), and set `[package].license`
to its SPDX expression. Packaging and the release workflow intentionally fail
without both. Do not publish the crate or distribute development archives before
this is resolved. `--allow-unlicensed` is solely for local/CI packaging tests.

## Supported release artifacts

- `x86_64-unknown-linux-musl-minimal`: static Linux binary, gzip + plain files.
- `x86_64-unknown-linux-gnu-zstd`: Linux glibc binary, gzip + plain + zstd.
  Built on Ubuntu 22.04; not guaranteed to run on older glibc systems.
- Rust MSRV: 1.85. Python packaging requires 3.11 or newer.
- Native Windows binaries are **not supported**. Use WSL. PowerShell completion
  targets `pwsh` on supported Unix systems (including inside WSL), not a Windows
  executable. Other Unix targets are source builds, not release-tested platforms.

## Maintainer steps

1. Resolve the license blocker; review README security boundaries and changelog.
2. Set the intended `Cargo.toml` version, regenerate `Cargo.lock`, and update the
   changelog. Run fmt, Clippy, default/all-feature tests, doctests, and MSRV checks.
3. Run `cargo package --locked --list --allow-dirty` locally to review included files; then
   `cargo package --locked --allow-dirty` locally if needed to verify the crate.
   The allow-dirty option is only for inspection, not a release requirement.
4. Build each target with its documented features; run the binary's smoke tests.
   For example, on a matching Linux host:

   ```sh
   cargo build --locked --release --all-features
   python3 scripts/package.py --binary target/release/ztail \
     --target x86_64-unknown-linux-gnu --flavor zstd --tag v0.1.0
   (cd target/dist && sha256sum -c *.sha256)
   ```

   The script checks the tag and binary version, generates all five completions,
   includes the generated manpage, normalizes archive metadata, and refuses to
   overwrite files. It compares --build-info target/features against the requested
   artifact name, preventing a minimal binary from being labeled as zstd or as a
   different architecture. This is a consistency check, not a binary signature or
   independent verification of ABI claims from an untrusted executable.
5. Push the reviewed commit and matching `vVERSION` tag. The Release workflow
   tests each flavor, builds and packages both targets, then creates a **draft**
   GitHub release. A manual workflow run must also select the version tag.
6. Download and verify checksums, smoke-test both archives on supported systems,
   review included licenses/dependency obligations, and inspect the draft notes.
   Publish the draft manually only after approval. No automatic crates.io publish
   is configured. SHA-256 sidecars detect corruption; they are not signatures.

The workflow grants write access only to the draft-release job, checks tag/version
agreement, and uses the lockfile. For a source publication, separately review the
crate's public API and metadata, then use `cargo publish --dry-run --locked` before
any explicitly authorized publication. Do not put tokens in scripts or archives.


## Local installation and offline help

```sh
make install                              # defaults to $HOME/.local, no sudo
make install CARGO_FEATURES=--all-features # add zstd
make install PREFIX=/usr DESTDIR="$PWD/target/staging" # package-manager staging
python3 scripts/check_install.py --all-features       # isolated regression test
make uninstall                            # same PREFIX/DESTDIR as installation
```

Installation uses GNU make/install on Linux, respects PREFIX/BINDIR/DATADIR/MANDIR,
and never invokes sudo. It installs the manpage and Bash/Zsh/Fish completions into
standard share locations. Elvish/PowerShell scripts go under
share/ztail/completions and still require explicit configuration as documented in
docs/shells-and-localization.md. Add a custom prefix's bin directory to PATH and
its share/man directory to MANPATH if your distribution does not discover it.
Uninstall removes only the eight installed files, not directories or other content;
use the exact same directory variables as at install time.

For binary archives, inspect the archive first, verify its .sha256 sidecar, then
install ztail, man/ztail.1 and the desired completion files to your chosen prefix.
The --build-info JSON is useful for bug reports and contains no host-specific paths.
