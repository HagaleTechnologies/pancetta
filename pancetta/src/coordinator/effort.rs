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

/// Apply per-effort-preset `Ft8Config` field overrides (PAN-156).
///
/// Until this ticket, `Ft8Config` flags could only be tuned globally
/// (or by [`HardwareTier`], via the now-retired `tier::apply_tier` — see
/// `tier.rs`'s module doc); there was no way to say "on for this
/// [`DecodeEffort`] preset, off for that one." This is the seam: it maps
/// an effort preset to a set of `Ft8Config` field overrides, applied
/// in-place onto whatever config the caller already has (so unrelated
/// fields — `protocol`, anything set outside this function — are left
/// untouched). `Auto` is resolved to its tier-equivalent literal preset
/// via [`resolve_effective_effort`] first, so it picks up the same
/// overrides a station manually set to that preset would get.
///
/// Called at coordinator startup, from the TUI's live effort-cycle
/// handler (`tui_relay.rs`'s `CycleDecodeEffort` arm), and from
/// `tier::initialize`/`tier::spawn_probe_worker`'s tier-resolution paths
/// (guarded by the same `Auto`-only race guard `seed_effort_budget`
/// uses there) — so a live preset switch OR a tier probe landing after
/// startup both re-apply the right fields immediately rather than only
/// at restart.
///
/// **Pure plumbing — no default decode behavior changes as part of this
/// ticket.** Every arm currently reproduces `Ft8Config::default()`'s
/// values for the fields it's prepared to override, so calling this for
/// any preset/tier combination today is byte-identical to not calling it
/// at all (`effort_overrides_are_currently_a_no_op_for_every_preset`
/// below is the regression guard). A later ticket (e.g. PAN-157) fills
/// in a real override for a specific preset once its own A/B confirms
/// the win — when it does, EVERY arm below must explicitly assign that
/// field (not just the overriding arm), so switching to a preset that
/// doesn't want the override reverts it rather than inheriting whatever
/// the config happened to hold before (round-1 review finding); and the
/// FT8 hot loop's decoder-rebuild-trigger comparison
/// (`coordinator/ft8.rs`, `last_max_passes`/`last_osd_depth`/
/// `last_protocol`) must gain that field too, or a live preset switch
/// can update the shared config without the running decoder ever
/// picking it up (round-1 review finding).
///
/// **Round-7 review finding — window atomicity.** `coordinator/ft8.rs`'s
/// hot loop reads `decode_effort_budget_ms` (a plain atomic) and
/// `ft8_config_shared` (a separate `try_read`) independently per window
/// (`ft8.rs:~1547-1569`). `docs/superpowers/specs/2026-07-06-decoder-
/// speed-overhaul-design.md` §6.2 states the authoritative invariant:
/// "effort changes take effect at the next window" — i.e. atomically, as
/// one unit, never a mix of the old budget with the new config or vice
/// versa. Today this can't actually tear, because nothing this function
/// touches feeds into the hot loop's read of either value. Once a real
/// field IS added here, that field's PR must also either (a) make the
/// hot loop consume budget and config as one atomically-published
/// snapshot (e.g. a shared generation counter it checks once per
/// window), or (b) demonstrate why the specific field added is exempt
/// from the §6.2 invariant. This is deliberately not solved here:
/// redesigning ft8.rs's per-window state acquisition needs a concrete
/// field to test against, and doing it speculatively risks introducing
/// a bug into the hot loop with no way to verify the fix actually closes
/// the gap.
pub(crate) fn apply_effort_overrides(
    effort: DecodeEffort,
    tier: HardwareTier,
    config: &mut Ft8Config,
) {
    match resolve_effective_effort(effort, tier) {
        DecodeEffort::Eco | DecodeEffort::Standard | DecodeEffort::Deep | DecodeEffort::Max => {}
        DecodeEffort::Auto => unreachable!("resolve_effective_effort never returns Auto"),
    }
    let _ = config; // silence unused-mut-arg warning until a real arm lands
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
    // PAN-156: effort-preset-conditional Ft8Config overrides
    // ------------------------------------------------------------------

    /// Pure plumbing must not change decode behavior on its own: applying
    /// any preset's overrides to a fresh default config must leave it
    /// byte-identical to the default, until a future ticket fills in a
    /// real per-preset field value.
    #[test]
    fn effort_overrides_are_currently_a_no_op_for_every_preset_and_tier() {
        for effort in [
            DecodeEffort::Eco,
            DecodeEffort::Standard,
            DecodeEffort::Deep,
            DecodeEffort::Max,
            DecodeEffort::Auto,
        ] {
            for tier in [
                HardwareTier::Slow,
                HardwareTier::Moderate,
                HardwareTier::Fast,
            ] {
                let mut config = Ft8Config::default();
                apply_effort_overrides(effort, tier, &mut config);
                // `Ft8Config` doesn't derive `PartialEq` (too many fields to
                // justify adding it just for this guard); compare via
                // `Debug` instead, which is already derived and structural.
                assert_eq!(
                    format!("{config:?}"),
                    format!("{:?}", Ft8Config::default()),
                    "{effort:?} on {tier:?} must not change any Ft8Config field yet"
                );
            }
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
        apply_effort_overrides(DecodeEffort::Standard, HardwareTier::Fast, &mut config);
        assert_eq!(config.protocol, pancetta_ft8::Protocol::Ft4);
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
