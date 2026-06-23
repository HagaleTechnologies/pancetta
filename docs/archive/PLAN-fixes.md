# Fix Plan: Dependency Vulnerabilities + Known Issues

## Part 1: Dependency Vulnerabilities

### 1.1 Quick fix: `bytes` (Medium — no API changes)

```bash
cargo update -p bytes
```

- Current: 1.10.1, fix: 1.11.1 (semver-compatible)
- Issue: integer overflow in `BytesMut::reserve`
- Pulled in by: `reqwest` → `h2` → `bytes` (pancetta-dx)
- Risk: None

### 1.2 Bump `validator` 0.18 → 0.20 (Medium — low risk)

- File: `pancetta-config/Cargo.toml`
- Change: `validator = { version = "0.20", features = ["derive"] }`
- Fixes: `idna` 0.5.0 Punycode label vulnerability (transitive via validator)
- Risk: Derive macro may have minor changes. Run `cargo check -p pancetta-config` and fix any compilation errors. Likely just re-derives.

### 1.3 Bump `ratatui` 0.28 → 0.29 (Low — moderate risk)

- File: `pancetta-tui/Cargo.toml`
- Change: `ratatui = "0.29"` (not 0.30 — too many breaking changes for a patch)
- Fixes: `lru` 0.12.5 Stacked Borrows violation (transitive via ratatui)
- Risk: Widget API changes between 0.28 and 0.29. Key areas to check:
  - `Frame` type parameter removed in 0.29 (`Frame<'_>` → `Frame`)
  - `Gauge`, `Paragraph`, `Block` APIs may have minor changes
  - Grep for `Frame<'_>` across pancetta-tui and update
  - Run `cargo check -p pancetta-tui` and fix

### 1.4 Bump `sqlx` 0.7 → 0.8 (Medium — high risk, do last)

- File: `pancetta-qso/Cargo.toml`
- Change: `sqlx = { version = "0.8", features = ["runtime-tokio-rustls", "sqlite", "chrono", "uuid"] }`
- Fixes: 2x binary protocol misinterpretation vulnerabilities
- Risk: Significant API changes in 0.8:
  - `SqlitePool::connect()` signature changes
  - Query builder API changes
  - `FromRow` derive may need updates
  - Feature flag names may have changed (check `runtime-tokio-rustls`)
- Files to check:
  - `pancetta-qso/src/database.rs` — all SQL queries and pool management
  - `pancetta-qso/src/async_database.rs`
  - `pancetta-dx/src/tracker.rs` — also uses sqlx
- Strategy: bump, `cargo check -p pancetta-qso -p pancetta-dx`, fix errors one by one

### 1.5 Dismiss stale alerts

These packages are no longer in the dependency tree:
- `aws-lc-sys` (3 high alerts) — removed by Dependabot PR #14
- `rsa` (1 low alert) — removed

Dismiss via GitHub UI or:
```bash
gh api repos/HagaleTechnologies/pancetta/dependabot/alerts/{id} -X PATCH -f state=dismissed -f dismissed_reason=no_bandwidth
```

### Verification after all bumps

```bash
cargo update
cargo check --workspace
cargo test --workspace
cargo audit  # if installed, confirms no remaining advisories
```

---

## Part 2: Known Issues from Multi-Protocol Session

### 2.1 GFSK matched filter for FT4 OTA decode

- Problem: FT4 uses GFSK BT=1.0 which spreads tone energy across adjacent frequency bins. Current decoder uses raw DFT symbol extraction, which works for rectangular/CPFSK but misreads GFSK-shaped symbols.
- Current workaround: FT4 round-trip tests use `PulseShape::Rectangular` modulation.
- Fix: Implement matched GFSK filter in decoder's `extract_symbols_complex()`:
  1. Generate GFSK pulse shape template for BT=1.0
  2. Cross-correlate received signal with each of the 4 tone templates
  3. Pick tone with highest correlation (replaces raw FFT bin magnitude)
- Files: `pancetta-ft8/src/decoder.rs` (in `extract_symbols_complex`, branch on `pp.modulation`)
- Difficulty: Medium. Need to precompute GFSK pulse templates and do per-symbol correlation.
- Test: Change FT4 round-trip tests to use `PulseShape::Gaussian { bt: 1.0 }` and verify decode still works.

### 2.2 CQ DX FT4 decode returns `<Unknown>`

- Problem: "CQ DX W1ABC FN42" encodes fine as FT4 but decodes as `<Unknown>` text.
- Likely cause: XOR scrambling interaction with CQ DX payload bits produces a bit pattern that the message parser doesn't recognize as type 0/1 (i3 bits may flip).
- Debug approach:
  1. Encode "CQ DX W1ABC FN42" as FT4, inspect raw 77-bit payload before and after XOR
  2. Check i3 bits (74-76) after XOR un-scramble — are they correct?
  3. If the XOR sequence happens to flip i3 bits, the un-scramble in the decoder may be wrong
- Files: `pancetta-ft8/src/decoder.rs` (XOR un-scramble in `decode_candidate`), `pancetta-ft8/src/message.rs` (parser)
- Difficulty: Small. Likely a bit-ordering issue in the XOR un-scramble.

---

## Execution Order

| Step | What | Risk | Time |
|------|------|------|------|
| 1.1 | `cargo update -p bytes` | None | 1 min |
| 1.2 | Bump validator 0.18 → 0.20 | Low | 10 min |
| 1.5 | Dismiss stale GitHub alerts | None | 5 min |
| 2.2 | Fix CQ DX FT4 decode | Low | 30 min |
| 1.3 | Bump ratatui 0.28 → 0.29 | Medium | 30-60 min |
| 2.1 | GFSK matched filter | Medium | 2-3 hrs |
| 1.4 | Bump sqlx 0.7 → 0.8 | High | 1-2 hrs |

Do the quick wins first (1.1, 1.2, 1.5, 2.2), then the medium-risk bumps (1.3), then the heavy lifts (2.1, 1.4).
