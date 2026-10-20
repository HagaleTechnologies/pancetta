//! Decode-effort preset → wall-time budget mapping (decoder-speed-overhaul
//! Task 14).
//!
//! Task 12 wired a per-window decode budget into the FT8 decode loop's two
//! budgeted call sites (`coordinator/ft8.rs`), but left it permanently `0`
//! (unlimited) — nothing wrote to it. Task 13 added the persisted
//! `[decoder]` config section (`pancetta_config::DecoderConfig`,
//! `effort: DecodeEffort` + `budget_ms: Option<u64>`), but nothing read it.
//! This module is the seam that connects them: [`preset_budget_ms`] maps an
//! effort preset (plus, for `Auto`, the probed [`HardwareTier`]) to a budget
//! in milliseconds, and [`seed_effort_budget`] publishes the result
//! (honoring an explicit `budget_ms` config override, which always wins
//! over the preset) together with the `Ft8Config` overrides derived from
//! it, as one [`DecodeWindowConfig`] change (PAN-164).
//!
//! ## Seeding sites
//!
//! The budget is published from four places, each holding the coordinator's
//! `decode_window` write guard:
//!
//! 1. **Coordinator startup** (`coordinator/mod.rs`) — seeds an initial value
//!    before the hardware tier is known, assuming [`HardwareTier::Fast`] (the
//!    same "innocent until proven otherwise" convention `tier::initialize`
//!    already uses for the `scoped_fast_path` atomic, which also defaults to
//!    the fast-tier assumption until a cache hit or probe completes).
//! 2. **Tier-probe completion** (`tier.rs`) — both the synchronous
//!    cache-hit path and the asynchronous background-probe-completion path
//!    re-seed with the now-known tier.
//! 3. **TUI effort cycle** (`tui_relay.rs`'s `CycleDecodeEffort` arm) —
//!    [`cycle_decode_effort`], the operator's `e` keybinding (spec
//!    2026-07-06, §6.2).
//! 4. **Config hot-reload** — still not wired: pancetta's coordinator has
//!    no *live* config-reload apply path (see `coordinator::health`'s C19
//!    doc comment: `pancetta_config::ConfigHotReload`'s file watcher exists
//!    but is never constructed anywhere in this crate; hot-reload is a
//!    documented no-op by design so a reload can never clobber latched QSO
//!    state). A future apply handler should call [`seed_effort_budget`]
//!    under the same write guard instead of re-deriving the mapping.

use std::sync::atomic::{AtomicU8, Ordering};

use pancetta_config::DecodeEffort;
use pancetta_ft8::tier_probe::HardwareTier;
use pancetta_ft8::Ft8Config;

/// Map a decode-effort preset (and, for `Auto`, the probed hardware tier) to
/// a per-window wall-time budget in milliseconds.
///
/// `0` is the no-preset-limit sentinel (the FT8 hot loop resolves it to
/// the protocol's slot ceiling, `coordinator/ft8.rs`). `Eco` (and `Auto` on `Slow` hardware)
/// deliberately use `1`, NOT `0` — a 1ms budget forces the decode loop's
/// anytime scheduler to stop after the floor stage(s) deterministically,
/// whereas `0` would mean *unlimited*, the opposite intent.
///
/// Values are the spec's starting points (2026-07-06 design doc §6.1) —
/// revisit against post-Phase-1 measurements in the A/B journal before the
/// on-air soak.
pub(crate) fn preset_budget_ms(effort: DecodeEffort, tier: HardwareTier) -> u64 {
    match effort {
        DecodeEffort::Eco => 1,
        DecodeEffort::Standard => 250,
        DecodeEffort::Deep => 1000,
        DecodeEffort::Max => 0,
        DecodeEffort::Auto => match tier {
            HardwareTier::Slow => 1,
            HardwareTier::Moderate => 250,
            HardwareTier::Fast => 1000,
        },
    }
}

/// Compute the effective budget (config override wins over the preset) and
/// publish it, with the effort overrides derived from that same value, into
/// `window` (PAN-164).
///
/// This is the single seeding entry point — called at coordinator startup
/// (with an assumed tier, before the probe resolves) and on tier-probe
/// completion (cache-hit or background-probe path); see the module doc.
/// Callers hold the `decode_window` write guard, so the FT8 hot loop sees
/// the whole change or none of it.
pub(crate) fn seed_effort_budget(
    effort: DecodeEffort,
    budget_override: Option<u64>,
    tier: HardwareTier,
    window: &mut DecodeWindowConfig,
) {
    let budget = budget_override.unwrap_or_else(|| preset_budget_ms(effort, tier));
    window.publish_effort(effort, tier, budget);
}

/// Cycle the operator's live decode-effort preset (Eco → Standard → Deep →
/// Max → Auto → Eco — decoder-speed-overhaul Task 15, TUI `e` keybinding)
/// and immediately publish the resulting budget and effort overrides into
/// `window` as one change (PAN-164).
///
/// Unlike [`seed_effort_budget`], this deliberately ignores any config
/// `budget_ms` override: an explicit operator keypress asking for a
/// different preset should win over a static config value, not have the
/// override silently re-clobber it back. `current_effort` is read/written
/// with the SAME stable `u8` encoding as [`DecodeEffort::as_u8`]/`from_u8`,
/// so it round-trips exactly through the atomic across repeated presses.
/// The caller holds the `decode_window` write guard across the whole call.
///
/// Returns `(new_preset, new_budget_ms)` — the caller (the TUI relay's
/// command handler) uses this to build the `DecodeEffortUpdate` echo sent
/// back to the operator; no active-QSO gate is needed here (a budget change
/// never invalidates in-flight decode state — spec §6.2), so this always
/// succeeds.
pub(crate) fn cycle_decode_effort(
    current_effort: &AtomicU8,
    window: &mut DecodeWindowConfig,
    tier: HardwareTier,
) -> (DecodeEffort, u64) {
    let current = DecodeEffort::from_u8(current_effort.load(Ordering::Acquire));
    let next = current.cycle();
    current_effort.store(next.as_u8(), Ordering::Release);
    let budget = preset_budget_ms(next, tier);
    window.publish_effort(next, tier, budget);
    (next, budget)
}

/// Resolve `Auto` to the literal preset it behaves like on the given
/// [`HardwareTier`], the same mapping [`preset_budget_ms`] uses for the
/// wall-clock budget (Slow↔Eco, Moderate↔Standard, Fast↔Deep). A literal
/// preset (not `Auto`) is returned unchanged. `apply_effort_overrides`
/// uses this so a field override tied to e.g. `Standard` also fires for
/// an `Auto` station on Moderate-tier hardware — round-1 review finding:
/// without this, overrides silently never applied to any `Auto` station.
fn resolve_effective_effort(effort: DecodeEffort, tier: HardwareTier) -> DecodeEffort {
    match effort {
        DecodeEffort::Auto => match tier {
            HardwareTier::Slow => DecodeEffort::Eco,
            HardwareTier::Moderate => DecodeEffort::Standard,
            HardwareTier::Fast => DecodeEffort::Deep,
        },
        literal => literal,
    }
}

/// Apply per-effort-preset `Ft8Config` field overrides (PAN-156/PAN-157).
///
/// Until PAN-156, `Ft8Config` flags could only be tuned globally (or by
/// [`HardwareTier`], via the now-retired `tier::apply_tier` — see
/// `tier.rs`'s module doc); there was no way to say "on for this
/// [`DecodeEffort`] preset, off for that one." This is the seam: it maps
/// an effort preset to a set of `Ft8Config` field overrides, applied
/// in-place onto whatever config the caller already has (so unrelated
/// fields — `protocol`, anything set outside this function — are left
/// untouched). `Auto` is resolved to its tier-equivalent literal preset
/// via [`resolve_effective_effort`] first, so it picks up the same
/// overrides a station manually set to that preset would get.
///
/// Production code reaches it only through [`DecodeWindowConfig`]'s
/// methods: [`seed_effort_budget`] (startup and
/// `tier::initialize`/`tier::spawn_probe_worker`'s tier-resolution
/// paths), [`cycle_decode_effort`] (the TUI's live effort-cycle handler)
/// and [`DecodeWindowConfig::switch_protocol`] (runtime mode switch) — so
/// a live preset switch OR a tier probe landing after startup both
/// re-apply the right fields immediately rather than only at restart,
/// always together with the budget they were derived from.
///
/// ## PAN-157: the first real override — `Max` only
///
/// PAN-157 originally proposed shipping Task W4.3's overlapping-signal
/// multipass technique (`max_decode_passes = 2` +
/// `time_varying_subtraction_enabled = true`) at `Standard`. Re-running
/// the ticket's own pre-registered A/B on the current decoder (full
/// `curated-hard-200` + `synth-clean`, 2026-09-17) found **zero** recall
/// delta at +978% cost — the general signal population's headroom this
/// technique used to recover in July has since been closed by other
/// shipped work (coherent-multipass default-on, LDPC iteration
/// increases, cross-cycle averaging, AP4 full-message-mask). That result
/// stands; see PR #389 and
/// `research/experiments/2026-09-17-w43-pan157-remeasurement-superseded.md`.
///
/// It does NOT generalize to every corpus, though: `synth-pair-200`
/// (WAVs synthesized as deliberately-overlapping signal pairs — exactly
/// the scenario this technique targets) reproduces the original W4.3 win
/// exactly under an unbounded budget (55.6%→97.2% weak-signal recovery),
/// even on today's decoder. The two results coexist because they measure
/// different populations: the general hard-200 distribution no longer
/// benefits, but the narrow overlapping-pair case still does.
///
/// So the override is bound to the literal `Max` preset ONLY:
/// - **Round-11 review finding, corrected:** `preset_budget_ms(Max, _)
///   == 0` is NOT literally unlimited in production — `coordinator/
///   ft8.rs`'s hot loop (`decode_budget_ceiling_ms`) maps the `0`
///   sentinel to the protocol's slot ceiling (2000ms for FT8, this
///   override's only active case). An earlier draft of this comment
///   claimed "unconditionally unlimited" and "no hardware... dependence"
///   — both overstated. What's actually true, empirically re-confirmed
///   under that REAL 2000ms ceiling (not the research harness's
///   `DecodeBudget::unlimited()`, which the original A/B used):
///   `research/scorecards/pan157-pair-{control,variant}-prodmax.json`
///   reproduces the SAME 55.6%→97.2% weak-signal win under the actual
///   production budget, on the same (slow, virtualized) test hardware
///   the round-9 diagnostic showed exhausting a 250ms Standard budget on
///   pass 1 alone. 2000ms is generous enough on this hardware; whether
///   it holds on genuinely slower target hardware (e.g. the Windows
///   MiniPC tier) is unverified and should be checked before an on-air
///   soak, not assumed from this one measurement.
///
///   **Round-13 review finding, second measurement-fidelity gap:** the
///   `prodmax` scorecards above still don't fully reproduce production
///   timing — `research/decoder.rs`'s harness starts its 2000ms deadline
///   fresh immediately before the native decode call, but the real
///   `ft8.rs` hot loop anchors the SAME 2000ms deadline at window
///   RECEIPT and performs waterfall generation, the optional scoped
///   fast-path decode, and the ft8_lib FFI decode BEFORE the main
///   native call that actually consumes `max_decode_passes`. So the
///   native call's true remaining headroom in production is `2000ms
///   minus that earlier work`, not a fresh 2000ms. Closing this
///   precisely needs either a full coordinator-path integration
///   benchmark or a remaining-headroom-aware override decision, neither
///   of which is a small patch — tracked as PAN-165, not fixed here.
///   Bounded, self-correcting risk in the meantime: if the native call
///   gets less time than 2000ms in a given window,
///   `DecodeBudget::has_time()` gating already makes pass 2 gracefully
///   skip for that window — not a wrong decode or a crash.
/// - `Max`'s override ALSO requires `config.protocol == Protocol::Ft8`
///   (round-9 review finding): the coherent-subtraction machinery it
///   enables is FT8-79-symbol-specific, so on FT4 (105 symbols) a
///   second pass just repeats the decode against an unchanged residual
///   — wasted work under FT4's tighter slot budget, not the measured
///   win. `Max` on any other protocol falls through to the same
///   explicit-default arm as `Eco`/`Standard`/`Deep`. This is why
///   `try_switch_operating_mode` (`coordinator/mod.rs`) now re-runs this
///   function right after writing the new protocol — a bare protocol
///   switch while already on `Max` previously never re-evaluated the
///   override at all, leaving an FT8-era override silently active under
///   the new protocol.
/// - Every other literal preset (`Eco`/`Standard`/`Deep`) explicitly
///   resets both fields to `Ft8Config::default()`'s values (`1`,
///   `false`). NOT leaving them untouched: this function mutates
///   `config` in place and is re-invoked on every tier probe/live
///   preset cycle, so an empty arm here would make a PRIOR `Max` call's
///   override sticky — an operator cycling Max→Deep would silently keep
///   running 2 passes under Deep's now-BOUNDED 1000ms budget forever,
///   the exact failure mode `Max`-only scoping exists to avoid (PAN-157
///   review round 9). "Don't push it in Eco/Standard/Deep" (Tony,
///   2026-09-17) is satisfied because those presets never apply the
///   override in the first place; "let someone force it" is satisfied
///   by the existing effort-cycling keybinding itself — cycling to
///   `Max` IS the forcing mechanism, so there's no separate manual
///   Ft8Config-field override path this function needs to preserve.
/// - `Auto` does NOT get this override today: `resolve_effective_effort`
///   only ever maps `Auto` to `Eco`/`Standard`/`Deep`, never `Max`, so
///   `Auto` stays a no-op here for now. A hardware-probed extension
///   (measure whether THIS host has real headroom for a second pass,
///   and apply the override for capable `Auto` stations even at a
///   bounded tier) is planned as a PAN-157 follow-up — deliberately
///   sequenced after this smaller, fully-justified increment lands,
///   rather than bundled in, since it touches the same probe/lock path
///   `tier.rs` just spent 9 review rounds hardening.
///
/// Both fields are always set TOGETHER by every arm, never
/// independently. Each is a decoder-rebuild trigger in
/// `coordinator/ft8.rs`'s `DecoderRebuildKey`
/// (`every_effort_override_field_is_a_rebuild_trigger` guards that for
/// any future field this function writes).
///
/// **Window atomicity (PAN-164).** The budget and the fields this
/// function writes publish together as one [`DecodeWindowConfig`] change
/// under one write guard. `coordinator/ft8.rs`'s hot loop reads them with
/// one `try_read()` per window (`refresh_decode_window`) and keeps the
/// whole previous state on contention or a failed decoder rebuild, so
/// `docs/superpowers/specs/2026-07-06-decoder-speed-overhaul-design.md`
/// §6.2's "effort changes take effect at the next window" holds as one
/// unit — never an old budget paired with a new config or vice versa.
pub(crate) fn apply_effort_overrides(
    effort: DecodeEffort,
    tier: HardwareTier,
    // The resolved effective per-window budget published with this
    // config (`0` = the protocol ceiling, see `preset_budget_ms`), not the
    // persisted `budget_ms` override (PAN-157 round 12). Production
    // callers are `DecodeWindowConfig::publish_effort` (the budget it is
    // publishing) and `DecodeWindowConfig::switch_protocol` (the budget
    // already published); other callers are tests only.
    effective_budget_ms: u64,
    config: &mut Ft8Config,
) {
    match resolve_effective_effort(effort, tier) {
        // Round-9-of-PAN-157 review finding (P1): explicitly restore both
        // fields to their true defaults here, not just "leave them
        // alone." `config` is mutated in place and this function is
        // re-called on every tier probe/cycle, so an empty arm here
        // would make a PRIOR call's `Max` override sticky forever once
        // the operator cycles away — Max's `max_decode_passes = 2` would
        // silently persist under a now-tighter-budget preset (Standard/
        // Deep), defeating the whole point of confining the override to
        // the one preset with the generous (empirically-confirmed
        // sufficient) 2000ms ceiling. This matches PAN-156's own original
        // contract for this function ("EVERY arm below must explicitly
        // assign that field... so switching to a preset that doesn't
        // want the override reverts it").
        DecodeEffort::Eco | DecodeEffort::Standard | DecodeEffort::Deep => {
            config.max_decode_passes = 1;
            config.time_varying_subtraction_enabled = false;
        }
        // Round-9-of-PAN-157 review finding (P2/protocol): the coherent-
        // subtraction machinery this override enables is FT8-79-symbol-
        // specific (`subtract_signal` hardcodes FT8's `NUM_SYMBOLS`).
        // FT4 has 105 symbols, so on FT4 a second pass just re-runs the
        // full decode against an UNCHANGED residual — wasted work under
        // FT4's tighter 800ms slot budget, not the measured win. Only
        // apply the override when the config is actually FT8; every
        // other protocol falls through to the same explicit-default arm
        // Eco/Standard/Deep use, so a station on `Max` in FT4 (or one
        // that switches protocol while already on `Max`, via
        // `try_switch_operating_mode`'s now-added re-application) gets
        // the correct un-overridden defaults, not a stale FT8 override.
        DecodeEffort::Max
            if config.protocol == pancetta_ft8::Protocol::Ft8
                && (effective_budget_ms == 0
                    || effective_budget_ms
                        >= super::ft8::decode_budget_ceiling_ms(
                            config.protocol.slot_ns() as u64
                        )) =>
        {
            config.max_decode_passes = 2;
            config.time_varying_subtraction_enabled = true;
        }
        DecodeEffort::Max => {
            config.max_decode_passes = 1;
            config.time_varying_subtraction_enabled = false;
        }
        DecodeEffort::Auto => unreachable!("resolve_effective_effort never returns Auto"),
    }
}

/// The per-window decode budget and the `Ft8Config` derived with it,
/// published together behind one lock (PAN-164).
///
/// `coordinator/ft8.rs`'s hot loop takes both from a single `try_read()`
/// per window, so it can never pair one publish's budget with another's
/// config. Fields are private: the only ways to change the budget
/// ([`Self::publish_effort`], via [`seed_effort_budget`] and
/// [`cycle_decode_effort`]) re-derive the effort overrides from that same
/// value under the caller's write guard.
#[derive(Debug, Clone)]
pub struct DecodeWindowConfig {
    ft8: Ft8Config,
    /// `0` = no preset limit; the hot loop resolves it to the protocol ceiling.
    budget_ms: u64,
}

impl DecodeWindowConfig {
    /// Wrap `ft8` and `budget_ms` as given, applying no effort overrides.
    /// Production code calls [`seed_effort_budget`] right after.
    pub fn new(ft8: Ft8Config, budget_ms: u64) -> Self {
        Self { ft8, budget_ms }
    }

    pub fn ft8(&self) -> &Ft8Config {
        &self.ft8
    }

    pub fn budget_ms(&self) -> u64 {
        self.budget_ms
    }

    /// Publish `budget_ms` and the overrides [`apply_effort_overrides`]
    /// derives from it, as one change.
    pub(crate) fn publish_effort(
        &mut self,
        effort: DecodeEffort,
        tier: HardwareTier,
        budget_ms: u64,
    ) {
        self.budget_ms = budget_ms;
        apply_effort_overrides(effort, tier, budget_ms, &mut self.ft8);
    }

    /// Runtime mode switch: set the protocol and re-derive the overrides
    /// against the budget already published. Never changes the budget.
    pub(crate) fn switch_protocol(
        &mut self,
        protocol: pancetta_ft8::Protocol,
        effort: DecodeEffort,
        tier: HardwareTier,
    ) {
        self.ft8.protocol = protocol;
        apply_effort_overrides(effort, tier, self.budget_ms, &mut self.ft8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_eco_is_floor_only_one_ms() {
        assert_eq!(preset_budget_ms(DecodeEffort::Eco, HardwareTier::Fast), 1);
        assert_eq!(
            preset_budget_ms(DecodeEffort::Eco, HardwareTier::Moderate),
            1
        );
        assert_eq!(preset_budget_ms(DecodeEffort::Eco, HardwareTier::Slow), 1);
    }

    #[test]
    fn preset_standard_is_250ms() {
        assert_eq!(
            preset_budget_ms(DecodeEffort::Standard, HardwareTier::Fast),
            250
        );
        assert_eq!(
            preset_budget_ms(DecodeEffort::Standard, HardwareTier::Moderate),
            250
        );
        assert_eq!(
            preset_budget_ms(DecodeEffort::Standard, HardwareTier::Slow),
            250
        );
    }

    #[test]
    fn preset_deep_is_1000ms() {
        assert_eq!(
            preset_budget_ms(DecodeEffort::Deep, HardwareTier::Fast),
            1000
        );
        assert_eq!(
            preset_budget_ms(DecodeEffort::Deep, HardwareTier::Moderate),
            1000
        );
        assert_eq!(
            preset_budget_ms(DecodeEffort::Deep, HardwareTier::Slow),
            1000
        );
    }

    #[test]
    fn preset_max_is_unlimited_zero() {
        assert_eq!(preset_budget_ms(DecodeEffort::Max, HardwareTier::Fast), 0);
        assert_eq!(
            preset_budget_ms(DecodeEffort::Max, HardwareTier::Moderate),
            0
        );
        assert_eq!(preset_budget_ms(DecodeEffort::Max, HardwareTier::Slow), 0);
    }

    #[test]
    fn preset_auto_maps_slow_to_one_ms_floor_only() {
        assert_eq!(preset_budget_ms(DecodeEffort::Auto, HardwareTier::Slow), 1);
    }

    #[test]
    fn preset_auto_maps_moderate_to_250ms() {
        assert_eq!(
            preset_budget_ms(DecodeEffort::Auto, HardwareTier::Moderate),
            250
        );
    }

    #[test]
    fn preset_auto_maps_fast_to_1000ms() {
        assert_eq!(
            preset_budget_ms(DecodeEffort::Auto, HardwareTier::Fast),
            1000
        );
    }

    #[test]
    fn seed_effort_budget_uses_preset_when_no_override() {
        let mut window = DecodeWindowConfig::new(Ft8Config::default(), 999);
        seed_effort_budget(
            DecodeEffort::Standard,
            None,
            HardwareTier::Fast,
            &mut window,
        );
        assert_eq!(window.budget_ms(), 250);
    }

    #[test]
    fn seed_effort_budget_override_wins_over_preset() {
        let mut window = DecodeWindowConfig::new(Ft8Config::default(), 0);
        seed_effort_budget(
            DecodeEffort::Eco,
            Some(5_000),
            HardwareTier::Slow,
            &mut window,
        );
        assert_eq!(window.budget_ms(), 5_000);
    }

    #[test]
    fn seed_effort_budget_auto_follows_tier_with_no_override() {
        let mut window = DecodeWindowConfig::new(Ft8Config::default(), 0);
        seed_effort_budget(
            DecodeEffort::Auto,
            None,
            HardwareTier::Moderate,
            &mut window,
        );
        assert_eq!(window.budget_ms(), 250);
    }

    // ------------------------------------------------------------------
    // decoder-speed-overhaul Task 15: TUI live effort cycling
    // ------------------------------------------------------------------

    #[test]
    fn cycle_decode_effort_advances_preset_and_writes_budget() {
        let current_effort = AtomicU8::new(DecodeEffort::Eco.as_u8());
        let mut window = DecodeWindowConfig::new(Ft8Config::default(), 1);
        let (next, budget_ms) =
            cycle_decode_effort(&current_effort, &mut window, HardwareTier::Fast);
        assert_eq!(next, DecodeEffort::Standard, "Eco -> Standard");
        assert_eq!(budget_ms, 250);
        assert_eq!(
            current_effort.load(Ordering::Acquire),
            DecodeEffort::Standard.as_u8(),
            "current-effort atomic must reflect the new preset"
        );
        assert_eq!(
            window.budget_ms(),
            250,
            "published budget must be updated to the new preset's budget"
        );
    }

    #[test]
    fn cycle_decode_effort_wraps_from_max_to_auto_and_resolves_via_tier() {
        let current_effort = AtomicU8::new(DecodeEffort::Max.as_u8());
        let mut window = DecodeWindowConfig::new(Ft8Config::default(), 0);
        let (next, budget_ms) =
            cycle_decode_effort(&current_effort, &mut window, HardwareTier::Slow);
        assert_eq!(next, DecodeEffort::Auto, "Max -> Auto");
        // Auto on a Slow tier resolves to the 1ms floor-only budget.
        assert_eq!(budget_ms, 1);
        assert_eq!(window.budget_ms(), 1);
    }

    #[test]
    fn cycle_decode_effort_wraps_from_auto_to_eco() {
        let current_effort = AtomicU8::new(DecodeEffort::Auto.as_u8());
        let mut window = DecodeWindowConfig::new(Ft8Config::default(), 1000);
        let (next, budget_ms) =
            cycle_decode_effort(&current_effort, &mut window, HardwareTier::Fast);
        assert_eq!(next, DecodeEffort::Eco, "Auto -> Eco");
        assert_eq!(budget_ms, 1);
        assert_eq!(window.budget_ms(), 1);
    }

    /// PAN-164: one cycle publishes the budget AND the overrides derived
    /// from it, so a reader of the window can never see one without the
    /// other.
    #[test]
    fn cycle_decode_effort_publishes_budget_and_overrides_together() {
        let current_effort = AtomicU8::new(DecodeEffort::Deep.as_u8());
        let mut window = DecodeWindowConfig::new(Ft8Config::default(), 1000);
        let (next, budget_ms) =
            cycle_decode_effort(&current_effort, &mut window, HardwareTier::Slow);
        assert_eq!(next, DecodeEffort::Max, "Deep -> Max");
        assert_eq!(budget_ms, 0);
        assert_eq!(window.budget_ms(), 0);
        assert_eq!(window.ft8().max_decode_passes, 2);
        assert!(window.ft8().time_varying_subtraction_enabled);

        let (next, budget_ms) =
            cycle_decode_effort(&current_effort, &mut window, HardwareTier::Slow);
        assert_eq!(next, DecodeEffort::Auto, "Max -> Auto");
        assert_eq!(budget_ms, 1);
        assert_eq!(window.budget_ms(), 1);
        assert_eq!(
            window.ft8().max_decode_passes,
            1,
            "Auto on Slow must clear Max's second pass in the same publish"
        );
        assert!(!window.ft8().time_varying_subtraction_enabled);
    }

    // ------------------------------------------------------------------
    // PAN-164: DecodeWindowConfig publishes budget + config as one unit
    // ------------------------------------------------------------------

    #[test]
    fn publish_effort_sets_budget_and_overrides_in_one_call() {
        let mut w = DecodeWindowConfig::new(Ft8Config::default(), 250);
        w.publish_effort(DecodeEffort::Max, HardwareTier::Fast, 0);
        assert_eq!(w.budget_ms(), 0);
        assert_eq!(w.ft8().max_decode_passes, 2);
        assert!(w.ft8().time_varying_subtraction_enabled);

        w.publish_effort(DecodeEffort::Standard, HardwareTier::Fast, 250);
        assert_eq!(w.budget_ms(), 250);
        assert_eq!(w.ft8().max_decode_passes, 1);
        assert!(!w.ft8().time_varying_subtraction_enabled);
    }

    #[test]
    fn publish_effort_derives_the_override_from_the_budget_it_publishes() {
        // Max with an explicit 250 ms budget can't afford pass 2 (round-11
        // rule), and the published budget is that same 250.
        let mut w = DecodeWindowConfig::new(Ft8Config::default(), 0);
        w.publish_effort(DecodeEffort::Max, HardwareTier::Fast, 250);
        assert_eq!(w.budget_ms(), 250);
        assert_eq!(w.ft8().max_decode_passes, 1);
    }

    #[test]
    fn switch_protocol_reuses_the_published_budget_and_never_changes_it() {
        let mut w = DecodeWindowConfig::new(
            Ft8Config {
                protocol: pancetta_ft8::Protocol::Ft4,
                ..Ft8Config::default()
            },
            250,
        );
        w.switch_protocol(
            pancetta_ft8::Protocol::Ft8,
            DecodeEffort::Max,
            HardwareTier::Fast,
        );
        assert_eq!(w.ft8().protocol, pancetta_ft8::Protocol::Ft8);
        assert_eq!(w.budget_ms(), 250);
        assert_eq!(
            w.ft8().max_decode_passes,
            1,
            "250 ms can't afford the FT8 second pass"
        );

        let mut w = DecodeWindowConfig::new(Ft8Config::default(), 0);
        w.publish_effort(DecodeEffort::Max, HardwareTier::Fast, 0);
        w.switch_protocol(
            pancetta_ft8::Protocol::Ft4,
            DecodeEffort::Max,
            HardwareTier::Fast,
        );
        assert_eq!(w.budget_ms(), 0);
        assert_eq!(
            w.ft8().max_decode_passes,
            1,
            "FT4 must clear the FT8-only override"
        );
    }

    #[test]
    fn new_applies_no_overrides() {
        let w = DecodeWindowConfig::new(Ft8Config::default(), 1000);
        assert_eq!(
            format!("{:?}", w.ft8()),
            format!("{:?}", Ft8Config::default())
        );
        assert_eq!(w.budget_ms(), 1000);
    }

    // ------------------------------------------------------------------
    // PAN-156: effort-preset-conditional Ft8Config overrides
    // ------------------------------------------------------------------

    /// PAN-157 bound the W4.3 overlapping-pair multipass override to the
    /// literal `Max` preset only (see `apply_effort_overrides`'s doc).
    /// Every OTHER preset — including `Auto`, which never literally
    /// resolves to `Max` today — must remain byte-identical to the
    /// default: no override, and no accidental clobber of a value the
    /// operator (or a future config file) set manually.
    #[test]
    fn effort_overrides_remain_a_no_op_for_every_preset_except_max() {
        for effort in [
            DecodeEffort::Eco,
            DecodeEffort::Standard,
            DecodeEffort::Deep,
            DecodeEffort::Auto,
        ] {
            for tier in [
                HardwareTier::Slow,
                HardwareTier::Moderate,
                HardwareTier::Fast,
            ] {
                let mut config = Ft8Config::default();
                apply_effort_overrides(effort, tier, 0, &mut config);
                // `Ft8Config` doesn't derive `PartialEq` (too many fields to
                // justify adding it just for this guard); compare via
                // `Debug` instead, which is already derived and structural.
                assert_eq!(
                    format!("{config:?}"),
                    format!("{:?}", Ft8Config::default()),
                    "{effort:?} on {tier:?} must not change any Ft8Config field"
                );
            }
        }
    }

    /// The literal `Max` preset is the ONE case with a real override:
    /// PAN-157's re-confirmed synth-pair-200 A/B reproduces the original
    /// W4.3 win exactly (55.6%→97.2% weak-signal recovery) for
    /// deliberately-overlapping signal pairs — re-confirmed AGAIN
    /// (round 11) under the REAL production 2000ms FT8 ceiling
    /// `preset_budget_ms(Max, _) == 0` actually maps to
    /// (`pan157-pair-*-prodmax.json`), not just the research harness's
    /// idealized `DecodeBudget::unlimited()`. Unlike Standard/Deep,
    /// where the round-9 PAN-156 diagnostic showed even PASS 1 alone can
    /// exhaust a 250ms budget on slow-enough hardware, 2000ms held up on
    /// the same test hardware — though that's one data point, not a
    /// guarantee across every hardware tier.
    #[test]
    fn max_preset_enables_the_w43_overlapping_pair_multipass_override() {
        for tier in [
            HardwareTier::Slow,
            HardwareTier::Moderate,
            HardwareTier::Fast,
        ] {
            let mut config = Ft8Config::default();
            apply_effort_overrides(DecodeEffort::Max, tier, 0, &mut config);
            assert_eq!(
                config.max_decode_passes, 2,
                "Max on {tier:?} must enable the second decode pass"
            );
            assert!(
                config.time_varying_subtraction_enabled,
                "Max on {tier:?} must enable time-varying subtraction"
            );
        }
    }

    /// Round-9 review finding (protocol): `Max`'s override is FT8-79-
    /// symbol-specific and must NOT apply on any other protocol — a
    /// second pass on FT4 just repeats the decode against an unchanged
    /// residual, wasted work under FT4's tighter slot budget.
    #[test]
    fn max_preset_does_not_override_on_non_ft8_protocol() {
        let mut config = Ft8Config {
            protocol: pancetta_ft8::Protocol::Ft4,
            ..Ft8Config::default()
        };
        apply_effort_overrides(DecodeEffort::Max, HardwareTier::Fast, 0, &mut config);
        assert_eq!(
            config.max_decode_passes, 1,
            "FT4 + Max must NOT enable the FT8-only second pass"
        );
        assert!(
            !config.time_varying_subtraction_enabled,
            "FT4 + Max must NOT enable the FT8-only override"
        );
    }

    /// Round-9 review finding (P1): `Max`'s override must NOT be sticky
    /// after cycling away. Applying `Max` then a lower preset to the
    /// SAME config object (mirroring how the real caller re-invokes this
    /// function on the same `Ft8Config` across live cycles) must leave
    /// both fields at their true defaults, not at whatever `Max` set.
    #[test]
    fn cycling_away_from_max_clears_the_multipass_override() {
        for next in [
            DecodeEffort::Eco,
            DecodeEffort::Standard,
            DecodeEffort::Deep,
        ] {
            let mut config = Ft8Config::default();
            apply_effort_overrides(DecodeEffort::Max, HardwareTier::Fast, 0, &mut config);
            assert_eq!(config.max_decode_passes, 2, "sanity: Max applied first");

            apply_effort_overrides(next, HardwareTier::Fast, 0, &mut config);
            assert_eq!(
                config.max_decode_passes, 1,
                "{next:?} must clear Max's max_decode_passes override, not inherit it"
            );
            assert!(
                !config.time_varying_subtraction_enabled,
                "{next:?} must clear Max's time_varying_subtraction_enabled override"
            );
        }
    }

    /// Overrides apply on top of whatever the caller's config already
    /// has — fields not named by this ticket (e.g. `protocol`, set by
    /// [rig].mode) must survive untouched.
    #[test]
    fn effort_overrides_preserve_fields_it_does_not_own() {
        let mut config = Ft8Config {
            protocol: pancetta_ft8::Protocol::Ft4,
            ..Ft8Config::default()
        };
        apply_effort_overrides(DecodeEffort::Standard, HardwareTier::Fast, 0, &mut config);
        assert_eq!(config.protocol, pancetta_ft8::Protocol::Ft4);
    }

    /// Round-11 review finding: an explicit `[decoder].budget_ms`
    /// override that's smaller than the FT8 ceiling must suppress the
    /// `Max` override entirely — the station's ACTUAL effective budget
    /// can't afford the second pass regardless of what preset name it's
    /// running under.
    #[test]
    fn max_preset_does_not_override_when_explicit_budget_is_too_small() {
        let mut config = Ft8Config::default();
        apply_effort_overrides(DecodeEffort::Max, HardwareTier::Fast, 250, &mut config);
        assert_eq!(
            config.max_decode_passes, 1,
            "an explicit 250ms override can't afford the second pass, \
             regardless of the Max preset name"
        );
        assert!(!config.time_varying_subtraction_enabled);
    }

    /// An explicit override AT OR ABOVE the FT8 ceiling (2000ms) is
    /// exactly as sufficient as the unbounded case — the override still
    /// gets the full ceiling's worth of time either way, so the
    /// override should still apply.
    #[test]
    fn max_preset_overrides_when_explicit_budget_meets_the_ceiling() {
        let mut config = Ft8Config::default();
        apply_effort_overrides(DecodeEffort::Max, HardwareTier::Fast, 2000, &mut config);
        assert_eq!(config.max_decode_passes, 2);
        assert!(config.time_varying_subtraction_enabled);
    }

    /// No explicit override (`None`) means the preset's own `0`/ceiling
    /// semantics apply — the common case, and the one every other test
    /// in this module already exercises via `None`.
    #[test]
    fn max_preset_overrides_when_there_is_no_explicit_budget() {
        let mut config = Ft8Config::default();
        apply_effort_overrides(DecodeEffort::Max, HardwareTier::Fast, 0, &mut config);
        assert_eq!(config.max_decode_passes, 2);
        assert!(config.time_varying_subtraction_enabled);
    }

    /// `Auto` must resolve through the probed hardware tier the same way
    /// the wall-time budget does (round-1 review finding) — verified via
    /// the pure resolver rather than `apply_effort_overrides` itself,
    /// since no field is overridden yet to observe through the latter.
    #[test]
    fn auto_resolves_to_the_tier_equivalent_literal_preset() {
        assert_eq!(
            resolve_effective_effort(DecodeEffort::Auto, HardwareTier::Slow),
            DecodeEffort::Eco
        );
        assert_eq!(
            resolve_effective_effort(DecodeEffort::Auto, HardwareTier::Moderate),
            DecodeEffort::Standard
        );
        assert_eq!(
            resolve_effective_effort(DecodeEffort::Auto, HardwareTier::Fast),
            DecodeEffort::Deep
        );
    }

    #[test]
    fn literal_presets_resolve_to_themselves_regardless_of_tier() {
        for effort in [
            DecodeEffort::Eco,
            DecodeEffort::Standard,
            DecodeEffort::Deep,
            DecodeEffort::Max,
        ] {
            for tier in [
                HardwareTier::Slow,
                HardwareTier::Moderate,
                HardwareTier::Fast,
            ] {
                assert_eq!(resolve_effective_effort(effort, tier), effort);
            }
        }
    }
}
