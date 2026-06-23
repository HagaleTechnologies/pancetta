# Median-filter DT-per-callsign history for sync/AP — design spec (hb-057)

**Status:** proposed (Session 1 scoping; diagnostic PROCEED)
**Hypothesis:** hb-057
**Author:** research harness, 2026-05-31
**Estimated effort:** 2 sessions (Session 2: implement + A/B sweep, Session 3: graduate or shelve)
**Parent / spawn context:** spawned 2026-05-25 from mr-002 (JTDX Feb-2022 commit "use median filter in average DT calculation"); deferred because multi-pass was disabled (hb-031); revived 2026-05-31 because multi-pass is BACK (hb-079 coherent, hb-080 N=3, hb-086 V1 joint-pair-retry — all GRADUATED).

## Why this is worth implementing now

When hb-057 was spawned, multi-pass was disabled, so any DT-prior the
mechanism produced couldn't inform a second decode attempt — value was
mostly latent. That has flipped: hb-079/080/086V1 reinstate multi-pass at
N=3 with coherent subtraction AND a joint-pair-retry on the residual.
The residual passes already run a localized Costas sync search (see
`localized_costas_sync_search` in `pancetta-ft8/src/decoder.rs`) which
restricts the *frequency* axis to ±N bins around a target position but
still scans the full ±2.0s time axis. A per-callsign DT prior can
narrow the time axis of that same localized search the same way V1's
position seeds narrow the frequency axis.

### Diagnostic result (Session 1, 2026-05-31)

Run: `cargo run --release -p pancetta-research --example hb057_dt_history_potential`
Corpus: refreshed top-20 worst hard-200 WAVs (1205 truths, 558 recovered, 647 missed under production multipass N=3 + V1 ON).

| metric | value |
|---|---|
| distinct multi-WAV callsigns | 250 |
| stable (var <0.1s) | 88 (35.2%) |
| moderate (0.1–0.3s) | 66 (26.4%) |
| unstable (var >0.3s) | 96 (38.4%) |
| missed with extractable sender | 639 / 647 (98.8%) |
| missed with multi-WAV DT history | 557 / 647 (86.1%) |
| **TARGET pop** (stable+moderate) | **250 / 647 (38.6%)** |
| Recoverable by ±0.2s prior (upper bound) | 250 / 647 (38.6%) |

Kill switch (10% target population) cleared by 3.86×. **PROCEED.**

Caveats on the upper bound:

- The ±0.2s window is comfortably larger than within-callsign DT noise for
  stable/moderate callsigns (moderate variance ≤0.3s, so leave-one-out
  median rarely strays past 0.2s from any sighting), so the recoverable
  = target-pop ceiling reflects the *gate's* coverage rather than what
  LDPC will actually convert. The implementation must measure real
  conversion in Session 2.
- The diagnostic builds DT history from baseline truth sightings. In
  production, pancetta builds history only from its *own* prior decodes.
  Real-world history is therefore a strict subset of the diagnostic's,
  and the cold-start cost is non-zero (a station never decoded before
  has no history → falls back to full sync).
- "Cross-WAV" here proxies for "cross-session" (the eval harness has no
  rolling-window concept). In production, the window is wall-clock
  bounded.

## Data structure

```rust
/// Per-callsign rolling DT history with median + IQR statistics. Sized
/// to cap memory and stale-data risk; older sightings are evicted FIFO.
pub struct CallsignDtHistory {
    /// Per-callsign ring buffer of (decode_time_utc, dt_s). Capacity 10
    /// is a round number — JTDX uses 5; we double it because pancetta's
    /// session windows are longer (24/7 ops vs JTDX's typical-operator
    /// session). Diagnostic showed the moderate bucket is dominated by
    /// callsigns with 3-5 sightings, so 10 is comfortably above the
    /// statistically-meaningful floor.
    entries: hashbrown::HashMap<String, VecDeque<DtSighting>>,
    /// Max age of a sighting before it expires (wall clock). Default
    /// 30 minutes — matches the rolling window the callsign continuity
    /// filter (`pancetta-qso::callsign_continuity`) uses for the
    /// recently-heard set.
    max_age: Duration,
    /// Per-callsign capacity. Default 10.
    capacity: usize,
}

#[derive(Clone, Copy)]
struct DtSighting {
    /// Wall-clock decode time, used for age-based eviction.
    at: SystemTime,
    /// The reported `time_offset` (DT in seconds, slot-relative) of the
    /// decoded message. Pancetta already computes this per-decode.
    dt_s: f64,
}

impl CallsignDtHistory {
    pub fn record(&mut self, callsign: &str, dt_s: f64, at: SystemTime) { ... }
    pub fn evict_expired(&mut self, now: SystemTime) { ... }
    /// Returns None if this callsign has <2 sightings (insufficient
    /// median; production sync fallback is correct).
    pub fn prior(&self, callsign: &str) -> Option<DtPrior> { ... }
}

pub struct DtPrior {
    /// Median DT across all current sightings for this callsign.
    pub median_dt: f64,
    /// Inter-quartile range (P75 - P25). Used to widen the prior gate
    /// when variance is higher.
    pub iqr: f64,
    pub sighting_count: usize,
}
```

Median + IQR (not mean + stddev): JTDX's reason is outlier robustness —
a single fade-induced bad-sync sighting at DT+1s shouldn't displace the
prior. Diagnostic confirms this matters: the unstable bucket (>0.3s
variance) is 38.4% of multi-WAV callsigns. Excluding the variance tail
via median preserves the prior's value for the moderate-variance head.

## Storage: where does it live?

| option | pros | cons |
|---|---|---|
| **per-decoder-thread** | zero cross-thread sync; co-located with the sync_search caller | each thread builds its own history independently — no cross-WAV / cross-session leverage (which IS the diagnostic's recovery population); cold-start on every thread spin-up |
| **coordinator-level (shared)** | cross-WAV / cross-thread history matches the diagnostic's population; survives decoder restarts as long as the coordinator persists; aligns with how the callsign continuity filter is already coordinator-scoped | requires a thread-safe handle (Arc<RwLock<CallsignDtHistory>>); a write per decode adds lock contention |

**Decision: coordinator-level.** The diagnostic's 38.6% recovery
population is *defined by* cross-WAV history (multi-WAV callsigns); a
per-thread design has zero access to it on a fresh start. Lock cost is
trivial: writes happen at the decode-emission rate (≤100/15s slot in
heavy traffic), reads happen once per sync candidate at the start of
the residual pass. `parking_lot::RwLock` with a `read()` for the prior
lookup keeps the hot path uncontended.

Persistence beyond process lifetime is *out of scope for V1*. If the
mechanism graduates, hb-057 V2 could persist the history to
`~/.pancetta/dt_history.json` and reload on startup — but the rolling
30-minute window means a cold start after a >30-min downtime is back
to empty, so persistence has limited carry-over value. Skip for V1.

## Wire into sync

The hook point is `localized_costas_sync_search`
(`pancetta-ft8/src/decoder.rs`, lines 2671–2749). Currently the time
axis loop is unrestricted (`for t0 in 0..=max_time_step`). The DT-prior
modification:

```rust
// New: per-CostasCandidate optional DT prior. None = full-time-axis scan
// (current behaviour). Some(prior) = restrict t0 around prior.median_dt.
//
// Wire-up: the multipass driver knows the callsign for each target
// position (it was DECODED in the prior pass; that's how we have a
// callsign at all). Pass the prior down with the target_positions
// vector. CostasCandidate already carries freq_sub/freq_bin; add an
// optional dt_prior payload.
//
// V1 algorithm: restrict t0 to the spectrogram steps whose
// real-time-offset lies in [median - max(0.2s, IQR * 3), median + ditto].
let time_window = match candidate.dt_prior {
    None => 0..=max_time_step, // unchanged
    Some(prior) => prior_time_step_range(prior, spectrogram, max_time_step),
};
for t0 in time_window {
    // ... existing inner loop
}
```

`prior_time_step_range` maps from real-time-offset (slot-relative
seconds) to spectrogram step using `time_padding` (the
`Spectrogram::time_padding` field already exists for negative-time
search; see decoder.rs:505). Window radius = `max(0.2s, IQR * 3)`:

- Floor of 0.2s tracks the diagnostic's chosen window (recoverable
  population is computed at this value); guards against pathological
  IQR=0 callsigns (stable bucket) collapsing to a sub-step window.
- IQR*3 scales the window for moderate-variance callsigns (3 IQR ≈ 6σ
  for Gaussian; FT8 DT distribution is heavier-tailed, but 3 IQR is a
  reasonable upper bound that includes ~99% of repeat sightings).

Where the prior is wired:

1. **Residual localized search** (`coherent_subtract_and_repass`'s
   localized pass + V1 joint-pair-retry's residual sync_search). These
   are the multi-pass passes hb-031's revival re-enabled, and the
   diagnostic's recovery population is **inside this scope**. Pass 1
   (production-config full sync_search) is NOT touched — its purpose is
   to surface callsigns, not exploit a history that doesn't exist yet.

2. **AP path** (`par_try_ap_decode`). AP knows the candidate callsign
   upfront (the entire mechanism is "decode the residual with this
   callsign injected"). The sync_search that *finds* the AP position
   can use the same DT prior to narrow time, increasing the SNR of the
   integrated Costas score at the right t0 (narrower noise integration
   window).

## Wire into AP — additional detail

`par_try_ap_decode` (called from V1 joint-pair-retry) currently:
1. Runs `localized_costas_sync_search` to find the position.
2. Extracts symbols at that position.
3. Computes soft LLRs.
4. Injects AP LLRs at the known callsign bits.
5. `decode_soft`.

Step 1 is where the DT prior helps: narrow the time-window before
running Costas. Steps 2-5 are unchanged. The prior is per-known-call
(AP2 = caller, AP1 = called); use the DT history of *whichever
callsign the AP path is injecting at the sender position*. The sender
maps to the bits-0-27 callsign in AP2 (and to bits-28-55 in the called
position, but typically AP2 cares about the caller).

## Configuration

| config | type | default | meaning |
|---|---|---|---|
| `dt_history_enabled` | bool | true | master switch (also gates the storage struct from being instantiated) |
| `dt_history_capacity` | usize | 10 | per-callsign ring buffer cap |
| `dt_history_max_age_s` | f64 | 1800.0 | sighting expiry (30 min) |
| `dt_history_window_floor_s` | f64 | 0.2 | minimum prior gate radius (the diagnostic's value) |
| `dt_history_window_iqr_scale` | f64 | 3.0 | gate = max(floor, IQR * scale) |
| `dt_history_min_sightings` | usize | 2 | below this, no prior (full ±2.0s scan) |

## Eval plan (Session 2)

A/B with `--hb057-dt-history-enabled` gating the storage struct +
`localized_costas_sync_search` time-window restriction. Sweep
`dt_history_window_floor_s` ∈ {0.1, 0.2, 0.3, 0.5} and
`dt_history_window_iqr_scale` ∈ {2.0, 3.0, 5.0, ∞}.

Watch metrics:

| metric | direction | gate |
|---|---|---|
| hard-200 recall | up | +12+ (matches hb-086 V1 grad bar) |
| hard-1000 recall | up | +15+ (proportional) |
| hard-200 novels | down or flat | no regression vs main |
| hard-1000 novels | down or flat | no regression |
| composite | up | ≥ +0.000350 (half the hb-086 V1 grad delta acceptable for a tighter mechanism) |
| elapsed | up | ≤ +5% (writes per decode + lock + narrower-but-not-tiny inner loop should be ≪ 5% overhead) |

Diagnostic-first kill-switch reruns inside Session 2 confirm the
recovery-population gate before the build, just like hb-086 V1 (78.3%
pair-likely vs 30% gate before that build).

## Risk inventory

| risk | mitigation |
|---|---|
| Cold-start (no history) regresses elapsed by adding bookkeeping cost for zero recall gain | The bookkeeping is O(1) per decode (HashMap insert + VecDeque push); negligible at decode-emission rates. The narrow-window path only fires when a prior exists, so cold-start traffic costs nothing extra at the sync_search itself. |
| Stale history (callsign moved DT after a long QSY or clock-resync) causes prior to *miss* the correct t0 | 30-min eviction window + median robustness across 10 sightings. If 3-of-10 sightings drift, median tracks them; if all 10 drift, the prior is correct after the eviction window. Worst case: 30 min of missed prior for that callsign, then re-acquired. |
| Prior leaks into pass 1 (full sync_search) and biases recall toward "known callsigns" / suppresses new-station discovery | Pass 1 is NEVER gated by DT prior — only the residual multipass passes and the AP-path sync_search use the prior. Discovery happens in pass 1; the prior accelerates *follow-up*. |
| Lock contention (Arc<RwLock<...>>) on hot decode path | Writes are per-emitted-decode (≤100/15s); reads are per-sync-candidate (a few hundred/slot but only at multipass time). Test in Session 2 with `--threads N`; if measurable, switch to `dashmap`. |
| Hit population shifts when measured outside top-20 (population may be top-20-specific) | Session 2 evaluates full hard-200 + hard-1000 + bench corpora, not just top-20. The 38.6% diagnostic is top-20 only; cross-corpus confirmation is part of the build. |

## Out-of-scope for V1

- Persistent storage across pancetta restarts (deferred to V2 if V1 grads).
- Cross-band DT history (each band has different propagation and
  per-station DT semantics; history is band-scoped in V1, or rather
  band-agnostic for V1 simplicity, with a noted risk).
- Mode-specific extensions to FT4 (FT8 only for V1).
- Using the prior to *reject* candidates whose t0 is far from the
  median (only narrow the search, never reject — rejection is a
  separate risk class).

## File touch list (Session 2 estimate)

- `pancetta-ft8/src/dt_history.rs` (new): `CallsignDtHistory` + `DtPrior` types.
- `pancetta-ft8/src/lib.rs`: re-export.
- `pancetta-ft8/src/decoder.rs`: `localized_costas_sync_search` accepts
  optional per-candidate DT prior; `coherent_subtract_and_repass` +
  `joint_pair_retry_pass` thread the prior down from the prior pass's
  decoded-callsigns.
- `pancetta-ft8/src/config.rs`: new config fields.
- `pancetta-ft8/src/ap.rs`: AP-path callers consume DT prior at sync.
- `pancetta/src/coordinator/*`: instantiate + own `CallsignDtHistory`;
  feed decode-emissions into `record()`; supply per-call prior into
  multipass.
- Eval harness: `--hb057-dt-history-enabled` flag, sweep configs.
