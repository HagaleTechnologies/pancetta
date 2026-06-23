# Algorithm spec: JS8Call-Improved soft combiner across repeated receptions

## Source attribution

- Origin: JS8Call-Improved (https://github.com/JS8Call-improved/JS8Call-improved)
- File path (for traceability, NOT to be quoted): `JS8_Mode/soft_combiner.h`
- License: GPL-3.0
- Reader date: 2026-06-08

## Purpose

When the same transmission is heard more than once (because the
operator re-keys it, or the band is repeating a beacon, or a station
is calling CQ over and over), each reception independently produces a
set of soft LLRs from the symbol demapper. None of those individual
receptions may have enough SNR to LDPC-decode on its own. But the
*sum* of the LLRs from multiple receptions — coherently aligned —
often does decode, because additive LLR combining is mathematically
equivalent to coherent averaging in the soft-decision domain.

The soft combiner identifies repeat candidates without operator help
and accumulates their LLRs into a single combined LLR stream that
LDPC sees as a higher-SNR version of the same signal. It is a
transparent time-diversity boost.

## Algorithm description (PROSE ONLY)

### Inputs

- Per-candidate metadata: mode (or submode), frequency bin (coarse
  integer bin from the candidate generator), time bin (also coarse
  integer), and the pair of LLR arrays (one per LDPC bit position; in
  the JS8 source there are two LLR estimates from two independent
  symbol-statistics paths, hence "soft pair").
- A configured cache TTL: how long an entry can sit before it's
  evicted as stale (typical: minutes).
- A configured signature width: the number of bits used in the
  approximate-match signature (32 in JS8Call-Improved).
- A configured Hamming tolerance: how many bits of the signature can
  differ between two candidates before they're considered different
  (4 in JS8Call-Improved).

### Outputs

- A possibly-combined LLR pair: either the input pass through
  unchanged (no prior repeat found) or an element-wise sum with one
  or more prior receptions of the same candidate.
- A repeat counter: how many receptions contributed to this output.
  Downstream code uses this for telemetry and for deciding whether to
  amplify decoder effort on heavily-repeated candidates.

### Steps

1. **Key construction**. For each incoming candidate:
   1. Build a coarse key as a tuple `(mode, freq_bin, time_bin)`.
      Note `time_bin` is the candidate's DT offset rounded to a
      coarse grid (one symbol period or finer); not wall clock.
   2. Build a 32-bit signature by sampling the LLR pair at fixed
      positions and packing the sign bits. The signature is a
      fingerprint: same payload → mostly-same signature; different
      payload → very different signature.
2. **Lookup**. Search the cache for entries whose coarse key matches
   exactly. Among those, find any whose 32-bit signature differs
   from the new candidate's signature by ≤ 4 bits (Hamming distance).
   The Hamming tolerance accommodates per-symbol noise that flips a
   handful of LLR signs between receptions without the underlying
   payload actually changing.
3. **Combine or insert**. If a match is found:
   1. Element-wise add the new candidate's LLR pair to the cached
      entry's LLR pair (both halves of the soft pair, in parallel).
   2. Increment the entry's repeat counter.
   3. Refresh its timestamp.
   4. Return the *combined* LLR pair (a copy, not a reference, since
      the LDPC decoder will mutate it) and the repeat counter.

   If no match:
   1. Insert a new entry keyed by `(coarse_key, signature)` carrying
      the incoming LLR pair and a fresh timestamp.
   2. Return the original LLR pair and `repeat_counter = 1`.
4. **Cleanup**. On every operation (or on a periodic timer):
   1. Drop entries older than the configured TTL.
   2. Drop entries that have been flagged as successfully decoded
      downstream — once a payload has cleared CRC, there is no value
      in continuing to accumulate softness for it.

### Numerical constants (facts, not expression)

- Signature width: 32 bits.
- Hamming tolerance: ≤ 4 bits.
- TTL: configurable; typical values are on the order of a few minutes
  for a 15-second-slot mode; longer for slower JS8 submodes.
- Cache capacity: bounded by TTL × candidate rate; in practice a few
  hundred entries on a busy band.

### Edge cases

- Signature collision (different payload, same coarse key, same
  signature): the LLR addition will produce nonsense — the combined
  LLRs will look noisy and will fail CRC downstream. The repeat
  counter is metadata, not truth. CRC remains the gate; signature
  collisions are tolerated as a low-rate decode failure, not a hazard.
- Time-bin wobble across receptions of the same actual signal: the
  coarse time-bin grid must be coarse enough to absorb the worst-case
  per-reception jitter (one symbol period works in practice for FT8;
  JS8's slower submodes can use a wider bin).
- Memory pressure: with too small a TTL the cache underperforms;
  with too large a TTL it grows unbounded on a busy band. The
  cleanup pass must run on every insert, not just on a timer, to
  prevent unbounded growth on bursty traffic.
- Frequency drift across receptions: a slow tx drifter will not key
  in to the same `freq_bin` across receptions. This mechanism does
  not handle that; pair it with the per-candidate frequency tracker
  (separate spec) if drift is a concern.

## Conflict with pancetta's existing mechanisms

- Pancetta has no cross-slot soft combining today. Each slot's
  decode is independent.
- This mechanism introduces persistent state across slots, owned by
  the decoder context. Implication: must be safely shareable across
  the multi-stream-TX-aware coordinator; needs interior mutability
  (Mutex or RwLock).
- Strong interaction with `hb-217` (RR73 truth recovery): JS8Call's
  soft combiner is the kind of thing that would close more of the
  weak-signal coverage gap that hb-218 is targeting at the capture-
  effect level. It does so by accumulating softness across calls
  rather than by joint decoding within one slot, so it stacks with a
  future hb-218 joint decoder rather than competing.

## Estimated Rust port effort

- ~250 LOC including the cache (HashMap keyed on coarse_key, with a
  Vec of `(signature, entry)` for the Hamming-distance walk), TTL
  cleanup, telemetry, unit tests, and config plumbing.
- 2 implementer sessions.

## Implementation notes for the implementer thread

- New module: `pancetta-ft8/src/decoder/soft_combiner.rs`.
- Public API:
  - `struct SoftCombiner { cache, cfg, ... }`
  - `fn combine(&mut self, key: CandidateKey, llrs: &[f32; N]) -> CombinedLlrs`
  - `fn mark_decoded(&mut self, key: CandidateKey)` to evict on
    successful CRC.
  - `fn cleanup(&mut self)` (also called internally on every insert).
- `CandidateKey` is `(mode, freq_bin, time_bin)`. `mode` is a `u8`
  for now (FT8 only), but spec it as an enum to leave room for JS8
  submodes.
- Cache layout: `HashMap<CoarseKey, Vec<Entry>>`, where `Entry` holds
  the 32-bit signature, the combined LLR buffer, the repeat counter,
  and `Instant` last-touched. The Hamming-≤4 scan over the Vec is
  O(small number); buckets stay tiny in practice.
- Signature construction must be deterministic across runs:
  hash-stable. The simplest correct approach is to sample the LLR
  pair at 32 fixed bit positions (e.g., the first 32 even-indexed
  positions) and pack the sign bits. Document this in the spec.
- Config knobs in `pancetta-config::Ft8Config` or a new
  `Ft8SoftCombinerConfig`: `enabled` (default false until hb-bench
  validates), `ttl_seconds` (default 180), `hamming_tolerance`
  (default 4), `signature_width` (default 32, mostly a hash-stable
  constant, not really tunable).
- Hot-path insertion site: in `decoder.rs`, after the LLR extraction
  step and before the LDPC step, route LLRs through
  `SoftCombiner::combine`. Use the *combined* output for LDPC. On
  successful CRC, call `mark_decoded`.
- Telemetry: log `repeat_counter` whenever a decode succeeds after a
  combine, with a `target: "decoder.softcombine"` for filtering.
  Surface in the research scorecard so we can quantify how often this
  mechanism actually contributes to a fresh decode vs. just sitting
  there.
- Bench gate: as a new hypothesis in the bank. Suggest `hb-221` or
  next-free. The right test corpus is one with deliberate
  repetitions (CQ contests, beacons); pancetta's existing hard-200
  may not exercise this mechanism. Note for the hypothesis bank:
  this is "characterise on a different corpus" rather than "graduate
  on hard-200".
