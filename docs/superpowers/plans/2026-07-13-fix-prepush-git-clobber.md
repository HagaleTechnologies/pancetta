# Fix Pre-Push Hook Git Clobber (cargo-deny GIT_DIR leak) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop the pre-push hook from corrupting the pushing branch (local + remote) via cargo-deny's advisory-db auto-update inheriting the hook's `GIT_DIR` environment.

**Architecture:** One-line environment sanitization in `scripts/check.sh` before any cargo invocation, plus a regression guard. No product code changes.

**Tech Stack:** bash, cargo-deny.

## Root cause (confirmed 2026-07-13 by live process capture)

Six incidents (2026-07-03 → 07-12) of a just-pushed branch being force-reset to a stale
SHA, local and remote, within seconds of `git push`. Reproduced on demand: a probe push
triggered, 3 s later, these processes (parent: cargo-deny, grandparent: the pre-push hook):

```
git -C ~/.cargo/advisory-dbs/advisory-db-<hash> reset --hard
git -C ~/.cargo/advisory-dbs/advisory-db-<hash> fetch
git -C ~/.cargo/advisory-dbs/advisory-db-<hash> reset --hard FETCH_HEAD
```

git runs hooks with `GIT_DIR` exported. `GIT_DIR` **overrides** `-C`-based repo
discovery, so RustSec's advisory-db refresh operates on the *pushing repo*, not the
advisory DB: the checked-out branch is hard-reset to whatever `FETCH_HEAD` last held
(hence "always the same stale SHA"), and because this happens mid-push, git can transfer
the moved ref — clobbering the remote branch too. Every force-push repair re-runs the
hook and re-clobbers, which is why repairs "fought back". Explicit-SHA refspec pushes
(`git push origin <sha>:refs/heads/<name>`) were immune because a raw SHA cannot be
re-read from a moved branch ref.

## Global Constraints

- The hook must keep running cargo-deny (supply-chain gate stays).
- Fix must protect ALL cargo child processes in the hook, not just cargo-deny (any
  build.rs or cargo tool that shells out to git has the same exposure).
- No change to the hook-installation mechanism (`--install-hook` symlink).

---

### Task 1: Sanitize the git hook environment in check.sh

**Files:**
- Modify: `scripts/check.sh` (immediately after `set -euo pipefail`, before the `cd`)

**Interfaces:** none.

- [ ] **Step 1: Add the unset line**

After `set -euo pipefail` (line 33) and before `cd "$(git rev-parse --show-toplevel)"` (line 35), insert:

```bash
# When git runs this script as a pre-push hook it exports GIT_DIR (and sometimes
# GIT_WORK_TREE / GIT_INDEX_FILE / GIT_PREFIX). Those env vars OVERRIDE `git -C`
# repo discovery in every child process — cargo-deny's RustSec advisory-db
# auto-update shells out to `git -C ~/.cargo/advisory-dbs/... reset --hard
# FETCH_HEAD`, which with GIT_DIR leaked would hard-reset THIS repo's pushing
# branch instead (root-caused 2026-07-13 after six branch-clobber incidents;
# see docs/superpowers/plans/2026-07-13-fix-prepush-git-clobber.md).
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_PREFIX
```

Note: the `cd "$(git rev-parse --show-toplevel)"` on the next line already puts us at
the repo root, so plain repo discovery still resolves correctly after the unset.

- [ ] **Step 2: Regression-verify with a probe push**

```bash
# From any worktree of this repo with the hook installed:
git rev-parse HEAD                      # note the SHA
git push origin HEAD:refs/heads/tmp-clobber-probe
sleep 5
git reflog --date=iso | head -3         # must show NO new "reset: moving to" entries
git rev-parse HEAD                      # must equal the noted SHA
git ls-remote origin tmp-clobber-probe  # must equal the noted SHA
git push --no-verify origin --delete tmp-clobber-probe
```

Expected: HEAD unchanged, no reset reflog entries, remote probe ref matches the pushed SHA.

- [ ] **Step 3: Verify the hook still gates**

```bash
scripts/check.sh --fast 2>&1 | tail -3
```

Expected: runs fmt/clippy/check/deny and prints "all checks passed".

- [ ] **Step 4: Commit**

```bash
git add scripts/check.sh
git commit -m "fix(hooks): unset GIT_DIR family in check.sh — cargo-deny advisory-db update was hard-resetting the pushing branch"
```

---

### Task 2: Belt-and-braces — pin cargo-deny behavior

**Files:**
- Modify: `scripts/check.sh` (the cargo-deny invocation, line ~141)

**Interfaces:** none.

- [ ] **Step 1: Check whether the installed cargo-deny supports gix-based fetch**

```bash
cargo deny --version
cargo deny check advisories --help 2>&1 | grep -i "disable-fetch\|offline" | head -5
```

If `--disable-fetch` (or an `[advisories] git-fetch-with-cli = false` config option in
deny.toml's schema — check `cargo deny check --help` and the cargo-deny book) exists in
the installed version: proceed. If the version predates these options, note it in the
commit message and skip Step 2 (Task 1 alone fully fixes the incident).

- [ ] **Step 2 (conditional): Prefer non-CLI advisory fetch**

If supported, change the deny invocation in check.sh from:

```bash
    run "cargo deny"        cargo deny check bans licenses sources advisories
```

to fetch the advisory DB via built-in gix rather than the git CLI (exact flag/config per
Step 1's findings — e.g. `CARGO_NET_GIT_FETCH_WITH_CLI=false` env or the documented
deny.toml key), keeping behavior otherwise identical. Do NOT use `--disable-fetch`/
`--offline` blindly — a never-updated advisory DB silently weakens the gate; only switch
the fetch *mechanism*, not the freshness.

- [ ] **Step 3: Re-run the Task 1 Step 2 probe + commit**

```bash
git add scripts/check.sh
git commit -m "chore(hooks): fetch advisory DB without shelling out to git CLI in check.sh"
```

---

## Also fixed by this (recorded for the session log)

- The "mystery FETCH_HEAD resetter" first noted in iteration-workflow sessions.
- The recommendation "push to a new branch name" is obsolete once this lands; the
  durable safe-push recipe during any future hook suspicion remains
  `git push origin <sha>:refs/heads/<name>` (immune by construction).
