# The `pancetta-ft8` decoder crate contract (PAN-176)

This document is the contract for programs (manta is the named first
consumer) that depend on `pancetta-ft8` to decode FT8 and FT4 without
pulling in Pancetta's station, audio-device, rig-control or network code.
It is the artifact to diff against the FT8/FT4 decoder interface agreed in
dispensa; reconciliation gaps found there are filed as follow-ups.

The two scenarios the ticket states:

1. **A program decodes a cycle using only the decoder crate.** Given a
   program depending only on `pancetta-ft8`, when it passes one 15-second
   12 kHz FT8 cycle of audio, then it receives the same decodes Pancetta's
   station gets for that audio.
2. **The crate builds without audio, rig or network dependencies.** Given a
   clean build of the decoder crate alone, when its dependency tree is
   listed, then no audio-device, rig-control or network crates appear.

## Dependency line

A Cargo dependency declaration (not pancetta config):

<!-- docs-snippets: skip -->
```toml
[dependencies]
pancetta-ft8 = { git = "https://github.com/HagaleTechnologies/pancetta", tag = "pancetta-ft8-v0.9.6" }
```

Default features only. Opt-in features stay off: `transmit` (pulls
`cpal`, `serialport`, `chrono`, `parking_lot`, `tokio`, `serde`, `serde_json`,
`sha2`), `gpio` (`rppal`), `benchmark` (`hound`, `serde`, `serde_json`,
`sha2`), `ft2`, `ffi`, `debug-decode`, `serde`. Default is `std` +
`neural_osd`.

## Inputs

| Property | FT8 | FT4 |
|---|---|---|
| Sample rate | 12 000 Hz mono (`f32`) | 12 000 Hz mono (`f32`) |
| Full cycle | 15.0 s / 180 000 samples | 7.5 s / 90 000 samples |
| Decode window used | first 151 680 samples (12.64 s) | first 151 680 samples (zero-padded past 5.04 s message span) |
| Anchoring | sample 0 = slot boundary | sample 0 = slot boundary |

```rust
use pancetta_ft8::{decode_cycle, Protocol};

let messages = decode_cycle(&samples, Protocol::Ft8)?; // or Protocol::Ft4
```

Input handling (never a panic): input shorter than the decode window is
zero-padded to `WINDOW_SAMPLES` (151 680); longer input is truncated to the
first `WINDOW_SAMPLES` samples (the message spans the first 12.64 s of a
boundary-anchored cycle, and the native decoder over-costs 4–5x on longer
buffers); empty input returns `Ft8Error::InsufficientData`.

`CycleDecoder::new(Protocol)` + `decode_cycle(&mut self, samples)` is the
reuse form for continuous decoding; the free function constructs a fresh
decoder per call.

## Outputs

Each `DecodedMessage` carries (units and reference points):

- `text: String` — the decoded message text (WSJT-X rendering).
- `frequency_offset: f64` — audio frequency in Hz.
- `time_offset: f64` — DT in seconds, **slot-boundary-relative**: 0.0 means
  the signal started at the cycle's first sample.
- `snr_db: f32` — SNR in dB, WSJT-X 2500 Hz reference.
- `confidence: f32`, `ldpc_errors: i32`, `slot_parity`, `acceptance` — as
  elsewhere in the crate.

## Parity definition

`decode_cycle` returns exactly the composition the station coordinator runs
with neutral state:

1. C reference pass: `Ft8Decoder::decode_window_ft8lib_protocol(&window, protocol)`
   (protocol-aware; FT4 cycles pass `Protocol::Ft4`),
   from `pancetta-ft8/src/decoder.rs` (the coordinator call site is
   `pancetta/src/coordinator/ft8.rs`, primary decode path, C pass first).
2. Native AP pass: `decode_window_with_ap_scoped(&window, &ApContext::default(), None)`
   — the plain entry point delegates with `DecodeBudget::unlimited()`. For
   FT4 the decoder is built from a config whose `protocol` is `Protocol::Ft4`,
   mirroring the coordinator's protocol-keyed decoder rebuild.
3. Merge: `merge_ft8lib_first` (`pancetta-ft8/src/merge.rs`, PAN-110) —
   the C decodes first, then any native-only decode (e.g. from AP
   injection), deduplicated by exact message text.

Station-specific context (live `ApContext` from QSO state, hb-229/230
narrow filter, wall-clock decode budget, cross-sequence A7) is QSO- and
load-derived; a consumer with no QSO state gets the station's quiet-band
behaviour.

Tolerances against the live station for the same audio: message texts and
frequency offsets match exactly; SNR within 1.0 dB; DT within 0.15 s (the
live station window is anchored 0.5 s before the slot boundary and its DT
is corrected by that lead; `decode_cycle` is already boundary-relative).
The returned message set can differ from a station running with active QSO
AP context, narrow filters or exhausted budgets — those are station-state
effects, not decoder differences.

Pinned by `tests/cycle_api_tests.rs`: parity-by-construction against the
hand-run composition on committed fixtures, golden decode sets, input
length contract, DT semantics, `CycleDecoder` statelessness, and an FT4
round-trip.

## Dependency guarantee

```console
$ cargo tree -p pancetta-ft8 --edges normal | grep -iE "cpal|alsa|serialport|rppal|hamlib|tokio|mio|reqwest|hyper|network-interface|libudev"
(no output)
```

CI-enforced by `tests/standalone_deps_test.rs`: the default normal
dependency tree must contain no audio-device, rig-control or network
crate; `--features transmit` must pull them (proving the gate can bite);
and a default build must compile the real C decoder (`libft8_lib.a`) when
the submodule is initialized. The only in-repo dependency is `pancetta-core`
(`serde`, `serde_json`, `thiserror`, `chrono` — none of them audio, rig or
network).

## Submodule / C decoder

The reference C decoder is the `vendor/ft8_lib` git submodule (kgoba/ft8_lib,
MIT, pinned at `9fec6ca`). Cargo 1.98 fetches git-dependency submodules
automatically (verified 2026-10-30 by building a scratch crate against a
git snapshot); older cargo versions need `git submodule update --init` in
the source checkout. Without the submodule the crate still builds with
reduced decode recall and a loud build-script warning.

## Versioning and tag policy

- The crate sets its own explicit `version` in `pancetta-ft8/Cargo.toml`
  (starting at 0.9.6, the workspace number it previously inherited), bumped
  independently of the workspace.
- External consumers pin the `pancetta-ft8-v<version>` git tag; the
  release process cuts the tag on the merge commit, not this branch.
- `publish = false` stays (workspace-wide pre-1.0 posture); registry
  publication (which would also need a versioned, published
  `pancetta-core`) is a separate ticket.
- `tests/versioning_contract_test.rs` pins the tag-scheme/version link and
  the README's contract section.
