use super::*;
use crate::coordinator::hamlib::RigConnState;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8};
use std::sync::{Arc, Mutex};

fn policy(p: pancetta_core::TxPolicy) -> Arc<AtomicU8> {
    Arc::new(AtomicU8::new(p.as_u8()))
}

/// An empty pending slot -- the common case (nothing carried over from
/// a prior failed teardown replay).
fn no_pending() -> Arc<Mutex<Option<ComponentMessage>>> {
    Arc::new(Mutex::new(None))
}

/// The frequency sibling of `no_pending()` -- PAN-35 keyed the
/// frequency pending slot by VFO, so its empty state is an empty map
/// rather than `None`.
fn no_pending_frequency() -> Arc<Mutex<HashMap<pancetta_hamlib::Vfo, ComponentMessage>>> {
    Arc::new(Mutex::new(HashMap::new()))
}

fn pending_split_msg() -> ComponentMessage {
    ComponentMessage::new(
        ComponentId::Hamlib,
        ComponentId::Hamlib,
        MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
            enabled: true,
            tx_frequency: 14_078_000,
        }),
        std::time::Instant::now(),
    )
}

/// Not in flight -- the common case (no CAT call currently executing).
fn not_in_flight() -> Arc<AtomicU32> {
    Arc::new(AtomicU32::new(0))
}

/// PAN-115: a `rig_conn_state` atomic holding `state`.
fn rig(state: RigConnState) -> Arc<AtomicU8> {
    Arc::new(AtomicU8::new(state.as_u8()))
}

/// PAN-115: an `audio_output_alive` flag.
fn audio_alive(alive: bool) -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(alive))
}

#[test]
fn permits_tx_when_everything_is_ready() {
    let policy = policy(pancetta_core::TxPolicy::Full);
    let restart_inhibit = Arc::new(AtomicU32::new(0));
    let hamlib_loop_ready = Arc::new(AtomicBool::new(true));
    assert_eq!(
        tx_hard_mute_reason(
            &policy,
            &restart_inhibit,
            &hamlib_loop_ready,
            &no_pending_frequency(),
            &no_pending(),
            &not_in_flight(),
            &rig(RigConnState::Connected),
            &audio_alive(true),
            &{
                let gain = super::super::receive_overload::GainSafety::default();
                gain.confirmed_off(0);
                std::sync::Arc::new(gain)
            },
            &pancetta_hamlib::DataModeReadiness::new(false),
        ),
        None
    );
}

#[test]
fn restart_inhibit_still_mutes_independent_of_loop_readiness() {
    let policy = policy(pancetta_core::TxPolicy::Full);
    let restart_inhibit = Arc::new(AtomicU32::new(1));
    let hamlib_loop_ready = Arc::new(AtomicBool::new(true));
    assert!(tx_hard_mute_reason(
        &policy,
        &restart_inhibit,
        &hamlib_loop_ready,
        &no_pending_frequency(),
        &no_pending(),
        &not_in_flight(),
        &rig(RigConnState::Connected),
        &audio_alive(true),
        &{
            let gain = super::super::receive_overload::GainSafety::default();
            gain.confirmed_off(0);
            std::sync::Arc::new(gain)
        },
        &pancetta_hamlib::DataModeReadiness::new(false),
    )
    .is_some());
}

/// PAN-19 round-7 review (Codex P1): the actual bug this guards
/// against. A `LoopReadyOutcome::TimedOut` startup still returns
/// `Ok(())` from `start_hamlib_component` (must not bail -- the
/// HIGH-fix invariant), which releases `TxInhibitGuard`
/// (`tx_restart_inhibit` back to 0) in `health.rs`. Simulate exactly
/// that end state -- `tx_restart_inhibit == 0` (already released) but
/// `hamlib_command_loop_ready == false` (never confirmed, because
/// `TimedOut` never sets it true) -- and confirm PTT is STILL refused
/// at this gate, independent of the restart-supervision counter having
/// already released.
#[test]
fn hamlib_loop_not_ready_mutes_tx_even_after_restart_inhibit_has_released() {
    let policy = policy(pancetta_core::TxPolicy::Full);
    // The exact post-`TimedOut` state: the restart-supervision inhibit
    // has ALREADY been released (0), simulating `TxInhibitGuard`
    // having dropped normally once `start_hamlib_component` returned
    // `Ok(())` on a timeout.
    let restart_inhibit = Arc::new(AtomicU32::new(0));
    let hamlib_loop_ready = Arc::new(AtomicBool::new(false));

    let reason = tx_hard_mute_reason(
        &policy,
        &restart_inhibit,
        &hamlib_loop_ready,
        &no_pending_frequency(),
        &no_pending(),
        &not_in_flight(),
        &rig(RigConnState::Connected),
        &audio_alive(true),
        &{
            let gain = super::super::receive_overload::GainSafety::default();
            gain.confirmed_off(0);
            std::sync::Arc::new(gain)
        },
        &pancetta_hamlib::DataModeReadiness::new(false),
    );
    assert!(
        reason.is_some(),
        "TX must stay muted when the Hamlib command loop hasn't confirmed readiness, even \
             though the restart-supervision inhibit counter has already released"
    );
}

/// PAN-19 round-14 review (Codex P1): the actual bug this guards
/// against. A pending `SetSplit` from a prior generation's failed
/// teardown replay is requeued (round 10) into `hamlib_pending_split`
/// after a momentarily-full channel, waiting for the polling task's
/// next ~500ms retry -- but the message loop IS ready to consume
/// commands during that gap (`hamlib_command_loop_ready == true`), and
/// the restart-supervision inhibit has already released
/// (`restart_inhibit == 0`). Without this check, PTT-on would slip
/// through here and transmit with the rig still holding its stale,
/// pre-crash split configuration -- an off-frequency transmission.
#[test]
fn undelivered_pending_split_mutes_tx_even_when_the_loop_is_ready() {
    let policy = policy(pancetta_core::TxPolicy::Full);
    let restart_inhibit = Arc::new(AtomicU32::new(0));
    let hamlib_loop_ready = Arc::new(AtomicBool::new(true));
    let pending_split = Arc::new(Mutex::new(Some(pending_split_msg())));

    let reason = tx_hard_mute_reason(
        &policy,
        &restart_inhibit,
        &hamlib_loop_ready,
        &no_pending_frequency(),
        &pending_split,
        &not_in_flight(),
        &rig(RigConnState::Connected),
        &audio_alive(true),
        &{
            let gain = super::super::receive_overload::GainSafety::default();
            gain.confirmed_off(0);
            std::sync::Arc::new(gain)
        },
        &pancetta_hamlib::DataModeReadiness::new(false),
    );
    assert!(
        reason.is_some(),
        "TX must stay muted while a pending SetSplit is still undelivered, even though the \
             Hamlib command loop has confirmed readiness and the restart inhibit has released"
    );

    // Once the pending item clears (delivered by the polling task's
    // retry, or applied and the slot drained), PTT-on must be
    // permitted again -- the fix must not become overly conservative.
    *pending_split.lock().unwrap() = None;
    assert_eq!(
        tx_hard_mute_reason(
            &policy,
            &restart_inhibit,
            &hamlib_loop_ready,
            &no_pending_frequency(),
            &pending_split,
            &not_in_flight(),
            &rig(RigConnState::Connected),
            &audio_alive(true),
            &{
                let gain = super::super::receive_overload::GainSafety::default();
                gain.confirmed_off(0);
                std::sync::Arc::new(gain)
            },
            &pancetta_hamlib::DataModeReadiness::new(false),
        ),
        None,
        "PTT-on must be permitted again once the pending SetSplit has been delivered"
    );
}

/// Same as above, for the frequency slot -- both pending kinds must be
/// covered, not just split.
#[test]
fn undelivered_pending_frequency_also_mutes_tx() {
    let policy = policy(pancetta_core::TxPolicy::Full);
    let restart_inhibit = Arc::new(AtomicU32::new(0));
    let hamlib_loop_ready = Arc::new(AtomicBool::new(true));
    let mut pending_frequency_map = HashMap::new();
    pending_frequency_map.insert(
        pancetta_hamlib::Vfo::A,
        ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetFrequency {
                vfo: 0,
                frequency: 14_074_000,
            }),
            std::time::Instant::now(),
        ),
    );
    let pending_frequency = Arc::new(Mutex::new(pending_frequency_map));

    assert!(tx_hard_mute_reason(
        &policy,
        &restart_inhibit,
        &hamlib_loop_ready,
        &pending_frequency,
        &no_pending(),
        &not_in_flight(),
        &rig(RigConnState::Connected),
        &audio_alive(true),
        &{
            let gain = super::super::receive_overload::GainSafety::default();
            gain.confirmed_off(0);
            std::sync::Arc::new(gain)
        },
        &pancetta_hamlib::DataModeReadiness::new(false),
    )
    .is_some());
}

/// PAN-35 regression guard: a pending command for ONE VFO must still
/// mute TX even though the OTHER VFO's slot is empty -- before this
/// fix, both VFOs shared a single slot, so this distinction didn't
/// exist; now that the slot is keyed by VFO, `has_undelivered_pending_
/// hamlib_state` must check the map as a whole (any entry), not
/// assume a specific VFO's entry.
#[test]
fn undelivered_pending_frequency_on_either_vfo_alone_still_mutes_tx() {
    let policy = policy(pancetta_core::TxPolicy::Full);
    let restart_inhibit = Arc::new(AtomicU32::new(0));
    let hamlib_loop_ready = Arc::new(AtomicBool::new(true));

    for vfo in [pancetta_hamlib::Vfo::A, pancetta_hamlib::Vfo::B] {
        let mut pending_frequency_map = HashMap::new();
        pending_frequency_map.insert(
            vfo,
            ComponentMessage::new(
                ComponentId::Hamlib,
                ComponentId::Hamlib,
                MessageType::RigControl(crate::message_bus::RigControlMessage::SetFrequency {
                    vfo: if vfo == pancetta_hamlib::Vfo::A { 0 } else { 1 },
                    frequency: 14_074_000,
                }),
                std::time::Instant::now(),
            ),
        );
        let pending_frequency = Arc::new(Mutex::new(pending_frequency_map));

        assert!(
            tx_hard_mute_reason(
                &policy,
                &restart_inhibit,
                &hamlib_loop_ready,
                &pending_frequency,
                &no_pending(),
                &not_in_flight(),
                &rig(RigConnState::Connected),
                &audio_alive(true),
                &{
                    let gain = super::super::receive_overload::GainSafety::default();
                    gain.confirmed_off(0);
                    std::sync::Arc::new(gain)
                },
                &pancetta_hamlib::DataModeReadiness::new(false),
            )
            .is_some(),
            "a pending command for {vfo:?} alone must still mute TX"
        );
    }
}

#[test]
fn tx_policy_disabled_still_mutes_when_everything_else_is_ready() {
    let policy = policy(pancetta_core::TxPolicy::Disabled);
    let restart_inhibit = Arc::new(AtomicU32::new(0));
    let hamlib_loop_ready = Arc::new(AtomicBool::new(true));
    assert_eq!(
        tx_hard_mute_reason(
            &policy,
            &restart_inhibit,
            &hamlib_loop_ready,
            &no_pending_frequency(),
            &no_pending(),
            &not_in_flight(),
            &rig(RigConnState::Connected),
            &audio_alive(true),
            &{
                let gain = super::super::receive_overload::GainSafety::default();
                gain.confirmed_off(0);
                std::sync::Arc::new(gain)
            },
            &pancetta_hamlib::DataModeReadiness::new(false),
        ),
        Some("TX policy is Disabled")
    );
}

/// A poisoned pending-slot lock must fail CLOSED (still muted) -- an
/// unknown pending state is never "safe to key".
#[test]
fn poisoned_pending_lock_fails_closed() {
    let policy = policy(pancetta_core::TxPolicy::Full);
    let restart_inhibit = Arc::new(AtomicU32::new(0));
    let hamlib_loop_ready = Arc::new(AtomicBool::new(true));
    let pending_split = no_pending();
    let poison_guard = pending_split.clone();
    let _ = std::thread::spawn(move || {
        let _guard = poison_guard.lock().unwrap();
        panic!("deliberately poison the lock");
    })
    .join();
    assert!(pending_split.is_poisoned());

    assert!(
        tx_hard_mute_reason(
            &policy,
            &restart_inhibit,
            &hamlib_loop_ready,
            &no_pending_frequency(),
            &pending_split,
            &not_in_flight(),
            &rig(RigConnState::Connected),
            &audio_alive(true),
            &{
                let gain = super::super::receive_overload::GainSafety::default();
                gain.confirmed_off(0);
                std::sync::Arc::new(gain)
            },
            &pancetta_hamlib::DataModeReadiness::new(false),
        )
        .is_some(),
        "a poisoned pending-slot lock must fail closed (still muted), not be treated as \
             'nothing pending'"
    );
}

/// PAN-19 round-16 review (Codex P1) regression guard: "keep restored
/// rig state pending through CAT application". The exact scenario the
/// finding describes: the pending slot is ALREADY empty (cleared at
/// hand-off time by `deliver_pending_hamlib_state`, before the message
/// loop even started the underlying CAT call), the loop is ready, and
/// restart isn't inhibiting -- yet a `set_frequency`/`set_split_freq`/
/// `set_split` call is genuinely in flight right now. PTT must still
/// be refused: the empty pending slot alone (round 14) doesn't mean
/// the rig's state is confirmed correct.
#[test]
fn in_flight_command_mutes_tx_even_when_the_pending_slot_is_already_empty() {
    let policy = policy(pancetta_core::TxPolicy::Full);
    let restart_inhibit = Arc::new(AtomicU32::new(0));
    let hamlib_loop_ready = Arc::new(AtomicBool::new(true));
    let in_flight = Arc::new(AtomicU32::new(1));

    let reason = tx_hard_mute_reason(
        &policy,
        &restart_inhibit,
        &hamlib_loop_ready,
        &no_pending_frequency(),
        &no_pending(),
        &in_flight,
        &rig(RigConnState::Connected),
        &audio_alive(true),
        &{
            let gain = super::super::receive_overload::GainSafety::default();
            gain.confirmed_off(0);
            std::sync::Arc::new(gain)
        },
        &pancetta_hamlib::DataModeReadiness::new(false),
    );
    assert!(
        reason.is_some(),
        "TX must stay muted while a rig frequency/split command is in flight, even though \
             both pending slots are already empty"
    );

    // Once the CAT call resolves and the in-flight flag clears, PTT-on
    // must be permitted again -- the fix must not become overly
    // conservative.
    in_flight.store(0, Ordering::Release);
    assert_eq!(
        tx_hard_mute_reason(
            &policy,
            &restart_inhibit,
            &hamlib_loop_ready,
            &no_pending_frequency(),
            &no_pending(),
            &in_flight,
            &rig(RigConnState::Connected),
            &audio_alive(true),
            &{
                let gain = super::super::receive_overload::GainSafety::default();
                gain.confirmed_off(0);
                std::sync::Arc::new(gain)
            },
            &pancetta_hamlib::DataModeReadiness::new(false),
        ),
        None,
        "PTT-on must be permitted again once the in-flight CAT call has resolved"
    );
}

/// All-healthy inputs except the two PAN-115 ones, which the caller passes.
fn reason_with(
    restart: u32,
    loop_ready: bool,
    policy_value: pancetta_core::TxPolicy,
    rig_state: &Arc<AtomicU8>,
    audio: &Arc<AtomicBool>,
) -> Option<&'static str> {
    tx_hard_mute_reason(
        &policy(policy_value),
        &Arc::new(AtomicU32::new(restart)),
        &Arc::new(AtomicBool::new(loop_ready)),
        &no_pending_frequency(),
        &no_pending(),
        &not_in_flight(),
        rig_state,
        audio,
        &{
            let gain = super::super::receive_overload::GainSafety::default();
            gain.confirmed_off(0);
            std::sync::Arc::new(gain)
        },
        &pancetta_hamlib::DataModeReadiness::new(false),
    )
}

/// PAN-115 Gherkin 2: a lost rig/CAT connection mutes TX with an exact
/// reason, and clears as soon as the poll loop reports Connected again.
#[test]
fn rig_polling_failed_mutes_tx_as_rig_disconnected() {
    let rig_state = rig(RigConnState::PollingFailed);
    let audio = audio_alive(true);
    assert_eq!(
        reason_with(0, true, pancetta_core::TxPolicy::Full, &rig_state, &audio),
        Some("rig is disconnected")
    );
    rig_state.store(RigConnState::Connected.as_u8(), Ordering::Release);
    assert_eq!(
        reason_with(0, true, pancetta_core::TxPolicy::Full, &rig_state, &audio),
        None
    );
}

/// `NotConnected` covers the mock rig, rig control disabled and "not
/// tried yet"; it must not mute.
#[test]
fn rig_not_connected_does_not_mute() {
    assert_eq!(
        reason_with(
            0,
            true,
            pancetta_core::TxPolicy::Full,
            &rig(RigConnState::NotConnected),
            &audio_alive(true),
        ),
        None
    );
}

/// PAN-115 Gherkin 1: a dead audio output mutes TX with an exact reason,
/// and clears once output callbacks resume.
#[test]
fn dead_audio_output_mutes_tx() {
    let rig_state = rig(RigConnState::Connected);
    let audio = audio_alive(false);
    assert_eq!(
        reason_with(0, true, pancetta_core::TxPolicy::Full, &rig_state, &audio),
        Some("audio output device is dead")
    );
    audio.store(true, Ordering::Release);
    assert_eq!(
        reason_with(0, true, pancetta_core::TxPolicy::Full, &rig_state, &audio),
        None
    );
}

/// "not during a supervised restart": the restart reason keeps priority.
#[test]
fn restart_inhibit_outranks_rig_disconnected() {
    assert_eq!(
        reason_with(
            1,
            true,
            pancetta_core::TxPolicy::Full,
            &rig(RigConnState::PollingFailed),
            &audio_alive(true),
        ),
        Some("rig control is restarting")
    );
}

#[test]
fn loop_not_ready_outranks_rig_and_audio() {
    assert_eq!(
        reason_with(
            0,
            false,
            pancetta_core::TxPolicy::Full,
            &rig(RigConnState::PollingFailed),
            &audio_alive(false),
        ),
        Some("Hamlib command loop is not yet ready")
    );
}

#[test]
fn rig_disconnected_outranks_audio_dead_and_policy_disabled() {
    assert_eq!(
        reason_with(
            0,
            true,
            pancetta_core::TxPolicy::Disabled,
            &rig(RigConnState::PollingFailed),
            &audio_alive(false),
        ),
        Some("rig is disconnected")
    );
    assert_eq!(
        reason_with(
            0,
            true,
            pancetta_core::TxPolicy::Disabled,
            &rig(RigConnState::Connected),
            &audio_alive(false),
        ),
        Some("audio output device is dead")
    );
}

/// PAN-89: a frequency pending map holding one `SetFrequency` entry.
fn one_pending_frequency() -> Arc<Mutex<HashMap<pancetta_hamlib::Vfo, ComponentMessage>>> {
    let mut map = HashMap::new();
    map.insert(
        pancetta_hamlib::Vfo::A,
        ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetFrequency {
                vfo: 0,
                frequency: 14_074_000,
            }),
            std::time::Instant::now(),
        ),
    );
    Arc::new(Mutex::new(map))
}

/// PAN-89: the display-only cause list agrees with the gate for every
/// combination of its inputs (2 × 2 × 3 × 2 × 2 × 2 × 3 = 288 cases).
#[test]
fn tx_mute_causes_first_matches_tx_hard_mute_reason_for_every_input_combination() {
    let mut cases = 0;
    for inhibit in [0u32, 1] {
        for loop_ready in [true, false] {
            for conn in [
                RigConnState::NotConnected,
                RigConnState::Connected,
                RigConnState::PollingFailed,
            ] {
                for alive in [true, false] {
                    for pending in [false, true] {
                        for in_flight in [0u32, 1] {
                            for p in [
                                pancetta_core::TxPolicy::Full,
                                pancetta_core::TxPolicy::RespondOnly,
                                pancetta_core::TxPolicy::Disabled,
                            ] {
                                for data_mode_ready in [true, false] {
                                    let data_mode =
                                        pancetta_hamlib::DataModeReadiness::new(!data_mode_ready);
                                    let tx_policy = policy(p);
                                    let restart_inhibit = Arc::new(AtomicU32::new(inhibit));
                                    let hamlib_loop_ready = Arc::new(AtomicBool::new(loop_ready));
                                    let pending_frequency = if pending {
                                        one_pending_frequency()
                                    } else {
                                        no_pending_frequency()
                                    };
                                    let pending_split = no_pending();
                                    let command_in_flight = Arc::new(AtomicU32::new(in_flight));
                                    let rig_conn = rig(conn);
                                    let output_alive = audio_alive(alive);

                                    let gate = tx_hard_mute_reason(
                                        &tx_policy,
                                        &restart_inhibit,
                                        &hamlib_loop_ready,
                                        &pending_frequency,
                                        &pending_split,
                                        &command_in_flight,
                                        &rig_conn,
                                        &output_alive,
                                        &{
                                            let gain =
                                                super::super::receive_overload::GainSafety::default(
                                                );
                                            gain.confirmed_off(0);
                                            std::sync::Arc::new(gain)
                                        },
                                        &data_mode,
                                    );
                                    let causes = tx_mute_causes(
                                        &tx_policy,
                                        &restart_inhibit,
                                        &hamlib_loop_ready,
                                        &pending_frequency,
                                        &pending_split,
                                        &command_in_flight,
                                        &rig_conn,
                                        &output_alive,
                                        &{
                                            let gain =
                                                super::super::receive_overload::GainSafety::default(
                                                );
                                            gain.confirmed_off(0);
                                            std::sync::Arc::new(gain)
                                        },
                                        &data_mode,
                                    );
                                    let case = format!(
                                        "inhibit={inhibit} loop_ready={loop_ready} conn={conn:?} \
                                         alive={alive} pending={pending} in_flight={in_flight} \
                                         policy={p:?}"
                                    );
                                    assert_eq!(
                                        causes.first().map(|c| c.as_reason()),
                                        gate,
                                        "{case}"
                                    );
                                    assert_eq!(causes.is_empty(), gate.is_none(), "{case}");
                                    cases += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    assert_eq!(cases, 576);
}

#[test]
fn receive_gain_mute_matches_readiness_for_stable_and_unconfirmed_gain() {
    let gain = Arc::new(super::super::receive_overload::GainSafety::default());
    gain.confirmed_off(0);
    for blocked in [false, true] {
        if blocked {
            gain.fail_gain();
        }
        let verdict = tx_hard_mute_reason(
            &policy(pancetta_core::TxPolicy::Full),
            &Arc::new(AtomicU32::new(0)),
            &Arc::new(AtomicBool::new(true)),
            &no_pending_frequency(),
            &no_pending(),
            &not_in_flight(),
            &rig(RigConnState::Connected),
            &audio_alive(true),
            &gain,
            &pancetta_hamlib::DataModeReadiness::new(false),
        );
        let causes = tx_mute_causes(
            &policy(pancetta_core::TxPolicy::Full),
            &Arc::new(AtomicU32::new(0)),
            &Arc::new(AtomicBool::new(true)),
            &no_pending_frequency(),
            &no_pending(),
            &not_in_flight(),
            &rig(RigConnState::Connected),
            &audio_alive(true),
            &gain,
            &pancetta_hamlib::DataModeReadiness::new(false),
        );
        assert_eq!(verdict, causes.first().map(|cause| cause.as_reason()));
        assert_eq!(
            causes.contains(&TxMuteCause::ReceiveGainUnconfirmed),
            blocked
        );
        if blocked {
            assert_eq!(
                verdict,
                Some("receive gain change is in progress or unconfirmed")
            );
        }
    }
}

#[test]
fn tx_mute_causes_lists_every_active_cause_in_gate_priority_order() {
    let causes = tx_mute_causes(
        &policy(pancetta_core::TxPolicy::Disabled),
        &Arc::new(AtomicU32::new(0)),
        &Arc::new(AtomicBool::new(true)),
        &no_pending_frequency(),
        &no_pending(),
        &not_in_flight(),
        &rig(RigConnState::PollingFailed),
        &audio_alive(false),
        &{
            let gain = super::super::receive_overload::GainSafety::default();
            gain.confirmed_off(0);
            std::sync::Arc::new(gain)
        },
        &pancetta_hamlib::DataModeReadiness::new(false),
    );
    assert_eq!(
        causes,
        vec![
            TxMuteCause::RigDisconnected,
            TxMuteCause::AudioOutputDead,
            TxMuteCause::PolicyDisabled,
        ]
    );

    // Every predicate true at once: all seven, in the gate's order.
    let split = no_pending();
    *split.lock().unwrap() = Some(pending_split_msg());
    let causes = tx_mute_causes(
        &policy(pancetta_core::TxPolicy::Disabled),
        &Arc::new(AtomicU32::new(1)),
        &Arc::new(AtomicBool::new(false)),
        &no_pending_frequency(),
        &split,
        &Arc::new(AtomicU32::new(2)),
        &rig(RigConnState::PollingFailed),
        &audio_alive(false),
        &{
            let gain = super::super::receive_overload::GainSafety::default();
            gain.confirmed_off(0);
            std::sync::Arc::new(gain)
        },
        &pancetta_hamlib::DataModeReadiness::new(false),
    );
    assert_eq!(
        causes,
        vec![
            TxMuteCause::RigRestarting,
            TxMuteCause::HamlibLoopNotReady,
            TxMuteCause::RigDisconnected,
            TxMuteCause::AudioOutputDead,
            TxMuteCause::RigStatePending,
            TxMuteCause::RigCommandInFlight,
            TxMuteCause::PolicyDisabled,
        ]
    );
}

#[test]
fn tx_mute_cause_reasons_are_byte_identical_to_the_gate_strings() {
    let expected = [
        (TxMuteCause::RigRestarting, "rig control is restarting"),
        (
            TxMuteCause::HamlibLoopNotReady,
            "Hamlib command loop is not yet ready",
        ),
        (TxMuteCause::RigDisconnected, "rig is disconnected"),
        (TxMuteCause::AudioOutputDead, "audio output device is dead"),
        (
            TxMuteCause::RigStatePending,
            "pending rig frequency/split state has not been delivered yet",
        ),
        (
            TxMuteCause::RigCommandInFlight,
            "a rig frequency/split command is still being applied",
        ),
        (TxMuteCause::PolicyDisabled, "TX policy is Disabled"),
    ];
    for (cause, reason) in expected {
        assert_eq!(cause.as_reason(), reason, "{cause:?}");
    }

    // And each one is what the gate itself returns when it is the only
    // active cause.
    let split = no_pending();
    *split.lock().unwrap() = Some(pending_split_msg());
    let only = |inhibit: u32,
                ready: bool,
                conn: RigConnState,
                alive: bool,
                split: &Arc<Mutex<Option<ComponentMessage>>>,
                in_flight: u32,
                p: pancetta_core::TxPolicy| {
        tx_hard_mute_reason(
            &policy(p),
            &Arc::new(AtomicU32::new(inhibit)),
            &Arc::new(AtomicBool::new(ready)),
            &no_pending_frequency(),
            split,
            &Arc::new(AtomicU32::new(in_flight)),
            &rig(conn),
            &audio_alive(alive),
            &{
                let gain = super::super::receive_overload::GainSafety::default();
                gain.confirmed_off(0);
                std::sync::Arc::new(gain)
            },
            &pancetta_hamlib::DataModeReadiness::new(false),
        )
    };
    let full = pancetta_core::TxPolicy::Full;
    let ok = RigConnState::Connected;
    let none = no_pending();
    assert_eq!(
        only(1, true, ok, true, &none, 0, full),
        Some(TxMuteCause::RigRestarting.as_reason())
    );
    assert_eq!(
        only(0, false, ok, true, &none, 0, full),
        Some(TxMuteCause::HamlibLoopNotReady.as_reason())
    );
    assert_eq!(
        only(0, true, RigConnState::PollingFailed, true, &none, 0, full),
        Some(TxMuteCause::RigDisconnected.as_reason())
    );
    assert_eq!(
        only(0, true, ok, false, &none, 0, full),
        Some(TxMuteCause::AudioOutputDead.as_reason())
    );
    assert_eq!(
        only(0, true, ok, true, &split, 0, full),
        Some(TxMuteCause::RigStatePending.as_reason())
    );
    assert_eq!(
        only(0, true, ok, true, &none, 1, full),
        Some(TxMuteCause::RigCommandInFlight.as_reason())
    );
    assert_eq!(
        only(
            0,
            true,
            ok,
            true,
            &none,
            0,
            pancetta_core::TxPolicy::Disabled
        ),
        Some(TxMuteCause::PolicyDisabled.as_reason())
    );
}
