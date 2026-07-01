//! Append-only JSONL audit log for the pancetta station agent.
//!
//! Security posture (dispensa ADR-0002 §5 — operator attribution + audit):
//! every armed-TX state change, TX request/denial, local-kill toggle, and
//! local-consent change is recorded as one JSON object per line (JSONL) in a
//! durable append-only file. The log exists so an operator (or an auditor) can
//! reconstruct exactly what the remote path did and *who* it was attributed to.
//!
//! Design invariants:
//! - **Clock-injected.** [`AuditEvent::ts_unix_ms`] is supplied by the caller;
//!   this module never reads a wall clock. That keeps the audit layer pure and
//!   deterministically testable, and lets the state machine that produces the
//!   events own the single source of "now".
//! - **Never panics on IO error.** The station must not crash because its audit
//!   file is unwritable (full disk, read-only mount, path removed). On any IO
//!   failure [`AuditLog::append`] logs a `warn!` (target `agent.audit`) and
//!   returns. A missing audit line is a diagnostic loss, never a station fault.
//! - **Append-only.** The file is opened in append mode on every write; we never
//!   truncate or rewrite prior lines.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Category of an auditable agent event.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum AuditKind {
    /// The armed-TX state machine transitioned into the armed state.
    Armed,
    /// The armed-TX state machine transitioned out of the armed state
    /// (operator disarm, TTL expiry, or heartbeat loss).
    Disarmed,
    /// A remote TX was requested (attributed to an operator callsign).
    TxRequested,
    /// A remote TX was denied by the safety gate (with a reason in `detail`).
    TxDenied,
    /// The station-local kill switch was engaged or cleared.
    LocalKill,
    /// The station-local consent gate (`remote_tx_enabled`) changed.
    LocalConsentChanged,
}

/// A single append-only audit record.
///
/// `ts_unix_ms` is injected by the caller — this type never reads a clock.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AuditEvent {
    /// Event timestamp in unix milliseconds, supplied by the caller.
    pub ts_unix_ms: i64,
    /// Category of the event.
    pub kind: AuditKind,
    /// Operator the event is attributed to, if any.
    pub operator_callsign: Option<String>,
    /// Free-form human-readable detail (e.g. a denial reason).
    pub detail: String,
}

/// An append-only JSONL audit log backed by a file on disk.
///
/// Cheap to clone/hold; the file handle is opened per-append so concurrent
/// holders do not share a mutable handle.
#[derive(Clone, Debug)]
pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    /// Create an audit log that appends to `path`.
    ///
    /// The path is not opened or created until the first [`append`](Self::append);
    /// tests inject a tempfile path here.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The path this log appends to.
    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Append one event as a single JSON line (`{...}\n`).
    ///
    /// Opens the file in append mode, writes the serialized event plus a
    /// newline, and flushes. **Never panics**: on serialization or IO error it
    /// logs a `warn!` (target `agent.audit`) and returns. The audit failing must
    /// not crash the station.
    pub fn append(&self, ev: &AuditEvent) {
        let mut line = match serde_json::to_string(ev) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    target: "agent.audit",
                    error = %e,
                    "failed to serialize audit event; dropping"
                );
                return;
            }
        };
        line.push('\n');

        let mut file = match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(
                    target: "agent.audit",
                    error = %e,
                    path = %self.path.display(),
                    "failed to open audit log for append; dropping event"
                );
                return;
            }
        };

        if let Err(e) = file.write_all(line.as_bytes()) {
            tracing::warn!(
                target: "agent.audit",
                error = %e,
                path = %self.path.display(),
                "failed to write audit event; dropping"
            );
            return;
        }
        if let Err(e) = file.flush() {
            tracing::warn!(
                target: "agent.audit",
                error = %e,
                path = %self.path.display(),
                "failed to flush audit log"
            );
        }
    }
}

/// The default production audit-log path: `~/.pancetta/agent-audit.log`.
///
/// Falls back to `$HOME/.pancetta/...`, then to the current directory if no
/// home can be resolved (audit should still land *somewhere* rather than error).
pub fn default_audit_path() -> PathBuf {
    let home = dirs::home_dir()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".pancetta").join("agent-audit.log")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::sync::atomic::{AtomicU64, Ordering};

    // Deterministic unique suffix for temp files — NOT a clock, NOT random.
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A tempfile path with automatic cleanup on drop.
    struct TempPath(PathBuf);

    impl TempPath {
        fn new(tag: &str) -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let path =
                std::env::temp_dir().join(format!("pancetta-agent-audit-test-{tag}-{pid}-{n}.log"));
            // Ensure a clean slate if a prior run left the file behind.
            let _ = std::fs::remove_file(&path);
            Self(path)
        }
        fn path(&self) -> &PathBuf {
            &self.0
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn ev(ts: i64, kind: AuditKind, op: Option<&str>, detail: &str) -> AuditEvent {
        AuditEvent {
            ts_unix_ms: ts,
            kind,
            operator_callsign: op.map(str::to_string),
            detail: detail.to_string(),
        }
    }

    fn read_lines(path: &PathBuf) -> Vec<String> {
        let f = std::fs::File::open(path).expect("open audit file");
        BufReader::new(f)
            .lines()
            .map(|l| l.expect("read line"))
            .collect()
    }

    #[test]
    fn appends_n_events_as_n_parseable_lines_in_order() {
        let tp = TempPath::new("order");
        let log = AuditLog::new(tp.path().clone());

        let events = vec![
            ev(1000, AuditKind::Armed, Some("K5ARH"), "armed"),
            ev(2000, AuditKind::TxRequested, Some("K5ARH"), "tx#1"),
            ev(3000, AuditKind::TxDenied, Some("K5ARH"), "heartbeat lost"),
            ev(4000, AuditKind::Disarmed, Some("K5ARH"), "ttl expired"),
        ];
        for e in &events {
            log.append(e);
        }

        let lines = read_lines(tp.path());
        assert_eq!(lines.len(), events.len(), "one line per event");

        for (line, expected) in lines.iter().zip(events.iter()) {
            let parsed: AuditEvent = serde_json::from_str(line).expect("parse line");
            assert_eq!(&parsed, expected, "line parses back to the exact event");
        }
        // Order preserved.
        let parsed: Vec<AuditEvent> = lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(parsed, events);
    }

    #[test]
    fn each_line_is_a_single_json_object_with_no_embedded_newline() {
        let tp = TempPath::new("single-line");
        let log = AuditLog::new(tp.path().clone());
        log.append(&ev(
            42,
            AuditKind::LocalConsentChanged,
            None,
            "multi\nword\tdetail with spaces",
        ));
        let bytes = std::fs::read(tp.path()).unwrap();
        // Exactly one trailing newline, none in the middle.
        assert_eq!(bytes.iter().filter(|&&b| b == b'\n').count(), 1);
        assert_eq!(*bytes.last().unwrap(), b'\n');
    }

    #[test]
    fn round_trip_preserves_all_fields_including_none_operator() {
        let tp = TempPath::new("roundtrip");
        let log = AuditLog::new(tp.path().clone());
        let with_op = ev(123, AuditKind::LocalKill, Some("W1XYZ"), "kill engaged");
        let no_op = ev(456, AuditKind::TxDenied, None, "not armed");
        log.append(&with_op);
        log.append(&no_op);

        let lines = read_lines(tp.path());
        let a: AuditEvent = serde_json::from_str(&lines[0]).unwrap();
        let b: AuditEvent = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(a, with_op);
        assert_eq!(b, no_op);
        assert_eq!(b.operator_callsign, None);
    }

    #[test]
    fn every_kind_serializes_and_round_trips() {
        let tp = TempPath::new("kinds");
        let log = AuditLog::new(tp.path().clone());
        let kinds = [
            AuditKind::Armed,
            AuditKind::Disarmed,
            AuditKind::TxRequested,
            AuditKind::TxDenied,
            AuditKind::LocalKill,
            AuditKind::LocalConsentChanged,
        ];
        for (i, k) in kinds.iter().enumerate() {
            log.append(&ev(i as i64, k.clone(), None, "d"));
        }
        let lines = read_lines(tp.path());
        assert_eq!(lines.len(), kinds.len());
        for (line, k) in lines.iter().zip(kinds.iter()) {
            let parsed: AuditEvent = serde_json::from_str(line).unwrap();
            assert_eq!(&parsed.kind, k);
        }
    }

    #[test]
    fn append_does_not_panic_when_path_parent_is_a_file() {
        // Create a regular file, then treat it as a *directory* in the path —
        // opening "<file>/child.log" must fail at the IO layer, not panic.
        let tp = TempPath::new("parent-is-file");
        std::fs::write(tp.path(), b"i am a file, not a directory").unwrap();
        let unwritable = tp.path().join("child.log");
        let log = AuditLog::new(unwritable);
        // Must not panic.
        log.append(&ev(1, AuditKind::Armed, None, "should be dropped"));
        log.append(&ev(2, AuditKind::Disarmed, None, "also dropped"));
    }

    #[test]
    fn append_does_not_panic_on_nonexistent_deep_parent() {
        // A path whose parent directory does not exist: create(true) cannot
        // create intermediate dirs, so open fails — must be swallowed.
        let tp = TempPath::new("deep");
        let missing = tp.path().join("nope").join("still-nope").join("audit.log");
        let log = AuditLog::new(missing);
        log.append(&ev(1, AuditKind::TxDenied, None, "dropped, no panic"));
    }

    #[test]
    fn default_audit_path_ends_with_expected_suffix() {
        let p = default_audit_path();
        assert!(p.ends_with("agent-audit.log"), "path = {}", p.display());
        assert!(
            p.to_string_lossy().contains(".pancetta"),
            "path = {}",
            p.display()
        );
    }
}
