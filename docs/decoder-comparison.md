# Pancetta FT8 Decoder vs. ft8_lib — Decode Yield & Approach

How does Pancetta's **native Rust decoder** compare to the reference
[`kgoba/ft8_lib`](https://github.com/kgoba/ft8_lib) C decoder it ships
alongside? This note reports a measured, reproducible comparison on a large
real-world corpus and explains *why* the numbers come out the way they do.

## TL;DR

On **1,201 real off-air recordings** sampled across a 28,822-file corpus,
Pancetta's native decoder produced **+11.6% more decodes than ft8_lib on the
same audio** (5,581 vs. 5,003) while recovering **90.7%** of everything ft8_lib
found. The extra yield comes from **parallel multi-candidate decoding**, which
lets Pancetta afford more sync candidates and deeper recovery passes inside
FT8's 15-second real-time budget.

> **Honest caveat up front:** ft8_lib is used here as the *reference oracle*, so
> the comparison cannot credit Pancetta for correct decodes ft8_lib misses, and
> a fraction of our "extra" decodes are false positives (trimmed in production
> by the FP filter). A neutral-truth study is planned to quantify the split —
> see [Caveats](#caveats--what-this-does-and-doesnt-show).

## Methodology

- **Tool:** `pancetta benchmark-decode <dir>` decodes every WAV with **both**
  decoders on **identical audio** — Pancetta's native Rust decoder
  (`decode_window`) and ft8_lib (via FFI) — then dedups and compares the
  message sets.
- **Fair decoder-vs-decoder baseline:** the native side runs *without*
  a-priori (AP) context here. In production Pancetta runs ft8_lib as the
  primary decoder **plus** its native decoder as a secondary, AP-enhanced pass,
  so live yield is higher than these no-AP numbers.
- **Corpus:** 1,201 WAVs sampled every 24th file across 28,822 real off-air
  15-second recordings, spanning many bands, times, and signal conditions.
- **Build:** release (`opt-level=3`, LTO), Apple Silicon. ft8_lib is the
  vendored `kgoba/ft8_lib` reference, which is also Pancetta's validation
  oracle (the `ft8lib_crossval_tests` confirm bit-exactness on the shared path).

## Results

| Metric | Pancetta | ft8_lib |
|---|---:|---:|
| Total decodes | **5,581** | 5,003 |
| Per file (avg) | 4.6 | 4.2 |

| Comparison | Count |
|---|---:|
| Δ total | **+578 (+11.6%)** |
| Agreed (decoded by both) | 4,540 |
| Pancetta-only (we got, ft8_lib didn't) | 1,041 |
| ft8_lib-only (it got, we didn't) | 463 |
| Recall of ft8_lib's set | **90.7%** |
| Parity (`both / max(total)`) | 81.3% |

Read plainly:

- We produce **~12% more total decodes** than ft8_lib on the same audio.
- We **recover ~91%** of ft8_lib's decodes; the ~9% we miss (463) is mostly
  where ft8_lib's full sliding-frame sync edges us out on marginal signals.
- We surface **1,041 decodes ft8_lib didn't** — a mix of genuine extra catches
  and false positives (see caveats).

## Our approach: parallel multi-candidate decoding

FT8 has a hard **15-second slot budget**: all decoding for a window must finish
before the next window arrives. That budget is the real constraint — the more
candidate signals you can fully evaluate within it, the more you decode.

ft8_lib's reference decoder is **single-threaded**. Pancetta's native decoder
**fans the per-candidate decode out across CPU cores with [Rayon]** — Costas 2-D
sync produces a list of candidate (time, frequency) positions, and each one is
run through symbol extraction → max-log LLRs → sum-product LDPC → OSD fallback
**in parallel** (`par_decode_candidate`, with the AP0 candidate loop running
across Rayon workers).

That parallelism is what makes the *extra work affordable* within the slot:

- **More sync candidates kept** — a higher `max_sync_candidates` ceiling means
  weaker/closely-spaced candidates survive into LDPC instead of being truncated
  for time.
- **Deeper recovery per candidate** — sum-product LDPC iterations plus an OSD
  fallback (ordered-statistics decoding) recover codewords belief-propagation
  alone misses.
- **All inside the real-time budget** — on a multi-core host the candidates
  decode concurrently, so doing more work per slot doesn't blow the deadline.

More candidate attempts × deeper recovery, kept within budget by parallelism,
is why Pancetta extracts more decodes from the same audio. (In production the
secondary native pass *also* injects a-priori context — known QSO partner,
recently-heard callsigns — to recover exchange frames the generic pass can't,
on top of everything measured above.)

## Caveats — what this does and doesn't show

- **ft8_lib is the reference, not neutral truth.** Anything ft8_lib doesn't
  find is counted against us even if it's a *correct* decode it missed — so
  these numbers, if anything, understate us at the margin.
- **Not every "extra" is a win.** The 1,041 Pancetta-only decodes are a mix of
  real catches and false positives. Aggressive candidate breadth costs FP
  pressure; production runs an FP filter (callsign-continuity + suspicion
  scoring) to trim the low-confidence ones.
- **A neutral-truth study is planned** — scoring Pancetta, ft8_lib, and WSJT-X
  against the *union* of all three (or a known-transmitted corpus) — to
  quantify how many of the extras are genuine.

## Reproduce

```bash
# Decode a directory of 15-second WAVs with both decoders and compare:
cargo run --release -- benchmark-decode /path/to/wavs --format text
# Machine-readable:
cargo run --release -- benchmark-decode /path/to/wavs --format json
```

[Rayon]: https://github.com/rayon-rs/rayon
