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
/// ## PAN-157: the multipass override — `Max` only
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
///   pass 1 alone. 2000ms was generous enough on that hardware (the
///   12-core aarch64 runner behind the prodmax scorecards); slower hosts
///   are covered by the production-anchored record below, and the
///   Windows MiniPC tier still needs its own rerun before an on-air soak.
///
///   **Round-13 review finding (PAN-165), resolved:** the prodmax
///   harness started a fresh 2000ms deadline right before the native
///   call, while production anchors it at window receipt and spends part
///   of it on waterfall generation, the optional scoped fast-path decode
///   and the ft8_lib call first. The measurement was re-run with that
///   receipt-anchored sequence
///   (`research/experiments/2026-10-21-pan165-production-anchored-max-timing.md`,
///   `research/scorecards/pan165-*.json`). The decoder's multipass loop
///   now subtracts and starts another pass only while the receipt-anchored
///   budget still has room for a pass's unconditional work, so a late
///   native call skips pass 2 instead of overrunning the slot. On hosts
///   where one pass takes about 2 s, `Max` yields no recall gain over
///   `Deep`.
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
///   resets both multipass fields to `Ft8Config::default()`'s values
///   (`1`, `false`). NOT leaving them untouched: this function mutates
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
/// - `Auto` does NOT get the multipass override today:
///   `resolve_effective_effort` only ever maps `Auto` to
///   `Eco`/`Standard`/`Deep`, never `Max`. (Auto on `Slow` hardware does
///   get PAN-158's fine sync below, because it resolves to `Eco`.) A
///   hardware-probed extension
///   (measure whether THIS host has real headroom for a second pass,
///   and apply the override for capable `Auto` stations even at a
///   bounded tier) is planned as a PAN-157 follow-up — deliberately
///   sequenced after this smaller, fully-justified increment lands,
///   rather than bundled in, since it touches the same probe/lock path
///   `tier.rs` just spent 9 review rounds hardening.
///
/// Both multipass fields are always set TOGETHER by every arm, never
/// independently.
///
/// ## PAN-158: fine sync — FT8 `Eco` at its 1 ms budget only
///
/// `fine_sync_enabled` (Task W3.3's per-candidate fine time/frequency
/// refinement) is on exactly when the resolved preset is `Eco` (literal
/// `Eco`, or `Auto` on `Slow` hardware), the protocol is FT8 and the
/// published budget is 1 ms. Task W3.3b measured a recall win only in
/// that floor-only regime, where no budget-gated rest of the candidate
/// pool exists to starve; under Standard's 250 ms one slow candidate
/// starved the rest. An explicit `budget_ms` override other than 1
/// (including `0`, the protocol ceiling) restores that rest, so Eco with
/// an override stays off. The flag is assigned on every call, so leaving
/// the regime clears it. Evidence:
/// `research/experiments/2026-10-25-pan158-eco-fine-sync.md`.
///
/// Every field this function writes is a decoder-rebuild trigger in
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
    // PAN-158: assigned on every call, so leaving the eligible regime
    // (another preset, FT4, or a budget override) clears it.
    config.fine_sync_enabled = resolve_effective_effort(effort, tier) == DecodeEffort::Eco
        && config.protocol == pancetta_ft8::Protocol::Ft8
        && effective_budget_ms == 1;
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
    /// literal `Max` preset only (see `apply_effort_overrides`'s doc), and
    /// PAN-158 bound fine sync to FT8 Eco at its 1 ms budget. Every other
    /// combination of a non-`Max` preset — including `Auto`, which never
    /// literally resolves to `Max` today — must remain byte-identical to
    /// the default: no override, and no accidental clobber of a value the
    /// operator (or a future config file) set manually. Each preset is
    /// checked at budget `0` (never eligible) and at its real preset
    /// budget, where only Eco and Auto-on-Slow turn fine sync on.
    #[test]
    fn effort_overrides_are_a_no_op_outside_max_and_eligible_eco() {
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
                for budget_ms in [0, preset_budget_ms(effort, tier)] {
                    let mut config = Ft8Config::default();
                    apply_effort_overrides(effort, tier, budget_ms, &mut config);
                    let eligible = budget_ms == 1
                        && resolve_effective_effort(effort, tier) == DecodeEffort::Eco;
                    // `Ft8Config` doesn't derive `PartialEq` (too many fields
                    // to justify adding it just for this guard); compare via
                    // `Debug` instead, which is already derived and structural.
                    assert_eq!(
                        format!("{config:?}"),
                        format!(
                            "{:?}",
                            Ft8Config {
                                fine_sync_enabled: eligible,
                                ..Ft8Config::default()
                            }
                        ),
                        "{effort:?} on {tier:?} at {budget_ms} ms must change no \
                         Ft8Config field beyond the Eco fine-sync override"
                    );
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // PAN-158: fine sync on FT8 Eco at its 1 ms budget only
    // ------------------------------------------------------------------

    const ALL_EFFORTS: [DecodeEffort; 5] = [
        DecodeEffort::Eco,
        DecodeEffort::Standard,
        DecodeEffort::Deep,
        DecodeEffort::Max,
        DecodeEffort::Auto,
    ];
    const ALL_TIERS: [HardwareTier; 3] = [
        HardwareTier::Slow,
        HardwareTier::Moderate,
        HardwareTier::Fast,
    ];

    /// A config with fields the overrides do not own set away from their
    /// defaults, so a clobber shows up in the `Debug` comparison.
    fn custom_config(protocol: pancetta_ft8::Protocol) -> Ft8Config {
        Ft8Config {
            protocol,
            nsym_combining_enabled: true,
            per_candidate_freq_tracker_enabled: true,
            cross_sequence_a7_enabled: true,
            ..Ft8Config::default()
        }
    }

    #[test]
    fn fine_sync_is_on_exactly_for_ft8_eco_at_one_ms() {
        for effort in ALL_EFFORTS {
            for tier in ALL_TIERS {
                for protocol in [pancetta_ft8::Protocol::Ft8, pancetta_ft8::Protocol::Ft4] {
                    for budget_ms in [0u64, 1, 2, 250, 1000, 2000, 5000] {
                        let mut config = custom_config(protocol);
                        apply_effort_overrides(effort, tier, budget_ms, &mut config);

                        let eco = effort == DecodeEffort::Eco
                            || (effort == DecodeEffort::Auto && tier == HardwareTier::Slow);
                        let ft8 = protocol == pancetta_ft8::Protocol::Ft8;
                        let multipass = effort == DecodeEffort::Max
                            && ft8
                            && (budget_ms == 0 || budget_ms >= 2000);
                        let mut expected = custom_config(protocol);
                        expected.fine_sync_enabled = eco && ft8 && budget_ms == 1;
                        if multipass {
                            expected.max_decode_passes = 2;
                            expected.time_varying_subtraction_enabled = true;
                        }
                        assert_eq!(
                            format!("{config:?}"),
                            format!("{expected:?}"),
                            "{effort:?}/{tier:?}/{protocol:?}/{budget_ms} ms"
                        );
                    }
                }
            }
        }
    }

    /// The real caller re-applies overrides to the same config, so every
    /// way out of the eligible regime must clear the flag, and coming back
    /// must set it again. The W3.4/W3.6 companion flags stay as they were.
    #[test]
    fn leaving_eligible_eco_clears_fine_sync_and_returning_sets_it() {
        use pancetta_ft8::Protocol::{Ft4, Ft8};
        let exits: [(DecodeEffort, HardwareTier, pancetta_ft8::Protocol, u64); 9] = [
            (DecodeEffort::Standard, HardwareTier::Fast, Ft8, 250),
            (DecodeEffort::Deep, HardwareTier::Fast, Ft8, 1000),
            (DecodeEffort::Max, HardwareTier::Fast, Ft8, 0),
            (DecodeEffort::Auto, HardwareTier::Moderate, Ft8, 250),
            (DecodeEffort::Auto, HardwareTier::Fast, Ft8, 1000),
            (DecodeEffort::Eco, HardwareTier::Fast, Ft4, 1),
            (DecodeEffort::Eco, HardwareTier::Fast, Ft8, 250),
            (DecodeEffort::Eco, HardwareTier::Fast, Ft8, 0),
            (DecodeEffort::Standard, HardwareTier::Fast, Ft8, 1),
        ];
        let mut config = custom_config(Ft8);
        for (effort, tier, protocol, budget_ms) in exits {
            config.protocol = Ft8;
            apply_effort_overrides(DecodeEffort::Eco, HardwareTier::Fast, 1, &mut config);
            assert!(
                config.fine_sync_enabled,
                "sanity: eligible Eco applied first"
            );

            config.protocol = protocol;
            apply_effort_overrides(effort, tier, budget_ms, &mut config);
            assert!(
                !config.fine_sync_enabled,
                "{effort:?}/{tier:?}/{protocol:?}/{budget_ms} ms must clear fine sync"
            );
            assert!(config.nsym_combining_enabled);
            assert!(config.per_candidate_freq_tracker_enabled);
        }
        config.protocol = Ft8;
        apply_effort_overrides(DecodeEffort::Auto, HardwareTier::Slow, 1, &mut config);
        assert!(config.fine_sync_enabled, "Auto on Slow at 1 ms is eligible");
    }

    #[test]
    fn seed_effort_budget_enables_fine_sync_only_at_eco_preset_budget() {
        for (effort, budget_override, tier, budget, on) in [
            (DecodeEffort::Eco, None, HardwareTier::Fast, 1, true),
            (DecodeEffort::Eco, Some(1), HardwareTier::Fast, 1, true),
            (DecodeEffort::Eco, Some(250), HardwareTier::Fast, 250, false),
            (DecodeEffort::Eco, Some(0), HardwareTier::Fast, 0, false),
            (DecodeEffort::Auto, None, HardwareTier::Slow, 1, true),
            (DecodeEffort::Auto, None, HardwareTier::Moderate, 250, false),
            (DecodeEffort::Auto, None, HardwareTier::Fast, 1000, false),
            (
                DecodeEffort::Standard,
                Some(1),
                HardwareTier::Fast,
                1,
                false,
            ),
        ] {
            let mut window = DecodeWindowConfig::new(Ft8Config::default(), 999);
            seed_effort_budget(effort, budget_override, tier, &mut window);
            assert_eq!(
                (window.budget_ms(), window.ft8().fine_sync_enabled),
                (budget, on),
                "{effort:?} override {budget_override:?} on {tier:?}"
            );
        }
    }

    /// A full live cycle publishes each preset's own budget (replacing a
    /// static override) with the matching fine-sync flag.
    #[test]
    fn cycle_decode_effort_publishes_fine_sync_with_each_presets_budget() {
        for (tier, expected) in [
            (
                HardwareTier::Slow,
                [
                    (DecodeEffort::Eco, 1, true),
                    (DecodeEffort::Standard, 250, false),
                    (DecodeEffort::Deep, 1000, false),
                    (DecodeEffort::Max, 0, false),
                    (DecodeEffort::Auto, 1, true),
                ],
            ),
            (
                HardwareTier::Moderate,
                [
                    (DecodeEffort::Eco, 1, true),
                    (DecodeEffort::Standard, 250, false),
                    (DecodeEffort::Deep, 1000, false),
                    (DecodeEffort::Max, 0, false),
                    (DecodeEffort::Auto, 250, false),
                ],
            ),
        ] {
            let current_effort = AtomicU8::new(DecodeEffort::Auto.as_u8());
            let mut window = DecodeWindowConfig::new(Ft8Config::default(), 0);
            // A static 250 ms override on Eco: outside the measured regime.
            seed_effort_budget(DecodeEffort::Eco, Some(250), tier, &mut window);
            assert!(!window.ft8().fine_sync_enabled);
            for (effort, budget, on) in expected {
                let (next, budget_ms) = cycle_decode_effort(&current_effort, &mut window, tier);
                assert_eq!((next, budget_ms), (effort, budget), "{tier:?}");
                assert_eq!(
                    (window.budget_ms(), window.ft8().fine_sync_enabled),
                    (budget, on),
                    "{effort:?} on {tier:?}"
                );
            }
        }
    }

    #[test]
    fn switch_protocol_rederives_fine_sync_against_the_published_budget() {
        let mut w = DecodeWindowConfig::new(Ft8Config::default(), 0);
        w.publish_effort(DecodeEffort::Eco, HardwareTier::Fast, 1);
        assert!(w.ft8().fine_sync_enabled);

        w.switch_protocol(
            pancetta_ft8::Protocol::Ft4,
            DecodeEffort::Eco,
            HardwareTier::Fast,
        );
        assert_eq!(w.budget_ms(), 1);
        assert!(!w.ft8().fine_sync_enabled, "FT4 must clear fine sync");

        w.switch_protocol(
            pancetta_ft8::Protocol::Ft8,
            DecodeEffort::Eco,
            HardwareTier::Fast,
        );
        assert_eq!(w.budget_ms(), 1);
        assert!(w.ft8().fine_sync_enabled, "back on FT8 Eco at 1 ms");

        let mut w = DecodeWindowConfig::new(Ft8Config::default(), 0);
        seed_effort_budget(DecodeEffort::Eco, Some(250), HardwareTier::Fast, &mut w);
        w.switch_protocol(
            pancetta_ft8::Protocol::Ft8,
            DecodeEffort::Eco,
            HardwareTier::Fast,
        );
        assert_eq!(w.budget_ms(), 250);
        assert!(
            !w.ft8().fine_sync_enabled,
            "a protocol switch keeps the 250 ms override, so fine sync stays off"
        );
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
    /// the pure resolver here; `fine_sync_is_on_exactly_for_ft8_eco_at_one_ms`
    /// observes it through `apply_effort_overrides`.
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
