//! Reasons built from the inputs that actually produced a DX Hunter rank.

use crate::app::{App, DxStation, RankOrigin};
use crate::dx_need::{canonical_grid_square, DxNeedStatus};
use chrono::{DateTime, Utc};
use pancetta_qso::priority::{PortableSuffix, PriorityTier, TIER_BAND_WIDTH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Headline {
    InvalidCallsign,
    AllTimeNew,
    NewEntityOnBand {
        band: String,
    },
    /// `known` is cqdx's loaded needed-entity verdict; None while the list
    /// is not loaded (PAN-93).
    NeededEntity {
        known: Option<bool>,
    },
    Notable,
    SpecialEvent,
    NewGrid {
        grid: Option<String>,
        band: String,
        confirmed: bool,
    },
    NoAwardNeed,
    TierOnly(PriorityTier),
    NotScored,
}

impl Headline {
    pub fn text(&self) -> String {
        match self {
            Self::InvalidCallsign => "callsign looks invalid".into(),
            Self::AllTimeNew => "all-time new entity".into(),
            Self::NewEntityOnBand { band } => format!("new entity on {band}"),
            Self::NeededEntity { known } => match known {
                Some(true) => "needed entity (cqdx list)",
                Some(false) => "maybe needed entity (cqdx list changed)",
                None => "maybe needed entity (cqdx list not loaded)",
            }
            .into(),
            Self::Notable => "notable station (cqdx)".into(),
            Self::SpecialEvent => "special-event callsign".into(),
            Self::NewGrid {
                grid,
                band,
                confirmed,
            } => {
                let grid = grid.as_deref().unwrap_or("unknown");
                if *confirmed {
                    format!("new grid {grid} on {band}")
                } else {
                    format!("grid {grid} may be new on {band} (grid log not loaded)")
                }
            }
            Self::NoAwardNeed => "no award need".into(),
            Self::TierOnly(tier) => match tier {
                PriorityTier::Suspect => "callsign looks invalid",
                PriorityTier::Standard => "no award need",
                PriorityTier::PerBandGridNew => "new grid on this band",
                PriorityTier::SpecialStation => "special or notable station",
                PriorityTier::PerBandDxccNew => "new or needed entity",
                PriorityTier::Atno => "all-time new entity",
            }
            .into(),
            Self::NotScored => "not scored yet (no decode or cqdx data)".into(),
        }
    }

    pub fn tier(&self) -> Option<PriorityTier> {
        Some(match self {
            Self::InvalidCallsign => PriorityTier::Suspect,
            Self::AllTimeNew => PriorityTier::Atno,
            Self::NewEntityOnBand { .. } | Self::NeededEntity { .. } => {
                PriorityTier::PerBandDxccNew
            }
            Self::Notable | Self::SpecialEvent => PriorityTier::SpecialStation,
            Self::NewGrid { .. } => PriorityTier::PerBandGridNew,
            Self::NoAwardNeed => PriorityTier::Standard,
            Self::TierOnly(tier) => *tier,
            Self::NotScored => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caution {
    AlreadyWorked { band: String },
    FailedThisSession,
    NetworkOnly,
    OneReporter,
    StaleSpot { age: String },
}

impl Caution {
    pub fn text(&self) -> String {
        match self {
            Self::AlreadyWorked { band } => format!("already worked on {band}"),
            Self::FailedThisSession => "QSO failed this session".into(),
            Self::NetworkOnly => "network spot only".into(),
            Self::OneReporter => "1 network reporter".into(),
            Self::StaleSpot { age } => format!("network spot {age} old"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DxReason {
    pub headline: Headline,
    pub cautions: Vec<Caution>,
    pub portable: Option<PortableSuffix>,
    pub rarity: Option<&'static str>,
    pub heard: Option<String>,
}

impl DxReason {
    pub fn summary_clauses(&self) -> Vec<String> {
        let mut clauses = self.compact_clauses();
        if let Some(rarity) = self.rarity {
            clauses.push(rarity.into());
        }
        if let Some(heard) = &self.heard {
            clauses.push(heard.clone());
        }
        clauses
    }

    pub fn compact_clauses(&self) -> Vec<String> {
        let mut clauses = vec![self.headline.text()];
        clauses.extend(self.cautions.iter().map(Caution::text));
        if let Some(suffix) = self.portable {
            clauses.push(format!("maybe POTA/SOTA ({} suffix)", suffix.as_str()));
        }
        clauses
    }

    pub fn summary(&self) -> String {
        self.summary_clauses().join("; ")
    }
    pub fn compact(&self) -> String {
        self.compact_clauses().join("; ")
    }
}

fn band_label(station: &DxStation) -> String {
    let band = pancetta_qso::utils::frequency_to_band(station.frequency * 1e6);
    if station.frequency <= 0.0 || !station.frequency.is_finite() || band.ends_with("MHZ") {
        "this band".into()
    } else {
        band
    }
}

// Same units as ui::format_time_ago, with an explicit clock for reproducible reasons.
fn age_text(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86400)
    }
}

pub fn reason_for(
    station: &DxStation,
    need: Option<&DxNeedStatus>,
    now: DateTime<Utc>,
) -> DxReason {
    let Some((origin, rank)) = station.rank_basis() else {
        return DxReason {
            headline: Headline::NotScored,
            cautions: vec![],
            portable: None,
            rarity: None,
            heard: None,
        };
    };
    let band = band_label(station);
    let tier = PriorityTier::from_display_u32(rank.score);
    let headline = match rank.factors {
        None => Headline::TierOnly(tier),
        Some(f) => match tier {
            PriorityTier::Suspect => Headline::InvalidCallsign,
            PriorityTier::Atno => Headline::AllTimeNew,
            PriorityTier::PerBandDxccNew if f.dxcc_new_on_band => {
                Headline::NewEntityOnBand { band: band.clone() }
            }
            PriorityTier::PerBandDxccNew => Headline::NeededEntity {
                known: need.and_then(|n| n.dxcc_needed),
            },
            PriorityTier::SpecialStation if f.notable => Headline::Notable,
            PriorityTier::SpecialStation => Headline::SpecialEvent,
            PriorityTier::PerBandGridNew => Headline::NewGrid {
                grid: station
                    .grid_square
                    .as_deref()
                    .and_then(canonical_grid_square),
                band: band.clone(),
                confirmed: need.and_then(|n| n.grid_needed) == Some(true),
            },
            PriorityTier::Standard => Headline::NoAwardNeed,
        },
    };
    let inputs = [station.local_rank, station.network_rank]
        .into_iter()
        .flatten()
        .filter_map(|r| r.factors)
        .collect::<Vec<_>>();
    let mut cautions = Vec::new();
    if station.worked_before || inputs.iter().any(|f| f.worked_on_band) {
        cautions.push(Caution::AlreadyWorked { band });
    }
    if inputs.iter().any(|f| f.recent_failure) {
        cautions.push(Caution::FailedThisSession);
    }
    if station.local_rank.is_none() {
        cautions.push(Caution::NetworkOnly);
    }
    // Spot-quality cautions follow the source the displayed score came from,
    // so a mixed row ranked by a weak spot still says so.
    if origin == RankOrigin::NetworkSpot {
        if station.reporter_count == Some(1) {
            cautions.push(Caution::OneReporter);
        }
        if let Some(last_seen) = station.last_seen_network {
            let age = now.timestamp().saturating_sub(last_seen);
            if age > 300 {
                cautions.push(Caution::StaleSpot { age: age_text(age) });
            }
        }
    }
    let rarity = rank.factors.and_then(|f| {
        let rarity = f.terms.rarity.input;
        if rarity >= 0.95 {
            Some("legendary rarity (cqdx)")
        } else if rarity >= 0.80 {
            Some("very rare (cqdx)")
        } else if rarity >= 0.65 {
            Some("rare (cqdx)")
        } else {
            None
        }
    });
    let heard = station.local_rank.map(|local| {
        let snr = local.factors.map_or(station.snr, |f| i32::from(f.snr_db));
        let elapsed = now.signed_duration_since(local.at);
        let slot_ms = if station.mode.eq_ignore_ascii_case("FT4") {
            7500
        } else {
            15000
        };
        if elapsed.num_milliseconds() < slot_ms {
            format!("heard this slot at {snr:+} dB")
        } else {
            format!(
                "heard {} ago at {snr:+} dB",
                age_text(elapsed.num_seconds())
            )
        }
    });
    DxReason {
        headline,
        cautions,
        portable: rank.factors.and_then(|f| f.portable_suffix),
        rarity,
        heard,
    }
}

/// Fit whole clauses into terminal cells; a discarded clause is always marked.
pub fn fit_clauses(clauses: &[String], width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let all = clauses.join("; ");
    if ratatui::text::Line::raw(all.as_str()).width() <= width {
        return all;
    }
    let mut kept = String::new();
    for clause in clauses {
        let candidate = if kept.is_empty() {
            clause.clone()
        } else {
            format!("{kept}; {clause}")
        };
        if ratatui::text::Line::raw(format!("{candidate}; …")).width() > width {
            break;
        }
        kept = candidate;
    }
    if kept.is_empty() {
        "…".into()
    } else {
        format!("{kept}; …")
    }
}

pub enum FocusedReason<'a> {
    NoFocus,
    NotRanked(String),
    Ranked(String, &'a DxStation),
}

pub fn focused(app: &App, now: DateTime<Utc>) -> FocusedReason<'_> {
    let Some(call) = app.focused_callsign() else {
        return FocusedReason::NoFocus;
    };
    match app.dx_station_for(&call) {
        Some(station)
            if now.signed_duration_since(station.last_seen).num_seconds()
                <= App::DX_HUNTER_STALE_SECS =>
        {
            FocusedReason::Ranked(call.to_owned(), station)
        }
        _ => FocusedReason::NotRanked(call.to_owned()),
    }
}

/// PAN-93: the "needed entity" row's value and source. The value is what the
/// score used; the source names the cqdx list's scope and age, says when the
/// list has changed since the score, or says the score used the
/// home/logged-prefix guess because the list is not loaded.
fn needed_entity_row(
    scored_needed: bool,
    need: Option<&DxNeedStatus>,
    now: DateTime<Utc>,
) -> (String, String) {
    let yes_no = |v: bool| if v { "yes" } else { "no" };
    match need.and_then(|n| n.dxcc_needed) {
        None => (
            "unknown".into(),
            if scored_needed {
                "cqdx list not loaded; scored as needed (not a home or logged prefix)"
            } else {
                "cqdx list not loaded; scored as not needed (home or logged prefix)"
            }
            .into(),
        ),
        Some(listed) if listed != scored_needed => (
            yes_no(scored_needed).into(),
            format!(
                "scored before the cqdx list changed (now {})",
                yes_no(listed)
            ),
        ),
        Some(listed) => {
            let source = need.and_then(|n| n.dxcc_list.as_ref()).map_or_else(
                || "cqdx needed list".to_string(),
                |list| {
                    format!(
                        "cqdx needed list, {}, {} old",
                        list.band.as_deref().unwrap_or("all bands"),
                        age_text(now.signed_duration_since(list.fetched_at).num_seconds())
                    )
                },
            );
            (yes_no(listed).into(), source)
        }
    }
}

/// Logical lines; the overlay wraps and scrolls them at the available width.
pub struct WhyDetail {
    pub lines: Vec<String>,
}

pub fn detail_for(
    station: &DxStation,
    need: Option<&DxNeedStatus>,
    now: DateTime<Utc>,
) -> WhyDetail {
    let Some((origin, rank)) = station.rank_basis() else {
        return WhyDetail { lines: vec![format!("{} has not been scored yet: it came from the DX cluster with no decode or cqdx data.", station.call_sign)] };
    };
    let reason = reason_for(station, need, now);
    let tier = reason.headline.tier().unwrap_or(PriorityTier::Suspect);
    let tier_name = match tier {
        PriorityTier::Suspect => "invalid-callsign",
        PriorityTier::Standard => "no-award-need",
        PriorityTier::PerBandGridNew => "new-grid",
        PriorityTier::SpecialStation => "special-station",
        PriorityTier::PerBandDxccNew => "new-or-needed-entity",
        PriorityTier::Atno => "all-time-new",
    };
    let floor = tier as u32 * TIER_BAND_WIDTH;
    let within = station.priority_score % TIER_BAND_WIDTH;
    let mut lines = vec![
        format!(
            "Pri {}: {tier_name} tier ({floor}–{}), {within} of {} within the tier",
            station.priority_score,
            floor + TIER_BAND_WIDTH - 1,
            TIER_BAND_WIDTH - 1
        ),
        format!("Why: {}", reason.summary()),
        String::new(),
    ];
    if let Some(suffix) = reason.portable {
        lines.push(format!("Portable suffix: maybe POTA/SOTA ({}), not a confirmed park or summit activation. This is a guess from the callsign suffix. Activators who send a bare callsign are not detected.", suffix.as_str()));
        lines.push(String::new());
    }
    if let Some(f) = rank.factors {
        let band = band_label(station);
        let yes_no = |v: bool| {
            if v {
                "yes".to_string()
            } else {
                "no".to_string()
            }
        };
        let portable = f.portable_suffix.map_or_else(
            || "none".into(),
            |suffix| match suffix {
                PortableSuffix::Pota | PortableSuffix::Sota => format!("maybe {}", suffix.as_str()),
                _ => suffix.as_str().into(),
            },
        );
        let (needed_value, needed_source) = needed_entity_row(f.needed_dxcc, need, now);
        let inputs = [
            ("needed entity".into(), needed_value, needed_source.as_str()),
            (
                format!("entity new on {band}"),
                yes_no(f.dxcc_new_on_band),
                "your log",
            ),
            ("all-time new entity".into(), yes_no(f.atno), "cqdx"),
            ("notable station".into(), yes_no(f.notable), "cqdx"),
            (
                "special-event callsign".into(),
                yes_no(f.special_event_callsign),
                "callsign pattern",
            ),
            (
                format!("grid new on {band}"),
                yes_no(f.grid_new_on_band),
                "your log",
            ),
            (
                "portable suffix".into(),
                portable,
                "callsign suffix (guess)",
            ),
            (
                "rarity".into(),
                format!("{:.2}", f.terms.rarity.input),
                "cqdx (0.50 when unknown)",
            ),
            (
                "signal".into(),
                format!("{:+} dB", f.snr_db),
                if origin == RankOrigin::LocalDecode {
                    "this decode"
                } else {
                    "cqdx spot"
                },
            ),
            (
                format!("already worked on {band}"),
                yes_no(f.worked_on_band),
                "your log",
            ),
            (
                "QSO failed this session".into(),
                yes_no(f.recent_failure),
                "this session",
            ),
            (
                "network reports".into(),
                f.network_reports.map_or_else(
                    || "none".into(),
                    |(count, snr)| format!("{count}, best {snr:+} dB"),
                ),
                "cqdx",
            ),
        ];
        lines.push("What the score used                 value      source".into());
        lines.extend(
            inputs
                .into_iter()
                .map(|(label, value, source)| format!("  {label:<32}  {value:<10} {source}")),
        );
        lines.push(String::new());
        lines.push("Within-tier score".into());
        let t = f.terms;
        for (name, term) in [
            ("rarity", t.rarity),
            ("portable suffix", t.portable),
            ("signal", t.signal),
            ("already worked", t.worked),
            ("QSO failed", t.recent_failure),
        ] {
            lines.push(format!(
                "  {name:<20} {:.2} × {:.2} = {:+.3}",
                term.input,
                term.weight,
                term.contribution()
            ));
        }
        lines.push(format!(
            "  network reports              bonus = {:+.3}",
            t.network_bonus
        ));
        lines.push(format!(
            "  ({:+.3} ÷ {:.2} ceiling) × {:.2} freshness = {:.3} → {within} of {}",
            t.raw(),
            t.ceiling,
            t.freshness,
            t.secondary(),
            TIER_BAND_WIDTH - 1
        ));
    } else {
        lines.push("Scoring inputs unavailable (this producer sent a score only).".into());
    }
    lines.push(match origin {
        RankOrigin::LocalDecode => "Scored by the station from your log and this session's history (local decode).",
        RankOrigin::NetworkSpot => "Scored by the TUI from cqdx spot data. Per-band and failure history come only from an earlier local decode.",
    }.into());
    WhyDetail { lines }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{RankedBy, SpotSource};
    use crate::dx_need::NeedListSource;
    use chrono::Duration;
    use pancetta_qso::priority::{
        NullLookup, PriorityScorer, PriorityWeights, WorkedStationLookup,
    };

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }
    fn station(call: &str) -> DxStation {
        let explained = PriorityScorer::new(PriorityWeights::default(), Box::new(NullLookup))
            .score_tiered_explained(call, Some("EM34"), -7, 14_074_000.0);
        DxStation {
            call_sign: call.into(),
            grid_square: Some("EM34".into()),
            frequency: 14.074,
            mode: "FT8".into(),
            last_seen: now(),
            snr: -7,
            distance: None,
            bearing: None,
            worked_before: false,
            needed: false,
            atno: false,
            band_needed: false,
            priority_score: explained.score.as_display_u32(),
            local_rank: Some(RankedBy {
                score: explained.score.as_display_u32(),
                factors: Some(explained.factors),
                at: now(),
            }),
            network_rank: None,
            source: SpotSource::Local,
            entity_name: None,
            rarity_tier: None,
            reporter_count: None,
            is_notable: false,
            notable_type: None,
            confidence: None,
            best_snr_network: None,
            last_seen_network: None,
            audio_offset_hz: None,
            slot_parity: None,
            watchlisted: false,
        }
    }
    fn set_tier(s: &mut DxStation, tier: PriorityTier) {
        s.priority_score = tier as u32 * TIER_BAND_WIDTH + 123;
        s.local_rank.as_mut().unwrap().score = s.priority_score;
    }
    fn factors(s: &mut DxStation) -> &mut pancetta_qso::priority::PriorityFactors {
        s.local_rank.as_mut().unwrap().factors.as_mut().unwrap()
    }

    #[test]
    fn headline_per_tier() {
        let cases = [
            (PriorityTier::Suspect, "callsign looks invalid"),
            (PriorityTier::Atno, "all-time new entity"),
            (
                PriorityTier::PerBandDxccNew,
                "maybe needed entity (cqdx list not loaded)",
            ),
            (PriorityTier::SpecialStation, "special-event callsign"),
            (
                PriorityTier::PerBandGridNew,
                "grid EM34 may be new on 20M (grid log not loaded)",
            ),
            (PriorityTier::Standard, "no award need"),
        ];
        for (tier, expected) in cases {
            let mut s = station("K1ABC");
            set_tier(&mut s, tier);
            assert_eq!(reason_for(&s, None, now()).headline.text(), expected);
        }
        let mut s = station("K1ABC");
        set_tier(&mut s, PriorityTier::PerBandDxccNew);
        factors(&mut s).dxcc_new_on_band = true;
        assert_eq!(
            reason_for(&s, None, now()).headline.text(),
            "new entity on 20M"
        );
        s.frequency = 0.0;
        assert_eq!(
            reason_for(&s, None, now()).headline.text(),
            "new entity on this band"
        );
        s.frequency = 123.0;
        assert_eq!(
            reason_for(&s, None, now()).headline.text(),
            "new entity on this band"
        );
        set_tier(&mut s, PriorityTier::SpecialStation);
        factors(&mut s).notable = true;
        assert_eq!(
            reason_for(&s, None, now()).headline.text(),
            "notable station (cqdx)"
        );
    }

    struct Lookup {
        bits: u8,
        rarity: f64,
        reports: Option<(u32, i32)>,
    }
    impl WorkedStationLookup for Lookup {
        fn is_needed_dxcc(&self, _: &str) -> bool {
            self.bits & 1 != 0
        }
        fn is_dxcc_needed_on_band(&self, _: &str, _: f64) -> bool {
            self.bits & 2 != 0
        }
        fn is_atno(&self, _: &str) -> bool {
            self.bits & 4 != 0
        }
        fn is_notable(&self, _: &str) -> bool {
            self.bits & 8 != 0
        }
        fn is_grid_needed_on_band(&self, _: &str, _: f64) -> bool {
            self.bits & 16 != 0
        }
        fn is_duplicate(&self, _: &str, _: f64) -> bool {
            self.bits & 32 != 0
        }
        fn is_recent_failure(&self, _: &str) -> bool {
            self.bits & 64 != 0
        }
        fn is_needed_grid(&self, _: &str) -> bool {
            false
        }
        fn rarity(&self, _: &str) -> f64 {
            self.rarity
        }
        fn network_snr(&self, _: &str) -> Option<(u32, i32)> {
            self.reports
        }
    }
    #[test]
    fn headline_tier_always_matches_the_score_tier() {
        for bits in 0..128 {
            for rarity in [0.5, 0.85] {
                for reports in [None, Some((5, -10)), Some((1, -27))] {
                    let scorer = PriorityScorer::new(
                        PriorityWeights::default(),
                        Box::new(Lookup {
                            bits,
                            rarity,
                            reports,
                        }),
                    );
                    for call in ["K1ABC", "K1ABC/P", "W1A", "<...>"] {
                        for snr in [-24, -7, 10] {
                            for grid in [None, Some("FN42")] {
                                let e =
                                    scorer.score_tiered_explained(call, grid, snr, 14_074_000.0);
                                let mut s = station(call);
                                s.grid_square = grid.map(str::to_owned);
                                s.priority_score = e.score.as_display_u32();
                                s.local_rank = Some(RankedBy {
                                    score: s.priority_score,
                                    factors: Some(e.factors),
                                    at: now(),
                                });
                                assert_eq!(
                                    reason_for(&s, None, now()).headline.tier(),
                                    Some(e.score.tier)
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn grid_headline_is_hedged_unless_pan86_confirms() {
        let mut s = station("K1ABC");
        set_tier(&mut s, PriorityTier::PerBandGridNew);
        for grid_needed in [None, Some(false), Some(true)] {
            let need = DxNeedStatus {
                grid_needed,
                ..Default::default()
            };
            let expected = if grid_needed == Some(true) {
                "new grid EM34 on 20M"
            } else {
                "grid EM34 may be new on 20M (grid log not loaded)"
            };
            assert_eq!(reason_for(&s, Some(&need), now()).headline.text(), expected);
        }
        assert!(reason_for(&s, None, now())
            .headline
            .text()
            .contains("may be new"));
    }
    #[test]
    fn needed_entity_headline_is_hedged_unless_cqdx_confirms() {
        let mut s = station("K1ABC");
        set_tier(&mut s, PriorityTier::PerBandDxccNew);
        factors(&mut s).needed_dxcc = true;
        for (dxcc_needed, expected) in [
            (None, "maybe needed entity (cqdx list not loaded)"),
            (Some(false), "maybe needed entity (cqdx list changed)"),
            (Some(true), "needed entity (cqdx list)"),
        ] {
            let need = DxNeedStatus {
                dxcc_needed,
                ..Default::default()
            };
            assert_eq!(reason_for(&s, Some(&need), now()).headline.text(), expected);
        }
        assert_eq!(
            reason_for(&s, None, now()).headline.text(),
            "maybe needed entity (cqdx list not loaded)"
        );
    }
    #[test]
    fn needed_entity_detail_row_names_value_and_provenance() {
        let list = |band: Option<&str>, age: Duration| NeedListSource {
            band: band.map(str::to_owned),
            fetched_at: now() - age,
        };
        let row = |scored: bool, need: Option<&DxNeedStatus>| {
            let mut s = station("K1ABC");
            set_tier(&mut s, PriorityTier::PerBandDxccNew);
            factors(&mut s).needed_dxcc = scored;
            detail_for(&s, need, now())
                .lines
                .into_iter()
                .find(|line| line.starts_with("  needed entity "))
                .unwrap()
        };
        let expect =
            |value: &str, source: &str| format!("  {:<32}  {value:<10} {source}", "needed entity");
        // (dxcc_needed, dxcc_list, f.needed_dxcc) → (value, source)
        for (dxcc_needed, dxcc_list, scored, value, source) in [
            (
                Some(true),
                Some(list(Some("20M"), Duration::minutes(4))),
                true,
                "yes",
                "cqdx needed list, 20M, 4m old",
            ),
            (
                Some(false),
                Some(list(None, Duration::hours(2))),
                false,
                "no",
                "cqdx needed list, all bands, 2h old",
            ),
            (
                Some(false),
                Some(list(Some("20M"), Duration::minutes(4))),
                true,
                "yes",
                "scored before the cqdx list changed (now no)",
            ),
            (
                Some(true),
                Some(list(Some("20M"), Duration::minutes(4))),
                false,
                "no",
                "scored before the cqdx list changed (now yes)",
            ),
            (Some(true), None, true, "yes", "cqdx needed list"),
            (
                None,
                None,
                true,
                "unknown",
                "cqdx list not loaded; scored as needed (not a home or logged prefix)",
            ),
            (
                None,
                None,
                false,
                "unknown",
                "cqdx list not loaded; scored as not needed (home or logged prefix)",
            ),
        ] {
            let need = DxNeedStatus {
                dxcc_needed,
                dxcc_list,
                ..Default::default()
            };
            assert_eq!(row(scored, Some(&need)), expect(value, source));
        }
        // No need provider at all reads the same as a list that is not loaded.
        for (scored, source) in [
            (
                true,
                "cqdx list not loaded; scored as needed (not a home or logged prefix)",
            ),
            (
                false,
                "cqdx list not loaded; scored as not needed (home or logged prefix)",
            ),
        ] {
            assert_eq!(row(scored, None), expect("unknown", source));
        }
    }
    #[test]
    fn cautions_in_order_and_merged_across_scorings() {
        let mut s = station("K1ABC/P");
        s.network_rank = s.local_rank;
        s.network_rank.as_mut().unwrap().score += 1000;
        factors(&mut s).recent_failure = true;
        s.worked_before = true;
        assert_eq!(
            reason_for(&s, None, now())
                .cautions
                .iter()
                .map(Caution::text)
                .collect::<Vec<_>>(),
            ["already worked on 20M", "QSO failed this session"]
        );
    }
    #[test]
    fn network_only_cautions() {
        let mut s = station("K1ABC");
        s.network_rank = s.local_rank.take();
        s.reporter_count = Some(1);
        s.last_seen_network = Some((now() - Duration::seconds(400)).timestamp());
        assert_eq!(
            reason_for(&s, None, now())
                .cautions
                .iter()
                .map(Caution::text)
                .collect::<Vec<_>>(),
            [
                "network spot only",
                "1 network reporter",
                "network spot 6m old"
            ]
        );
        s.last_seen_network = Some((now() - Duration::seconds(299)).timestamp());
        assert_eq!(reason_for(&s, None, now()).cautions.len(), 2);
    }

    #[test]
    fn mixed_row_warns_only_when_the_spot_wins() {
        let mut s = station("K1ABC");
        let mut spot = s.local_rank.unwrap();
        spot.score += 1;
        s.network_rank = Some(spot);
        s.reporter_count = Some(1);
        s.last_seen_network = Some((now() - Duration::seconds(400)).timestamp());
        let texts = |s: &DxStation| {
            reason_for(s, None, now())
                .cautions
                .iter()
                .map(Caution::text)
                .collect::<Vec<_>>()
        };
        assert_eq!(texts(&s), ["1 network reporter", "network spot 6m old"]);
        s.network_rank.as_mut().unwrap().score -= 2;
        assert!(texts(&s).is_empty());
    }
    #[test]
    fn heard_this_slot_boundaries() {
        let mut s = station("K1ABC");
        for (mode, age, expected) in [
            ("FT8", 14, "heard this slot at -7 dB"),
            ("FT8", 16, "heard 16s ago at -7 dB"),
            ("FT4", 7, "heard this slot at -7 dB"),
            ("FT4", 8, "heard 8s ago at -7 dB"),
        ] {
            s.mode = mode.into();
            s.local_rank.as_mut().unwrap().at = now() - Duration::seconds(age);
            assert_eq!(reason_for(&s, None, now()).heard.as_deref(), Some(expected));
        }
        s.local_rank = None;
        assert!(reason_for(&s, None, now()).heard.is_none());
    }
    #[test]
    fn rarity_labels() {
        let mut s = station("K1ABC");
        for (rarity, expected) in [
            (0.98, Some("legendary rarity (cqdx)")),
            (0.85, Some("very rare (cqdx)")),
            (0.65, Some("rare (cqdx)")),
            (0.64, None),
            (0.5, None),
        ] {
            factors(&mut s).terms.rarity.input = rarity;
            assert_eq!(reason_for(&s, None, now()).rarity, expected);
        }
    }
    #[test]
    fn summary_and_compact_order() {
        let mut s = station("K1ABC/P");
        factors(&mut s).recent_failure = true;
        factors(&mut s).terms.rarity.input = 0.85;
        let reason = reason_for(&s, None, now());
        assert_eq!(
            reason.compact(),
            "no award need; QSO failed this session; maybe POTA/SOTA (/P suffix)"
        );
        assert_eq!(reason.summary(), "no award need; QSO failed this session; maybe POTA/SOTA (/P suffix); very rare (cqdx); heard this slot at -7 dB");
    }
    #[test]
    fn portable_hedge_is_truncation_safe() {
        for suffix in ["/P", "/POTA", "/S", "/SOTA", "/PORT"] {
            let s = station(&format!("K1ABC{suffix}"));
            let r = reason_for(&s, None, now());
            for text in [r.summary(), r.compact()]
                .into_iter()
                .chain(detail_for(&s, None, now()).lines)
            {
                for (end, _) in text
                    .char_indices()
                    .chain(std::iter::once((text.len(), '\0')))
                {
                    let prefix = &text[..end];
                    for marker in ["POTA", "SOTA"] {
                        if let Some(at) = prefix.find(marker) {
                            assert!(
                                prefix[..at].contains("maybe")
                                    || prefix.contains("not a confirmed"),
                                "{prefix}"
                            );
                        }
                    }
                }
            }
            for text in [r.summary(), r.compact()] {
                for forbidden in ["activation", "activator", "park", "summit"] {
                    assert!(!text.contains(forbidden));
                }
            }
        }
    }
    #[test]
    fn no_portable_text_without_a_suffix() {
        let s = station("K1ABC");
        let r = reason_for(&s, None, now());
        for text in [r.summary(), r.compact()]
            .into_iter()
            .chain(detail_for(&s, None, now()).lines)
        {
            assert!(!text.contains("POTA") && !text.contains("SOTA"));
        }
    }
    #[test]
    fn unscored_and_tier_only_rows() {
        let mut s = station("K1ABC");
        s.local_rank = None;
        assert_eq!(
            reason_for(&s, None, now()).summary(),
            "not scored yet (no decode or cqdx data)"
        );
        for (tier, label) in [
            (PriorityTier::Suspect, "callsign looks invalid"),
            (PriorityTier::Standard, "no award need"),
            (PriorityTier::PerBandGridNew, "new grid on this band"),
            (PriorityTier::SpecialStation, "special or notable station"),
            (PriorityTier::PerBandDxccNew, "new or needed entity"),
            (PriorityTier::Atno, "all-time new entity"),
        ] {
            let score = tier as u32 * TIER_BAND_WIDTH + 123;
            s.local_rank = Some(RankedBy {
                score,
                factors: None,
                at: now(),
            });
            s.priority_score = score;
            assert_eq!(reason_for(&s, None, now()).headline.text(), label);
        }
    }
    #[test]
    fn fit_clauses_drops_whole_clauses_and_marks_it() {
        let clauses = [
            "no award need".into(),
            "QSO failed this session".into(),
            "heard this slot at -7 dB".into(),
        ];
        assert_eq!(fit_clauses(&clauses, 30), "no award need; …");
        assert_eq!(fit_clauses(&clauses, 100), clauses.join("; "));
        assert_eq!(fit_clauses(&clauses, 0), "");
        assert_eq!(fit_clauses(&clauses, 1), "…");
    }
    #[test]
    fn detail_lists_every_input_with_its_source() {
        let mut s = station("K1ABC/P");
        factors(&mut s).recent_failure = true;
        let d = detail_for(&s, None, now());
        let labels = [
            "needed entity",
            "entity new on 20M",
            "all-time new entity",
            "notable station",
            "special-event callsign",
            "grid new on 20M",
            "portable suffix",
            "rarity",
            "signal",
            "already worked on 20M",
            "QSO failed this session",
            "network reports",
        ];
        let sources = [
            "cqdx list not loaded",
            "your log",
            "cqdx",
            "cqdx",
            "callsign pattern",
            "your log",
            "callsign suffix (guess)",
            "cqdx (0.50 when unknown)",
            "this decode",
            "your log",
            "this session",
            "cqdx",
        ];
        let start = d
            .lines
            .iter()
            .position(|line| line.starts_with("What the score used"))
            .unwrap()
            + 1;
        for (row, (label, source)) in d.lines[start..start + 12]
            .iter()
            .zip(labels.into_iter().zip(sources))
        {
            assert!(row.contains(label) && row.contains(source), "{row}");
        }
        let t = s.local_rank.unwrap().factors.unwrap().terms;
        let arithmetic_start = d
            .lines
            .iter()
            .position(|line| line == "Within-tier score")
            .unwrap()
            + 1;
        for (line, (label, term)) in d.lines[arithmetic_start..].iter().zip([
            ("rarity", t.rarity),
            ("portable suffix", t.portable),
            ("signal", t.signal),
            ("already worked", t.worked),
            ("QSO failed", t.recent_failure),
        ]) {
            assert_eq!(
                line,
                &format!(
                    "  {label:<20} {:.2} × {:.2} = {:+.3}",
                    term.input,
                    term.weight,
                    term.contribution()
                )
            );
        }
        assert!(d.lines[arithmetic_start + 5].contains(&format!("bonus = {:+.3}", t.network_bonus)));
        assert!(d.lines.iter().any(|line| line.contains(&format!(
            "{:+.3} ÷ {:.2} ceiling",
            t.raw(),
            t.ceiling
        ))));
        assert!(
            d.lines
                .iter()
                .any(|line| line
                    .contains(&format!("→ {} of 999", s.priority_score % TIER_BAND_WIDTH)))
        );
        assert_eq!(
            d.lines.last().unwrap(),
            "Scored by the station from your log and this session's history (local decode)."
        );
        s.network_rank = s.local_rank.take();
        assert_eq!(detail_for(&s, None, now()).lines.last().unwrap(), "Scored by the TUI from cqdx spot data. Per-band and failure history come only from an earlier local decode.");
    }
}
