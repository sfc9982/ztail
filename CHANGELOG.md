# Changelog

## Unreleased (0.1.0)

- Incremental gzip and optional zstd decoding; plain-text tail and follow.
- Bounded decode steps, reverse plain-file scanning, buffered output, rotation
  draining, strict regular-file checks, gzip header CRC validation, and escaped
  file names. See README for remaining I/O and terminal safety boundaries.
- Static completions for Bash, Zsh, Fish, Elvish, and PowerShell.
- English and Simplified Chinese help and application diagnostics.
- Per-stream automatic color; explicit `--color`, `NO_COLOR`, and CLICOLOR support.
- Linux release packaging with completions, documentation, and SHA-256 sidecars.

- Generated man(1) manual, stable JSON build information, and prefix/DESTDIR-aware
  installation/uninstallation with completion and manual assets.
- Archive checks reject mismatched binary targets and format feature sets.
- Contributor/security guidance, issue forms, Dependabot, RustSec checks, pinned
  workflow actions, and LF editing conventions.

- Adopted the MIT license; source and binary packages include the license text.

No public release has been made by this change.
