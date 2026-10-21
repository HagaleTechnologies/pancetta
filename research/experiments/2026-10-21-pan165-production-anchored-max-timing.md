---
slug: pan165-production-anchored-max-timing
mode: ft8
state: shipped
created: 2026-10-21T03:43:00Z
last_updated: 2026-10-21
branch: PAN-165
parent_hypothesis: PAN-157's prodmax scorecards started a fresh 2000 ms deadline right before the native call; production anchors it at window receipt and spends part of it first
wild_card: false
scorecard: research/scorecards/pan165-*.json
delta_vs_main: Max multipass now gated on receipt-anchored headroom; see the tables for recall and overrun before and after
disposition: SHIPPED. Headroom-aware multipass gate in pancetta-ft8, production-anchored timing mode in eval, per-window `window timing` debug line in the coordinator.
---

# PAN-165: production-anchored timing for the `Max` preset's second pass

## Question

PAN-157 bound a second decode pass (`max_decode_passes = 2` plus
`time_varying_subtraction_enabled = true`) to the `Max` effort preset and
justified it with `research/scorecards/pan157-pair-{control,variant}-prodmax.json`
(55.6% → 97.2% weak-signal recovery on `synth-pair-200` under a 2000 ms
budget). That harness started a fresh 2000 ms deadline right before the native
decode call. Production (`pancetta/src/coordinator/ft8.rs`) anchors the same
deadline at window receipt and first spends part of it on waterfall
generation, the optional scoped fast-path decode (active QSO) and the ft8_lib
FFI decode. Does the win hold, and does pass 2 still fit, once that overhead
is charged against the same deadline?

## Host

- CPU: AMD Ryzen 9 5950X 16-Core Processor (`lscpu` model name)
- `nproc`: 32
- cgroup `cpu.max`: `600000 100000` (a 6-CPU quota)
- Build profile: `release` (`opt-level = 3`, `lto = true`, `codegen-units = 1`), rustc 1.98.0
- ft8lib_available: true (every `pan165-*` scorecard records
  `window_timing.ft8lib_available: true`; the `pancetta-ft8/vendor/ft8_lib`
  submodule was initialised, so the ft8_lib call was real, not a stub)
- This is not the Windows MiniPC and not the 12-core aarch64 runner behind the
  prodmax scorecards. Numbers here say nothing about the MiniPC; see
  "MiniPC rerun" below.

Measurement hygiene, stated plainly: the runs ran serially from one script,
with before/after pairs back to back. The "before" runs used a release `eval`
built from this branch before the gate change, the "after" runs one built with
it. The first `pan165-pair-control-receipt` and
`pan165-pair-variant-receipt-before` runs overlapped a `nice -n 19` debug build
of this branch, so both were rerun afterwards with the host to themselves; the
committed scorecards are those clean reruns (finding 7 records what the
contended run showed). The host is a cgroup slice of a shared 32-CPU machine,
so run-to-run noise of about 100 ms in window times is expected.

## Commands

```bash
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_KEY_1 -u GIT_CONFIG_KEY_2 \
    -u GIT_CONFIG_VALUE_0 -u GIT_CONFIG_VALUE_1 -u GIT_CONFIG_VALUE_2 -u GIT_ASKPASS \
    git -c safe.directory='*' submodule update --init pancetta-ft8/vendor/ft8_lib
cargo build --release -p pancetta-research --bin eval --bin gen-synth-pair
./target/release/gen-synth-pair --config research/corpus/synth/manifests/synth_pair_200.config.json \
  --output /tmp/synth_pair_200.manifest.json   # regenerates the gitignored WAVs
E=./target/release/eval; P2="--max-passes 2 --time-varying-subtraction-enabled"
S=research/scorecards
# Before the gate (Phase 2 build) and after it (Phase 3 build), same flags:
$E --tier fixtures       --mode ft8 --effort max --max-passes 1 --budget-anchor receipt --output $S/pan165-fixtures-control-receipt.json
$E --tier fixtures       --mode ft8 --effort max $P2 --budget-anchor receipt --output $S/pan165-fixtures-variant-receipt-{before,after}.json
$E --tier synth-pair-200 --mode ft8 --effort max --max-passes 1 --budget-anchor receipt --output $S/pan165-pair-control-receipt.json
$E --tier synth-pair-200 --mode ft8 --effort max $P2 --budget-anchor receipt --output $S/pan165-pair-variant-receipt-{before,after}.json
$E --tier synth-pair-200 --mode ft8 --effort max $P2 --budget-anchor receipt --qso-partner-hz 1500 --output $S/pan165-pair-variant-receipt-qso-{before,after}.json
$E --tier synth-pair-200 --mode ft8 --effort max $P2 --budget-anchor call    --output $S/pan165-pair-variant-call-{before,after}.json
```

`--effort max` resolves to the FT8 ceiling (2000 ms), as production maps
`Max`'s `0` sentinel through `decode_budget_ceiling_ms`. `--budget-anchor call`
is the prodmax timing model; `receipt` runs waterfall → (scoped, with
`--qso-partner-hz`) → ft8_lib → native under one receipt-anchored deadline and
scores the native call's decodes only, as the prodmax scorecards did.
`--qso-partner-hz 1500` is the synth-pair strong signal's fixed base frequency
(`pancetta-research/src/synth.rs`); it runs the scoped call at ±5 bins and
narrows the native call to ±60 Hz, production's default QSO path.
The control runs are not repeated after the gate: a 1-pass config never
reaches either gate (`single_pass_config_reports_one_pass_and_no_multipass_stages`).

## Results

**Table 1.** pre-native overhead (ms, p50 / p95 / max) and headroom at the native call (ms, p50 / min)

| scorecard | waterfall | scoped | ft8_lib | pre-native total | headroom at native |
|---|---|---|---|---|---|
| `pan165-pair-control-receipt.json` | 3 / 3 / 6 | not run | 13 / 14 / 19 | 16 / 18 / 24 | 1984 / 1976 |
| `pan165-pair-variant-call-before.json` | 0 / 0 / 0 | not run | 0 / 0 / 0 | 0 / 0 / 0 | 2000 / 2000 |
| `pan165-pair-variant-call-after.json` | 0 / 0 / 0 | not run | 0 / 0 / 0 | 0 / 0 / 0 | 2000 / 2000 |
| `pan165-pair-variant-receipt-before.json` | 3 / 5 / 6 | not run | 13 / 13 / 18 | 16 / 18 / 22 | 1984 / 1978 |
| `pan165-pair-variant-receipt-after.json` | 4 / 6 / 9 | not run | 13 / 17 / 25 | 18 / 22 / 31 | 1982 / 1969 |
| `pan165-pair-variant-receipt-qso-before.json` | 6 / 7 / 14 | 648 / 874 / 1044 | 13 / 17 / 24 | 666 / 894 / 1068 | 1338 / 932 |
| `pan165-pair-variant-receipt-qso-after.json` | 6 / 7 / 10 | 609 / 844 / 977 | 12 / 15 / 21 | 626 / 863 / 997 | 1375 / 1003 |
| `pan165-fixtures-control-receipt.json` | 3 / 4 / 6 | not run | 12 / 13 / 13 | 16 / 17 / 18 | 1984 / 1982 |
| `pan165-fixtures-variant-receipt-before.json` | 3 / 3 / 6 | not run | 12 / 13 / 13 | 15 / 16 / 18 | 1985 / 1982 |
| `pan165-fixtures-variant-receipt-after.json` | 4 / 7 / 11 | not run | 12 / 18 / 20 | 17 / 26 / 29 | 1983 / 1971 |

**Table 2.** synth-pair-200 recovery (180 WAVs)

| scorecard | anchor | weak recovered | strong recovered | windows where pass 2 ran |
|---|---|---|---|---|
| `pan165-pair-control-receipt.json` | receipt | 101/180 (56.1%) | 176/180 | 0/180 |
| `pan165-pair-variant-call-before.json` | call | 175/180 (97.2%) | 178/180 | 178/180 |
| `pan165-pair-variant-call-after.json` | call | 175/180 (97.2%) | 178/180 | 178/180 |
| `pan165-pair-variant-receipt-before.json` | receipt | 175/180 (97.2%) | 178/180 | 178/180 |
| `pan165-pair-variant-receipt-after.json` | receipt | 175/180 (97.2%) | 178/180 | 178/180 |
| `pan165-pair-variant-receipt-qso-before.json` | receipt + QSO 1500 Hz | 175/180 (97.2%) | 178/180 | 178/180 |
| `pan165-pair-variant-receipt-qso-after.json` | receipt + QSO 1500 Hz | 175/180 (97.2%) | 178/180 | 176/180 |

**Table 3.** window time against the 2000 ms deadline (ms)

| scorecard | window_total p50 / p95 / max | windows over deadline | max over deadline |
|---|---|---|---|
| `pan165-fixtures-control-receipt.json` | 741 / 875 / 879 | 0/13 | 0 |
| `pan165-fixtures-variant-receipt-before.json` | 852 / 3744 / 3879 | 4/13 | 1879 |
| `pan165-fixtures-variant-receipt-after.json` | 954 / 2015 / 2032 | 2/13 | 32 |
| `pan165-pair-control-receipt.json` | 917 / 1091 / 1223 | 0/180 | 0 |
| `pan165-pair-variant-call-before.json` | 1266 / 1555 / 1672 | 0/180 | 0 |
| `pan165-pair-variant-call-after.json` | 1307 / 1598 / 1782 | 0/180 | 0 |
| `pan165-pair-variant-receipt-before.json` | 1260 / 1476 / 1573 | 0/180 | 0 |
| `pan165-pair-variant-receipt-after.json` | 1361 / 1643 / 1808 | 0/180 | 0 |
| `pan165-pair-variant-receipt-qso-before.json` | 1391 / 1863 / 2126 | 4/180 | 126 |
| `pan165-pair-variant-receipt-qso-after.json` | 1338 / 1799 / 2012 | 1/180 | 12 |

## Findings

1. **The prodmax win reproduces on this host under every timing model.**
   With the second pass, weak-signal recovery is 175/180 (97.2%) whether the
   deadline starts at the native call (`variant-call`, the prodmax model),
   at window receipt (`variant-receipt`), or at receipt with an active QSO
   (`variant-receipt-qso`), against 101/180 (56.1%) for the 1-pass control.
   That matches `pan157-pair-{control,variant}-prodmax.json` (55.6% → 97.2%).
   Here one full-band pass finishes well inside 2000 ms (control p50 window
   917 ms), so pass 2 has room in 176-178 of 180 windows. The 2-core
   probe in the PAN-165 plan (cgroup `cpu.max` = `200000 100000`, same CPU
   model) found the opposite: one pass took about 2.1 s, and Max recovered
   88/180 weak signals, the same as its 1-pass control, under both timing
   models. The win is a property of hosts where pass 0 leaves room, not of
   the preset.
2. **Pre-native overhead without a QSO is small.** Waterfall plus the real
   ft8_lib call cost 15-18 ms at p50 and at most 31 ms, so the native call
   starts with at least 1969 ms of its 2000 ms.
3. **With a QSO, the scoped fast-path call is the dominant pre-native cost.**
   It took 609-648 ms at p50 and up to 1044 ms, leaving the native call
   932-1003 ms at worst. The ±60 Hz narrowing of the native call keeps pass
   2 affordable regardless (it ran in 176-178/180 windows). The scoped call
   has no cap or budget of its own beyond the shared deadline; this record
   notes that and does not change it.
4. **Correction to the PAN-165 research doc.** Its scoped-call scenario ran a
   *full-band* native call after the scoped call. In production the native
   call is narrowed to ±60 Hz around the partner whenever a QSO is active
   (`pancetta/src/coordinator/qso_filter.rs`, `DEFAULT_HALF_WINDOW_HZ`), so
   scoped + full-band happens only under the `PANCETTA_QSO_FILTER_OFF=1`
   operator override. `--qso-partner-hz` reproduces the default (narrowed) path.
5. **Before the gate, busy windows overran the deadline by seconds.** On the
   fixtures tier, 4 of 13 windows ended past the 2000 ms deadline, the worst
   by 1879 ms (a 3.9 s window). `has_time()` only decided whether
   subtraction or pass 2 *started*: time-varying subtraction then ran over
   every decoded message (about 140 ms per message on the 2-core probe host)
   and the next pass's unconditional prefix ran regardless of what was left.
   After the gate, the worst overrun is 32 ms (one subtracted message), and
   the 1-pass control never overran. In production that overrun delays
   every decode of the window, ft8_lib's included, because they are merged
   after the native call.
6. **The gate costs no recall where pass 2 fits.** Weak recovery is
   175/180 before and after in all three synth-pair variants. Pass 2 ran in
   178/180 windows before and after for `call` and `receipt`; with a QSO it
   ran in 176/180 after against 178/180 before, with the same recovery.
   `window_total_ms.max` rose by more than 100 ms in two pairs: `call`
   1672 → 1782 ms and `receipt` 1573 → 1808 ms (p50 +41 ms and +101 ms).
   When the gate passes, the after build does the same decode work plus one
   `Instant::now()` per pass and per subtracted message, so this is
   run-to-run noise on the shared host, not gate cost; both stay inside the
   deadline with no window over it. The other two pairs fell (`receipt-qso`
   2126 → 2012 ms, fixtures 3879 → 2032 ms).
7. **Incidental observation under CPU contention.** The first
   `variant-receipt-before` run overlapped a nice-19 debug build (see Host).
   It recovered 164/180 weak signals, pass 2 ran in only 133/180 windows,
   and 97/180 windows ended past the deadline (worst by 411 ms, p50 window
   2009 ms). That scorecard was replaced by the clean rerun above; there is
   no matching contended after-gate run, so it is not a before/after
   comparison. It does show how quickly the pre-gate behaviour degrades when
   the host has less CPU than it expects.
8. **The legacy `BudgetTracker` is a third, previously invisible gate.**
   `BudgetTracker::new(2000 + 500 × osd_depth)` is anchored at the start of
   each decode call and checked before every pass. Pre-change observation
   from the PAN-165 plan's Probe 2 (2-core host, 18 synth-pair WAVs at
   stride 10, `DecodeBudget::unlimited()`): `passes=1: mean=2105ms weak=9/18`,
   `passes=2: mean=2261ms weak=9/18`, and 16 of 18 windows showed the legacy
   tracker cancelling pass 2. So the `*-unlimited` scorecards are not
   unlimited on slow hosts. In production the receipt-anchored deadline is
   never later, so the tracker is redundant there. It now reports a
   `multipass-legacy-deadline` stage; retiring it is an open follow-up in
   `docs/DECISIONS/config-and-platform.md`.

## What changed in the code

- `pancetta-ft8`: `DecodeBudget::{deadline, remaining, has_time_for}`;
  `DecodeBudgetReport::passes_run`; `multipass-subtract` and
  `multipass-legacy-deadline` stage entries. The next pass, and each
  end-of-pass subtraction, starts only while
  `has_time_for(pass_floor_cost)` holds, where `pass_floor_cost` is the
  costliest start-to-S1-floor prefix this call has paid. Under
  `DecodeBudget::unlimited()` and for 1-pass configs nothing changes.
- `pancetta-research`: `eval --budget-anchor <call|receipt>`,
  `--qso-partner-hz`, `--effort max`; `window_timing` and `pair_recovery`
  in anchored scorecards (unanchored scorecards keep their keys).
- `pancetta` coordinator: one `window timing:` debug line per window on the
  `decode.budget` target. The TUI chip and health panel are unchanged.

## MiniPC rerun (operator-gated, not run here)

On the Windows MiniPC, from a checkout of this branch:

```bash
git submodule update --init pancetta-ft8/vendor/ft8_lib
cargo build --release -p pancetta-research --bin eval --bin gen-synth-pair
./target/release/gen-synth-pair --config research/corpus/synth/manifests/synth_pair_200.config.json \
  --output /tmp/synth_pair_200.manifest.json
# then the same eval command list as above, writing to a scratch directory
```

Run nothing else while it runs. Compare `window_timing.windows_multipass_ran`,
`max_over_deadline_ms` and `pair_recovery.weak_recovered` with the tables
above. Then, on air on the `Max` preset for at least 30 minutes (120 or more
windows), with and without an active QSO:

```bash
RUST_LOG="pancetta=info,decode.budget=debug,warn" pancetta
grep "window timing:" ~/.pancetta/logs/pancetta.log.*
```

Each window logs `waterfall`, `scoped`, `ft8lib`, `pre_native`,
`headroom_at_native`, `native`, `passes_run`, `total` and `over_deadline`.
