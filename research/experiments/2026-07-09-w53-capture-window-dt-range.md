---
slug: w53-capture-window-dt-range
mode: ft8
state: shipped-opt-in
created: 2026-07-09T00:00:00Z
last_updated: 2026-07-09T00:00:00Z
branch: worktree-decoder-tp-sensitivity
parent_hypothesis: decoder-tp-sensitivity plan Task W5.3 (spec Section 6 — the
  FT8 Costas sync sweep's time-step search is structurally non-negative, so
  the capture window handed to the decoder must itself contain real audio
  from before the nominal slot boundary to recover negative-dt signals;
  historical citation "slot-edge bucket at 48.3% recall" / "wild-50 0/96 was
  capture misalignment")
wild_card: false
delta_vs_main: |
  This is a COORDINATOR-level change (pancetta-config/DecoderConfig +
  pancetta/coordinator/{mod,dsp,ft8}.rs), not a pancetta-ft8/Ft8Config
  change, so it cannot be measured via the standard `eval`/`compare`
  hard-200/chrono_replay harness at all (see Investigation section — proven
  by dependency graph: pancetta-research has zero dependency on
  pancetta-config, and its decode_wav() calls pancetta_ft8::Ft8Decoder
  directly, bypassing the coordinator's DSP capture pipeline entirely). A
  purpose-built synthetic slot-edge corpus was built instead
  (pancetta-research/examples/w53_slot_edge_bucket_recall.rs), bucketed
  identically to Batch 36 C2 (research/experiments/2026-06-06-batch-36.md),
  SNR=-16dB, N=12 reps/dt point (N=36/bucket for the 3-point buckets):

    bucket        | default_lead(0.5s) | extended_lead(1.0s) | delta
    <-1.5          |   0.0%             |   0.0%              |   +0.0
    -1.5..-1.0     |   0.0%             |   0.0%              |   +0.0
    -1.0..-0.5     |  16.7% (6/36)      |  55.6% (20/36)       |  +38.9  <- headline
    -0.5..0        |  50.0%             |  50.0%               |   +0.0
    0..0.5         |  75.0%             |  58.3%               |  -16.7  (noise, N=12)
    0.5..1.0       |  50.0%             |  33.3%               |  -16.7  (noise, N=12)
    1.0..1.5       |  58.3%             |  75.0%               |  +16.7  (noise, N=12)
    1.5..2.0       |  66.7%             |  66.7%               |   +0.0
    >=2.0          |  41.7% (10/24)     |  37.5% (9/24)        |   -4.2  (noise, N=24)

  Headline: the [-1.0,-0.5) bucket (the brief's dt=-0.8/-1.0 test targets)
  goes from 16.7% to 55.6% recall — matching, in direction and rough
  magnitude, Batch 36 C2's real-corpus finding for the same bucket (20.3%
  recall in hard-200, "the real cliff"). The <-1.5 and [-1.5,-1.0) buckets
  stay at 0% even with the extended lead — expected and structural: the
  extended 1.0s lead's floor is ~-(1.0+0.16)=-1.16s, so dt=-1.2/-1.6 remain
  out of reach by design (matches Batch 36's own "dt<-1 cliff... genuinely
  structural" finding). All other buckets show only sampling noise (small
  N=12/point, ±16.7 swings both directions, no consistent sign) — expected,
  since widening the LEAD only moves the window's start earlier and cannot
  plausibly affect positive-dt recall (confirmed also by the dedicated
  dt=+2.2s non-regression unit tests, which pass under both lead configs).

  Elapsed decode cost (speed-plan gate, same synthetic run, 144 decodes per
  lead config): default_lead 549.4 ms/window avg -> extended_lead 668.0
  ms/window avg (+118.5ms, +21.6%). Real, non-trivial, but bounded and
  entirely opt-in (flag defaults off).

  hard-200/noise_1000/chrono_replay via the standard eval/compare harness:
  NOT run — proven a no-op by dependency-graph inspection (`cargo tree -p
  pancetta-research -i pancetta-config` errors "did not match any
  packages" -- pancetta-research cannot even see the new config field) and
  by zero-diff in pancetta-ft8/pancetta-research source (this task's diff
  touches only pancetta-config + pancetta/coordinator/*, neither read by
  the eval binaries). Running eval/compare would be a provable tautology,
  not a real measurement; full `cargo test --workspace --features transmit`
  (93 test-result blocks, 0 failed) is the substitute regression gate for
  those crates, confirming zero incidental change.
disposition: SHIP `pancetta_config::DecoderConfig::extended_capture_window_enabled`
  as a new, default-OFF, opt-in coordinator flag (NOT flipped to true by
  default — this is a genuine new capability, not a default-behavior
  change). Rationale: (1) real, demonstrated fix for a real, confirmed
  gap (dt=-0.8s/-1.0s TDD RED->GREEN, matching the historically-diagnosed
  structural cliff at dt<-0.5s); (2) zero behavior change when off (full
  workspace suite green, byte-identical default capture window, confirmed
  by construction — window_lead_secs resolves to the unchanged
  WINDOW_LEAD_SECS constant unless the flag is set); (3) a real, bounded,
  quantified, OPT-IN cost (+21.6% decode wall-time per window on this
  synthetic benchmark) that an operator explicitly accepts for the recall
  gain, mirroring the existing `[decoder].effort` preset philosophy
  (trading decode thoroughness for latency/CPU is already an established,
  operator-facing knob in this codebase); (4) does NOT claim to fix the
  full historical "48.3%"/"1376 truths" framing from decoder.rs's own
  stale comment (`costas_partial_metric_enabled`'s doc, line ~1775) --
  that framing was already corrected by Batch 36 (2026-06-06, a month
  before this session's design spec was written): only ~83 of hard-200's
  1376 dt<0 truths sit below dt=-0.5s where a real cliff exists; the
  bulk (1293/1376, in [-0.5,0)) already recall normally (50.3%, same as
  [0,0.5)'s 50.9%). The design spec's Section 6 citation of "48.3%
  recall, 1376 truths" is the STALE, uncorrected framing, not Batch 36's
  corrected one -- flagged as a doc-drift finding, not fixed as part of
  this task (out of scope: it is a `pancetta-ft8/src/decoder.rs` doc
  comment on an unrelated flag, `costas_partial_metric_enabled`).
  Deliberately did NOT widen the trailing/positive-dt edge (decode_phase):
  empirically, dt=+2.2s (the brief's other named test case) ALREADY
  decodes today with the unwidened default window (real trailing audio +
  LDPC redundancy already cover it, confirmed by direct investigation
  BEFORE writing any code) -- extending decode_phase further would touch
  the DX-slot-aware TX scheduler / half-duplex parity logic documented
  elsewhere in the coordinator (real QSO-turnaround latency + an
  interaction risk with already-carefully-tuned real-time systems), for a
  benefit this task could not demonstrate a need for. Flagged as a
  candidate follow-up, not built now.
follow_ups:
  - decoder.rs's costas_partial_metric_enabled doc comment (line ~1775)
    cites the stale pre-Batch-36 "48.3% recall, 1376 truths" framing --
    should be corrected to cite the Batch 36 C2 corrected numbers
    (dt<-0.5: 83 truths, 20.3%/0%/0% by finer bucket; dt in [-0.5,0):
    1293 truths, 50.3%, no cliff) the next time that flag is touched.
  - extended_capture_window_enabled could be wired into the
    `[decoder].effort` preset system (e.g. auto-enable at Deep/Max) in a
    future task, rather than requiring a second, separate operator toggle.
  - trailing/positive-dt edge (decode_phase) widening was investigated and
    deliberately not built (see disposition) -- worth its own task if a
    future measurement shows a real gap there, with explicit attention to
    DX-slot-aware TX scheduling interactions.
  - the synthetic slot-edge corpus (w53_slot_edge_bucket_recall.rs) used
    N=12/dt (N=36-40/bucket) for time-budget reasons; a future revisit
    could raise N for tighter CIs on the headline bucket if this mechanism
    is ever escalated to default-on.
---

# Task W5.3: Capture window covers real dt range

See the task report (`.superpowers/sdd/task-W5.3-report.md`) for the full
investigation note, TDD evidence, and self-review. This log exists per the
plan's standing "every A/B result gets an experiment log" rule.
