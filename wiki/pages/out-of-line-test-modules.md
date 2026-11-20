# What will bite you about out-of-line test modules?

Test modules live in `src/<module>/tests/<name>.rs` and are declared with
`#[cfg(test)] #[path = "..."] mod <name>;` (PAN-326, CONTRIBUTING.md § Unit Tests).
Of the four files PAN-326 names, `qso_manager.rs` and `station_agent/mod.rs`
follow this. `coordinator/tx.rs` and `coordinator/qso.rs` still hold their inline
modules until a follow-up moves them. Many other files, such as `decoder.rs`,
`coordinator/hamlib.rs` and `priority_evaluator.rs`, also still hold inline test
modules.

The `#[path]` string is relative to the directory that holds the declaring file.
From `qso_manager.rs` it is `"qso_manager/tests/x.rs"`. Writing `"tests/x.rs"`
there looks for `pancetta-qso/src/tests/x.rs` and fails to compile. From
`station_agent/mod.rs` it is `"tests/x.rs"`.

The file is still a child of the declaring module, so `use super::*;` reaches
private items, and sibling test modules share helpers through `super::<sibling>::`.
Do not put all test files under one new `mod tests;` parent: `use super::*;`
would then reach that parent, not the code under test.

A new directory under `pancetta/src/coordinator/` is a new row in
`docs/ARCHITECTURE.md`'s coordinator module table, and the `architecture_drift`
test fails until the row exists. Moving `coordinator/qso.rs`'s tests creates
`coordinator/qso/`. Regenerate the table in the same PR with
`PANCETTA_REGEN_DOCS=1 cargo test -p pancetta-config --test architecture_drift coordinator_module_table_is_current`.
`tx/` and `station_agent/` already have rows.

`include_str!` resolves from the test file. When `tx.rs` moves,
`pan273_worker_unkey_tests` reads `tx.rs` with `include_str!("tx.rs")`, and that
must become `include_str!("../../tx.rs")`. `pan115_worker_gate_tests` holds the TX
worker harness: thirteen sibling modules in `tx.rs`, plus `hamlib.rs` and
`tx/abort_transcripts.rs`, import it, so renaming it edits all of them.

Renaming a test module renames every test in it, which changes `--list` output,
nextest filters and `scripts/pan325_load_loop.sh`.

When moving a module, copy the text and run `cargo fmt --all`. Do not dedent by
hand: a string literal in `tx/tests/attempt_identity_tests.rs` has leading
spaces that are part of its value. Review a move with `git diff --patience`; the
default diff misaligns repeated test lines.

The payoff is build time. Cargo reads a `#[cfg(test)]` module file only for the
lib-test unit, so editing one rebuilds that unit alone. Touching a test file in
`qso_manager/tests/` reruns `rustc` once, and touching `qso_manager.rs` reruns it
21 times. The lib-test unit itself is not smaller.
