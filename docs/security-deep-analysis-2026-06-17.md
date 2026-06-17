# Pancetta Deep Security Analysis — 2026-06-17

**Author:** Claude Opus 4.8 (under K5ARH supervision)
**Scope:** Adversarial deep-dive focused on the question *"can a crafted FT8
transmission cause remote code execution or memory corruption?"*, plus decoder
robustness, QSO/autonomous manipulation, network/file/process, and a
workspace-wide `unsafe`/supply-chain sweep.
**Codebase as-of:** `9d3c1313` (post Phase-1 cleanup + Phase-2/3 security review).
**Methodology:** 5 parallel deep-read audit agents (one per attack-surface
dimension), each reading the actual code, followed by 3-skeptic adversarial
verification of every Critical/High finding. Follow-on to
`docs/security-review-2026-04-29.md`.

---

## Executive Summary

**There is no remote-code-execution or memory-corruption path from the bytes of
a received FT8 signal.** The full production RX path
(`coordinator/ft8.rs:466` → `Ft8Decoder::decode_window_ft8lib` →
`ft8lib_decode_audio` → the vendored C `ft8_lib`) was traced end-to-end; every
attacker-content-driven write lands in a bounded buffer, the candidates buffer
is correctly capped, LDPC/CRC index only compile-time-constant tables, and the
recent FFI hardening (`cstr_from_fixed_buf` in-bounds NUL check, no-op hash
callbacks that make the whole callsign-hash OOB family unreachable, an
independent Rust-side SNR bounds re-derivation) is airtight. The pure-Rust
decoder is equally well-defended: the 77-bit unpack is length-guarded, every
attacker-derived table index is bounds-checked or modular, and every sort in the
live path is NaN-safe (`unwrap_or(Ordering::Equal)`). The 2026-04-29 Critical
(C-1, sender verification) holds across **every** state-transition arm, including
the new Phase-5 `(RespondingToCq, ReportAck)` skip-rung arm. Outside the FFI
there are only 4 production `unsafe` sites (all sound/benign), no git
dependencies, and `cargo deny` is clean.

**Zero Critical, zero High** findings survived verification. The 10 findings are
all integrity, availability/defense-in-depth, or cosmetic, and every
RF-reachable one is bounded by FT8's accepted no-authentication trust model.

---

## Findings & disposition

| ID | Sev | Class | Disposition |
|----|-----|-------|-------------|
| compound-call-logged-callsign-overwrite | Low | RF | **FIXED** `9d3c1313` |
| completed-duration-cast-wrap | Info | internal | **FIXED** `9d3c1313` |
| modevalue-redundant-send-sync | Info | none | **FIXED** `41be7c60` |
| wav-bits-per-sample-underflow (×2) | Low | malicious-WAV | **FIXED** `41be7c60` |
| cluster-recent-spots-unbounded | Low | remote-service | **FIXED** `41be7c60` |
| decode-thread-no-panic-isolation | Medium | RF (theoretical) | **DOCUMENTED — operator decision** (see below) |
| autonomous-cq-phantom-completion-and-log | Medium | RF | **DOCUMENTED — inherent to FT8** (Phase-5 checklist) |
| int-to-dd-nondigit-report | Info | RF (cosmetic) | **NOTED** — vendored C, matches WSJT-X, not a bug |
| ffi-abi-drift-guard-static-asserts-only | Info | none | **NOTED** — optional `offset_of!` hardening |

### Fixed (code landed, with tests)

- **compound-call-logged-callsign-overwrite** (`qso_manager.rs`, `exchange.rs`):
  the "upgrade logged callsign to the most-complete form" logic could be abused
  by an RF attacker who knows the partner's base call to overwrite the logged
  ADIF callsign with an attacker-chosen compound (e.g. `BOGUS9/G8BCG/MM`). Now
  gated by `is_safe_compound_upgrade`: a longer form is accepted only when it is
  the same base, strictly *adds* recognized prefix/suffix tokens (same rule
  `validate_callsign` enforces), and never substitutes a different affix; a
  rejected upgrade is logged at `warn target:"qso.security"`. The legitimate
  `G8BCG` → `EA8/G8BCG` completion still works.
- **completed-duration-cast-wrap**: `(now - started_at).num_seconds() as u32` in
  the 3 completion arms now clamps `.max(0)` against backward clock skew (mirrors
  the existing `check_timeouts_at` signed-comparison fix).
- **modevalue-redundant-send-sync**: deleted the redundant (future-fragile)
  manual `unsafe impl Send/Sync for ModeValue`; the auto-derived bounds are
  correct and will now catch a future non-thread-safe field.
- **wav-bits-per-sample-underflow**: `--wav` playback now rejects
  `bits_per_sample ∉ 1..=32` before the `1 << (bits-1)` shift (offline-mode DoS;
  not on the live path).
- **cluster-recent-spots-unbounded**: the DX-cluster dedup map is now pruned to
  the dedup window on each insert (bounds a hostile/MITM telnet feed; also fixes
  a latent stale-key dedup bug).

### Documented — operator decisions

- **decode-thread-no-panic-isolation** (Medium, *theoretical*). The FT8 decode
  loop runs in one `spawn_blocking` task with no `catch_unwind`; a panic on a
  decoder input would exit the loop and silently deafen the station until a
  manual restart. **Important nuance:** the release profile sets
  `panic = "abort"` (a deliberate choice — `Cargo.toml:129`), so in-process
  `catch_unwind` is *ineffective in production* (a panic aborts immediately,
  nothing to catch). The audit also rated this "not a demonstrated crash — the
  FT8 decoder is bit-exact and heavily tested." Therefore the appropriate
  mitigation is **operational, not a code patch**:
  1. **Recommended (no code change):** run pancetta under a process supervisor
     with restart-on-failure — `systemd` `Restart=on-failure` (Linux MiniPC) or
     `launchd` `KeepAlive` (macOS). A decode-thread abort then self-heals in
     seconds. This also covers any other unexpected abort.
  2. **Alternative (policy change, operator's call):** switch
     `[profile.release] panic = "unwind"` and wrap the per-window decode in
     `catch_unwind` (log+skip+continue) for in-process resilience — trading the
     deliberate "predictable failure / smaller binary" semantics for staying up
     through a hypothetical decoder panic. Not done here because flipping that
     deliberate policy is an operator decision and the crash is undemonstrated.
  A coarse health watchdog (flag "no decode for N slots" as unhealthy) is a
  cheap orthogonal add under either option.

- **autonomous-cq-phantom-completion-and-log** (Medium, *inherent to FT8*).
  When pancetta calls CQ (autonomous CQ-self, or the Phase-5 Auto path) the
  `CallingCq → CqResponse` arm — by FT8 design — accepts *any* station that
  addresses our call as the responder (the responder is legitimately unknown),
  and Phase 5 now drives that exchange to a logged (and optionally
  externally-uploaded) completion unattended. The pounce-direction gates
  (min_dx_score / recently_responded / dx_busy / FP filter) do **not** apply to a
  station *answering our own CQ*. This is the accepted no-auth FT8 trust model,
  but worth two operator considerations for the **Phase-5 on-air validation**:
  (a) consider running the callsign-continuity FP filter on responders-to-our-CQ,
  not just pounce targets, so fabricated/garbage calls don't auto-complete;
  (b) consider not auto-uploading *Auto-initiated* completions to external
  logbooks (ClubLog/QRZ) without a review/quarantine step, mirroring the bounded
  auto-73 path. Added to the Phase-5 on-air checklist; not changed in code
  because unattended autonomous logging is the intended Phase-5 behavior and the
  right trade-off is the operator's to set.

### Noted (no action needed)

- **int-to-dd-nondigit-report**: a decoded out-of-range signal report can render
  a non-digit char in the message text — purely cosmetic, **not** a buffer
  overflow (the write stays within the 7-byte field). Vendored ft8_lib behavior,
  matches WSJT-X. Optional: clamp in the Rust message parser for output hygiene.
- **ffi-abi-drift-guard-static-asserts-only**: the hand-declared FFI structs are
  guarded by `assert!(size_of == N)` (total size) but not per-field offsets. Not
  attacker-reachable (the C source is vendored and pinned; real drift would
  change a size and fail the build). Optional hardening: add `offset_of!`
  assertions and document the pinned `vendor/ft8_lib` upstream commit.

---

## Areas confirmed clean (no findings)

- **RF → C-FFI**: no content-driven OOB write; candidates buffer capped; bounded
  message buffers; CRC/LDPC index constant tables only; in-bounds NUL check live.
- **Rust decoder**: no RF-reachable panic/OOB/overflow/NaN crash; length-guarded
  unpack; NaN-safe sorts; bounds-checked indices.
- **QSO/autonomous**: sender verification (`from==DX && to==us`) on every
  state-advance arm incl. the new skip-rung; anti-flood gates intact; no phantom
  completion of an *established* QSO.
- **unsafe/supply-chain**: 4 sound production `unsafe` sites outside the FFI; no
  git deps; `cargo deny` clean; no `danger_accept_invalid_certs`.

---

## Notes for the operator

- **The RF attack surface is solid.** The headline question — can someone with a
  transceiver on your frequency corrupt memory or run code on the station? — is
  **no**, with the full path traced. The remaining RF-reachable findings are
  about *integrity* (don't log a contact that didn't happen the way we recorded
  it) and *availability* (a hypothetical decoder panic), both bounded.
- **One real operational recommendation:** run the station under a
  restart-on-failure supervisor. It's the correct mitigation for the
  panic-isolation finding under the existing `panic = "abort"` policy, and good
  practice regardless.
- **Two Phase-5 on-air checklist items** (responder FP-filtering, Auto-upload
  quarantine) are integrity trade-offs for you to decide when you validate the
  autonomous loop on the air.
