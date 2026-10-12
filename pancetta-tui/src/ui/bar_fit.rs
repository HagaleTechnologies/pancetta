//! Priority-fit for one-row bars (PAN-88): the title bar and the footer
//! key-hint row.
//!
//! A bar is a list of [`BarSegment`]s in display order. Each segment has an
//! *admit* step (brings it in at its compact form when it has one, its full
//! form otherwise) and, when it has a compact form, an *upgrade* step
//! (compact → full). [`fit_bar`] sorts every step by rank (lower = more
//! important) and applies them greedily: a step that pushes the bar past
//! its budget is reverted, and a reverted step never blocks a later, smaller
//! one. Rank-0 admit steps are pinned and never reverted.
//!
//! Interleaving admit and upgrade steps in one list is deliberate. "Full
//! form first" drops a whole chip (e.g. the frequency) to keep a long form
//! of a lower-value chip; "compact first" lets a low-value chip crowd out
//! a high-value one's full form. One ordered table expresses both trades.
//! When the row fits, every step succeeds, so the output is exactly the
//! naive concatenation of every full form.
//!
//! [`prefix_fit`] is the column-table counterpart used by DX Hunter and
//! Callers: walk the columns in priority order and stop at the first one
//! that doesn't fit.

use ratatui::text::Span;

/// One chip (or key hint) in a priority-fitted bar.
pub(crate) struct BarSegment<'a> {
    /// Emitted before this segment when an earlier (display-order) segment is visible.
    pub sep: &'static str,
    pub full: Vec<Span<'a>>,
    pub compact: Option<Vec<Span<'a>>>,
    /// Admit-step rank: lower = more important; 0 = pinned (never dropped).
    pub admit: u8,
    /// Upgrade-step rank (compact → full); ignored when `compact` is None.
    pub upgrade: u8,
}

impl<'a> BarSegment<'a> {
    /// A segment with no compact form.
    pub(crate) fn new(sep: &'static str, full: Vec<Span<'a>>, admit: u8) -> Self {
        Self {
            sep,
            full,
            compact: None,
            admit,
            upgrade: 0,
        }
    }

    /// A segment that is admitted at `compact` and upgraded to `full`.
    pub(crate) fn with_compact(
        sep: &'static str,
        full: Vec<Span<'a>>,
        compact: Vec<Span<'a>>,
        admit: u8,
        upgrade: u8,
    ) -> Self {
        Self {
            sep,
            full,
            compact: Some(compact),
            admit,
            upgrade,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SegmentForm {
    Hidden,
    Compact,
    Full,
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(Span::width).sum()
}

fn form_spans<'s, 'a>(seg: &'s BarSegment<'a>, form: SegmentForm) -> Option<&'s [Span<'a>]> {
    match form {
        SegmentForm::Hidden => None,
        SegmentForm::Full => Some(&seg.full),
        SegmentForm::Compact => Some(seg.compact.as_deref().unwrap_or(&seg.full)),
    }
}

/// Display width (terminal columns) of the bar `segs` would render as with
/// `forms`. The first visible segment omits its separator.
pub(crate) fn bar_width(segs: &[BarSegment<'_>], forms: &[SegmentForm]) -> usize {
    let mut width = 0;
    let mut any_visible = false;
    for (seg, &form) in segs.iter().zip(forms) {
        if let Some(spans) = form_spans(seg, form) {
            if any_visible {
                width += Span::raw(seg.sep).width();
            }
            width += spans_width(spans);
            any_visible = true;
        }
    }
    width
}

/// Choose a form for every segment so the bar fits `budget` columns (pinned
/// segments excepted), following the module-level step order.
pub(crate) fn fit_bar(segs: &[BarSegment<'_>], budget: usize) -> Vec<SegmentForm> {
    // (rank, is_upgrade, display index): admit steps sort before an upgrade
    // of equal rank, and ties fall back to display order.
    let mut steps: Vec<(u8, bool, usize)> = Vec::with_capacity(segs.len() * 2);
    for (i, seg) in segs.iter().enumerate() {
        steps.push((seg.admit, false, i));
        if seg.compact.is_some() {
            steps.push((seg.upgrade, true, i));
        }
    }
    steps.sort_unstable();

    let mut forms = vec![SegmentForm::Hidden; segs.len()];
    for (rank, is_upgrade, i) in steps {
        let prev = forms[i];
        let next = if is_upgrade {
            if prev != SegmentForm::Compact {
                continue;
            }
            SegmentForm::Full
        } else if segs[i].compact.is_some() {
            SegmentForm::Compact
        } else {
            SegmentForm::Full
        };
        forms[i] = next;
        let pinned = rank == 0 && !is_upgrade;
        if !pinned && bar_width(segs, &forms) > budget {
            forms[i] = prev;
        }
    }
    forms
}

/// Flatten `segs` into spans using `forms` (separators included).
pub(crate) fn bar_spans<'a>(segs: Vec<BarSegment<'a>>, forms: &[SegmentForm]) -> Vec<Span<'a>> {
    let mut out = Vec::new();
    let mut any_visible = false;
    for (seg, &form) in segs.into_iter().zip(forms) {
        let spans = match form {
            SegmentForm::Hidden => continue,
            SegmentForm::Full => seg.full,
            SegmentForm::Compact => seg.compact.unwrap_or(seg.full),
        };
        if any_visible {
            out.push(Span::raw(seg.sep));
        }
        out.extend(spans);
        any_visible = true;
    }
    out
}

/// Prefix rule for adaptive table columns (PAN-88, D3): walk the columns'
/// minimum widths in priority order, adding 1 column of spacing between
/// columns, and return how many fit in `budget` before the first one that
/// doesn't. Monotonic in `budget`, so a wider panel never loses a column a
/// narrower one had.
pub(crate) fn prefix_fit(widths_in_priority: impl IntoIterator<Item = u16>, budget: u16) -> usize {
    let mut used: u16 = 0;
    let mut count = 0;
    for w in widths_in_priority {
        let need = if count == 0 { w } else { w.saturating_add(1) };
        match used.checked_add(need) {
            Some(total) if total <= budget => {
                used = total;
                count += 1;
            }
            _ => break,
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(sep: &'static str, full: &'static str, admit: u8) -> BarSegment<'static> {
        BarSegment::new(sep, vec![Span::raw(full)], admit)
    }

    fn seg_c(
        sep: &'static str,
        full: &'static str,
        compact: &'static str,
        admit: u8,
        upgrade: u8,
    ) -> BarSegment<'static> {
        BarSegment::with_compact(
            sep,
            vec![Span::raw(full)],
            vec![Span::raw(compact)],
            admit,
            upgrade,
        )
    }

    fn text(segs: Vec<BarSegment<'_>>, forms: &[SegmentForm]) -> String {
        bar_spans(segs, forms)
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    fn sample() -> Vec<BarSegment<'static>> {
        vec![
            seg(" | ", "Pancetta TUI", 20),
            seg(" | ", "N0CALL", 0),
            seg_c(" | ", "14.074 MHz 20M", "14.074 20M", 6, 14),
            seg(" | ", "FT8", 5),
            seg(" ", " TX: FULL ", 0),
            seg_c(" ", " DECODE: AUTO 0ms ", " DEC: AUTO ", 13, 19),
        ]
    }

    #[test]
    fn everything_fits_shows_all_full_forms() {
        let segs = sample();
        let forms = fit_bar(&segs, 500);
        assert!(forms.iter().all(|f| *f == SegmentForm::Full), "{forms:?}");
        assert_eq!(
            text(sample(), &forms),
            "Pancetta TUI | N0CALL | 14.074 MHz 20M | FT8  TX: FULL   DECODE: AUTO 0ms "
        );
    }

    #[test]
    fn pinned_segments_survive_an_impossible_budget() {
        let segs = sample();
        let forms = fit_bar(&segs, 0);
        assert_eq!(forms[1], SegmentForm::Full);
        assert_eq!(forms[4], SegmentForm::Full);
        for i in [0, 2, 3, 5] {
            assert_eq!(forms[i], SegmentForm::Hidden, "segment {i}");
        }
        assert_eq!(text(sample(), &forms), "N0CALL  TX: FULL ");
    }

    #[test]
    fn lowest_rank_admission_is_dropped_first() {
        let segs = vec![
            seg(" ", "AAAA", 1),
            seg(" ", "BBBB", 2),
            seg(" ", "CCCC", 3),
        ];
        // Room for two of the three.
        let forms = fit_bar(&segs, 9);
        assert_eq!(
            forms,
            vec![SegmentForm::Full, SegmentForm::Full, SegmentForm::Hidden]
        );
    }

    #[test]
    fn compact_form_is_used_when_full_does_not_fit_and_upgraded_when_room_allows() {
        let segs = vec![seg_c("", "LONGFORM", "SHORT", 1, 2)];
        assert_eq!(fit_bar(&segs, 6), vec![SegmentForm::Compact]);
        assert_eq!(fit_bar(&segs, 8), vec![SegmentForm::Full]);
        assert_eq!(fit_bar(&segs, 4), vec![SegmentForm::Hidden]);
    }

    #[test]
    fn a_skipped_step_does_not_block_later_smaller_steps() {
        let segs = vec![
            seg(" ", "PIN", 0),
            seg(" ", "A-VERY-WIDE-SEGMENT", 5),
            seg(" ", "N", 6),
        ];
        let forms = fit_bar(&segs, 6);
        assert_eq!(
            forms,
            vec![SegmentForm::Full, SegmentForm::Hidden, SegmentForm::Full]
        );
        assert_eq!(text(segs, &forms), "PIN N");
    }

    #[test]
    fn first_visible_segment_omits_its_separator() {
        let segs = vec![seg(" | ", "Pancetta TUI", 9), seg(" | ", "N0CALL", 0)];
        let forms = fit_bar(&segs, 8);
        assert_eq!(forms[0], SegmentForm::Hidden);
        assert_eq!(bar_width(&segs, &forms), 6);
        assert_eq!(text(segs, &forms), "N0CALL");
    }

    #[test]
    fn width_counts_display_columns_not_bytes() {
        let segs = vec![
            seg("", "⚠", 1),
            seg(" ", "⏸", 1),
            seg(" ", "—", 1),
            seg(" ", "★", 1),
        ];
        let forms = vec![SegmentForm::Full; 4];
        assert_eq!(bar_width(&segs, &forms), 7);
        assert_eq!(fit_bar(&segs, 7), forms);
    }

    #[test]
    fn bar_width_never_exceeds_budget_when_pinned_fit() {
        let pinned_width = bar_width(
            &sample(),
            &[
                SegmentForm::Hidden,
                SegmentForm::Full,
                SegmentForm::Hidden,
                SegmentForm::Hidden,
                SegmentForm::Full,
                SegmentForm::Hidden,
            ],
        );
        for budget in 0..=200usize {
            let segs = sample();
            let forms = fit_bar(&segs, budget);
            let w = bar_width(&segs, &forms);
            if budget >= pinned_width {
                assert!(w <= budget, "budget {budget}: width {w}");
            }
            assert_eq!(
                w,
                text(sample(), &forms).chars().count(),
                "bar_width must match the rendered text at budget {budget}"
            );
        }
    }

    #[test]
    fn prefix_fit_stops_at_the_first_column_that_does_not_fit() {
        // 4 + (1+4) = 9 fits; the next 8-wide column would need 18.
        assert_eq!(prefix_fit([4, 4, 8, 1], 9), 2);
        assert_eq!(prefix_fit([4, 4, 8, 1], 18), 3);
        assert_eq!(prefix_fit([4, 4, 8, 1], 20), 4);
        assert_eq!(prefix_fit([4], 3), 0);
        assert_eq!(prefix_fit([], 10), 0);
    }
}
