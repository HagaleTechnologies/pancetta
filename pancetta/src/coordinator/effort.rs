//! Decode-effort preset → wall-time budget mapping (decoder-speed-overhaul
//! Task 14).
//!
//! Task 12 wired a `decode_effort_budget_ms: Arc<AtomicU64>` atomic into the
//! FT8 decode loop's two budgeted call sites (`coordinator/ft8.rs`), but left
//! it permanently `0` (unlimited) — nothing wrote to it. Task 13 added the
//! persisted `[decoder]` config section (`pancetta_config::DecoderConfig`,
//! `effort: DecodeEffort` + `budget_ms: Option<u64>`), but nothing read it.
//! This module is the seam that connects them: [`preset_budget_ms`] maps an
//! effort preset (plus, for `Auto`, the probed [`HardwareTier`]) to a budget
//! in milliseconds, and [`seed_effort_budget`] is the single place that
//! writes the result into the shared atomic (honoring an explicit
//! `budget_ms` config override, which always wins over the preset).
//!
//! ## Seeding sites
//!
//! `seed_effort_budget` is called from three places:
//!
//! 1. **Coordinator startup** (`coordinator/mod.rs`) — seeds an initial value
//!    before the hardware tier is known, assuming [`HardwareTier::Fast`] (the
//!    same "innocent until proven otherwise" convention `tier::initialize`
//!    already uses for the `scoped_fast_path` atomic, which also defaults to
//!    the fast-tier assumption until a cache hit or probe completes).
//! 2. **Tier-probe completion** (`tier.rs`) — both the synchronous
//!    cache-hit path and the asynchronous background-probe-completion path
//!    re-seed with the now-known tier.
//! 3. **Config hot-reload** — as of this task, pancetta's coordinator has no
//!    wired *live* config-reload apply path (see `coordinator::health`'s C19
//!    doc comment: `pancetta_config::ConfigHotReload`'s file watcher exists
//!    but is never constructed anywhere in this crate; hot-reload is a
//!    documented no-op by design so a reload can never clobber latched QSO
//!    state). There is therefore no existing "config changed live" call site
//!    to hook today. `seed_effort_budget` is exposed as `pub(crate)`
//!    specifically so that whichever lands first — a general hot-reload
//!    apply handler, or the effort-cycling TUI keybinding described in the
//!    design spec (2026-07-06, §6.2) — can call it directly instead of
//!    re-deriving the mapping.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use pancetta_config::DecodeEffort;
use pancetta_ft8::tier_probe::HardwareTier;
use pancetta_ft8::Ft8Config;

/// Map a decode-effort preset (and, for `Auto`, the probed hardware tier) to
/// a per-window wall-time budget in milliseconds.
///
/// `0` is the unlimited sentinel (matches `decode_effort_budget_ms`'s
/// contract in `coordinator/ft8.rs`). `Eco` (and `Auto` on `Slow` hardware)
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
/// store it into the shared atomic.
///
/// This is the single seeding entry point — called at coordinator startup
/// (with an assumed tier, before the probe resolves), on tier-probe
/// completion (cache-hit or background-probe path), and available for a
/// future live-reload/TUI-cycle call site (see the module doc).
pub(crate) fn seed_effort_budget(
    effort: DecodeEffort,
    budget_override: Option<u64>,
    tier: HardwareTier,
    decode_effort_budget_ms: &AtomicU64,
) {
    let budget = budget_override.unwrap_or_else(|| preset_budget_ms(effort, tier));
    decode_effort_budget_ms.store(budget, Ordering::Release);
}

/// Cycle the operator's live decode-effort preset (Eco → Standard → Deep →
/// Max → Auto → Eco — decoder-speed-overhaul Task 15, TUI `e` keybinding)
/// and immediately write the resulting budget into the shared atomic.
///
/// Unlike [`seed_effort_budget`], this deliberately ignores any config
/// `budget_ms` override: an explicit operator keypress asking for a
/// different preset should win over a static config value, not have the
/// override silently re-clobber it back. `current_effort` is read/written
/// with the SAME stable `u8` encoding as [`DecodeEffort::as_u8`]/`from_u8`,
/// so it round-trips exactly through the atomic across repeated presses.
///
/// Returns `(new_preset, new_budget_ms)` — the caller (the TUI relay's
/// command handler) uses this to build the `DecodeEffortUpdate` echo sent
/// back to the operator; no active-QSO gate is needed here (a budget change
/// never invalidates in-flight decode state — spec §6.2), so this always
/// succeeds.
pub(crate) fn cycle_decode_effort(
    current_effort: &AtomicU8,
    decode_effort_budget_ms: &AtomicU64,
    tier: HardwareTier,
) -> (DecodeEffort, u64) {
    let current = DecodeEffort::from_u8(current_effort.load(Ordering::Acquire));
    let next = current.cycle();
    current_effort.store(next.as_u8(), Ordering::Release);
    let budget = preset_budget_ms(next, tier);
    decode_effort_budget_ms.store(budget, Ordering::Release);
    (next, budget)
}

/// Per-`DecodeEffort`-preset `Ft8Config` field overrides (PAN-156).
///
/// `preset_budget_ms` only ever varied the wall-time budget by preset;
/// decode *technique* flags (`Ft8Config` fields like
/// `coherent_multipass_iterations`) stayed single global values regardless
/// of preset. That meant a technique measured to help at one preset's
/// realistic budget but cost too much at another's (e.g. `Standard`
/// bounded vs `Max` unbounded) could only ship as an all-or-nothing global
/// default — so a real, measured win could get declined purely for lack of
/// per-preset wiring, not because it didn't work. This struct is the seam:
/// [`effort_ft8_overrides`] maps a preset (and, for `Auto`, the probed
/// tier, mirroring [`preset_budget_ms`]'s own resolution) to the resolved
/// values for the fields it controls, and [`apply_effort_ft8_overrides`]
/// writes them into a live `Ft8Config`.
///
/// Every field here is a concrete resolved value, not an `Option` — the
/// mapping always assigns both fields explicitly (falling back to
/// `Ft8Config::default()`'s value when a preset has no override), so
/// cycling AWAY from an overridden preset correctly restores the default
/// rather than leaving a stale override behind. No other live code path
/// mutates these two fields (confirmed: `coordinator::tier::apply_tier`
/// stopped touching `Ft8Config` in decoder-speed-overhaul Task 14; the only
/// other live `Ft8Config` mutation, `try_switch_operating_mode`, only
/// touches `protocol`), so always-resolve-explicitly can't race a
/// competing writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EffortFt8Overrides {
    pub(crate) max_decode_passes: usize,
    pub(crate) time_varying_subtraction_enabled: bool,
}

/// Map a decode-effort preset (and, for `Auto`, the probed hardware tier)
/// to the `Ft8Config` field values it should run with.
///
/// PAN-156 itself is pure plumbing — every arm here resolves to
/// `Ft8Config::default()`'s values, so wiring this in changes no decode
/// behavior. A later change (PAN-157) is expected to give `Standard` (and
/// `Auto` on the tier `preset_budget_ms` treats as Standard-equivalent,
/// i.e. `Moderate`) a real override, once re-confirmed via the
/// `pancetta-research` A/B harness.
pub(crate) fn effort_ft8_overrides(effort: DecodeEffort, tier: HardwareTier) -> EffortFt8Overrides {
    let default = EffortFt8Overrides {
        max_decode_passes: Ft8Config::default().max_decode_passes,
        time_varying_subtraction_enabled: Ft8Config::default().time_varying_subtraction_enabled,
    };
    match effort {
        DecodeEffort::Eco => default,
        DecodeEffort::Standard => default,
        DecodeEffort::Deep => default,
        DecodeEffort::Max => default,
        DecodeEffort::Auto => match tier {
            HardwareTier::Slow => default,
            HardwareTier::Moderate => default,
            HardwareTier::Fast => default,
        },
    }
}

/// Resolve and write the effort-conditional `Ft8Config` overrides into
/// `cfg`. Called wherever the effective preset (or, for `Auto`, the
/// resolved tier) becomes known or changes: coordinator startup, both
/// `tier::initialize` resolution paths (cache-hit and background-probe
/// completion), and the TUI's live effort-cycle keybinding.
pub(crate) fn apply_effort_ft8_overrides(effort: DecodeEffort, tier: HardwareTier, cfg: &mut Ft8Config) {
    let overrides = effort_ft8_overrides(effort, tier);
    cfg.max_decode_passes = overrides.max_decode_passes;
    cfg.time_varying_subtraction_enabled = overrides.time_varying_subtraction_enabled;
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
        let atomic = AtomicU64::new(999);
        seed_effort_budget(DecodeEffort::Standard, None, HardwareTier::Fast, &atomic);
        assert_eq!(atomic.load(Ordering::Acquire), 250);
    }

    #[test]
    fn seed_effort_budget_override_wins_over_preset() {
        let atomic = AtomicU64::new(0);
        seed_effort_budget(DecodeEffort::Eco, Some(5_000), HardwareTier::Slow, &atomic);
        assert_eq!(atomic.load(Ordering::Acquire), 5_000);
    }

    #[test]
    fn seed_effort_budget_auto_follows_tier_with_no_override() {
        let atomic = AtomicU64::new(0);
        seed_effort_budget(DecodeEffort::Auto, None, HardwareTier::Moderate, &atomic);
        assert_eq!(atomic.load(Ordering::Acquire), 250);
    }

    // ------------------------------------------------------------------
    // decoder-speed-overhaul Task 15: TUI live effort cycling
    // ------------------------------------------------------------------

    #[test]
    fn cycle_decode_effort_advances_preset_and_writes_budget() {
        let current_effort = AtomicU8::new(DecodeEffort::Eco.as_u8());
        let budget = AtomicU64::new(1);
        let (next, budget_ms) = cycle_decode_effort(&current_effort, &budget, HardwareTier::Fast);
        assert_eq!(next, DecodeEffort::Standard, "Eco -> Standard");
        assert_eq!(budget_ms, 250);
        assert_eq!(
            current_effort.load(Ordering::Acquire),
            DecodeEffort::Standard.as_u8(),
            "current-effort atomic must reflect the new preset"
        );
        assert_eq!(
            budget.load(Ordering::Acquire),
            250,
            "decode_effort_budget_ms atomic must be updated to the new preset's budget"
        );
    }

    #[test]
    fn cycle_decode_effort_wraps_from_max_to_auto_and_resolves_via_tier() {
        let current_effort = AtomicU8::new(DecodeEffort::Max.as_u8());
        let budget = AtomicU64::new(0);
        let (next, budget_ms) = cycle_decode_effort(&current_effort, &budget, HardwareTier::Slow);
        assert_eq!(next, DecodeEffort::Auto, "Max -> Auto");
        // Auto on a Slow tier resolves to the 1ms floor-only budget.
        assert_eq!(budget_ms, 1);
        assert_eq!(budget.load(Ordering::Acquire), 1);
    }

    #[test]
    fn cycle_decode_effort_wraps_from_auto_to_eco() {
        let current_effort = AtomicU8::new(DecodeEffort::Auto.as_u8());
        let budget = AtomicU64::new(1000);
        let (next, budget_ms) = cycle_decode_effort(&current_effort, &budget, HardwareTier::Fast);
        assert_eq!(next, DecodeEffort::Eco, "Auto -> Eco");
        assert_eq!(budget_ms, 1);
        assert_eq!(budget.load(Ordering::Acquire), 1);
    }

    // ------------------------------------------------------------------
    // PAN-156: effort-conditional Ft8Config overrides
    // ------------------------------------------------------------------

    fn default_overrides() -> EffortFt8Overrides {
        EffortFt8Overrides {
            max_decode_passes: Ft8Config::default().max_decode_passes,
            time_varying_subtraction_enabled: Ft8Config::default()
                .time_varying_subtraction_enabled,
        }
    }

    #[test]
    fn effort_ft8_overrides_is_default_for_every_preset_and_tier_today() {
        // PAN-156 is pure plumbing: nothing has opted in to an override yet,
        // so every (effort, tier) pair must resolve to Ft8Config::default()'s
        // values. PAN-157 is expected to change this for Standard.
        let presets = [
            DecodeEffort::Eco,
            DecodeEffort::Standard,
            DecodeEffort::Deep,
            DecodeEffort::Max,
            DecodeEffort::Auto,
        ];
        let tiers = [
            HardwareTier::Slow,
            HardwareTier::Moderate,
            HardwareTier::Fast,
        ];
        for effort in presets {
            for tier in tiers {
                assert_eq!(
                    effort_ft8_overrides(effort, tier),
                    default_overrides(),
                    "{effort:?} on {tier:?} must be a no-op override in PAN-156"
                );
            }
        }
    }

    #[test]
    fn apply_effort_ft8_overrides_leaves_default_config_unchanged_when_no_override() {
        let mut cfg = Ft8Config::default();
        apply_effort_ft8_overrides(DecodeEffort::Standard, HardwareTier::Fast, &mut cfg);
        assert_eq!(cfg.max_decode_passes, Ft8Config::default().max_decode_passes);
        assert_eq!(
            cfg.time_varying_subtraction_enabled,
            Ft8Config::default().time_varying_subtraction_enabled
        );
    }

    #[test]
    fn apply_effort_ft8_overrides_restores_default_after_a_stale_override() {
        // Simulates cycling AWAY from a (future) overridden preset: a config
        // that was left with non-default values for the two controlled
        // fields must be restored to Ft8Config::default()'s values, not left
        // stale, once resolved against a preset with no override.
        let mut cfg = Ft8Config {
            max_decode_passes: 99,
            time_varying_subtraction_enabled: true,
            ..Ft8Config::default()
        };
        apply_effort_ft8_overrides(DecodeEffort::Eco, HardwareTier::Fast, &mut cfg);
        assert_eq!(cfg.max_decode_passes, Ft8Config::default().max_decode_passes);
        assert_eq!(
            cfg.time_varying_subtraction_enabled,
            Ft8Config::default().time_varying_subtraction_enabled
        );
    }

    #[test]
    fn apply_effort_ft8_overrides_does_not_touch_unrelated_fields() {
        let mut cfg = Ft8Config {
            protocol: pancetta_ft8::Protocol::Ft4,
            ..Ft8Config::default()
        };
        apply_effort_ft8_overrides(DecodeEffort::Standard, HardwareTier::Fast, &mut cfg);
        assert_eq!(cfg.protocol, pancetta_ft8::Protocol::Ft4);
    }
}
