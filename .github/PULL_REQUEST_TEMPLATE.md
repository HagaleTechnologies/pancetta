## Summary

What changes and why. Link the issue / discussion if relevant.

## Test plan

- [ ] `cargo test --workspace --features transmit` (Linux / macOS / Windows where applicable)
- [ ] `cargo test -p pancetta-hamlib --lib -- --test-threads=1` (if rigctld path touched)
- [ ] `cargo clippy --workspace --features transmit` clean
- [ ] `cargo fmt --all -- --check` clean
- [ ] Manual verification (describe what you exercised)

## Security / privacy

- [ ] No new credentials, tokens, or callsigns committed.
- [ ] No widening of network surfaces (rigctld bind, HTTP base URLs, etc.)
- [ ] No new `unsafe` blocks (or: justified inline with a rationale comment)

## Notes for reviewer

Anything subtle, intentional trade-offs, or follow-up items left for
later PRs.
