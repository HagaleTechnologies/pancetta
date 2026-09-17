---
name: catalyst-repo-conventions
description: Read this codebase and draft an AGENTS.md for it — the conventions, layout, and gotchas any agent working here should know before touching code.
disable-model-invocation: true
---

# Catalyst Repo Conventions

Read the repository and draft an `AGENTS.md` that captures what a new agent needs to know before it starts changing code — not a restatement of the README, but the conventions and traps that are only obvious once you've looked.

This skill **writes a file**. It must be invoked explicitly — never run it just because a task happens to touch this repo.

## Process

1. **Survey before writing.** Read the top-level layout, the package manifest(s), the test setup, and CI configuration. If an `AGENTS.md` or `CLAUDE.md` already exists, read it fully — this skill updates an existing file in place rather than starting over.
2. **Look for what's surprising, not what's obvious.** A good `AGENTS.md` entry answers "why would this trip someone up?" — a build step that isn't in the README, a test suite that needs a specific flag, a directory that looks editable but is generated. Skip anything a competent engineer would infer from the code itself.
3. **Cite what you found, don't invent conventions.** Every claim in the draft should trace back to something actually read in this repo — a file, a config value, a comment. If you're guessing, say so explicitly rather than presenting a guess as an established convention.
4. **Show the draft before writing.** Present the proposed `AGENTS.md` (or the diff, if one already exists) and confirm before committing it to disk.
5. **Write it.** Prefer a single `AGENTS.md` at the repository root; only split per-directory if the repo is a genuine monorepo with materially different conventions per package.

## What this skill does not do

It does not invent process the repository doesn't already have, and it does not overwrite manual edits in an existing `AGENTS.md` without showing you the diff first.
