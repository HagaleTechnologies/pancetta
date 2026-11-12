//! Bounded transient feedback, independent of readiness alerts.
use pancetta_core::DiagnosticLevel;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    Local,
    Relay,
}
#[derive(Debug, Clone)]
pub(crate) struct StatusEntry {
    pub text: String,
    pub level: DiagnosticLevel,
    origin: Origin,
    code: Option<u32>,
    alert: Option<u64>,
    sequence: u64,
    since: Instant,
}
fn rank(level: DiagnosticLevel) -> u8 {
    match level {
        DiagnosticLevel::Info => 0,
        DiagnosticLevel::Warn => 1,
        DiagnosticLevel::Error => 2,
    }
}
impl StatusEntry {
    fn active(&self, now: Instant) -> bool {
        let seconds = match self.level {
            DiagnosticLevel::Info => 5,
            DiagnosticLevel::Warn => 15,
            DiagnosticLevel::Error => 30,
        };
        now.saturating_duration_since(self.since) < Duration::from_secs(seconds)
    }
}
#[derive(Debug, Default)]
pub(crate) struct StatusMessages {
    entries: VecDeque<StatusEntry>,
    sequence: u64,
}
impl StatusMessages {
    pub fn push(
        &mut self,
        text: String,
        level: DiagnosticLevel,
        origin: Origin,
        code: Option<u32>,
        alert: Option<u64>,
        now: Instant,
    ) {
        self.prune(now);
        self.sequence += 1;
        if let Some(entry) = self.entries.iter_mut().find(|e| {
            e.text == text
                && e.level == level
                && e.origin == origin
                && e.code == code
                && e.alert == alert
        }) {
            entry.since = now;
            entry.sequence = self.sequence;
            return;
        }
        self.entries.push_back(StatusEntry {
            text,
            level,
            origin,
            code,
            alert,
            sequence: self.sequence,
            since: now,
        });
        if self.entries.len() > 8 {
            let index = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| (rank(e.level), e.sequence))
                .unwrap()
                .0;
            self.entries.remove(index);
        }
    }
    pub fn selected(&self, now: Instant) -> Option<&StatusEntry> {
        let selected = self
            .entries
            .iter()
            .filter(|e| e.active(now))
            .max_by_key(|e| (rank(e.level), e.sequence))?;
        self.entries
            .iter()
            .filter(|e| {
                e.active(now) && e.origin == Origin::Local && e.sequence > selected.sequence
            })
            .max_by_key(|e| e.sequence)
            .or(Some(selected))
    }
    pub fn prune(&mut self, now: Instant) {
        self.entries.retain(|e| e.active(now));
    }
    pub fn resolve(&mut self, code: u32) -> usize {
        let before = self.entries.len();
        self.entries.retain(|e| e.code != Some(code));
        before - self.entries.len()
    }
    pub fn acknowledge(&mut self, id: u64) {
        self.entries.retain(|e| e.alert != Some(id));
    }
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    #[cfg(test)]
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use DiagnosticLevel::*;
    fn push(
        s: &mut StatusMessages,
        text: &str,
        level: DiagnosticLevel,
        origin: Origin,
        now: Instant,
    ) {
        s.push(text.into(), level, origin, None, None, now);
    }
    #[test]
    fn pan351_ttl_boundaries() {
        for (level, ttl) in [(Info, 5), (Warn, 15), (Error, 30)] {
            let now = Instant::now();
            let mut s = StatusMessages::default();
            push(&mut s, "Decoded: 14 messages", level, Origin::Relay, now);
            assert!(s.selected(now + Duration::from_secs(ttl - 1)).is_some());
            assert!(s.selected(now + Duration::from_secs(ttl)).is_none());
            s.prune(now + Duration::from_secs(ttl));
            assert_eq!(s.len(), 0);
        }
    }
    #[test]
    fn pan351_priority_and_local_feedback() {
        let now = Instant::now();
        let mut s = StatusMessages::default();
        push(&mut s, "warn", Warn, Origin::Relay, now);
        push(&mut s, "error", Error, Origin::Relay, now);
        push(&mut s, "info", Info, Origin::Relay, now);
        assert_eq!(s.selected(now).unwrap().text, "error");
        push(&mut s, "key", Info, Origin::Local, now);
        assert_eq!(s.selected(now).unwrap().text, "key");
        assert_eq!(
            s.selected(now + Duration::from_secs(5)).unwrap().text,
            "error"
        );
        push(&mut s, "new error", Error, Origin::Relay, now);
        assert_eq!(s.selected(now).unwrap().text, "new error");
        push(&mut s, "newest error", Error, Origin::Relay, now);
        assert_eq!(s.selected(now).unwrap().text, "newest error");
    }
    #[test]
    fn pan351_bounded_deduplicated_statuses() {
        let now = Instant::now();
        let mut s = StatusMessages::default();
        push(&mut s, "repeat", Info, Origin::Relay, now);
        push(
            &mut s,
            "repeat",
            Info,
            Origin::Relay,
            now + Duration::from_secs(4),
        );
        assert_eq!(s.len(), 1);
        assert!(s.selected(now + Duration::from_secs(5)).is_some());
        assert!(s.selected(now + Duration::from_secs(9)).is_none());
        for i in 0..8 {
            push(&mut s, &i.to_string(), Error, Origin::Relay, now);
        }
        for i in 0..28 {
            push(&mut s, &i.to_string(), Info, Origin::Relay, now);
            assert_eq!(s.len(), 8);
        }
        assert!(s.entries.iter().all(|e| e.level == Error));
    }
}
