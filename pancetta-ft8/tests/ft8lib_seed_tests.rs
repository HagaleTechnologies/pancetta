//! PAN-7 — ft8_lib sync-candidate seed extraction and pass-0 union.
//!
//! Integration tests that need the **real** vendored ft8_lib C library.
//! File-gated on `not(ft8lib_stub)` so a stub build skips them outright rather
//! than passing vacuously and hiding a broken translator, and on `transmit`
//! because `Ft8Encoder` / `Ft8Modulator` — used to build the synthetic signals —
//! are re-exported only under that feature.
#![cfg(all(not(ft8lib_stub), feature = "transmit"))]

use pancetta_ft8::ft8_lib_ffi::{ft8lib_find_candidate_seeds, ftx_protocol_t, Ft8LibSeedSet};
use pancetta_ft8::{Ft8Encoder, Ft8Modulator, WINDOW_SAMPLES};

/// Encode + modulate `text` at `freq_offset` Hz into one full FT8 window,
/// zero-padded to `WINDOW_SAMPLES` so the monitor sees a whole slot.
fn synth_window(text: &str, freq_offset: f64) -> Vec<f32> {
    let mut encoder = Ft8Encoder::new();
    let symbols = encoder.encode_message(text, None).expect("encode");
    let mut modulator = Ft8Modulator::new_default().expect("modulator");
    let mut samples = modulator
        .modulate_symbols(&symbols, freq_offset)
        .expect("modulate");
    samples.resize(WINDOW_SAMPLES, 0.0);
    samples
}

/// ft8_lib's own reported frequency for a seed, using the upstream demo's
/// formula (`vendor/ft8_lib/demo/decode_ft8.c`, MIT).
fn top_seed_hz(set: &Ft8LibSeedSet) -> f32 {
    let s = set.seeds[0];
    (set.min_bin as f32 + s.freq_offset as f32 + s.freq_sub as f32 / set.freq_osr as f32)
        / set.symbol_period
}

#[test]
fn find_candidate_seeds_returns_descending_scores_for_a_synthetic_signal() {
    let tx = synth_window("CQ K5ARH EM10", 0.0);
    let set = ft8lib_find_candidate_seeds(&tx, ftx_protocol_t::FTX_PROTOCOL_FT8, 200);

    assert!(
        !set.seeds.is_empty(),
        "ft8_lib found no candidates in a clean synthetic signal — the monitor was not fed correctly"
    );
    assert!(
        set.seeds.windows(2).all(|w| w[0].score >= w[1].score),
        "ftx_find_candidates heap-sorts descending (vendor/ft8_lib/ft8/decode.c:235-250); \
         the seed set must preserve that order"
    );

    // The monitor scalars are what make the returned coordinates interpretable;
    // they must be copied out before monitor_free, and they must match the
    // already-measured ft8_lib configuration (f_min 100 Hz, f_max 3000 Hz).
    assert_eq!(
        set.min_bin, 16,
        "monitor_config f_min=100Hz maps to min_bin 16"
    );
    assert_eq!(set.time_osr, 2);
    assert_eq!(set.freq_osr, 2);
    assert_eq!(set.block_size, 1920, "FT8 block_size at 12 kHz");
    assert!(set.num_bins > 0 && set.num_blocks > 0);
}

#[test]
fn find_candidate_seeds_respects_the_requested_budget() {
    let tx = synth_window("CQ K5ARH EM10", 0.0);

    let five = ft8lib_find_candidate_seeds(&tx, ftx_protocol_t::FTX_PROTOCOL_FT8, 5);
    assert!(
        five.seeds.len() <= 5,
        "requested 5 seeds, got {}",
        five.seeds.len()
    );

    // Zero must short-circuit: never hand ftx_find_candidates a dangling heap.
    let none = ft8lib_find_candidate_seeds(&tx, ftx_protocol_t::FTX_PROTOCOL_FT8, 0);
    assert!(
        none.seeds.is_empty(),
        "a zero budget must return an empty set without calling into C"
    );
}

#[test]
fn top_seed_tracks_the_transmitted_signal_position() {
    let a = ft8lib_find_candidate_seeds(
        &synth_window("CQ K5ARH EM10", 0.0),
        ftx_protocol_t::FTX_PROTOCOL_FT8,
        50,
    );
    let b = ft8lib_find_candidate_seeds(
        &synth_window("CQ K5ARH EM10", 100.0),
        ftx_protocol_t::FTX_PROTOCOL_FT8,
        50,
    );
    assert!(!a.seeds.is_empty() && !b.seeds.is_empty());

    // A +100 Hz transmit shift must move the strongest seed by ~100 Hz. This is
    // self-referential (no hardcoded base frequency) and catches a min_bin or
    // freq_sub mistake in the scalar copy-out.
    let delta = top_seed_hz(&b) - top_seed_hz(&a);
    assert!(
        (delta - 100.0).abs() < 7.0,
        "a +100 Hz shift should move the top seed ~100 Hz, got {delta}"
    );

    // The signal starts at sample 0, so the strongest candidate sits at dt ≈ 0.
    let s = a.seeds[0];
    let t = (s.time_offset as f32 + s.time_sub as f32 / a.time_osr as f32) * a.symbol_period;
    assert!(
        t.abs() < 0.5,
        "expected dt≈0 for a window-aligned signal, got {t}"
    );
}

// ===========================================================================
// Phase 3 — the pass-0 union block
// ===========================================================================

use pancetta_ft8::{Ft8Config, Ft8Decoder, SyncCandidateRecord};

fn read_wav(path: &str) -> Vec<f32> {
    let reader = hound::WavReader::open(path).unwrap();
    reader
        .into_samples::<i16>()
        .map(|s| s.unwrap() as f32 / 32768.0)
        .collect()
}

fn fixture(subpath: &str) -> String {
    format!(
        "{}/tests/fixtures/wav/{}",
        env!("CARGO_MANIFEST_DIR"),
        subpath
    )
}

/// A real off-air recording — several simultaneous signals, which is the only
/// condition under which the saturation story is testable.
fn busy_wav() -> Vec<f32> {
    read_wav(&fixture("wsjt/181201_180245.wav"))
}

fn dump_for(cfg: Ft8Config, samples: &[f32]) -> Vec<SyncCandidateRecord> {
    let mut d = Ft8Decoder::new(cfg).expect("decoder");
    d.decode_window_with_candidate_dump(samples)
        .expect("decode")
        .1
}

#[test]
fn seeded_decode_admits_candidates_below_min_sync_score() {
    let tx = busy_wav();

    // `min_sync_score` raised so the native sweep emits only strong
    // candidates, and the cap left generous so nothing is truncated away. A
    // seed is admitted precisely BECAUSE it fell below the gate — that bypass
    // is the mechanism, so this is the test that proves the block does its job.
    let base = Ft8Config {
        min_sync_score: 6.0,
        max_sync_candidates: 400,
        ..Ft8Config::default()
    };

    let off = dump_for(
        Ft8Config {
            ft8lib_sync_seeds_enabled: false,
            ..base.clone()
        },
        &tx,
    );
    let on = dump_for(
        Ft8Config {
            ft8lib_sync_seeds_enabled: true,
            ..base.clone()
        },
        &tx,
    );

    assert!(
        off.iter().all(|r| !r.via_ft8lib_seed),
        "with the flag off, nothing may be attributed to ft8_lib seeding"
    );

    let seeded: Vec<_> = on
        .iter()
        .filter(|r| r.pass == 0 && r.via_ft8lib_seed)
        .collect();
    assert!(
        !seeded.is_empty(),
        "the seeded run must admit at least one ft8_lib-sourced position the \
         native sweep did not find"
    );
    assert!(
        seeded.iter().any(|r| r.sync_score < 6.0),
        "at least one admitted seed must score BELOW min_sync_score — bypassing \
         that gate is the entire mechanism"
    );
}

#[test]
fn seeded_decode_respects_the_candidate_cap() {
    let tx = busy_wav();
    const CAP: usize = 50;

    let on = dump_for(
        Ft8Config {
            ft8lib_sync_seeds_enabled: true,
            max_sync_candidates: CAP,
            ..Ft8Config::default()
        },
        &tx,
    );

    let pass0 = on.iter().filter(|r| r.pass == 0).count();
    assert!(
        pass0 <= CAP,
        "the post-union pass-0 list must respect max_sync_candidates ({CAP}), got {pass0} — \
         the sibling block forgot its own truncate"
    );
}

/// The automated form of Key Discovery 1 — the measured fact that the whole
/// ship/no-ship decision turns on.
///
/// On busy audio the native Costas sweep is *exhaustive* over its own grid and
/// already produces more cells above `min_sync_score` than the cap admits, so
/// every ft8_lib-sourced position is either a duplicate the dedup erases or a
/// lower-scoring cell the truncate drops. The mechanism is therefore inert **by
/// construction, not by tuning**.
///
/// The measured result is stronger than the plan predicted: on this fixture
/// `kept_seeds == 0` at the default cap of 200 *and* at 400, so doubling the cap
/// does **not** hand seeds a slot — arm C of the Phase 4 measurement is expected
/// to be as inert as arm B. Only raising `min_sync_score` (which shrinks the
/// native list itself) frees slots, which is why
/// `seeded_decode_admits_candidates_below_min_sync_score` has to raise it to 6.0
/// to observe the mechanism working at all.
///
/// If this test ever fires it is NOT necessarily a regression — it means the
/// saturation premise changed, and Phase 4's numbers plus the experiment log's
/// verdict have to be re-measured rather than patched.
#[test]
fn seed_survival_is_capped_out_on_busy_audio_at_the_default_gate() {
    let tx = busy_wav();
    let default_cap = Ft8Config::default().max_sync_candidates;

    let kept = |cap: usize| -> (usize, usize) {
        let d = dump_for(
            Ft8Config {
                ft8lib_sync_seeds_enabled: true,
                max_sync_candidates: cap,
                ..Ft8Config::default()
            },
            &tx,
        );
        let pass0: Vec<_> = d.iter().filter(|r| r.pass == 0).collect();
        let seeds = pass0.iter().filter(|r| r.via_ft8lib_seed).count();
        (seeds, pass0.len())
    };

    let (kept_default, total_default) = kept(default_cap);
    let (kept_raised, total_raised) = kept(default_cap * 2);

    println!(
        "PAN-7 seed survival: cap {default_cap} -> {kept_default}/{total_default} seeded; \
         cap {} -> {kept_raised}/{total_raised} seeded",
        default_cap * 2
    );

    assert!(
        total_default <= default_cap && total_raised <= default_cap * 2,
        "both arms must respect their own cap"
    );
    assert!(
        total_default == default_cap,
        "premise check: the native list must SATURATE the cap on this fixture, \
         else the inertness assertions below prove nothing (got {total_default} \
         of {default_cap})"
    );
    assert_eq!(
        kept_default, 0,
        "Key Discovery 1: at the default gate the native sweep saturates the cap, \
         so no ft8_lib-sourced position may survive on busy audio"
    );
    assert_eq!(
        kept_raised, 0,
        "doubling the cap to {} does not rescue the mechanism either — the native \
         sweep saturates that too. This is the measured fact behind the arm-C \
         expectation in research/experiments/2026-08-03-ft8lib-sync-seed-union*.md",
        default_cap * 2
    );
}

/// Plan Phase 3 §Tests First: the `S0-ft8lib-seed` budget stage carries the
/// seed-survival counters in assertable form — `(label, elapsed_ms, kept_seeds,
/// raw - translated)` — so this pins both that the observability exists at all
/// and what it currently reports. The `debug!(target: "ft8.seed", …)` record
/// carries the same numbers for humans, but asserting on a log line would mean
/// adding a tracing-capture dev-dependency the crate does not have.
///
/// These counters are load-bearing, not diagnostics: without them an inert run
/// and a genuine null result are indistinguishable in the Phase 4 measurement.
#[test]
fn seed_survival_counters_are_emitted() {
    use pancetta_ft8::DecodeBudget;

    let tx = busy_wav();

    let stage = |on: bool, cap: usize| -> Option<(&'static str, u32, u32, u32)> {
        let mut d = Ft8Decoder::new(Ft8Config {
            ft8lib_sync_seeds_enabled: on,
            max_sync_candidates: cap,
            ..Ft8Config::default()
        })
        .expect("decoder");
        let (_msgs, report) = d
            .decode_window_budgeted(&tx, DecodeBudget::unlimited())
            .expect("decode");
        report
            .stages
            .iter()
            .copied()
            .find(|s| s.0 == "S0-ft8lib-seed")
    };

    let default_cap = Ft8Config::default().max_sync_candidates;

    assert!(
        stage(false, default_cap).is_none(),
        "with the flag off the seed block must not run, so it must contribute no \
         budget stage — this is the budget-report half of the flag-off promise"
    );

    let (_label, _elapsed_ms, kept_seeds, dropped) =
        stage(true, default_cap).expect("with the flag on, S0-ft8lib-seed must be reported");

    assert_eq!(
        kept_seeds, 0,
        "Key Discovery 1 again, read through the budget report rather than the \
         candidate dump: on busy audio at the default cap no seed survives"
    );
    assert!(
        dropped > 0,
        "raw - translated must be positive: ft8_lib sweeps time_offset in \
         [-10,20) (vendor/ft8_lib/ft8/decode.c:205) and pancetta's time_step is \
         usize with time_padding 0, so the negative-row third of that range is \
         unrepresentable and MUST be dropped rather than clamped (clamping to 0 \
         would fabricate a different spectrogram position). Zero drops means the \
         envelope/negative-row filter stopped running."
    );
}

#[test]
fn seeded_decode_is_a_noop_on_pass_gt_zero() {
    // Whether a given recording reaches a residual pass depends on what pass 0
    // decoded and subtracted, so sweep a few real fixtures: the invariant is
    // checked on every one, and the liveness assert at the end guarantees at
    // least one of them actually exercised a later pass.
    //
    // Every fixture here decodes on pass 0 and therefore rolls into pass 1 —
    // the loop only breaks early when a pass adds no new message
    // (`decoder.rs:4288-4295`). The two other obvious WSJT fixtures
    // (`wsjt/170709_135615.wav`, `basicft8/170923_082000.wav`) decode nothing
    // at all, so they can never reach a residual pass and would contribute
    // only dead weight to the sweep.
    const FIXTURES: [&str; 3] = [
        "wsjt/181201_180245.wav",
        "wsjt/210703_133430.wav",
        "basicft8/live_now.wav",
    ];

    let mut saw_later_pass = false;
    for f in FIXTURES {
        let tx = read_wav(&fixture(f));
        let on = dump_for(
            Ft8Config {
                ft8lib_sync_seeds_enabled: true,
                // The shipped default is a single pass, so multipass must be
                // requested explicitly or there is no residual pass to check.
                max_decode_passes: 3,
                // Load-independence, NOT a shortcut. `decode_window_*` arms a
                // 2500 ms wall-clock `BudgetTracker` for the WHOLE decode
                // (`decoder.rs:3088-3091`, `osd_depth = Some(1)`) and the pass
                // loop breaks on `budget.expired()` BEFORE pass 1 starts
                // (`:3110-3113`). At the default cap of 200 a busy fixture's
                // pass 0 already costs 1.3-2.1 s single-threaded, so under
                // `cargo test`'s default thread fan-out on a loaded machine the
                // budget expires first, no residual pass runs, and the liveness
                // assert below fails for a reason that has nothing to do with
                // seeding. At cap 40 pass 0 costs ~0.5-0.7 s — a ~4x margin —
                // and all three fixtures reach pass 1 deterministically. The
                // cap does not affect the invariant under test: attribution is
                // keyed on the per-pass `ft8lib_seed_keys` set, not on list
                // length.
                max_sync_candidates: 40,
                ..Ft8Config::default()
            },
            &tx,
        );

        if on.iter().any(|r| r.pass > 0) {
            saw_later_pass = true;
        }
        assert!(
            on.iter().filter(|r| r.pass > 0).all(|r| !r.via_ft8lib_seed),
            "{f}: injection is pass-0 only — residual passes re-search subtracted \
             audio, where ft8_lib's candidates (computed on the unsubtracted \
             slot) would be stale"
        );
    }

    assert!(
        saw_later_pass,
        "no fixture reached a residual pass — the pass-0-only claim went untested. \
         If this fires, suspect the 2500 ms wall-clock BudgetTracker \
         (decoder.rs:3088) expiring during pass 0 before the loop can reach \
         pass 1 (decoder.rs:3110), not a change in seeding behavior"
    );
}
