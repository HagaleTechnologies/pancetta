//! PAN-344: coordinator-side registry of explicit per-QSO upload results.
//!
//! The outbox deletes an accepted entry and publishes only aggregate
//! destination health, so neither a missing queue row nor a health count
//! can prove that a particular contact was accepted. This registry retains
//! the latest explicit results by QSO UUID and target, bounded to the last
//! [`CAPACITY`] completion registrations, and the TUI relay snapshots it on
//! its existing two-second health tick.
//!
//! It is deliberately separate from `UploadHealth`, which keeps its
//! existing aggregate readiness contract. No payloads, credentials or URLs
//! enter here — only UUID, callsign, the captured enabled target set, a
//! per-contact revision and display-safe reasons.

use pancetta_tui::upload_status::{
    sort_targets, QsoUploadStatus, TargetUploadStatus, UploadState, UploadTarget,
};

/// Map an outbox destination to its display target. LoTW never goes through
/// the outbox (it is a separate one-shot path), so it has no mapping here.
pub fn upload_target_for_destination(
    destination: pancetta_dx::upload_outbox::UploadDestination,
) -> UploadTarget {
    use pancetta_dx::upload_outbox::UploadDestination::*;
    match destination {
        ClubLog => UploadTarget::ClubLog,
        QrzLogbook => UploadTarget::Qrz,
        Eqsl => UploadTarget::Eqsl,
        Cqdx => UploadTarget::Cqdx,
    }
}

/// How many completion registrations to retain, by registration order.
pub const CAPACITY: usize = 100;

/// The shared handle the coordinator, upload subscriber and outbox worker
/// all clone.
pub type SharedUploadStatusRegistry = std::sync::Arc<std::sync::Mutex<UploadStatusRegistry>>;

/// Bounded registry of per-QSO upload statuses.
#[derive(Debug)]
pub struct UploadStatusRegistry {
    /// Latest registration order; oldest evicted first.
    entries: Vec<QsoUploadStatus>,
    capacity: usize,
}

impl UploadStatusRegistry {
    /// A fresh shared registry. A restart starts empty: existing pending
    /// uploads keep retrying via the outbox, but no prior Recent-QSOs
    /// history (and no accepted claim) is invented.
    pub fn shared() -> SharedUploadStatusRegistry {
        std::sync::Arc::new(std::sync::Mutex::new(Self::with_capacity(CAPACITY)))
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Vec::new(),
            capacity,
        }
    }

    /// Register a completion's complete enabled target set (typically all
    /// `Pending`). Later config changes do not rewrite a contact's
    /// denominator. Re-registering an existing UUID is a no-op.
    pub fn register(
        &mut self,
        uuid: &str,
        callsign: &str,
        mut targets: Vec<(UploadTarget, TargetUploadStatus)>,
    ) {
        if self.entries.iter().any(|e| e.uuid == uuid) {
            return;
        }
        sort_targets(&mut targets);
        self.entries.push(QsoUploadStatus {
            uuid: uuid.to_string(),
            callsign: callsign.to_string(),
            revision: 0,
            targets,
        });
        while self.entries.len() > self.capacity {
            self.entries.remove(0);
        }
    }

    /// Record one target's transition for a registered contact. Results for
    /// evicted (or never registered) UUIDs are ignored, never reinserted.
    /// An explicit acceptance is never regressed by an older snapshot.
    pub fn record(&mut self, uuid: &str, target: UploadTarget, state: UploadState, reason: &str) {
        let Some(entry) = self.entries.iter_mut().find(|e| e.uuid == uuid) else {
            return;
        };
        let Some((_, slot)) = entry.targets.iter_mut().find(|(t, _)| *t == target) else {
            return;
        };
        if slot.state == UploadState::Accepted && state != UploadState::Accepted {
            return;
        }
        slot.state = state;
        slot.reason = TargetUploadStatus::new(state, reason).reason;
        entry.revision += 1;
    }

    /// Mark every captured target of a registered contact (used for local
    /// preparation failures that stop all uploads).
    pub fn record_all(&mut self, uuid: &str, state: UploadState, reason: &str) {
        let Some(entry) = self.entries.iter().find(|e| e.uuid == uuid) else {
            return;
        };
        let targets: Vec<UploadTarget> = entry.targets.iter().map(|(t, _)| *t).collect();
        for target in targets {
            self.record(uuid, target, state, reason);
        }
    }

    /// The full bounded snapshot (cloned; called off the delivery paths).
    pub fn snapshot(&self) -> Vec<QsoUploadStatus> {
        self.entries.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pancetta_tui::upload_status::UploadState::*;

    #[test]
    fn pan344_retention_and_restart() {
        let mut registry = UploadStatusRegistry::with_capacity(3);
        assert!(
            registry.snapshot().is_empty(),
            "a fresh registry invents no history"
        );

        let pending = |n: usize| {
            (
                UploadTarget::Qrz,
                TargetUploadStatus::pending(&format!("queued {n}")),
            )
        };
        for i in 0..3 {
            registry.register(&format!("u-{i}"), &format!("K{i}ABC"), vec![pending(i)]);
        }
        assert_eq!(registry.snapshot().len(), 3);

        // Registration-order eviction: the oldest completion leaves first.
        registry.register("u-3", "K3ABC", vec![pending(3)]);
        assert_eq!(registry.snapshot().len(), 3);
        let snap = registry.snapshot();
        assert!(snap.iter().all(|e| e.uuid != "u-0"), "oldest evicted");
        assert!(snap.iter().any(|e| e.uuid == "u-3"));

        // Results for evicted IDs are ignored, never reinserted; they cannot
        // evict newer completions either.
        registry.record("u-0", UploadTarget::Qrz, Accepted, "accepted");
        let snap = registry.snapshot();
        assert_eq!(snap.len(), 3);
        assert!(snap.iter().all(|e| e.uuid != "u-0"));
        assert_eq!(
            snap.iter()
                .find(|e| e.uuid == "u-1")
                .unwrap()
                .state(UploadTarget::Qrz),
            Some(Pending)
        );

        // Explicit acceptance survives later (stale) non-acceptance updates.
        registry.record("u-1", UploadTarget::Qrz, Accepted, "accepted");
        registry.record("u-1", UploadTarget::Qrz, Pending, "queued");
        let entry = registry
            .snapshot()
            .into_iter()
            .find(|e| e.uuid == "u-1")
            .unwrap();
        assert_eq!(entry.state(UploadTarget::Qrz), Some(Accepted));
        assert!(
            entry.revision >= 1,
            "each recorded transition bumps the revision"
        );

        // Records for unknown targets and unregistered UUIDs are no-ops.
        registry.record("u-1", UploadTarget::Lotw, Accepted, "accepted");
        registry.record("missing", UploadTarget::Qrz, Accepted, "accepted");
        assert_eq!(registry.snapshot().len(), 3);

        // record_all marks every captured target (preparation failure).
        registry.record_all("u-2", Failed, "upload skipped: ADIF render failed");
        let entry = registry
            .snapshot()
            .into_iter()
            .find(|e| e.uuid == "u-2")
            .unwrap();
        assert_eq!(entry.state(UploadTarget::Qrz), Some(Failed));

        // A restart's registry is empty and stays empty without events.
        let restarted = UploadStatusRegistry::shared();
        assert!(restarted.lock().unwrap().snapshot().is_empty());
    }
}
