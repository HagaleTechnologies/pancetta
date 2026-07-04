//! Shared frequency-to-bin-index math for the TX-placement instrument.
//!
//! `pancetta` (coordinator, degradation checks) and `pancetta-tui` (openness
//! strip rendering, click-to-park) each hold their own copy of the placement
//! snapshot's per-bin openness data, so both need to map a frequency to its
//! bin index. Before this module existed they reimplemented the math
//! independently and disagreed at the exact top edge of the range (issue
//! #97 item 3) — one side floored + clamped into the last bin, the other
//! truncated and fell one bin past the end. This is the single
//! implementation both sides call.

/// Map a frequency in Hz to its bin index within `range`, given a uniform
/// `bin_hz` bin width and `n_bins` total bins. Returns `None` when `freq_hz`
/// falls outside `range` (inclusive at both ends) or the inputs are
/// degenerate (`n_bins == 0` or `bin_hz <= 0.0`).
///
/// The result is clamped to `n_bins - 1` so a frequency exactly at the top
/// edge (`range.1`) resolves to the last bin rather than one past the end.
pub fn bin_index_for_freq(
    freq_hz: f64,
    range: (f64, f64),
    bin_hz: f64,
    n_bins: usize,
) -> Option<usize> {
    let (lo, hi) = range;
    if n_bins == 0 || bin_hz <= 0.0 || freq_hz < lo || freq_hz > hi {
        return None;
    }
    let idx = ((freq_hz - lo) / bin_hz).floor().max(0.0) as usize;
    Some(idx.min(n_bins - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_of_range_is_none() {
        assert_eq!(bin_index_for_freq(100.0, (200.0, 2600.0), 25.0, 96), None);
        assert_eq!(bin_index_for_freq(2700.0, (200.0, 2600.0), 25.0, 96), None);
    }

    #[test]
    fn maps_hz_to_bin() {
        assert_eq!(
            bin_index_for_freq(1480.0, (200.0, 2600.0), 25.0, 96),
            Some(51)
        );
    }

    #[test]
    fn top_edge_clamps_to_last_bin_instead_of_one_past_the_end() {
        assert_eq!(
            bin_index_for_freq(2600.0, (200.0, 2600.0), 25.0, 96),
            Some(95)
        );
    }

    #[test]
    fn degenerate_inputs_are_none() {
        assert_eq!(bin_index_for_freq(500.0, (200.0, 2600.0), 25.0, 0), None);
        assert_eq!(bin_index_for_freq(500.0, (200.0, 2600.0), 0.0, 96), None);
    }
}
