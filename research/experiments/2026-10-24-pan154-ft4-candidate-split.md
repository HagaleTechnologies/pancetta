---
slug: pan154-ft4-candidate-split
mode: ft4
state: shipped
created: 2026-10-24T20:07:16.447710762+00:00
last_updated: 2026-10-24
branch: PAN-154
scorecard: research/scorecards/pan154-ft4-candidate-split.json
delta_vs_main: measurement only; no decoder change
disposition: Investigate decode failures first; separate candidate-cap work at weaker SNRs and correction of published ceiling wording.
---

# PAN-154: FT4 candidate versus decode failures

Of 193 jt9-truth misses, **26 (13.5%) had no candidate produced**
and **167 (86.5%) had a candidate produced, decode failed**.
The main follow-up should investigate the decode pipeline. At -16 through
-14 dB, 94 of 95 misses already had a candidate. The original 78% figure
is the endpoint of a sweep that stopped at -14 dB; this run recovers 50/50
at -13 dB and again at -12 dB.

## Question

> Developers should learn whether FT4's recall ceiling comes from missed candidates or failed decodes before building a fix.

## Method

The committed synth-ft4 tier contains 550 single-signal CQ WAVs, 50 at
each SNR from -24 through -14 dB, with AWGN referenced to a 2500 Hz
bandwidth. WAVs were regenerated from the existing config without changing
the committed config or manifest. Each truth message is a manifest
`encoded_message` that a real jt9-cache decode text contains. Native recovery
uses the same contains predicate on final decoded messages.

| Corpus identity | SHA-256 |
|---|---|
| Manifest | `b7eeaeee6e7cc186324ea6b10539b2a6b7025b17212ea5b2683605162c6131a9` |
| Generator config | `4f1b84c915aa6f10926a1131fe352ff691a2113d86c009fa7fd34aa950731783` |
| WAV set | `b2f472ce37860c243e8cd59f642abf17d8b8b7bc04b0fbef46b2b729df78e686` |

The WAV-set hash is SHA-256 over the concatenated lowercase hex hashes of
each WAV in manifest order. It equals PAN-110's recorded audio identity.

The oracle was Ubuntu 24.04's WSJT-X 2.7.0~rc3+repack-1build2, invoked by
`baseline` as `jt9 -5 -d 3`. The scorecard records
`jt9 (/tmp/jt9-wsjtx-2.7.0-rc3/jt9)`. The following extraction recipe
ran successfully without root. jt9's writable working directory is
separate from the checkout; its cache and the WAVs remain gitignored.

```bash
mkdir -p /tmp/pan154-jt9/debs /tmp/pan154-jt9-work /tmp/jt9-wsjtx-2.7.0-rc3
(cd /tmp/pan154-jt9/debs && apt-get download wsjtx libfftw3-single3 libgfortran5 libqt5core5t64 \
  libdouble-conversion3 libpcre2-16-0 libicu74 libb2-1)
for d in /tmp/pan154-jt9/debs/*.deb; do
  dpkg-deb -x "$d" /tmp/pan154-jt9/root
done
cat > /tmp/jt9-wsjtx-2.7.0-rc3/jt9 <<'SH'
#!/bin/sh
export LD_LIBRARY_PATH=/tmp/pan154-jt9/root/usr/lib/x86_64-linux-gnu
cd /tmp/pan154-jt9-work && exec /tmp/pan154-jt9/root/usr/bin/jt9 "$@"
SH
chmod +x /tmp/jt9-wsjtx-2.7.0-rc3/jt9
/tmp/jt9-wsjtx-2.7.0-rc3/jt9 --help
```

The native decoder uses `Ft8Config::default()` with only
`protocol = Protocol::Ft4`, unlimited time, `max_sync_candidates = 200`
and `min_sync_score = 3.0`. The existing
`Ft8Decoder::decode_window_with_candidate_dump` entry point observes
candidates admitted to the per-candidate decode loop, after threshold and
selection. No production code changed.

Truth frequency is `base_freq_hz`; truth time is `0.5 + dt_s` seconds
from buffer start, matching `gen_synth.rs::LEAD_IN_S_FT4`. A candidate
from any pass counts inside the inclusive primary gate of +/-10.5 Hz and
+/-0.096 s. Those widths cover one FT4 frequency grid step with slack
and four time grid steps. The nearest candidate minimizes squared distance
in FT4 grid units, 10.4167 Hz and 0.024 s.

Every jt9-truth miss gets exactly one label:

- `no candidate produced`: no admitted record inside the primary gate.
- `candidate produced, decode failed`: at least one admitted record inside
  the gate, but the final decode set does not recover the truth. A record
  that decoded another text still receives this label.

The sensitivity gate is +/-20.9 Hz and +/-0.16 s. Calibration requires
every recovered truth to have a primary-gated candidate and fails closed
otherwise. The 55 recovered truths had nearest candidates at most
5.08 Hz and 0.012 s away.
The tool also aborts on missing or inconsistent jt9 caches, WAV-loading
errors, decode errors, non-FT4 manifests or count-invariant failures.

Only primary no-candidate jt9-truth misses are rechecked, with only the
candidate cap raised to 1,000,000. This recheck considers pass 0, before
subtraction. It admitted between 2697 and 3012 candidates, so the raised cap never bound. All 550 default windows filled
the 200-candidate cap, as the tool's progress output reported. The tool
processes WAVs in parallel but collects rows in manifest order.

Commands from the repository root:

```bash
cargo build --release -p pancetta-research --bin gen-synth --bin baseline --bin ft4-candidate-split
./target/release/gen-synth --config research/corpus/synth/manifests/ft4_clean.config.json \
  --output /tmp/pan154/ft4_clean.regen.manifest.json
./target/release/baseline --tier synth --mode ft4 \
  --synth-manifest research/corpus/synth/manifests/ft4_clean.manifest.json \
  --jt9-path /tmp/jt9-wsjtx-2.7.0-rc3/jt9
./target/release/ft4-candidate-split \
  --manifest research/corpus/synth/manifests/ft4_clean.manifest.json \
  --baseline-dir research/baselines/ft4 --cap-lifted-recheck 1000000 \
  --output research/scorecards/pan154-ft4-candidate-split.json
```

## Results

The tool's stdout, including its unrounded-count-derived percentages:

```text
PAN-154 FT4 candidate split: native decoder, Ft8Config::default() + Protocol::Ft4, unlimited budget
corpus research/corpus/synth/manifests/ft4_clean.manifest.json (550 WAVs), truth = jt9, gate +/-10.5 Hz / +/-0.096 s

| SNR (dB) | jt9 truths | Pancetta recovered | jt9-truth misses | no candidate produced | candidate produced, decode failed |
|---:|---:|---:|---:|---:|---:|
| -24 | 0 | 0 | 0 | 0 (-) | 0 (-) |
| -23 | 0 | 0 | 0 | 0 (-) | 0 (-) |
| -22 | 0 | 0 | 0 | 0 (-) | 0 (-) |
| -21 | 0 | 0 | 0 | 0 (-) | 0 (-) |
| -20 | 1 | 0 | 1 | 1 (100.0%) | 0 (0.0%) |
| -19 | 9 | 0 | 9 | 5 (55.6%) | 4 (44.4%) |
| -18 | 40 | 0 | 40 | 12 (30.0%) | 28 (70.0%) |
| -17 | 48 | 0 | 48 | 7 (14.6%) | 41 (85.4%) |
| -16 | 50 | 3 | 47 | 1 (2.1%) | 46 (97.9%) |
| -15 | 50 | 13 | 37 | 0 (0.0%) | 37 (100.0%) |
| -14 | 50 | 39 | 11 | 0 (0.0%) | 11 (100.0%) |
| Total | 248 | 55 | 193 | 26 (13.5%) | 167 (86.5%) |

TOTAL jt9-truth misses: 193 | no candidate produced: 26 (13.5%) | candidate produced, decode failed: 167 (86.5%)
sensitivity gate +/-20.9 Hz / +/-0.16 s: no candidate produced 23, candidate produced, decode failed 170 (3 labels changed)
cap-lifted recheck (max_sync_candidates=1000000) of 26 no-candidate misses: 25 present above min_sync_score, cut by the max_sync_candidates cap; 1 absent above min_sync_score; 1 truth recovered
gate calibration: 55 recovered truths, nearest candidate max |df| 5.08 Hz, max |dt| 0.012 s
pancetta-only recoveries (jt9 missed): 0; manifest misses outside jt9 truth: 302 (226 no candidate produced, 76 candidate produced, decode failed)
```

The full list of no-candidate misses follows. Offsets are signed relative
to truth. The cap-lifted label distinguishes a candidate above threshold
that the default cap cut from a candidate absent above threshold.

| SNR dB | Message | Nearest df Hz | Nearest dt s | Nearest sync | Recheck pass-0 count | Best gated sync | Cap-lifted label | Truth recovered |
|---:|---|---:|---:|---:|---:|---:|---|:---:|
| -20 | CQ K1AAS FM06 | -39.22 | 0.372 | 5.384 | 2932 | 3.984 | present above min_sync_score, cut by the max_sync_candidates cap | no |
| -19 | CQ K1AAK FN31 | -48.55 | -0.053 | 5.037 | 3004 | 4.791 | present above min_sync_score, cut by the max_sync_candidates cap | no |
| -19 | CQ K1AAS FM06 | -7.08 | -0.247 | 5.185 | 2951 | 4.337 | present above min_sync_score, cut by the max_sync_candidates cap | no |
| -19 | CQ K1ABP EN91 | -66.81 | -0.377 | 5.124 | 2954 | 3.771 | present above min_sync_score, cut by the max_sync_candidates cap | no |
| -19 | CQ K1ABQ EM10 | -18.56 | -0.097 | 6.025 | 3007 | 4.767 | present above min_sync_score, cut by the max_sync_candidates cap | no |
| -19 | CQ K1ABT DM13 | -120.09 | 0.291 | 5.110 | 2995 | 4.680 | present above min_sync_score, cut by the max_sync_candidates cap | no |
| -18 | CQ K1AAG CN85 | 13.68 | 0.007 | 5.238 | 3009 | 3.829 | present above min_sync_score, cut by the max_sync_candidates cap | no |
| -18 | CQ K1AAI EN61 | 79.36 | -0.272 | 5.261 | 2932 | 3.264 | present above min_sync_score, cut by the max_sync_candidates cap | no |
| -18 | CQ K1AAK FN31 | -39.45 | 0.092 | 5.395 | 2929 | 3.535 | present above min_sync_score, cut by the max_sync_candidates cap | no |
| -18 | CQ K1AAV EN91 | 37.32 | 0.119 | 5.255 | 2971 | 4.687 | present above min_sync_score, cut by the max_sync_candidates cap | no |
| -18 | CQ K1AAZ DM13 | 0.59 | -0.234 | 5.092 | 2698 | 4.006 | present above min_sync_score, cut by the max_sync_candidates cap | no |
| -18 | CQ K1ABB DN70 | -33.20 | -0.064 | 6.158 | 2956 | none | absent above min_sync_score | no |
| -18 | CQ K1ABG FM29 | 37.73 | 0.342 | 5.043 | 2992 | 4.967 | present above min_sync_score, cut by the max_sync_candidates cap | no |
| -18 | CQ K1ABI CM87 | 65.22 | 0.069 | 5.013 | 2847 | 4.813 | present above min_sync_score, cut by the max_sync_candidates cap | no |
| -18 | CQ K1ABK EN34 | -17.49 | 0.108 | 5.030 | 2973 | 4.802 | present above min_sync_score, cut by the max_sync_candidates cap | no |
| -18 | CQ K1ABR EL29 | -114.92 | 0.112 | 5.935 | 2893 | 4.519 | present above min_sync_score, cut by the max_sync_candidates cap | no |
| -18 | CQ K1ABU CN85 | -78.49 | -0.036 | 5.647 | 2886 | 4.772 | present above min_sync_score, cut by the max_sync_candidates cap | no |
| -18 | CQ K1ABX EM79 | 220.58 | -0.023 | 4.914 | 2697 | 4.040 | present above min_sync_score, cut by the max_sync_candidates cap | no |
| -17 | CQ K1AAB EN91 | 131.29 | -0.065 | 5.208 | 2889 | 3.773 | present above min_sync_score, cut by the max_sync_candidates cap | no |
| -17 | CQ K1AAG CN85 | 53.02 | 0.206 | 5.467 | 2840 | 3.692 | present above min_sync_score, cut by the max_sync_candidates cap | no |
| -17 | CQ K1AAJ EM79 | 37.17 | 0.108 | 5.182 | 2936 | 3.883 | present above min_sync_score, cut by the max_sync_candidates cap | no |
| -17 | CQ K1ABB DN70 | -67.43 | 0.067 | 5.110 | 2928 | 4.677 | present above min_sync_score, cut by the max_sync_candidates cap | no |
| -17 | CQ K1ABF FN20 | -320.23 | 0.079 | 5.464 | 3004 | 4.214 | present above min_sync_score, cut by the max_sync_candidates cap | no |
| -17 | CQ K1ABN EM64 | 110.21 | 0.038 | 4.999 | 3000 | 3.787 | present above min_sync_score, cut by the max_sync_candidates cap | no |
| -17 | CQ K1ABV DN70 | 37.11 | -0.185 | 6.469 | 2865 | 4.493 | present above min_sync_score, cut by the max_sync_candidates cap | no |
| -16 | CQ K1AAU FN42 | -6.48 | -0.208 | 5.469 | 3012 | 4.370 | present above min_sync_score, cut by the max_sync_candidates cap | yes |

All eleven misses at -14 dB had well-placed admitted candidates. Their
nearest candidates were:

| Message | Gated candidates | abs df Hz | abs dt s | Sync score |
|---|---:|---:|---:|---:|
| CQ K1AAC EM10 | 4 | 3.59 | 0.003 | 9.328 |
| CQ K1AAE FM19 | 2 | 4.38 | 0.010 | 5.855 |
| CQ K1AAK FN31 | 5 | 3.00 | 0.009 | 8.707 |
| CQ K1AAZ DM13 | 3 | 1.10 | 0.010 | 7.623 |
| CQ K1ABB DN70 | 4 | 5.07 | 0.003 | 5.089 |
| CQ K1ABF FN20 | 6 | 2.76 | 0.012 | 7.823 |
| CQ K1ABG FM29 | 4 | 0.55 | 0.007 | 7.699 |
| CQ K1ABH EL98 | 4 | 2.49 | 0.002 | 10.418 |
| CQ K1ABM FM06 | 4 | 4.87 | 0.001 | 9.780 |
| CQ K1ABQ EM10 | 5 | 4.84 | 0.004 | 9.569 |
| CQ K1ABS FM19 | 3 | 1.19 | 0.008 | 9.069 |

## Supplement outside the committed tier

This separate scratch corpus changes only the label, output directory and
SNR steps to -13 and -12 dB. Its 100 WAVs are excluded from the committed
scorecard and from every percentage above.

```bash
python3 - <<'PYTHON'
import json
c = json.load(open('research/corpus/synth/manifests/ft4_clean.config.json'))
c.update(label='ft4_clean_ext_scratch', snr_steps_db=[-13.0, -12.0],
         output_dir='/tmp/pan154/ext_wavs')
json.dump(c, open('/tmp/pan154/ft4_ext.config.json', 'w'), indent=2)
PYTHON
./target/release/gen-synth --config /tmp/pan154/ft4_ext.config.json \
  --output /tmp/pan154/ft4_ext.manifest.json
./target/release/baseline --tier synth --mode ft4 \
  --synth-manifest /tmp/pan154/ft4_ext.manifest.json \
  --jt9-path /tmp/jt9-wsjtx-2.7.0-rc3/jt9
./target/release/ft4-candidate-split --manifest /tmp/pan154/ft4_ext.manifest.json \
  --output /tmp/pan154/ft4_ext.split.json
```

```text
PAN-154 FT4 candidate split: native decoder, Ft8Config::default() + Protocol::Ft4, unlimited budget
corpus /tmp/pan154/ft4_ext.manifest.json (100 WAVs), truth = jt9, gate +/-10.5 Hz / +/-0.096 s

| SNR (dB) | jt9 truths | Pancetta recovered | jt9-truth misses | no candidate produced | candidate produced, decode failed |
|---:|---:|---:|---:|---:|---:|
| -13 | 50 | 50 | 0 | 0 (-) | 0 (-) |
| -12 | 50 | 50 | 0 | 0 (-) | 0 (-) |
| Total | 100 | 100 | 0 | 0 (-) | 0 (-) |

TOTAL jt9-truth misses: 0 | no candidate produced: 0 (-) | candidate produced, decode failed: 0 (-)
sensitivity gate +/-20.9 Hz / +/-0.16 s: no candidate produced 0, candidate produced, decode failed 0 (0 labels changed)
gate calibration: 100 recovered truths, nearest candidate max |df| 5.21 Hz, max |dt| 0.012 s
pancetta-only recoveries (jt9 missed): 0; manifest misses outside jt9 truth: 0 (0 no candidate produced, 0 candidate produced, decode failed)
```

The 78% figure is where W0.4's sweep stopped, not a plateau. Pancetta reaches
50/50 one dB higher. Both supplementary bins also have 50/50 jt9 truths.

## Comparison with the planning probe

The fresh run matches Appendix A of the PAN-154 plan: 248 jt9 truths,
55 native recoveries, 193 misses split 26/167, sensitivity split 23/170
with three flips, and cap-lifted results 25 present, one absent, one
recovered. Every per-SNR jt9 and native count matches W0.4. The 26
no-candidate identities and the eleven -14 dB miss identities also match.
The supplementary bins reproduce 50/50 for both decoders. There is no
count discrepancy to explain.

## Caveats

- This measures synthetic, single-signal AWGN, not crowded on-air audio.
- This measures the unlimited native decoder. Production merges ft8_lib
  and native results under an effort budget; PAN-110 recorded 43/50 at
  -14 dB for that different pipeline.
- "Candidate produced" means admitted to the decode loop. The cap-lifted
  recheck is what distinguishes capped candidates from missing
  above-threshold candidates.
- This Ubuntu jt9 build differs from W0.4's macOS build, although every
  per-bin oracle count agrees.
- "Decode failed" does not yet distinguish demodulation/LLR, BP or OSD.
- With a smaller user-selected recheck cap, absence is inconclusive if
  the recheck fills that cap. The tool reports a separate inconclusive
  sub-label and count for this case. The committed run's million-candidate
  cap did not bind.

## Follow-up direction

Investigate why well-placed, strong candidates fail to recover truth at
-16 through -14 dB. That decode-side investigation addresses 94/95 misses
in those bins, including all eleven at -14 dB, and is the first candidate
follow-up ticket.

A separate candidate-selection investigation should examine the
200-candidate cap at weaker SNRs. Raising it exposes 25 of the 26 missing
truth-position candidates, but recovers only one extra truth. Most of
those signals still need decode-side improvements after admission.

Correct the published "caps at 78%" wording in a separate documentation
ticket with its published-claims checks. This experiment supports a sweep
endpoint statement, not an asymptotic recall ceiling. No fixes or
published-claim edits are included in PAN-154.
