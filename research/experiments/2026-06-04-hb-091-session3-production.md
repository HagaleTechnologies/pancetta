# hb-091 — a8-style early-decode (Session 3 production wiring + QSO/hr A/B)

**Date**: 2026-06-04
**Branch**: iter/2026-06-04-hb-091-s3bc
**Status**: **SESSION-3-COMPLETE — SHELVE default-on; gated infrastructure shipped (env-var enabled)**
**Effort**: ~3 hours (S3b coordinator wiring + S3c A/B sim + analysis)

## What this session did

Two deliverables in one batch:

1. **S3b — Coordinator scoped fast-path** (production code, gated default-off).
   The FT8 component now runs an early scoped Costas search at the in-QSO
   partner's freq_bin BEFORE the standard ft8_lib + native pipeline,
   forwarding hits to the QSO state machine immediately. Gated behind
   `PANCETTA_SCOPED_FAST_PATH=1` env var.

2. **S3c — QSO/hour A/B simulation** (the deployment gate).
   `pancetta-research/examples/hb091_qso_per_hour_ab.rs`. Empirically
   driven Monte Carlo: samples decode wall-clock from the S3 latency
   profile distribution (M4 Mac Mini, 2026-06-04), maps tx-late
   truncation to partner-decode failure probability under three fade
   scenarios, simulates 2000 QSO trials per (arm, scenario) and
   reports QSOs/hour Δ.

## S3b — Coordinator wiring

Touched files:

- `pancetta/src/coordinator/mod.rs`: new field
  `active_qso_freq_hz: Arc<RwLock<Option<f64>>>` mirroring the
  existing `active_qso_ap` pattern.
- `pancetta/src/coordinator/qso.rs`: extend the StateChanged handler
  that already maintains `active_qso_ap` to also write
  `active_qso_freq_hz` from `QsoState::frequency()`. Cleared on
  QsoCompleted (alongside `active_qso_ap`).
- `pancetta/src/coordinator/ft8.rs`: before the standard ft8_lib +
  native decode, check `PANCETTA_SCOPED_FAST_PATH=1` env var. If
  enabled AND `active_qso_freq_hz` is `Some(freq_hz)`, derive
  `center_bin = round(freq_hz / 6.25)`, range `(center − 5)..=(center + 5)`,
  and call `decoder.decode_window_scoped`. Forward hits to
  `ComponentId::Qso` via the message bus immediately. The standard
  pipeline runs unchanged after; the QSO state machine deduplicates
  via `is_message_relevant` (mismatched messages discarded at
  `target: "qso.security"` warn level).

Why `HALF_WIDTH = 5` (±31 Hz): per Session 2's width-sweep, ±2 bins
SHELVED at 94.05% retention; ±5 bins PROCEED at 96.03%. Marginal
scoping cost cut from −182 decodes to −84 — width-tuning bound, not
fundamental.

Why env var (not config flag): the gating is for staged rollout while
the QSO/hr A/B is unsettled across hardware tiers. Once a tier is
greenlit, the flag flips per-tier; once all tiers are greenlit, the
flag deletes. Avoids polluting `pancetta-config` with a dead
config field. Pattern matches other research-gated knobs in
pancetta-ft8 (`Ft8Config` has `coherent_*` and similar feature
toggles).

Why dual-decode (scoped FIRST + standard SECOND) instead of
scoped-only: scoped restricts the sweep to one partner. Pancetta
decodes EVERYTHING in the band (waterfall + autonomous CQ scanner +
PSKReporter feed). The standard pipeline is the only path that
captures non-partner decodes — must run unconditionally. Scoped is
an additive fast-path for the in-QSO partner specifically.

Tests: all 13 `loopback_qso` integration tests pass. Env-var-off
default leaves the FT8 component byte-for-byte equivalent to the
prior pipeline.

## S3c — QSO/hour A/B simulation

### Model

For each `(arm, fade_scenario)` pair, simulate `N_TRIALS=2000` QSO
attempts. Per QSO: 4 legs (CQ→resp, resp→R+report, R+report→RR73,
RR73→73). Per leg:

1. Sample `decode_ms` from arm's empirical distribution
   (linear interpolation between percentile anchor points from the
   S3 latency profile).
2. `tx_late_ms = max(0, decode_ms − 2000)` (pancetta's slot budget).
3. Partner-decode failure probability = `fade_scenario.fail_prob(tx_late_ms)`.
4. On failure: +15s retry slot, re-roll up to 3 retries. If all 3
   fail, abandon the leg (QSO marked unsuccessful).

Fade scenarios:

| scenario | fail_prob mapping |
|---|---|
| no-fade | always 0 |
| moderate | tx_late ∈ [100, 500)ms → 0.30; ≥500ms → 0.70 |
| heavy | tx_late ∈ [50, 300)ms → 0.50; ≥300ms → 0.90 |

Deployment gate: PROCEED-default-on if scoped QSOs/hour ≥ 1.10×
full QSOs/hour in any scenario.

### Empirical distributions (from S3 latency profile, M4)

```
arm     | min |  p50 |  p90 |  p95 |  p99 |  max
full    | 300 |  862 | 1980 | 2132 | 2332 | 2446
scoped  | 298 |  329 |  605 |  712 |  866 |  917
```

### Results

```
scenario     arm                 succ_rate      QSOs/hour       vs full
no-fade      full only                1.00          30.00            —
             scoped fast-path         1.00          30.00          1.00x
moderate     full only                1.00          29.44            —
             scoped fast-path         1.00          30.00          1.02x
heavy        full only                1.00          28.68            —
             scoped fast-path         1.00          30.00          1.05x

best ratio observed: 1.05x
AUTO-DECISION: SHELVE (keep primitive + env-var gate)
```

### Interpretation

On the **M4 Mac Mini reference machine**, full decode is fast enough
that even the bimodal tail busts don't drive meaningful retry losses:
no-fade =0%, moderate ~2%, heavy ~5%. None clear the +10% gate.

Why so small: full p99=2332ms busts the 2000ms budget by 332ms; max
2446ms by 446ms. Under heavy fade, 446ms truncation has fail_prob
0.90, but it only hits the 99th percentile of decodes — rare overall
on this distribution.

**The reasoning the user surfaced ("people may run this on minimal
hardware") is now the load-bearing question.** On hardware where
full p95 spills well past 2000ms, the bust rate compounds and
scoped's relative win grows. M4 result is the OPTIMISTIC bound on
"do we need this?" If even M4 had cleared +10%, MiniPC would clear
by much more.

## Decision: SHELVE production default-on; ship gated infrastructure

**SHELVE default-on for M4 reference hardware.** Scoped fast-path
doesn't clear the +10% QSO/hr gate on M4.

**KEEP the gated infrastructure**:
- Decoder primitive (`decode_window_scoped`) — production-grade,
  reusable for future scoping work (e.g., residual subtraction with
  frequency hints).
- Coordinator wiring (`PANCETTA_SCOPED_FAST_PATH=1` env var) — ready
  to flip for any hardware tier where the latency profile justifies.
- A/B simulation example — re-runnable with new distributions for
  any future hardware tier.

**Next decision point: characterize the operator's Windows 11 MiniPC
tier**. Run `decode_latency_profile` on that machine, plug new
percentiles into `hb091_qso_per_hour_ab` constants, re-run. If the
MiniPC distribution clears +10% in any fade scenario, recommend
flipping `PANCETTA_SCOPED_FAST_PATH=1` per-tier (e.g., via a
hardware-detection startup check or a recommended environment
setting in the runbook). This is the **broader hardware-tier
classifier** work the user flagged — concrete next step.

## Engineering substance check

- **Mechanism cited**: WSJT-X-Improved Release_Notes.txt
  (DG2YCB) v3.0.0 250924 — "a8 decoding technology" baseline.
- **Empirical numbers** all traceable to artifacts:
  `decode_latency_profile.rs` output → `hb091_qso_per_hour_ab.rs`
  distributions → simulated QSOs/hr.
- **Bootstrap CI gate**: simulation is N=2000 trials per cell;
  ratios are point estimates. Per the engineering-substance check
  policy, small-delta default flips need CI gating. This SHELVE
  is itself a "no flip" decision so the CI bar doesn't apply, but
  for any future tier-greenlight on MiniPC we'd want N≥5000 trials
  and explicit CI on the ratio.
- **Honest framing**: the journal didn't pretend the M4 result
  generalizes to MiniPC. It explicitly identifies what's settled
  (M4 doesn't clear the gate) and what's open (MiniPC unmeasured).

## Artifacts

- `pancetta/src/coordinator/mod.rs` — `active_qso_freq_hz` field.
- `pancetta/src/coordinator/qso.rs` — write path on StateChanged + QsoCompleted.
- `pancetta/src/coordinator/ft8.rs` — read path + scoped fast-path
  dispatch + forwarding to QSO bus.
- `pancetta-research/examples/hb091_qso_per_hour_ab.rs` — A/B sim.
- `research/experiments/2026-06-04-hb-091-session3-production.md` —
  this journal.

## Lessons

1. **"Operationally meaningful" is hardware-dependent.** The
   recall-feasibility PROCEED at S2 + the wall-clock-savings PROCEED
   at S3-mechanism were both real findings. But neither directly
   answered "does this improve QSO/hr on the deployment target?"
   The A/B simulation is the closest we can get without on-air
   measurement, and it says M4 is fast enough not to need this.
2. **Build gated infrastructure when the deployment-tier picture is
   incomplete.** This batch shipped a fast-path that's invisible by
   default on M4. The next tier (MiniPC) can flip it without
   re-implementing anything. The cost of leaving it dormant is
   roughly zero (env-var check + RwLock read, both cheap).
3. **Distributions matter more than means.** Mean decode wall-clock
   on M4 is 1136ms — comfortably inside the 2000ms budget. p99
   busts it by 332ms. The mean would falsely suggest "no problem";
   the percentile distribution exposes the real risk.
4. **Documenting NEGATIVE results well is a load-bearing part of
   the engineering substance check.** SHELVE-with-evidence beats
   PROCEED-on-vibes. This journal records what we measured, what
   we built, and what's still open — so the next iteration starts
   from the right place.
