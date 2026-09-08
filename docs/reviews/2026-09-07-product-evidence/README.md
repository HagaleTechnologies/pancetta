# Product assessment evidence — 2026-09-07

Companion to the [product assessment](../2026-09-07-product-assessment.md).
Source baseline: 17ffe360fbc1f1f2afac6615aad76d6df39f4c32.

## Current rendering and parser probe

[capture.rs](capture.rs) uses the real App and Ratatui TestBackend. It does not
start the station coordinator, audio, rig, uploaders, or a transmitter.
It supplies a fixture station identity and disables transmit policy.

Twelve text buffers are retained: four activity views at 80×24, 100×30 and
132×40. Trailing blank cells are stripped from these files; visible cell
content is preserved. The probe also renders two isolated QSO-detail
fixtures. Those extra buffers are not retained: only qso_statuses was
populated, so the top-level active-QSO banner is not a consistent full
application scenario. They establish rendering without panic, not workflow
correctness.

The App constructor can read persisted view/effort preferences; the probe
overrides those values before drawing. Clock values are real capture time,
so repeat runs differ there. This is an observed fixture, not a golden
snapshot test or an on-air claim.

Reproduce from an isolated checkout of the baseline, with the repository's
normal local build prerequisites. Copy capture.rs into a previously unused
example path and run:

    mkdir -p pancetta-tui/examples
    cp docs/reviews/2026-09-07-product-evidence/capture.rs pancetta-tui/examples/product_review_capture.rs
    cargo run -p pancetta-tui --example product_review_capture -- /tmp/pancetta-product-captures-UNIQUE

Use a unique output directory and a local-disk Cargo target. Remove only
the copied example afterward. The helper's dependencies are already declared
by pancetta-tui; no manifest or lockfile edits were required.

The command returned **exit 0**. [probe-output.txt](probe-output.txt) retains
its application output, omitting compiler lines containing local paths.
The helper was formatted after execution; formatting did not change its logic.

| Probe | Result |
|---|---|
| UTC in idle header, 80×24 | Absent in 4/4 views |
| UTC in idle header, 100×30 | Absent in 4/4 views |
| UTC in idle header, 132×40 | Present in 4/4 views |
| Minimal station TOML | Parsed |
| Minimal autonomous TOML | Error: missing slot_parity |
| Minimal UDP TOML | Error: missing destination |
| GUIDE same-host UDP recipe | Error: missing multicast_interface |
| GUIDE cross-machine UDP recipe | Error: missing instance_id |
| Serialized default TOML | 962 lines |
| Shared help table | 41 entries |

The parser probe uses toml::from_str::<pancetta_config::Config>. It does not
invoke the CLI's layered loader, exercise a station configuration, or send
UDP. The GUIDE probes include the documented destination/interface fields;
they are not just an enabled toggle.

## Reused test evidence

Tests were run earlier in this same review session in the isolated security
review worktree at the same source baseline. Production code was unchanged.
Full logs and security reproductions remain with the private security report.

- Required full workspace command, cargo test --workspace --features
  transmit: **failed, exit 101**. The research eval test
  novel_classification_tests::curated_tier_classification_is_report_only_and_correct
  panicked at pancetta-research/src/bin/eval.rs:3090:89 with a malformed
  baseline cache missing freq_hz.
- Shipped-crate command, cargo test --workspace --exclude pancetta-research
  --features transmit: **3,527 passed, 0 failed, 6 ignored**, in 72 result
  groups. This includes eight temporary audit regression tests; it is not
  an untouched-checkout count.
- Deterministic Hamlib command, cargo test -p pancetta-hamlib --lib --
  --test-threads=1: **27 passed, 0 failed**.

These suites were not rerun for the documentation-only product assessment.
Passing subsets do not make the full required workspace suite green.

## Limits

No live radio, production account, upload acceptance test, external operator
study, accessibility certification, or fresh decoder corpus comparison was
performed for this artifact. Existing screenshots and historical decoder
measurements are identified separately in the report.
