//! GridWithRAck exchange-shape matcher — see
//! docs/superpowers/specs/2026-08-30-contest-mode-design.md §2.

use super::tokenizer::tokenize_directed_message;

/// A decoded message recognized as a `GridWithRAck` contest ack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContestMatch {
    pub to_station: String,
    pub from_station: String,
    pub grid: String,
}

/// Recognize `"<to> <from> R <grid>"` — the state-QSO-party / ARRL
/// International Digital Contest ack, standing in for a numeric report.
/// Grid must be 4 characters: first two `A`-`R`, last two digits — the
/// same shape pancetta-ft8's decoder already accepts (message.rs's
/// `unpackgrid`).
pub fn match_grid_with_r_ack(text: &str) -> Option<ContestMatch> {
    let msg = tokenize_directed_message(text)?;
    let grid_candidate = msg.trailing.strip_prefix("R ")?;
    if !is_valid_grid(grid_candidate) {
        return None;
    }
    Some(ContestMatch {
        to_station: msg.to_station,
        from_station: msg.from_station,
        grid: grid_candidate.to_string(),
    })
}

fn is_valid_grid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 4
        && (b'A'..=b'R').contains(&b[0])
        && (b'A'..=b'Q').contains(&b[1])
        && b[2].is_ascii_digit()
        && b[3].is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_pan_49_repro_text() {
        // The actual decoded line from ~/.pancetta/logs/pancetta.log.2026-08-30
        // (K5TD acking our grid during the 2026-08-29/30 KSQP session).
        let m = match_grid_with_r_ack("K5ARH K5TD R EM40").unwrap();
        assert_eq!(m.to_station, "K5ARH");
        assert_eq!(m.from_station, "K5TD");
        assert_eq!(m.grid, "EM40");
    }

    #[test]
    fn rejects_plain_grid_without_r_prefix() {
        assert!(match_grid_with_r_ack("K5TD K5ARH EM10").is_none());
    }

    #[test]
    fn rejects_numeric_report_with_r_prefix() {
        // "R-12" is ONE token (no space) — must not be misread as a grid ack.
        assert!(match_grid_with_r_ack("K1ABC W9XYZ R-12").is_none());
    }

    #[test]
    fn rejects_malformed_grid_after_r() {
        assert!(match_grid_with_r_ack("K5ARH K5TD R RR73").is_none());
        assert!(match_grid_with_r_ack("K5ARH K5TD R EM4").is_none());
    }

    #[test]
    fn rejects_cq() {
        assert!(match_grid_with_r_ack("CQ KSQP W0S DM99").is_none());
    }
}
