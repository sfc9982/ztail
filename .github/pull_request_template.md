## Summary

Describe the user-visible change and link the relevant issue, if any.

## Validation

List commands actually run and their results. Explicitly note untested platforms
or feature combinations; do not replace evidence with an unchecked “tests pass”.

## Compatibility and safety

Explain effects on raw output bytes, pipes/exit codes, follow/rotation behavior,
resource bounds, optional zstd support, and Rust 1.85 compatibility as relevant.

- [ ] Added a focused regression test, or explained why it is not applicable.
- [ ] Updated help/manual/docs and CHANGELOG.md for observable behavior changes.
- [ ] Used synthetic/redacted test data; no credentials or private logs included.
- [ ] Reviewed the full patch and dependency changes, including generated code.
- [ ] No publication, license change, or unrelated user edits bundled into this PR.
