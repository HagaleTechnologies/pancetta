//! Display vocabulary only. Wire and persistence strings belong to their types.
use crate::{app::RigConnDisplay, view::ActiveView};
use pancetta_config::rig::PttMethod;
use pancetta_core::tx_meter::{ForwardPower, TxMeterReading};
use pancetta_core::ResponseStep;

pub(crate) fn rig_label(state: RigConnDisplay) -> &'static str {
    match state {
        RigConnDisplay::NotConnected => "not connected",
        RigConnDisplay::Connected => "connected",
        RigConnDisplay::PollingFailed => "polling failed",
    }
}

pub(crate) fn view_message(view: ActiveView) -> &'static str {
    match view {
        ActiveView::Operate => "Operate view",
        ActiveView::Hunt => "Hunt view",
        ActiveView::Run => "Run view",
        ActiveView::Monitor => "Monitor view",
    }
}

/// Short view name for the footer (PAN-388): `view_message` without " view".
pub(crate) fn view_name(view: ActiveView) -> &'static str {
    match view {
        ActiveView::Operate => "Operate",
        ActiveView::Hunt => "Hunt",
        ActiveView::Run => "Run",
        ActiveView::Monitor => "Monitor",
    }
}

pub(crate) fn reply_label(step: ResponseStep) -> &'static str {
    match step {
        ResponseStep::Grid => "grid",
        ResponseStep::Report => "signal report",
        ResponseStep::ReportAck => "acknowledged report",
        ResponseStep::Rr73 => "RR73",
        ResponseStep::SeventyThree => "73",
    }
}

pub(crate) fn ptt_label(method: &PttMethod) -> &'static str {
    match method {
        PttMethod::None => "none",
        PttMethod::Cat => "CAT",
        PttMethod::Serial => "serial",
        PttMethod::Parallel => "parallel",
        PttMethod::Usb => "USB",
        PttMethod::SoundCard => "sound card",
        PttMethod::Vox => "VOX",
        PttMethod::Network => "network",
    }
}

pub(crate) fn rarity_label(tier: Option<&str>) -> &'static str {
    match tier {
        Some("legendary") => "Legendary",
        Some("very_rare") => "Very rare",
        Some("rare") => "Rare",
        Some("uncommon") => "Uncommon",
        Some("common") => "Common",
        None | Some("") => "-",
        Some(_) => "Unknown",
    }
}

/// PAN-291: forward power as the Shift+S TX meter row shows it. Watts under
/// 10 keep one decimal; a 0..1 fraction shows as a percent of max power.
pub(crate) fn forward_power_label(power: ForwardPower) -> String {
    match power {
        ForwardPower::Watts(w) if w < 10.0 => format!("{w:.1} W"),
        ForwardPower::Watts(w) => format!("{w:.0} W"),
        ForwardPower::FractionOfMax(f) => format!("{:.0}% of max power", f * 100.0),
    }
}

/// PAN-291: the state of the Shift+S TX meter row, chosen by the caller in
/// the design's rule order (fresh reading, no CAT, unsupported, idle).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum TxMeterRow {
    /// A reading at most `TX_METER_LIVE_SECS` old, and the ALC warning limit
    /// when the warning is on and the session announced it.
    Live {
        reading: TxMeterReading,
        alc_limit: Option<f32>,
    },
    /// The rig is not connected or polling failed.
    NeedsCat,
    /// The rig lists neither meter.
    NotReported,
    /// Not keyed, no reading yet, or capability not known yet.
    Idle,
}

impl TxMeterRow {
    /// `true` when a live ALC reading is strictly above the announced limit.
    pub(crate) fn overdriven(self) -> bool {
        match self {
            TxMeterRow::Live {
                reading: TxMeterReading { alc: Some(alc), .. },
                alc_limit: Some(limit),
            } => alc > limit,
            _ => false,
        }
    }
}

/// PAN-291: the TX meter row text that follows the dot.
pub(crate) fn tx_meter_text(row: TxMeterRow) -> String {
    match row {
        TxMeterRow::Live { reading, alc_limit } => {
            let power = reading
                .forward_power
                .map(forward_power_label)
                .unwrap_or_else(|| "power n/a".to_string());
            let alc = match (reading.alc, alc_limit) {
                (Some(alc), Some(limit)) => format!("ALC {alc:.2} (limit {limit:.2})"),
                (Some(alc), None) => format!("ALC {alc:.2}"),
                (None, _) => "ALC n/a".to_string(),
            };
            let advice = if row.overdriven() {
                ", reduce audio drive"
            } else {
                ""
            };
            format!(" TX meter: {power}, {alc}{advice}")
        }
        TxMeterRow::NeedsCat => " TX meter: needs a connected CAT rig".to_string(),
        TxMeterRow::NotReported => {
            " TX meter: this rig does not report power or ALC over CAT".to_string()
        }
        TxMeterRow::Idle => " TX meter: shown while transmitting".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_power_label_formats_watts_and_fraction() {
        assert_eq!(forward_power_label(ForwardPower::Watts(48.0)), "48 W");
        assert_eq!(forward_power_label(ForwardPower::Watts(7.5)), "7.5 W");
        assert_eq!(forward_power_label(ForwardPower::Watts(100.0)), "100 W");
        assert_eq!(
            forward_power_label(ForwardPower::FractionOfMax(0.48)),
            "48% of max power"
        );
    }

    #[test]
    fn pan353_rendered_text_vocabulary_is_exhaustive() {
        use RigConnDisplay::*;
        for (state, expected) in [
            (NotConnected, "not connected"),
            (Connected, "connected"),
            (PollingFailed, "polling failed"),
        ] {
            assert_eq!(rig_label(state), expected);
        }
        use ActiveView::*;
        for (view, expected) in [
            (Operate, "Operate view"),
            (Hunt, "Hunt view"),
            (Run, "Run view"),
            (Monitor, "Monitor view"),
        ] {
            assert_eq!(view_message(view), expected);
        }
        use ResponseStep::*;
        for (step, expected) in [
            (Grid, "grid"),
            (Report, "signal report"),
            (ReportAck, "acknowledged report"),
            (Rr73, "RR73"),
            (SeventyThree, "73"),
        ] {
            assert_eq!(reply_label(step), expected);
        }
        use PttMethod::*;
        for (method, expected) in [
            (None, "none"),
            (Cat, "CAT"),
            (Serial, "serial"),
            (Parallel, "parallel"),
            (Usb, "USB"),
            (SoundCard, "sound card"),
            (Vox, "VOX"),
            (Network, "network"),
        ] {
            assert_eq!(ptt_label(&method), expected);
        }
    }
}

/// Scan only controlled application fixtures, never arbitrary incoming radio text.
#[cfg(test)]
pub(crate) fn assert_readable_buffer(buf: &ratatui::buffer::Buffer) {
    for y in 0..buf.area.height {
        let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        assert!(
            debug_token(&row).is_none(),
            "display leaked {:?}: {row}",
            debug_token(&row)
        );
    }
}

#[cfg(test)]
fn debug_token(text: &str) -> Option<&str> {
    for token in [
        "NotConnected",
        "PollingFailed",
        "ReportAck",
        "Rr73",
        "SeventyThree",
        "SoundCard",
        "View: Operate",
        "View: Hunt",
        "View: Run",
        "View: Monitor",
        "Some(",
        "Ok(",
        "Err(",
    ] {
        if text.contains(token) {
            return Some(token);
        }
    }
    let mut previous = "";
    for word in text
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty())
    {
        if ["legendary", "very_rare", "rare", "uncommon", "common"].contains(&word)
            && !(word == "rare" && previous == "Very")
        {
            return Some(word);
        }
        previous = word;
    }
    for (i, _) in text.match_indices(" {") {
        let name = text[..i].split_whitespace().last().unwrap_or("");
        if name.starts_with(|c: char| c.is_ascii_uppercase())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Some(name);
        }
    }
    None
}

#[cfg(test)]
mod scanner_tests {
    use super::*;
    #[test]
    fn pan353_rendered_text_scanner_catches_debug_shapes() {
        for bad in [
            "Rig: NotConnected",
            "Rig: PollingFailed",
            "Reply step: ReportAck",
            "Reply step: Rr73",
            "Reply step: SeventyThree",
            "PTT: SoundCard",
            "View: Hunt",
            "very_rare",
            "Some(1834.0)",
            "Status { active: true }",
        ] {
            assert!(debug_token(bad).is_some(), "{bad}");
        }
        for good in [
            "CQ N2ABC FN42",
            "N2ABC N0CALL RR73",
            "Rig: polling failed",
            "Reply step: acknowledged report",
            "Very rare",
            "1834 km",
            "Hunt view",
            "PTT: sound card",
        ] {
            assert!(debug_token(good).is_none(), "{good}");
        }
    }
}

pub(crate) const TX_MARKER: &str = "*";
pub(crate) const BUDGET_MARKER: &str = "!";
pub(crate) const PAUSE_MARKER: &str = "|";

#[cfg(test)]
mod marker_tests {
    use super::*;
    #[test]
    fn pan353_rendered_text_markers_are_one_bmp_cell() {
        for marker in [TX_MARKER, BUDGET_MARKER, PAUSE_MARKER] {
            assert_eq!(marker.chars().count(), 1);
            assert!(marker.chars().all(|c| (c as u32) <= 0xffff));
            assert_eq!(ratatui::text::Span::raw(marker).width(), 1);
        }
    }
}

#[cfg(test)]
#[path = "presentation/tests/view_name_tests.rs"]
mod view_name_tests;
