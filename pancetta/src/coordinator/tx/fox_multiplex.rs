//! FT8 Fox type 0.1 multiplexing plan (PAN-386).
//!
//! A WSJT-X Fox sends RR73 to one Hound and a report to another in one
//! DXpedition (type 0.1) frame: `K1ABC RR73; W9XYZ <KH7Z> -08`. Pancetta keeps
//! one QSO, one bundle item and one attempt id per Hound; the pairing happens
//! only here, at encode time, from the item texts. Nothing in this module does
//! I/O: it reads `TransmitRequestItem`s and returns a frame plan.

use crate::message_bus::TransmitRequestItem;

/// What one Fox stream's text says, when it is a type 0.1 candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FoxPart {
    /// `<hound> <fox> RR73`.
    Rr73 { hound: String, fox: String },
    /// `<hound> <fox> <report>`, a signed report in dB.
    Report { hound: String, fox: String, db: i8 },
}

impl FoxPart {
    fn fox(&self) -> &str {
        match self {
            FoxPart::Rr73 { fox, .. } | FoxPart::Report { fox, .. } => fox,
        }
    }
}

/// Parse a Fox stream's text into a type 0.1 candidate.
///
/// Accepts exactly three whitespace tokens: the Hound, the Fox (optionally in
/// `<...>`, brackets stripped), then `RR73` or a signed report `[+-]N` in
/// -30..=32 (what a type 0.1 report field can carry). Anything else (a CQ, an R-report, a 73, a grid) is `None`.
pub(crate) fn parse_fox_part(text: &str) -> Option<FoxPart> {
    let mut tokens = text.split_whitespace();
    let hound = tokens.next()?;
    let fox = tokens.next()?;
    let last = tokens.next()?;
    if tokens.next().is_some() {
        return None;
    }
    let fox = fox
        .strip_prefix('<')
        .and_then(|f| f.strip_suffix('>'))
        .unwrap_or(fox);
    if hound.is_empty() || fox.is_empty() || hound == "CQ" {
        return None;
    }
    if last == "RR73" {
        return Some(FoxPart::Rr73 {
            hound: hound.to_string(),
            fox: fox.to_string(),
        });
    }
    let sign = last.chars().next()?;
    if sign != '+' && sign != '-' {
        return None;
    }
    let digits = &last[1..];
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let db: i32 = last.parse().ok()?;
    // The type 0.1 report field holds 2*r5 - 30 dB (r5 = 0..31), so only
    // -30..=32 fits; anything else goes out as a standard frame rather than
    // being clamped to a different report.
    if !(-30..=32).contains(&db) {
        return None;
    }
    Some(FoxPart::Report {
        hound: hound.to_string(),
        fox: fox.to_string(),
        db: db as i8,
    })
}

/// One frame the Fox-aware encode sends.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FoxFrame {
    /// Indices into the input items this frame carries, lowest first. One
    /// index for a standard frame; the RR73 item's and the report item's
    /// indices for a type 0.1 frame.
    pub item_indices: Vec<usize>,
    /// Absolute audio frequency of the frame. A type 0.1 pair airs at the
    /// report item's slot, where that Hound's QSO already listens.
    pub frequency_offset: f64,
    /// `(rr73_to, report_to, fox, report_db)` for a type 0.1 frame.
    pub dxpedition: Option<(String, String, String, i8)>,
}

impl FoxFrame {
    fn single(index: usize, item: &TransmitRequestItem) -> Self {
        FoxFrame {
            item_indices: vec![index],
            frequency_offset: item.frequency_offset,
            dxpedition: None,
        }
    }
}

/// Plan the frames for a bundle.
///
/// One standard frame per item unless the protocol is FT8. In FT8 the n-th
/// Fox-admitted RR73 item pairs with the n-th Fox-admitted report item that
/// names the same Fox, in bundle order (WSJT-X's index pairing of its RR73
/// and report lists). Each pair airs at the report item's frequency. Frames
/// are ordered by their lowest item index.
pub(crate) fn plan_fox_frames(
    items: &[TransmitRequestItem],
    protocol: pancetta_ft8::Protocol,
    is_fox: &dyn Fn(&str) -> bool,
) -> Vec<FoxFrame> {
    let singles = || {
        items
            .iter()
            .enumerate()
            .map(|(i, item)| FoxFrame::single(i, item))
            .collect::<Vec<_>>()
    };
    if protocol != pancetta_ft8::Protocol::Ft8 {
        return singles();
    }

    let parts: Vec<Option<FoxPart>> = items
        .iter()
        .map(|item| {
            let fox_item = item.qso_id.as_deref().is_some_and(is_fox);
            if fox_item {
                parse_fox_part(&item.message_text)
            } else {
                None
            }
        })
        .collect();

    // Pair per Fox callsign, by order. `paired[i]` is the partner index.
    let mut paired: Vec<Option<usize>> = vec![None; items.len()];
    let mut used = vec![false; items.len()];
    for (i, part) in parts.iter().enumerate() {
        let Some(FoxPart::Rr73 { fox, .. }) = part else {
            continue;
        };
        if used[i] {
            continue;
        }
        let partner = parts.iter().enumerate().position(|(j, p)| {
            !used[j]
                && matches!(p, Some(FoxPart::Report { .. }))
                && p.as_ref().is_some_and(|p| p.fox() == fox)
        });
        if let Some(j) = partner {
            used[i] = true;
            used[j] = true;
            paired[i] = Some(j);
            paired[j] = Some(i);
        }
    }

    let mut frames = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        match paired[i] {
            None => frames.push(FoxFrame::single(i, item)),
            Some(j) if j < i => {} // already emitted with its lower index
            Some(j) => {
                let (rr73_idx, report_idx) = if matches!(parts[i], Some(FoxPart::Rr73 { .. })) {
                    (i, j)
                } else {
                    (j, i)
                };
                let (
                    Some(FoxPart::Rr73 {
                        hound: rr73_to,
                        fox,
                    }),
                    Some(FoxPart::Report {
                        hound: report_to,
                        db,
                        ..
                    }),
                ) = (&parts[rr73_idx], &parts[report_idx])
                else {
                    frames.push(FoxFrame::single(i, item));
                    frames.push(FoxFrame::single(j, &items[j]));
                    continue;
                };
                frames.push(FoxFrame {
                    item_indices: vec![i, j],
                    frequency_offset: items[report_idx].frequency_offset,
                    dxpedition: Some((rr73_to.clone(), report_to.clone(), fox.clone(), *db)),
                });
            }
        }
    }
    frames.sort_by_key(|f| f.item_indices[0]);
    frames
}
