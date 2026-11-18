//! PAN-385: config mapping for the autonomous band scheduler and the hop
//! executor that carries out an operator `ChangeBand`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use pancetta_core::utc_window::UtcWindow;
use pancetta_core::Band;
use tracing::{info, warn};

use crate::message_bus::{
    ComponentId, ComponentMessage, MessageBus, MessageType, QsoMessage, RigControlMessage,
};

#[cfg(test)]
#[path = "band_hop/tests/config_tests.rs"]
mod config_tests;
#[cfg(test)]
#[path = "band_hop/tests/execute_tests.rs"]
mod execute_tests;

/// Distinct bands of `autonomous.band_plan` and `band_hopping.bands`
/// (classified by dial), in first-seen order: the opportunity poll's
/// candidate set.
pub(crate) fn opportunity_candidate_bands(
    config: &pancetta_config::autonomous::AutonomousConfig,
) -> Vec<Band> {
    let mut out: Vec<Band> = Vec::new();
    let plan_bands = config
        .band_plan
        .iter()
        .flat_map(|w| w.bands.iter())
        .filter_map(|name| name.parse::<Band>().ok());
    let list_bands = config
        .band_hopping
        .bands
        .iter()
        .filter_map(|b| Band::from_frequency(b.dial_frequency));
    for band in plan_bands.chain(list_bands) {
        if !out.contains(&band) {
            out.push(band);
        }
    }
    out
}

/// The operator's band-hopping config from `[autonomous.band_hopping]` and
/// `[[autonomous.band_plan]]`. A plan window whose `utc` or band names do not
/// parse is skipped with a warning (config validation rejects both, so this
/// only guards a config that bypassed validation).
pub(crate) fn qso_band_hopping_config(
    config: &pancetta_config::autonomous::AutonomousConfig,
) -> pancetta_qso::autonomous::BandHoppingConfig {
    let hopping = &config.band_hopping;
    let plan = config
        .band_plan
        .iter()
        .filter_map(|window| {
            let parsed = match window.utc.parse::<UtcWindow>() {
                Ok(parsed) => parsed,
                Err(e) => {
                    warn!(
                        "autonomous.band_plan: skipping window {:?}: {}",
                        window.utc, e
                    );
                    return None;
                }
            };
            let mut bands = Vec::with_capacity(window.bands.len());
            for name in &window.bands {
                match name.parse::<Band>() {
                    Ok(band) => bands.push(band),
                    Err(_) => {
                        warn!(
                            "autonomous.band_plan: skipping window {:?}: unknown band {:?}",
                            window.utc, name
                        );
                        return None;
                    }
                }
            }
            Some(pancetta_qso::BandPlanEntry {
                window: parsed,
                bands,
            })
        })
        .collect();
    pancetta_qso::autonomous::BandHoppingConfig {
        enabled: hopping.enabled,
        hop_threshold: hopping.hop_threshold,
        bands: hopping
            .bands
            .iter()
            .map(|b| pancetta_qso::autonomous::BandEntry {
                dial_frequency: b.dial_frequency,
                band_name: b.band_name.clone(),
                priority: b.priority,
            })
            .collect(),
        plan,
        needed_activity: hopping.needed_activity,
        needed_idle_secs: hopping.needed_idle_secs,
        grey_line: hopping.grey_line,
    }
}

/// Shared state the hop executor reads and writes. Built once by the
/// Autonomous task.
#[derive(Clone)]
pub(crate) struct BandHopCtx {
    pub bus: MessageBus,
    /// The shared dial atomic.
    pub operating_frequency_hz: Arc<AtomicU64>,
    /// C9 dedup anchor: pancetta commanded this dial.
    pub last_freq_command: Arc<std::sync::Mutex<Option<(u64, Instant)>>>,
    /// Split TX dial (0 = simplex); a band change clears it.
    pub split_tx_hz: Arc<AtomicU64>,
    /// Remote gateway display-feed gate for the split relay.
    pub display_feed_enabled: Arc<AtomicBool>,
    /// The TX worker's keyed flag.
    pub ptt_active: Arc<AtomicBool>,
    /// Every key-down passes this gate before its PTT-on enqueue.
    pub ptt_sync_gate: Arc<tokio::sync::Mutex<()>>,
    /// FT4 mode: hop to the band's FT4 dial instead of the configured FT8 one.
    pub active_is_ft4: bool,
}

/// What `execute_band_hop` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BandHopOutcome {
    /// The dial moved and `SetFrequency` went to Hamlib.
    Moved { previous_hz: u64, dial_hz: u64 },
    /// Nothing was sent; the text says why (Diagnostics overlay line).
    Waiting(String),
}

/// Carry out an operator `ChangeBand` (PAN-385, PAN-287 style). Waits,
/// sending nothing, while a partner has answered (`exchanges`) or while the
/// rig is keyed (rechecked under `ptt_sync_gate`). Otherwise, under the gate,
/// stores the dial, stamps the C9 anchor, tears down QSOs and clears split on
/// a band change, then sends `SetFrequency`, and reports why in one
/// Diagnostics line. Never locks the autonomous operator, so it is safe
/// under `ptt_sync_gate`.
pub(crate) async fn execute_band_hop(
    ctx: &BandHopCtx,
    dial_frequency: u64,
    reason: &pancetta_qso::BandHopReason,
    exchanges: &[String],
) -> BandHopOutcome {
    match exchanges {
        [] => {}
        [partner] => {
            return BandHopOutcome::Waiting(
                reason.waiting_text(&format!("QSO with {partner} in progress")),
            );
        }
        many => {
            return BandHopOutcome::Waiting(
                reason.waiting_text(&format!("{} QSOs in progress", many.len())),
            );
        }
    }
    // FT4-mode dial override: the band-hop config carries FT8 dials. In FT4
    // mode, resolve the hopped band and substitute its FT4 sub-band
    // frequency. If the band has no FT4 frequency (e.g. 60m/160m) or can't
    // be resolved, keep the configured (FT8) dial and warn. FT8 mode never
    // enters this branch (byte-identical).
    let dial_frequency = if ctx.active_is_ft4 {
        match Band::from_frequency(dial_frequency).and_then(|b| b.ft4_frequency()) {
            Some(ft4) => ft4,
            None => {
                warn!(
                    target: "operator.override",
                    "FT4 mode: no FT4 dial for band-hop target {} Hz; using configured (FT8) dial",
                    dial_frequency
                );
                dial_frequency
            }
        }
    } else {
        dial_frequency
    };
    let bus = &ctx.bus;
    let old_freq_hz = {
        // PAN-287: every key-down passes `ptt_sync_gate` before its PTT-on
        // enqueue, so a keyed recheck and the retune under the gate cannot
        // interleave with a key-down.
        let _gate = ctx.ptt_sync_gate.lock().await;
        if super::spot_call::rig_is_keyed(&ctx.ptt_active, bus) {
            return BandHopOutcome::Waiting(reason.waiting_text("rig is keyed"));
        }
        // C9 — the autonomous operator is changing band. An active QSO can't
        // complete on the new band, so tear active QSOs down (same mechanism
        // as the TUI SetFrequency path) before/at the band switch. Capture
        // the *old* dial freq, update the shared atomic, and stamp the dedup
        // anchor so the hamlib poll loop doesn't double-fire when it reads
        // the new freq back off the rig.
        let old_freq_hz = ctx.operating_frequency_hz.load(Ordering::Relaxed);
        ctx.operating_frequency_hz
            .store(dial_frequency, Ordering::Relaxed);
        if let Ok(mut anchor) = ctx.last_freq_command.lock() {
            *anchor = Some((dial_frequency, Instant::now()));
        }
        if super::is_band_change(old_freq_hz, dial_frequency) {
            info!(
                target: "operator.override",
                "Autonomous band change {} Hz -> {} Hz — tearing down active QSOs",
                old_freq_hz, dial_frequency
            );
            let teardown = ComponentMessage::new(
                ComponentId::Autonomous,
                ComponentId::Qso,
                MessageType::QsoMessage(QsoMessage::BandChanged {
                    previous_hz: old_freq_hz,
                    new_hz: dial_frequency,
                }),
                Instant::now(),
            );
            if let Err(e) = bus.send_message(teardown).await {
                warn!("Autonomous band change: failed to send teardown: {}", e);
            }
            // A band change invalidates any split TX freq.
            if ctx.split_tx_hz.load(Ordering::Relaxed) != 0 {
                let store = super::forward_split_change(
                    bus,
                    ComponentId::Autonomous,
                    &ctx.split_tx_hz,
                    false,
                    0,
                )
                .await;
                // Push authoritative split state to the TUI chip via the
                // message bus (Autonomous → Tui relay).
                let split_clr_tui = ComponentMessage::new(
                    ComponentId::Autonomous,
                    ComponentId::Tui,
                    MessageType::SplitStatus { tx_hz: store },
                    Instant::now(),
                );
                let _ = bus.send_message(split_clr_tui).await;
                super::remote_gateway::relay_to_gateway(
                    bus,
                    &ctx.display_feed_enabled,
                    ComponentId::Autonomous,
                    MessageType::SplitStatus { tx_hz: store },
                )
                .await;
            }
        }
        let msg = ComponentMessage::new(
            ComponentId::Autonomous,
            ComponentId::Hamlib,
            MessageType::RigControl(RigControlMessage::SetFrequency {
                vfo: 0,
                frequency: dial_frequency,
            }),
            Instant::now(),
        );
        if let Err(e) = bus.send_message(msg).await {
            warn!("Failed to send ChangeBand: {}", e);
        }
        old_freq_hz
    };
    super::tx::emit_diagnostic_full(
        bus,
        ComponentId::Autonomous,
        "qso.autonomous",
        pancetta_core::DiagnosticLevel::Info,
        reason.why_text(),
        None,
        None,
    )
    .await;
    BandHopOutcome::Moved {
        previous_hz: old_freq_hz,
        dial_hz: dial_frequency,
    }
}
