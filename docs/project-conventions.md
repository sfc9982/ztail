# Rust/Linux CLI comparison and adopted conventions

Reviewed on 2026-11-14, using the projects' own repository documentation and source.
This is a focused engineering comparison, not a claim that ztail is feature-equivalent.
No upstream implementation or license text was copied into ztail.

| Reference | Useful practice | ztail change |
| --- | --- | --- |
| [fd Makefile](https://github.com/sharkdp/fd/blob/master/Makefile) | Install the executable, shell completions, and manpage together; support staging with DESTDIR. | Added a user-prefix Makefile with explicit managed-file uninstall and package-manager staging. |
| [bat contributor guide](https://github.com/sharkdp/bat/blob/master/CONTRIBUTING.md) | Test observable CLI behavior, document contribution/testing expectations. | Added contributor guide, issue forms, generator/pipe/installation regression checks. |
| [ripgrep build information](https://github.com/BurntSushi/ripgrep/blob/master/build.rs) and [FAQ](https://github.com/BurntSushi/ripgrep/blob/master/FAQ.md) | Build/runtime details help distinguish builds and explain nonobvious behavior. | Added stable JSON --build-info and explicit supported-format/exit-status contracts. Unlike a VCS hash, the target/features work from source archives without git. |
| [GitHub Actions hardening](https://docs.github.com/en/actions/security-for-github-actions/security-guides/security-hardening-for-github-actions) | Full action commit pinning, minimum token permissions. | Pinned action commits; Dependabot handles refresh PRs; release writes remain confined to the draft job. |
| [RustSec cargo-audit](https://github.com/RustSec/rustsec/tree/main/cargo-audit) | Audit the committed dependency graph against maintained advisories. | Added a separate read-only audit workflow. No ignored advisory list is prepopulated. |

## Gaps closed

1. `--manpage` generates an offline roff manual from the same clap definition as
   help and completion scripts. Additional sections document exit status,
   environment variables, file types, and safety boundaries. No build date enters
   generated output. Chinese manuals include invisible Han-character breakpoints
   for roff renderers rather than treating long Chinese sentences as Latin words.
   Releases now include man/ztail.1.
2. `--build-info` emits schema-versioned JSON with version, Cargo target triple,
   and enabled formats. It contains no usernames, source paths, timestamps, or
   private environment data. Existing `--version` output stays compatible.
3. Archive packaging rejects a target or feature flavor different from the binary's
   build information. This closes the previous gap where a gzip-only executable
   could accidentally be named as a zstd release. BUILD-INFO.json travels with it.
4. A prefix/DESTDIR-aware source install includes man(1) and completions; uninstall
   removes only the eight managed files. No auto-sudo or curl-pipe-shell installer.
5. Contribution/security guidance, issue forms, dependency update configuration,
   read-only advisory checks, action pinning, and LF editing rules make maintenance
   expectations explicit rather than relying on unwritten local knowledge.

## Deliberately not added

- No stdin/FIFO follow mode: the current incremental file/rotation model relies
  on regular-file offsets. Supporting streams needs a separate, tested I/O path,
  not removal of the regular-file security check.
- No grep, pager, syntax highlighting, or automatic decompression format expansion:
  ztail remains an incremental tail tool, not a clone of bat/fd/ripgrep.
- No native Windows or untested CPU targets claimed as supported. More platforms
  require CI, signal/filesystem-event tests, and runnable release verification.
- No self-update, automatic crates.io publication, or invented license/maintainer
  contact details. Licensing and final publication authorization remain explicit.


## Build-info JSON contract

Example for a minimal Linux glibc build:

```json
{"schema_version":1,"name":"ztail","version":"0.1.0","target":"x86_64-unknown-linux-gnu","features":["gzip","plain"]}
```

Consumers should parse JSON, not rely on whitespace, key order, or array order.
The features array describes available input formats, not every Cargo dependency
or compiler option. A zstd-enabled build also contains "zstd". Unknown additional
object fields may be ignored; incompatible changes to existing fields require a
new schema_version. All fields are build-derived, independent of locale/color,
and no input file is needed. This is diagnostic metadata, not an attestation that
an arbitrary executable is trustworthy or that it will run on every libc version.
