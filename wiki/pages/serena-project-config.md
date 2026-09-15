---
id: serena-project-config
title: What will bite you about the Serena project config?
kind: gotcha
status: current
maintainer: agent
sources:
  - .serena/project.yml
  - .serena/.gitignore
  - pancetta-core/tests/serena_project_config.rs
verified:
  commit: 4ba5ace
  date: 2026-09-06
links:
  - overview
---
`.serena/project.yml` is what gives catalyst-dev's codebase-analyzer,
codebase-locator, and codebase-pattern-finder subagents real `find_symbol` /
`find_referencing_symbols` search in this repo instead of a silent grep
fallback (PAN-77). Four things about it are not obvious from the file alone.

## Symptom

Either a `mcp__serena__activate_project` call errors or falls back to grep
with no error at all (the failure mode PAN-77 exists to close), or a
`git status` shows an unexpected local diff on `.serena/project.yml` after an
agent session that never touched it.

## The four traps

1. **CI's `changes` filter runs no Rust job on a `.serena/`-only diff — and
   that includes this guard test.** `.github/workflows/ci.yml`'s `changes` job
   matches only `**/*.rs`, `**/Cargo.toml`, `**/Cargo.lock`, and `ci.yml` — a
   pure `.serena/` edit matches none of them, so no Rust job runs and
   `pancetta-core/tests/serena_project_config.rs` does not execute either. The
   guard only fires when a `.serena/` change is bundled into the same commit
   as a `.rs`/`Cargo.toml`/`Cargo.lock` edit; a `.serena/`-only PR ships
   unverified until the weekly schedule lane runs (post-merge, not a gate).
2. **`thoughts` in `ignored_paths` guards a symlink, not a directory.** It
   points out of the repo at the shared thoughts pool (`.gitignore:188`).
   Without the entry, Serena's directory walker would follow it into the
   externally-mounted pool during indexing.
3. **`language_servers` is Rust-only by decision, not omission.** The Python
   files under `training/neural_osd/` and `scripts/` are reachable via
   `search_for_pattern` but not `find_symbol`; adding `python` needs a session
   that can first confirm a working pyright backend.
4. **Activation rewrites the tracked file in place if it sees a legacy
   schema key — and the rewrite strips hand-written comments.** The installed
   Serena migrates `languages:` → `language_servers:` and
   `additional_workspace_folders:` → `ls_additional_workspace_folders:` (also
   adding `ls_workspace_folders:`, `activation_command:`, and
   `activation_command_timeout:`) the first time it loads a config using the
   old names. That migration re-serializes the *entire* file, discarding every
   comment this file's authors interleaved into it — including the
   `ignored_paths` rationale block. Reproduced by running `uvx --from
   git+https://github.com/oraios/serena serena project index .` against a
   checkout carrying the legacy keys: `git status --porcelain` afterwards
   showed a 150+-line rewrite of `project.yml`, and
   `cargo test -p pancetta-core --test serena_project_config` then failed
   (`project_yml_declares_rust` looked for a `languages` key that no longer
   existed). The committed file now already uses the current key names for
   exactly this reason — activating against it produces **zero** diff — and
   the guard test fails loudly if the legacy `languages:` key reappears, so a
   revert that would retrigger this rewrite is caught before it lands.

## Where the invariant lives

- `.serena/project.yml` — the committed config; see its own inline comments
  for the `ignored_paths` rationale and the migration note next to
  `language_servers:`.
- `pancetta-core/tests/serena_project_config.rs` — the CI-visible guard: file
  presence, `project_name`, `read_only`, `language_servers` (rejecting the
  legacy `languages` key), the three load-bearing `ignored_paths` entries,
  that no `ignored_paths` entry (literal or glob) shadows a workspace
  member's `src/` directory or its `lib.rs`/`main.rs` crate root, and that
  `.serena/memories/codebase_map.md` names every workspace crate.
- `.serena/.gitignore` — keeps the generated `cache/` and an optional
  machine-local `project.local.yml` override out of git.

Before assuming the config is wrong, confirm the session actually has
`mcp__serena__*` tools connected at all — a session with none registered
looks identical to a broken config from the outside, and is a distinct,
unfixable-from-config failure mode.
