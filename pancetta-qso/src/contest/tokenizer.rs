//! Shared tokenizer for directed (non-CQ) decoded FT8 text — see
//! docs/superpowers/specs/2026-08-30-contest-mode-design.md §2.

/// A decoded message's callsigns and whatever trailing text follows them,
/// extracted independent of whether the trailing text matches any known
/// exchange shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectedMessage {
    pub to_station: String,
    pub from_station: String,
    pub trailing: String,
}

/// Extract `(to, from, trailing)` from decoded FT8 text whenever the first
/// two whitespace-separated tokens are present and it isn't a CQ. A plain
/// tokenizer, not a callsign validator or exchange-shape matcher — those
/// are the caller's job.
pub fn tokenize_directed_message(text: &str) -> Option<DirectedMessage> {
    let text = text.trim();
    if text.starts_with("CQ") {
        return None;
    }
    let mut parts = text.split_whitespace();
    let to_station = parts.next()?.to_string();
    let from_station = parts.next()?.to_string();
    let trailing: Vec<&str> = parts.collect();
    Some(DirectedMessage {
        to_station,
        from_station,
        trailing: trailing.join(" "),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_r_grid_ack_shape() {
        let m = tokenize_directed_message("K5ARH K5TD R EM40").unwrap();
        assert_eq!(m.to_station, "K5ARH");
        assert_eq!(m.from_station, "K5TD");
        assert_eq!(m.trailing, "R EM40");
    }

    #[test]
    fn tokenizes_plain_grid_shape() {
        let m = tokenize_directed_message("K5TD K5ARH EM10").unwrap();
        assert_eq!(m.trailing, "EM10");
    }

    #[test]
    fn returns_none_for_cq() {
        assert!(tokenize_directed_message("CQ KSQP W0S DM99").is_none());
    }

    #[test]
    fn returns_none_for_fewer_than_two_tokens() {
        assert!(tokenize_directed_message("K5ARH").is_none());
        assert!(tokenize_directed_message("").is_none());
    }

    #[test]
    fn trailing_is_empty_string_for_blank_exchange() {
        let m = tokenize_directed_message("K5TD K5ARH").unwrap();
        assert_eq!(m.trailing, "");
    }
}
