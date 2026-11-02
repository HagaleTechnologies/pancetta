//! Station-wide TX halt shared by the operator's emergency stop (Shift+Q)
//! and the SWR protection trip (PAN-251). A per-frame abort is not a halt:
//! the QSO engine would re-key the same frame next slot, so both callers
//! also cancel every QSO and leave the global policy Disabled.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::message_bus::{
    ComponentId, ComponentMessage, DurableDelivery, MessageBus, MessageType, QsoMessage,
};

#[derive(Clone)]
pub(crate) struct TxHaltHandles {
    pub(crate) abort_current_tx: Arc<AtomicBool>,
    pub(crate) autonomous_enabled: Arc<AtomicBool>,
    pub(crate) tx_policy: Arc<AtomicU8>,
}

impl TxHaltHandles {
    /// The gate-closing part. No await, so nothing suspends between the
    /// decision to halt and the policy store every PTT-on gate reads.
    pub(crate) fn halt(&self) {
        self.abort_current_tx.store(true, Ordering::Release);
        self.autonomous_enabled.store(false, Ordering::Release);
        self.tx_policy
            .store(pancetta_core::TxPolicy::Disabled.as_u8(), Ordering::Release);
    }
}

/// Clear the TX source, not just mute it: without this, returning the
/// policy to Full would resume every QSO. Durable, so a full QSO channel
/// parks the cancel instead of dropping it.
pub(crate) async fn cancel_all_qsos(bus: &MessageBus, source: ComponentId) -> DurableDelivery {
    bus.send_message_durable(ComponentMessage::new(
        source,
        ComponentId::Qso,
        MessageType::QsoMessage(QsoMessage::CancelAllQsos),
        Instant::now(),
    ))
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU32;

    fn handles() -> TxHaltHandles {
        TxHaltHandles {
            abort_current_tx: Arc::new(AtomicBool::new(false)),
            autonomous_enabled: Arc::new(AtomicBool::new(true)),
            tx_policy: Arc::new(AtomicU8::new(pancetta_core::TxPolicy::Full.as_u8())),
        }
    }

    fn assert_halted(h: &TxHaltHandles) {
        assert!(h.abort_current_tx.load(Ordering::Acquire));
        assert!(!h.autonomous_enabled.load(Ordering::Acquire));
        assert_eq!(
            pancetta_core::TxPolicy::from_u8(h.tx_policy.load(Ordering::Acquire)),
            pancetta_core::TxPolicy::Disabled
        );
    }

    #[test]
    fn halt_aborts_disables_autonomous_and_sets_policy_disabled() {
        let h = handles();
        h.halt();
        assert_halted(&h);
    }

    #[test]
    fn halt_is_idempotent() {
        let h = handles();
        h.halt();
        h.halt();
        assert_halted(&h);
    }

    #[test]
    fn after_halt_the_hard_mute_refuses_key_down() {
        let h = handles();
        let restart_inhibit = Arc::new(AtomicU32::new(0));
        let loop_ready = Arc::new(AtomicBool::new(true));
        let pending_frequency: Arc<
            std::sync::Mutex<HashMap<pancetta_hamlib::Vfo, ComponentMessage>>,
        > = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let pending_split: Arc<std::sync::Mutex<Option<ComponentMessage>>> =
            Arc::new(std::sync::Mutex::new(None));
        let in_flight = Arc::new(AtomicU32::new(0));
        let rig_conn_state = Arc::new(AtomicU8::new(
            super::super::hamlib::RigConnState::Connected.as_u8(),
        ));
        let audio_alive = Arc::new(AtomicBool::new(true));
        let reason = || {
            super::super::tx::tx_hard_mute_reason(
                &h.tx_policy,
                &restart_inhibit,
                &loop_ready,
                &pending_frequency,
                &pending_split,
                &in_flight,
                &rig_conn_state,
                &audio_alive,
                &Arc::default(),
            )
        };
        assert_eq!(reason(), None);
        h.halt();
        assert_eq!(reason(), Some("TX policy is Disabled"));
    }

    #[tokio::test]
    async fn cancel_all_qsos_reaches_the_qso_inbox() {
        let bus = MessageBus::new(8).unwrap();
        let (_tx, rx) = bus.create_channel(ComponentId::Qso).await.unwrap();
        assert_eq!(
            cancel_all_qsos(&bus, ComponentId::Hamlib).await,
            DurableDelivery::Enqueued
        );
        let msg = rx
            .try_recv()
            .expect("CancelAllQsos must be in the Qso inbox");
        assert_eq!(msg.source, ComponentId::Hamlib);
        assert!(matches!(
            msg.message_type,
            MessageType::QsoMessage(QsoMessage::CancelAllQsos)
        ));
        assert!(rx.try_recv().is_err());
    }
}
