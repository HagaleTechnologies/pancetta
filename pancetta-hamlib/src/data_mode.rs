//! Exact CAT mode policy, independent of the station's waveform protocol.
use anyhow::{anyhow, Result};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
    Arc,
};

/// Shared fail-closed admission state for automatic rig mode preparation.
#[derive(Clone, Debug)]
pub struct DataModeReadiness {
    enabled: Arc<AtomicBool>,
    epoch: Arc<AtomicU64>,
    ready_epoch: Arc<AtomicU64>,
    restore_vfo: Arc<AtomicU8>,
}
impl DataModeReadiness {
    /// Enabled policies start blocked; disabled policies never inhibit TX.
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(enabled)),
            epoch: Arc::new(AtomicU64::new(1)),
            ready_epoch: Arc::new(AtomicU64::new(0)),
            restore_vfo: Arc::new(AtomicU8::new(0)),
        }
    }
    /// Reset the policy when the rig component starts or changes configuration.
    pub fn configure(&self, enabled: bool) {
        self.invalidate();
        self.enabled.store(enabled, Ordering::Release);
    }
    /// Whether the policy currently permits key-down.
    pub fn is_ready(&self) -> bool {
        !self.enabled.load(Ordering::Acquire) || self.is_ready_for(self.epoch())
    }
    /// Invalidate mode confirmation before a connection or rig mutation.
    pub fn invalidate(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.ready_epoch.store(0, Ordering::Release);
    }
    pub(crate) fn restore_target(&self) -> u8 {
        self.restore_vfo.load(Ordering::Acquire)
    }
    pub(crate) fn reserve_restore(&self, epoch: u64, target: u8) {
        if self.epoch() == epoch {
            self.restore_vfo.store(target, Ordering::Release);
        }
    }
    pub(crate) fn restored(&self, epoch: u64, target: u8) {
        if self.epoch() == epoch {
            let _ =
                self.restore_vfo
                    .compare_exchange(target, 0, Ordering::AcqRel, Ordering::Acquire);
        }
    }
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }
    pub(crate) fn is_ready_for(&self, epoch: u64) -> bool {
        self.epoch() == epoch
            && self.ready_epoch.load(Ordering::Acquire) == epoch
            && self.restore_target() == 0
    }
    pub(crate) fn invalidate_epoch(&self, epoch: u64) {
        let _ = self
            .ready_epoch
            .compare_exchange(epoch, 0, Ordering::AcqRel, Ordering::Acquire);
    }
    pub(crate) fn confirm(&self, epoch: u64) {
        // An old component may finish after a restart. Its epoch cannot make
        // the current component ready, even if it publishes after configure.
        if self.epoch() == epoch {
            self.ready_epoch.fetch_max(epoch, Ordering::AcqRel);
        }
    }
}

/// Restricted, exact CAT mode and passband to apply to receive and transmit VFOs.
#[derive(Clone, Debug)]
pub struct DataModePolicy {
    token: &'static str,
    width: i32,
}
impl DataModePolicy {
    /// Validate a CAT mode token and a nonnegative Hamlib passband.
    pub fn new(mode: &str, width: Option<u32>) -> Result<Self> {
        let token = match mode.to_ascii_uppercase().as_str() {
            "PKTUSB" => "PKTUSB",
            "PKTLSB" => "PKTLSB",
            "USB" => "USB",
            "LSB" => "LSB",
            _ => return Err(anyhow!("Invalid rig.data_mode")),
        };
        let width = i32::try_from(width.unwrap_or(0))
            .map_err(|_| anyhow!("Invalid rig.data_passband_hz"))?;
        Ok(Self { token, width })
    }
    pub(crate) fn command(&self) -> String {
        format!("\\set_mode {} {}", self.token, self.width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_component_cannot_publish_or_clear_readiness_after_restart() {
        let ready = DataModeReadiness::new(true);
        let old = ready.epoch();
        ready.confirm(old);
        assert!(ready.is_ready());
        ready.configure(true);
        let current = ready.epoch();
        ready.confirm(old);
        assert!(!ready.is_ready());
        ready.confirm(current);
        assert!(ready.is_ready());
        ready.invalidate_epoch(old);
        assert!(ready.is_ready());
        ready.invalidate();
        ready.confirm(current);
        assert!(!ready.is_ready());
        ready.configure(false);
        assert!(ready.is_ready());
    }
    #[test]
    fn exact_tokens_and_widths() {
        for token in ["PKTUSB", "PKTLSB", "USB", "LSB"] {
            assert_eq!(
                DataModePolicy::new(&token.to_ascii_lowercase(), Some(3000))
                    .unwrap()
                    .command(),
                format!("\\set_mode {token} 3000")
            );
        }
        assert_eq!(
            DataModePolicy::new("PKTUSB", None).unwrap().command(),
            "\\set_mode PKTUSB 0"
        );
        assert!(DataModePolicy::new("USB\nT 1", None).is_err());
        assert!(DataModePolicy::new("USB", Some(u32::MAX)).is_err());
    }
}
