//! PAN-344: per-target upload state for Recent QSOs and the completion
//! toast.
//!
//! The coordinator retains explicit upload results per QSO UUID and target
//! (`pancetta::coordinator::qso_upload_status`) and relays full bounded
//! snapshots; this module holds the TUI-local model and the pure formatting
//! and toast helpers. Acceptance is only ever an explicit service result —
//! a missing entry is `Unknown`, never success, and an empty captured
//! target set is `Disabled`, never "all accepted".

use chrono::{DateTime, Utc};

/// One upload destination. Display order is fixed: QRZ, ClubLog, eQSL,
/// cqdx.io, LoTW — the fixed target letters identify services even when
/// their neighbours are disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum UploadTarget {
    Qrz,
    ClubLog,
    Eqsl,
    Cqdx,
    Lotw,
}

impl UploadTarget {
    /// The fixed single-letter cell identifier.
    pub fn letter(self) -> char {
        match self {
            UploadTarget::Qrz => 'Q',
            UploadTarget::ClubLog => 'C',
            UploadTarget::Eqsl => 'E',
            UploadTarget::Cqdx => 'D',
            UploadTarget::Lotw => 'L',
        }
    }

    /// The detail-card label.
    pub fn label(self) -> &'static str {
        match self {
            UploadTarget::Qrz => "QRZ Logbook",
            UploadTarget::ClubLog => "ClubLog",
            UploadTarget::Eqsl => "eQSL",
            UploadTarget::Cqdx => "cqdx.io",
            UploadTarget::Lotw => "LoTW",
        }
    }
}

/// One target's observed delivery state for one QSO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadState {
    /// Preparing, queued, saving or retrying.
    Pending,
    /// Explicit successful (or duplicate) service acceptance.
    Accepted,
    /// Service/account refusal of the record or credentials.
    Rejected,
    /// Local refusal or failure (queue full, missing payload, failed
    /// one-shot LoTW attempt).
    Failed,
    /// Data missing — never inferred from absence.
    Unknown,
}

impl UploadState {
    /// The row glyph for this state.
    pub fn glyph(self) -> char {
        match self {
            UploadState::Pending => '~',
            UploadState::Accepted => '\u{2713}',
            UploadState::Rejected => '\u{d7}',
            UploadState::Failed => '!',
            UploadState::Unknown => '?',
        }
    }

    /// The detail-card state word.
    pub fn word(self) -> &'static str {
        match self {
            UploadState::Pending => "pending",
            UploadState::Accepted => "accepted",
            UploadState::Rejected => "rejected",
            UploadState::Failed => "failed",
            UploadState::Unknown => "unknown",
        }
    }
}

/// Bound a reason string to display-safe text: control characters are
/// stripped and the result clipped to [`REASON_LIMIT`] characters.
pub fn sanitize_reason(text: &str) -> String {
    let mut cleaned: String = text.chars().filter(|c| !c.is_control()).collect();
    if cleaned.chars().count() > REASON_LIMIT {
        let prefix: String = cleaned.chars().take(REASON_LIMIT - 1).collect();
        cleaned = format!("{prefix}\u{2026}");
    }
    cleaned
}

/// Maximum retained reason length (characters, including the clip marker).
pub const REASON_LIMIT: usize = 64;

/// One target's state plus a bounded, display-safe reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetUploadStatus {
    pub state: UploadState,
    pub reason: String,
}

impl TargetUploadStatus {
    pub fn new(state: UploadState, reason: &str) -> Self {
        Self {
            state,
            reason: sanitize_reason(reason),
        }
    }

    pub fn pending(reason: &str) -> Self {
        Self::new(UploadState::Pending, reason)
    }
}

/// One QSO's retained upload status. `revision` increases monotonically per
/// contact so stale snapshots cannot overwrite newer ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QsoUploadStatus {
    pub uuid: String,
    pub callsign: String,
    pub revision: u64,
    /// Only enabled targets, in canonical display order.
    pub targets: Vec<(UploadTarget, TargetUploadStatus)>,
}

impl QsoUploadStatus {
    /// The state for `target`, if it was captured as enabled.
    pub fn state(&self, target: UploadTarget) -> Option<UploadState> {
        self.targets
            .iter()
            .find(|(t, _)| *t == target)
            .map(|(_, s)| s.state)
    }
}

/// Sort targets into the canonical display order.
pub fn sort_targets(targets: &mut [(UploadTarget, TargetUploadStatus)]) {
    targets.sort_by_key(|(t, _)| *t);
}

/// The aggregated verdict a snapshot supports, for toast wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadVerdict {
    /// Work scheduled but not yet accepted everywhere.
    Queued,
    /// Every captured target explicitly accepted (nonempty set).
    AllAccepted,
    /// At least one rejection or local failure.
    NeedsAttention,
    /// No enabled targets were captured.
    Disabled,
    /// At least one target's result is unavailable.
    Unknown,
}

/// Classify a snapshot. Precedence: empty set is `Disabled`; any
/// rejection/failure is `NeedsAttention`; any unknown result is `Unknown`;
/// all accepted is `AllAccepted`; anything left is still `Queued`.
pub fn verdict(status: &QsoUploadStatus) -> UploadVerdict {
    if status.targets.is_empty() {
        return UploadVerdict::Disabled;
    }
    let states: Vec<UploadState> = status
        .targets
        .iter()
        .map(|(target, _)| status.state(*target).unwrap_or(UploadState::Unknown))
        .collect();
    if states
        .iter()
        .any(|s| matches!(s, UploadState::Rejected | UploadState::Failed))
    {
        return UploadVerdict::NeedsAttention;
    }
    if states.contains(&UploadState::Unknown) {
        return UploadVerdict::Unknown;
    }
    if states.iter().all(|s| *s == UploadState::Accepted) {
        return UploadVerdict::AllAccepted;
    }
    UploadVerdict::Queued
}

/// The compact per-row glyph cells, e.g. `Q✓ C~ E×`. Only enabled targets
/// occupy cells, in canonical display order.
pub fn glyph_cells(status: &QsoUploadStatus) -> String {
    status
        .targets
        .iter()
        .map(|(t, s)| format!("{}{}", t.letter(), s.state.glyph()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The completion-toast text for a verdict.
pub fn toast_text(callsign: &str, verdict: UploadVerdict) -> String {
    match verdict {
        UploadVerdict::AllAccepted => format!("{callsign} uploaded"),
        UploadVerdict::NeedsAttention => format!("{callsign} logged \u{b7} uploads need attention"),
        UploadVerdict::Disabled => format!("{callsign} logged \u{b7} uploads disabled"),
        UploadVerdict::Unknown | UploadVerdict::Queued => {
            if verdict == UploadVerdict::Unknown {
                format!("{callsign} logged \u{b7} upload state unknown")
            } else {
                format!("{callsign} logged \u{b7} uploads queued")
            }
        }
    }
}

/// How long each toast text stays visible, in seconds.
pub const TOAST_SECS: i64 = 6;

/// The typed completion toast. Owned by a QSO UUID; never derived from a
/// callsign, aggregate health, an empty queue or a missing map entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionToast {
    pub uuid: String,
    pub callsign: String,
    /// Current text (starts as the queued wording).
    pub text: String,
    /// Whether a terminal transition (all-accepted / needs-attention) has
    /// already been shown for this contact.
    pub terminal_shown: bool,
    /// When the current text was first shown.
    pub since: DateTime<Utc>,
}

impl CompletionToast {
    /// Start the toast for a completed contact. "Queued" means upload work
    /// has been scheduled, not accepted or necessarily saved yet.
    pub fn start(
        uuid: &str,
        callsign: &str,
        status: Option<&QsoUploadStatus>,
        now: DateTime<Utc>,
    ) -> Self {
        let v = status.map(verdict).unwrap_or(UploadVerdict::Queued);
        let terminal = matches!(
            v,
            UploadVerdict::AllAccepted | UploadVerdict::NeedsAttention
        );
        Self {
            uuid: uuid.to_string(),
            callsign: callsign.to_string(),
            text: toast_text(callsign, v),
            terminal_shown: terminal,
            since: now,
        }
    }

    /// Apply a newer snapshot for this toast's contact. A terminal
    /// transition is shown once, restarting the timer; nonterminal
    /// rewordings replace the text in place without restarting it, and
    /// duplicate/pending snapshots change nothing.
    pub fn apply_status(&mut self, status: &QsoUploadStatus, now: DateTime<Utc>) {
        if status.uuid != self.uuid {
            return;
        }
        let v = verdict(status);
        match v {
            UploadVerdict::AllAccepted | UploadVerdict::NeedsAttention => {
                let text = toast_text(&self.callsign, v);
                if self.text != text {
                    // Reword an active toast in place. A terminal result
                    // arriving after expiry gets one fresh display interval.
                    if self.text_at(now).is_none() {
                        self.since = now;
                    }
                    self.text = text;
                    self.terminal_shown = true;
                }
            }
            UploadVerdict::Disabled | UploadVerdict::Unknown => {
                if !self.terminal_shown {
                    let text = toast_text(&self.callsign, v);
                    if self.text != text {
                        self.text = text;
                    }
                }
            }
            UploadVerdict::Queued => {
                if !self.terminal_shown {
                    self.text = toast_text(&self.callsign, v);
                }
            }
        }
    }

    /// The visible text, or `None` once expired.
    pub fn text_at(&self, now: DateTime<Utc>) -> Option<&str> {
        if now.signed_duration_since(self.since).num_seconds() >= TOAST_SECS {
            None
        } else {
            Some(self.text.as_str())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(
        uuid: &str,
        call: &str,
        revision: u64,
        targets: Vec<(UploadTarget, TargetUploadStatus)>,
    ) -> QsoUploadStatus {
        QsoUploadStatus {
            uuid: uuid.to_string(),
            callsign: call.to_string(),
            revision,
            targets,
        }
    }

    fn mixed() -> QsoUploadStatus {
        status(
            "u-1",
            "K1ABC",
            3,
            vec![
                (
                    UploadTarget::Qrz,
                    TargetUploadStatus::new(UploadState::Accepted, "accepted"),
                ),
                (
                    UploadTarget::ClubLog,
                    TargetUploadStatus::pending("retrying \u{b7} temporary service failure"),
                ),
                (
                    UploadTarget::Eqsl,
                    TargetUploadStatus::new(
                        UploadState::Rejected,
                        "record rejected \u{b7} invalid record",
                    ),
                ),
            ],
        )
    }

    /// AC1/AC2: the glyph cells and the toast wording truth table.
    #[test]
    fn pan344_toast_truth_table() {
        let m = mixed();
        assert_eq!(glyph_cells(&m), "Q\u{2713} C~ E\u{d7}");
        assert_eq!(verdict(&m), UploadVerdict::NeedsAttention);
        assert_eq!(
            toast_text("K1ABC", UploadVerdict::Queued),
            "K1ABC logged \u{b7} uploads queued"
        );
        assert_eq!(
            toast_text("K1ABC", UploadVerdict::AllAccepted),
            "K1ABC uploaded"
        );
        assert_eq!(
            toast_text("K1ABC", UploadVerdict::NeedsAttention),
            "K1ABC logged \u{b7} uploads need attention"
        );
        assert_eq!(
            toast_text("K1ABC", UploadVerdict::Disabled),
            "K1ABC logged \u{b7} uploads disabled"
        );
        assert_eq!(
            toast_text("K1ABC", UploadVerdict::Unknown),
            "K1ABC logged \u{b7} upload state unknown"
        );

        // All-accepted requires a NONEMPTY captured set of entirely accepted
        // targets (duplicates count as acceptance).
        let all = status(
            "u-2",
            "K1ABC",
            1,
            vec![
                (
                    UploadTarget::Qrz,
                    TargetUploadStatus::new(
                        UploadState::Accepted,
                        "duplicate \u{2014} already logged",
                    ),
                ),
                (
                    UploadTarget::ClubLog,
                    TargetUploadStatus::new(UploadState::Accepted, "accepted"),
                ),
            ],
        );
        assert_eq!(verdict(&all), UploadVerdict::AllAccepted);

        // An empty captured set is disabled, never all-accepted.
        let none = status("u-3", "K1ABC", 0, Vec::new());
        assert_eq!(verdict(&none), UploadVerdict::Disabled);

        // Accepted + pending (nothing terminal, nothing unknown) stays queued.
        let partial = status(
            "u-4",
            "K1ABC",
            0,
            vec![
                (
                    UploadTarget::Qrz,
                    TargetUploadStatus::new(UploadState::Accepted, "accepted"),
                ),
                (UploadTarget::Lotw, TargetUploadStatus::pending("queued")),
            ],
        );
        assert_eq!(verdict(&partial), UploadVerdict::Queued);

        // A known target with a missing result is unknown, not accepted.
        let unknown = status(
            "u-5",
            "K1ABC",
            0,
            vec![
                (
                    UploadTarget::Qrz,
                    TargetUploadStatus::new(UploadState::Accepted, "accepted"),
                ),
                (
                    UploadTarget::Eqsl,
                    TargetUploadStatus::new(UploadState::Unknown, "result unavailable"),
                ),
            ],
        );
        assert_eq!(verdict(&unknown), UploadVerdict::Unknown);

        // A local failure needs attention even while other targets wait.
        let failed = status(
            "u-6",
            "K1ABC",
            0,
            vec![
                (
                    UploadTarget::Cqdx,
                    TargetUploadStatus::new(UploadState::Failed, "upload queue full"),
                ),
                (UploadTarget::Lotw, TargetUploadStatus::pending("queued")),
            ],
        );
        assert_eq!(verdict(&failed), UploadVerdict::NeedsAttention);

        // Reasons are sanitized: control characters stripped, clipped with …
        let dirty = TargetUploadStatus::pending(&format!("bad{}reason", '\u{1b}'));
        assert_eq!(dirty.reason, "badreason");
        let long = sanitize_reason(&"x".repeat(100));
        assert!(long.chars().count() == REASON_LIMIT);
        assert!(long.ends_with('\u{2026}'));
    }

    /// AC8: toast timing, replacement and deduplication with injected times.
    #[test]
    fn pan344_toast_lifecycle() {
        let t0 = Utc::now();
        let mut toast = CompletionToast::start("u-1", "K1ABC", None, t0);
        assert_eq!(toast.text, "K1ABC logged \u{b7} uploads queued");
        assert_eq!(
            toast.text_at(t0),
            Some("K1ABC logged \u{b7} uploads queued")
        );

        // Duplicate/pending snapshots neither restart the timer nor repeat.
        let pending = status(
            "u-1",
            "K1ABC",
            1,
            vec![(UploadTarget::Qrz, TargetUploadStatus::pending("queued"))],
        );
        toast.apply_status(&pending, t0 + chrono::Duration::seconds(2));
        assert_eq!(toast.since, t0);
        assert!(!toast.terminal_shown);

        // Expiry hides the text.
        assert_eq!(toast.text_at(t0 + chrono::Duration::seconds(6)), None);

        // A terminal transition arriving after expiry shows once, for six
        // seconds from the transition.
        let accepted = status(
            "u-1",
            "K1ABC",
            2,
            vec![(
                UploadTarget::Qrz,
                TargetUploadStatus::new(UploadState::Accepted, "accepted"),
            )],
        );
        toast.apply_status(&accepted, t0 + chrono::Duration::seconds(7));
        assert_eq!(toast.text, "K1ABC uploaded");
        assert_eq!(
            toast.text_at(t0 + chrono::Duration::seconds(7)),
            Some("K1ABC uploaded")
        );
        assert_eq!(toast.text_at(t0 + chrono::Duration::seconds(13)), None);

        // A later regression snapshot cannot rewrite a shown terminal text.
        toast.apply_status(&pending, t0 + chrono::Duration::seconds(8));
        assert_eq!(toast.text, "K1ABC uploaded");

        // A snapshot for another contact never steals the toast.
        let other = status(
            "u-9",
            "W9XYZ",
            5,
            vec![(
                UploadTarget::Qrz,
                TargetUploadStatus::new(UploadState::Accepted, "accepted"),
            )],
        );
        toast.apply_status(&other, t0 + chrono::Duration::seconds(9));
        assert_eq!(toast.text, "K1ABC uploaded");

        // A needs-attention transition replaces the queued wording in place.
        let mut toast2 = CompletionToast::start("u-2", "W2XYZ", None, t0);
        let rejected = status(
            "u-2",
            "W2XYZ",
            1,
            vec![(
                UploadTarget::Eqsl,
                TargetUploadStatus::new(UploadState::Rejected, "record rejected"),
            )],
        );
        toast2.apply_status(&rejected, t0 + chrono::Duration::seconds(1));
        assert_eq!(toast2.text, "W2XYZ logged \u{b7} uploads need attention");
        assert_eq!(toast2.since, t0);
        let mut recovered = rejected.clone();
        recovered.targets[0].1 = TargetUploadStatus::new(UploadState::Accepted, "");
        toast2.apply_status(&recovered, t0 + chrono::Duration::seconds(8));
        assert_eq!(toast2.text, "W2XYZ uploaded");
        assert_eq!(toast2.since, t0 + chrono::Duration::seconds(8));
        toast2.apply_status(&recovered, t0 + chrono::Duration::seconds(9));
        assert_eq!(toast2.since, t0 + chrono::Duration::seconds(8));

        // A confirmed empty target set rewords to disabled, in place.
        let mut toast3 = CompletionToast::start("u-3", "N3ABC", None, t0);
        let disabled = status("u-3", "N3ABC", 1, Vec::new());
        toast3.apply_status(&disabled, t0 + chrono::Duration::seconds(1));
        assert_eq!(toast3.text, "N3ABC logged \u{b7} uploads disabled");
        assert_eq!(toast3.since, t0);
    }
}
