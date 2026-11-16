# What will bite you about out-of-line test modules?

Test modules live in `src/<module>/tests/<name>.rs` and are declared with
`#[cfg(test)] #[path = "..."] mod <name>;` (PAN-326, CONTRIBUTING.md § Unit Tests).

The `#[path]` string is relative to the directory that holds the declaring file.
From `coordinator/tx.rs` it is `"tx/tests/x.rs"`. Writing `"tests/x.rs"` there
looks for `coordinator/tests/x.rs` and fails to compile. From
`station_agent/mod.rs` it is `"tests/x.rs"`.

The file is still a child of the declaring module, so `use super::*;` reaches
private items. `worker_gate_tests` holds the TX worker harness. Thirteen sibling
modules, `hamlib.rs` and `abort_transcripts.rs` import it through
`super::worker_gate_tests::`. Do not put all test files under one new
`mod tests;` parent: `use super::*;` would then reach that parent, not the code
under test.

`include_str!` resolves from the test file. `worker_unkey_tests` reads `tx.rs`
with `include_str!("../../tx.rs")`.

Renaming a test module renames every test in it, which changes `--list` output,
nextest filters and `scripts/pan325_load_loop.sh`.

When moving a module, copy the text and run `cargo fmt --all`. Do not dedent by
hand: a string literal in `attempt_identity_tests` has leading spaces that are
part of its value. Review a move with `git diff --patience`; the default diff
misaligns repeated test lines.

The payoff is build time. Cargo reads a `#[cfg(test)]` module file only for the
lib-test unit, so editing one rebuilds that unit alone. Touching a test file in
`qso_manager/tests/` reruns `rustc` once, and touching `qso_manager.rs` reruns it
21 times. The lib-test unit itself is not smaller.
