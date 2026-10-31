//! Window-based evidence for possible TX-induced receive suppression.
use pancetta_core::Band;
use std::collections::{HashMap, VecDeque};

const DESENSE_BASELINE_WINDOWS: usize = 20;
const DESENSE_MIN_BASELINE_SAMPLES: usize = 5;
const DESENSE_MIN_BASELINE_MEAN: f64 = 3.0;
const DESENSE_DROP_RATIO: f64 = 0.25;
const DESENSE_BASELINE_MAX_AGE_MS: u64 = 1_200_000;
const DESENSE_PRE_TX_HEALTH_RATIO: f64 = 0.5;
const DESENSE_CAPTURE_PRE_ROLL_MS: u64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WindowClass {
    Tx,
    PostTx,
    Quiet,
}
impl WindowClass {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Tx => "tx",
            Self::PostTx => "post_tx",
            Self::Quiet => "quiet",
        }
    }
}
pub(super) struct WindowObservation {
    pub slot_start_ms: u64,
    pub slot_len_ms: u64,
    pub decoded_count: usize,
    pub band: Option<Band>,
    pub last_ptt_on_ms: u64,
    pub last_ptt_off_ms: u64,
    pub ptt_active_at_receipt: bool,
}
impl WindowObservation {
    fn class(&self) -> WindowClass {
        if self.last_ptt_on_ms == 0 {
            return WindowClass::Quiet;
        }
        if self.ptt_active_at_receipt {
            return WindowClass::Tx;
        }
        let start = self
            .slot_start_ms
            .saturating_sub(DESENSE_CAPTURE_PRE_ROLL_MS);
        let end = self.slot_start_ms.saturating_add(self.slot_len_ms);
        let off = if self.last_ptt_off_ms >= self.last_ptt_on_ms {
            self.last_ptt_off_ms
        } else {
            self.last_ptt_on_ms.saturating_add(self.slot_len_ms)
        };
        if self.last_ptt_on_ms < end && off >= start {
            WindowClass::Tx
        } else if off < start && off >= start.saturating_sub(self.slot_len_ms) {
            WindowClass::PostTx
        } else {
            WindowClass::Quiet
        }
    }
}
#[derive(Default)]
struct BandDesenseState {
    samples: VecDeque<(u64, usize)>,
    // Retain collapsed quiet observations separately, never in the baseline.
    last_quiet: Option<(u64, usize)>,
    flagged: bool,
}
#[derive(Default)]
pub(super) struct DesenseDetector {
    bands: HashMap<Band, BandDesenseState>,
}
impl DesenseDetector {
    pub(super) fn summary(&self, obs: &WindowObservation) -> (WindowClass, usize, f64) {
        let state = obs.band.and_then(|b| self.bands.get(&b));
        let n = state.map_or(0, |s| s.samples.len());
        let sum = state.map_or(0, |s| s.samples.iter().map(|x| x.1).sum::<usize>());
        (
            obs.class(),
            n,
            if n == 0 { 0.0 } else { sum as f64 / n as f64 },
        )
    }
    pub(super) fn observe(&mut self, obs: &WindowObservation) -> Option<String> {
        let band = obs.band?;
        let class = obs.class();
        let state = self.bands.entry(band).or_default();
        state.samples.retain(|(at, _)| {
            *at <= obs.slot_start_ms && obs.slot_start_ms - *at <= DESENSE_BASELINE_MAX_AGE_MS
        });
        let n = state.samples.len();
        let mean = if n == 0 {
            0.0
        } else {
            state.samples.iter().map(|x| x.1 as f64).sum::<f64>() / n as f64
        };
        if class == WindowClass::Tx {
            return None;
        }
        let ready = n >= DESENSE_MIN_BASELINE_SAMPLES && mean >= DESENSE_MIN_BASELINE_MEAN;
        let collapsed = ready && obs.decoded_count as f64 <= mean * DESENSE_DROP_RATIO;
        let healthy = ready && obs.decoded_count as f64 >= mean * DESENSE_PRE_TX_HEALTH_RATIO;
        if healthy {
            state.flagged = false;
        }
        let pre_tx_healthy = state.last_quiet.is_some_and(|(at, count)| {
            at < obs.last_ptt_on_ms
                && obs.slot_start_ms.saturating_sub(at) <= DESENSE_BASELINE_MAX_AGE_MS
                && count as f64 >= mean * DESENSE_PRE_TX_HEALTH_RATIO
        });
        if class == WindowClass::Quiet {
            state.last_quiet = Some((obs.slot_start_ms, obs.decoded_count));
            if !collapsed {
                state
                    .samples
                    .push_back((obs.slot_start_ms, obs.decoded_count));
                while state.samples.len() > DESENSE_BASELINE_WINDOWS {
                    state.samples.pop_front();
                }
            }
        }
        if class != WindowClass::PostTx || !collapsed || !pre_tx_healthy || state.flagged {
            return None;
        }
        state.flagged = true;
        let decoded_count = obs.decoded_count;
        Some(format!(
            "possible TX-induced desense on {band}: only {decoded_count} messages decoded in the first receive window after our transmission vs. a {mean:.1}-message recent {band} baseline (band healthy before TX) — if this keeps happening, check TX/RX audio-path isolation (e.g. a shared USB audio interface)"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn obs(slot: u64, count: usize) -> WindowObservation {
        WindowObservation {
            slot_start_ms: slot,
            slot_len_ms: 15_000,
            decoded_count: count,
            band: Band::from_frequency(7_074_000),
            last_ptt_on_ms: 0,
            last_ptt_off_ms: 0,
            ptt_active_at_receipt: false,
        }
    }
    fn seeded(count: usize) -> DesenseDetector {
        let mut d = DesenseDetector::default();
        for i in 1..=20 {
            d.observe(&obs(i * 15_000, count));
        }
        d
    }
    fn post(slot: u64, count: usize) -> WindowObservation {
        let mut o = obs(slot, count);
        o.last_ptt_on_ms = slot - 15_000;
        o.last_ptt_off_ms = slot - 2_000;
        o
    }
    #[test]
    fn tx_window_never_warns() {
        let mut d = seeded(10);
        let before = d.bands[&obs(0, 0).band.unwrap()].samples.clone();
        let mut o = post(330_000, 0);
        o.slot_start_ms = 315_000;
        assert_eq!(o.class(), WindowClass::Tx);
        assert!(d.observe(&o).is_none());
        assert_eq!(d.bands[&o.band.unwrap()].samples, before);
    }
    #[test]
    fn tx_window_with_manual_midwindow_ptt_never_warns() {
        let mut d = seeded(10);
        let mut o = obs(315_000, 0);
        o.last_ptt_on_ms = 318_000;
        o.last_ptt_off_ms = 320_000;
        assert_eq!(o.class(), WindowClass::Tx);
        assert!(d.observe(&o).is_none());
    }
    #[test]
    fn first_post_tx_rx_window_with_collapse_warns() {
        let mut d = seeded(10);
        let o = post(330_000, 1);
        assert_eq!(o.class(), WindowClass::PostTx);
        let text = d.observe(&o).unwrap();
        assert!(text.contains("40m"));
        assert!(text.contains(concat!("first receive window ", "after our transmission")));
        assert!(text.contains("band healthy before TX"));
    }
    #[test]
    fn second_post_tx_window_is_quiet_not_eligible() {
        let mut d = seeded(10);
        let mut o = post(330_000, 0);
        o.slot_start_ms += 15_000;
        assert_eq!(o.class(), WindowClass::Quiet);
        assert!(d.observe(&o).is_none());
        assert_eq!(d.bands[&o.band.unwrap()].samples.back().unwrap().0, 300_000);
        o.decoded_count = 10;
        d.observe(&o);
        assert_eq!(d.bands[&o.band.unwrap()].samples.back().unwrap().0, 345_000);
    }
    #[test]
    fn collapsed_window_never_pollutes_baseline() {
        let mut d = seeded(10);
        let b = obs(0, 0).band.unwrap();
        let before = d.bands[&b].samples.clone();
        d.observe(&obs(315_000, 0));
        d.observe(&post(345_000, 1));
        assert_eq!(d.bands[&b].samples, before);
    }
    #[test]
    fn band_mismatched_baseline_does_not_warn() {
        let mut d = DesenseDetector::default();
        for i in 1..=5 {
            let mut o = obs(i * 15_000, 10);
            o.band = Band::from_frequency(14_074_000);
            d.observe(&o);
        }
        let o = post(120_000, 0);
        assert!(d.observe(&o).is_none());
        assert_eq!(d.summary(&o).1, 0);
    }
    #[test]
    fn stale_baseline_does_not_warn() {
        let mut d = seeded(10);
        let o = post(300_000 + DESENSE_BASELINE_MAX_AGE_MS + 1, 0);
        assert!(d.observe(&o).is_none());
        assert_eq!(d.summary(&o).1, 0);
    }
    #[test]
    fn pre_tx_band_fade_does_not_warn() {
        let mut d = seeded(10);
        d.observe(&obs(315_000, 4));
        assert!(d.observe(&post(345_000, 0)).is_none());
    }
    #[test]
    fn episode_is_edge_triggered() {
        let mut d = seeded(10);
        assert!(d.observe(&post(330_000, 0)).is_some());
        assert!(d.observe(&post(360_000, 0)).is_none());
        d.observe(&obs(375_000, 10));
        assert!(d.observe(&post(405_000, 0)).is_some());
    }
    #[test]
    fn band_switch_isolation() {
        let mut d = seeded(10);
        assert!(d.observe(&post(330_000, 0)).is_some());
        let b = Band::from_frequency(14_074_000);
        for i in 1..=5 {
            let mut o = obs(330_000 + i * 15_000, 10);
            o.band = b;
            d.observe(&o);
        }
        let mut o = post(435_000, 0);
        o.band = b;
        assert!(d.observe(&o).is_some());
        assert!(d.observe(&post(465_000, 0)).is_none());
    }
    #[test]
    fn ft4_slot_length_supported() {
        let mut d = seeded(10);
        let mut o = post(322_500, 0);
        o.slot_len_ms = 7_500;
        o.last_ptt_on_ms = 315_000;
        o.last_ptt_off_ms = 321_000;
        assert_eq!(o.class(), WindowClass::PostTx);
        assert!(d.observe(&o).is_some());
    }
    #[test]
    fn unknown_band_is_inert() {
        let mut d = DesenseDetector::default();
        for hz in [0, 123] {
            let mut o = post(330_000, 0);
            o.band = Band::from_frequency(hz);
            assert!(d.observe(&o).is_none());
        }
        assert!(d.bands.is_empty());
    }
    #[test]
    fn never_transmitted_is_inert() {
        let mut d = seeded(10);
        let o = obs(315_000, 0);
        assert_eq!(o.class(), WindowClass::Quiet);
        assert!(d.observe(&o).is_none());
    }
    #[test]
    fn ptt_active_at_receipt_forces_tx_class() {
        let mut o = post(330_000, 0);
        o.last_ptt_on_ms = 1;
        o.ptt_active_at_receipt = true;
        assert_eq!(o.class(), WindowClass::Tx);
    }
    #[test]
    fn insufficient_quiet_band_and_mild_dip_do_not_warn() {
        for count in [1, 10] {
            let mut d = DesenseDetector::default();
            for i in 1..5 {
                d.observe(&obs(i * 15_000, count));
            }
            assert!(d.observe(&post(90_000, 0)).is_none());
        }
        assert!(seeded(1).observe(&post(330_000, 0)).is_none());
        assert!(seeded(10).observe(&post(330_000, 3)).is_none());
    }
    #[test]
    fn exact_thresholds_and_freshness_boundary() {
        assert!(seeded(20).observe(&post(330_000, 5)).is_some());
        let mut d = seeded(10);
        d.observe(&obs(315_000, 5));
        assert!(d.observe(&post(345_000, 0)).is_some());
        let mut d = DesenseDetector::default();
        for _ in 0..5 {
            d.observe(&obs(300_000, 10));
        }
        assert!(d
            .observe(&post(300_000 + DESENSE_BASELINE_MAX_AGE_MS, 0))
            .is_some());
    }
    #[test]
    fn quiet_window_baseline_is_bounded() {
        let mut d = seeded(10);
        for i in 21..=100 {
            d.observe(&obs(i * 15_000, 10));
        }
        assert_eq!(
            d.bands[&obs(0, 0).band.unwrap()].samples.len(),
            DESENSE_BASELINE_WINDOWS
        );
    }

    #[test]
    fn slot_boundaries_and_unknown_off() {
        let mut o = post(330_000, 0);
        o.last_ptt_off_ms = 329_000;
        assert_eq!(o.class(), WindowClass::Tx);
        o.last_ptt_off_ms = 328_999;
        assert_eq!(o.class(), WindowClass::PostTx);
        o.last_ptt_off_ms = 313_999;
        o.last_ptt_on_ms = 300_000;
        assert_eq!(o.class(), WindowClass::Quiet);
        o.last_ptt_on_ms = 330_000;
        o.last_ptt_off_ms = 0;
        assert_eq!(o.class(), WindowClass::Tx);
        o.last_ptt_on_ms = 345_000;
        assert_eq!(o.class(), WindowClass::Quiet);
    }
}
