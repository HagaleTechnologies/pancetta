---
name: Bug report
about: Something isn't working as documented
title: ''
labels: bug
---

## Summary

A clear, concise description of what's broken.

## Reproduction

Minimum steps to reproduce. Include the exact command, callsign / frequency
in use, and any TUI keybinds pressed.

```
1. ...
2. ...
3. ...
```

## Expected vs. actual

- **Expected:**
- **Actual:**

## Environment

- Pancetta version / commit: `git rev-parse --short HEAD`
- OS: (Linux / macOS / Windows; distro/version)
- Rust toolchain: `rustc --version`
- Rig (if relevant): make/model
- Hamlib: `rigctld --version` (if rig control involved)

## Logs

Attach `~/.pancetta/logs/pancetta.log` (or the relevant slice). Trim to
the failing window — multi-megabyte logs are hard to triage.

## Anything else

Screenshots, related issues, things you've already ruled out.
