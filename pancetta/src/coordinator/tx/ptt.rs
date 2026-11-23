use super::*;
use jiff::SignedDuration;
use pancetta_core::time::SignedDurationExt as _;
use pancetta_core::time::UtcTimestamp;

/// One evaluation of the pre-PTT wait (PAN-114, moved onto the shared
/// [`pancetta_core::SlotClock`] by PAN-483). Re-derived from a fresh clock
/// read each chunk, so a step during the wait is seen before PTT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::coordinator) enum PttWaitStep {
    Sleep(Duration),
    Reached,
    ClockJumped(pancetta_core::slot_clock::WallClockJump),
}

/// PAN-483: the resync a frame scheduled under `scheduled_resync_epoch` has
/// missed, if any. `Some(event.jump())` (the step's real size) once
/// `reading` carries a later resync epoch; `None` while the epoch is
/// unchanged, so a step under [`pancetta_core::slot_clock::SLOT_RESYNC_THRESHOLD_MS`]
/// never drops a frame. After several resyncs it reports the latest one's
/// step, not their sum.
pub(in crate::coordinator) fn resync_since(
    reading: &pancetta_core::slot_clock::SlotReading,
    scheduled_resync_epoch: u64,
) -> Option<pancetta_core::slot_clock::WallClockJump> {
    if reading.resync_epoch == scheduled_resync_epoch {
        return None;
    }
    reading.last_resync.map(|event| event.jump())
}

/// Pure decision behind [`wait_for_ptt_instant`]. A frame scheduled under an
/// older resync epoch is dropped with the step's real size, even when its
/// PTT instant is still ahead: its schedule was computed on pre-step time.
/// Otherwise it sleeps in chunks of at most 50 ms until `ptt_target`, then
/// reaches it.
pub(in crate::coordinator) fn ptt_wait_step(
    reading: &pancetta_core::slot_clock::SlotReading,
    ptt_target: UtcTimestamp,
    scheduled_resync_epoch: u64,
) -> PttWaitStep {
    if let Some(jump) = resync_since(reading, scheduled_resync_epoch) {
        return PttWaitStep::ClockJumped(jump);
    }
    let remaining = ptt_target - reading.now;
    if remaining > SignedDuration::ZERO {
        let chunk = remaining
            .to_std()
            .unwrap_or(Duration::ZERO)
            .min(Duration::from_millis(50));
        return PttWaitStep::Sleep(chunk);
    }
    PttWaitStep::Reached
}

/// PAN-117 (six-lane review L5 H-7): `true` when `now` falls more than
/// `late_max_ms` past `target_slot` — keying then would put so little of
/// the waveform on the air that no receiver could decode it. Applied at
/// the last instant before PTT in both worker arms (the Step 4d final
/// gate), because everything after the queue-time `schedule_tx` decision
/// (coalesce waits, encode/modulate, gate awaits) adds real delay the
/// queue-time check never sees. The boundary is inclusive on the key side
/// (`elapsed == cap` keys), mirroring `schedule_tx`'s
/// `mstr <= tx_late_max_ms`; a deferred target (target_slot in the
/// future) makes `now - target_slot` negative, so this only bites
/// current-slot late starts. Clock steps are covered by the resync rule,
/// [`resync_since`].
pub(super) fn key_time_too_late(
    now: UtcTimestamp,
    target_slot: UtcTimestamp,
    late_max_ms: u64,
) -> bool {
    now - target_slot > SignedDuration::from_millis(late_max_ms as i64)
}

/// Outcome of [`wait_for_ptt_instant`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PttWait {
    /// The wall clock reached the PTT instant.
    Reached,
    /// Shutdown or F8 abort flipped; same caller handling as
    /// `interruptible_sleep` returning `true`.
    Interrupted,
    /// A resync after the frame was scheduled; drop the frame.
    ClockJumped(pancetta_core::slot_clock::WallClockJump),
}

/// PAN-114, PAN-483: wait for the PTT engage instant on the shared slot
/// clock (re-read every <= 50 ms chunk) instead of one monotonic sleep
/// computed up front. Checks `shutdown`/`abort` before each evaluation,
/// exactly like [`interruptible_sleep`], so wake latency stays ~50 ms. With
/// no clock step this returns `Reached` at the same wall instant the old
/// monotonic sleep ended; after a resync it returns `ClockJumped`.
pub(super) async fn wait_for_ptt_instant(
    ptt_target: UtcTimestamp,
    scheduled_resync_epoch: u64,
    shutdown: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    abort: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    clock: &pancetta_core::SlotClock,
) -> PttWait {
    use std::sync::atomic::Ordering;
    loop {
        if shutdown.load(Ordering::Acquire) || abort.load(Ordering::Acquire) {
            return PttWait::Interrupted;
        }
        match ptt_wait_step(&clock.read(), ptt_target, scheduled_resync_epoch) {
            PttWaitStep::Sleep(chunk) => sleep(chunk).await,
            PttWaitStep::Reached => return PttWait::Reached,
            PttWaitStep::ClockJumped(jump) => return PttWait::ClockJumped(jump),
        }
    }
}

/// PAN-135: bounded retry for a safety send that must not be silently
/// dropped. Uses the PAN-43/45 schedule: 4 attempts over about 75 ms.
pub(crate) const SAFETY_SEND_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(5),
    Duration::from_millis(20),
    Duration::from_millis(50),
];

pub(super) fn ptt_off_message() -> ComponentMessage {
    ComponentMessage::new(
        ComponentId::Ft8Transmitter,
        ComponentId::Hamlib,
        MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt { state: false }),
        Instant::now(),
    )
}

/// What [`PttGuard::release`] achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PttRelease {
    /// PTT-off is enqueued on Hamlib's channel: handed off.
    Delivered,
    /// Every attempt dropped; the guard stays armed.
    Undelivered,
    /// PAN-273: no Hamlib channel is registered (`--replay`), so no PTT-on
    /// was ever enqueued; the guard stood down without sending.
    NoRig,
}

/// PAN-135: the shortest a disarm flush waits for its ack, and how far
/// past the revoked waveform's scheduled end an unconfirmed flush waits
/// (relay poll + audio-thread latency between Step 7's enqueue and the
/// samples reaching the output ring).
pub(super) const FLUSH_ACK_MIN_WAIT: Duration = Duration::from_millis(250);
pub(super) const FLUSH_PLAYBACK_SLACK: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DisarmFlush {
    /// The audio thread confirmed the flush reached the output ring.
    Applied,
    /// No confirmation: waited until the revoked audio had played out.
    WaitedOutPlayback,
    /// Shutdown ended the wait.
    Shutdown,
}

/// PAN-135 (PR #362 rounds 4/6, Codex P1): flush the revoked remote
/// waveform before the worker resumes held or queued work that may key
/// again. Wait for the audio thread to confirm the flush, the revoked
/// playback window to elapse, or shutdown. The playback bound applies
/// when confirmation cannot arrive, including `--no-audio` and a stalled
/// audio thread. The flush rides the same Audio FIFO as the frame's samples,
/// so it can never clear the buffer ahead of them.
pub(super) async fn flush_disarmed_audio_confirmed(
    message_bus: &MessageBus,
    sample_rate: u32,
    playback_end: tokio::time::Instant,
    shutdown: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> DisarmFlush {
    let deadline = std::cmp::max(
        playback_end + FLUSH_PLAYBACK_SLACK,
        tokio::time::Instant::now() + FLUSH_ACK_MIN_WAIT,
    );
    'attempts: for attempt in 0..=SAFETY_SEND_RETRY_DELAYS.len() {
        let (ack, mut waiter) = DeliveryAck::new();
        let flush = ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            ComponentId::Audio,
            MessageType::AudioFlush { sample_rate, ack },
            Instant::now(),
        );
        if matches!(message_bus.send_message_checked(flush).await, Ok(true)) {
            loop {
                if shutdown.load(Ordering::Acquire) {
                    return DisarmFlush::Shutdown;
                }
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    break 'attempts;
                }
                match waiter
                    .wait_for((deadline - now).min(Duration::from_millis(50)))
                    .await
                {
                    AckState::Applied => return DisarmFlush::Applied,
                    AckState::Pending => {}
                    AckState::NotApplied => break 'attempts,
                    AckState::Dropped => break, // lost in transit: send a fresh one
                }
            }
        }
        if let Some(delay) = SAFETY_SEND_RETRY_DELAYS.get(attempt) {
            sleep(*delay).await;
        }
    }
    error!(
        target: "pancetta::tx.ptt",
        "Remote disarm: audio flush not confirmed by the audio thread; holding the next request for {:?} until the revoked waveform has played out",
        deadline.saturating_duration_since(tokio::time::Instant::now())
    );
    while tokio::time::Instant::now() < deadline {
        if shutdown.load(Ordering::Acquire) {
            return DisarmFlush::Shutdown;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        sleep(remaining.min(Duration::from_millis(50))).await;
    }
    DisarmFlush::WaitedOutPlayback
}

/// PAN-273 (Codex P2, PR #587): the retrying PTT-off an armed
/// [`PttGuard`]'s `Drop` spawned, while it may still be outstanding.
/// Coordinator-owned, so a restarted TX worker shares it. [`PttGuard::new`]
/// waits for it before reserving the next key-down, so a fallback unkey can
/// never reach Hamlib's channel after a later PTT-on from the TX worker
/// (for example the re-key a supersede starts right after an undelivered
/// release).
#[derive(Debug, Clone, Default)]
pub(crate) struct PendingPttOff(
    pub(super) std::sync::Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
);

impl PendingPttOff {
    /// Record a fallback PTT-off so the next [`Self::settle`] waits for it
    /// and for any fallback already recorded. Both send independently.
    pub(super) fn push(&self, unkey: tokio::task::JoinHandle<()>) {
        let mut slot = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = Some(match slot.take() {
            None => unkey,
            Some(earlier) => tokio::task::spawn(async move {
                let _ = earlier.await;
                let _ = unkey.await;
            }),
        });
    }

    /// Wait until every fallback PTT-off recorded so far has finished
    /// (delivered or given up after its bounded retries). If this wait is
    /// cancelled (worker abort or restart), the unfinished handle goes back
    /// in the slot, so the next key-down still waits for it.
    /// Shutdown also awaits this before stopping the Hamlib drain (PAN-277).
    pub(crate) async fn settle(&self) {
        struct Waiting<'a> {
            slot: &'a PendingPttOff,
            unkey: Option<tokio::task::JoinHandle<()>>,
        }
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                if let Some(unkey) = self.unkey.take() {
                    self.slot.push(unkey);
                }
            }
        }
        let outstanding = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let mut waiting = Waiting {
            slot: self,
            unkey: outstanding,
        };
        if let Some(unkey) = waiting.unkey.as_mut() {
            let _ = unkey.await;
        }
        waiting.unkey = None;
    }
}

/// Guard that sends PTT-off when dropped, ensuring PTT is released
/// even if the transmitter task is cancelled mid-transmission.
pub(super) struct PttGuard {
    message_bus: MessageBus,
    pub(super) armed: bool,
    /// Mirrors the keyed state for the SWR poll / TUI. Set true on construct,
    /// cleared on drop (RAII — clears on every exit path incl. abort/panic).
    ptt_active: std::sync::Arc<std::sync::atomic::AtomicBool>,
    last_ptt_off_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Where `Drop` records its fallback PTT-off for the next `new` to await.
    pending_ptt_off: PendingPttOff,
}

impl PttGuard {
    /// `ptt_sync_gate` (PAN-143) makes the `ptt_active` store below mutually
    /// exclusive with `pancetta_qso::QsoManager::apply_tx_offset_switch`'s
    /// recheck-through-publish span — closing, not just narrowing, the race
    /// PAN-140 round 2 found (and round 3's finding that releasing too early
    /// reopens it further down the same function). See that gate's doc
    /// comment and `docs/superpowers/specs/
    /// 2026-09-12-pan-143-ptt-shared-synchronization-design.md`. An
    /// async-aware `tokio::sync::Mutex` — round 3 requires the QSO-manager
    /// side to hold the gate across its own `.await` event-emission points,
    /// which a blocking `std::sync::Mutex` cannot safely do — so this
    /// constructor is `async` accordingly; the wait here is expected to be
    /// negligible (the QSO-manager side never does I/O or hardware calls
    /// while holding it, only channel sends).
    ///
    /// PAN-273 (Codex P2, PR #587): first waits for any fallback PTT-off a
    /// previous armed guard's `Drop` left outstanding (at most its bounded
    /// retry schedule), so that unkey is serialized ahead of this guard's
    /// key-down. The TX-worker arms already settle before their audio
    /// refresh (single, multi) or hard-mute check (tune), so this wait is
    /// normally a no-op backstop.
    pub(super) async fn new(
        message_bus: MessageBus,
        ptt_active: std::sync::Arc<std::sync::atomic::AtomicBool>,
        ptt_sync_gate: &std::sync::Arc<tokio::sync::Mutex<()>>,
        last_ptt_on_ms: &std::sync::Arc<std::sync::atomic::AtomicU64>,
        last_ptt_off_ms: &std::sync::Arc<std::sync::atomic::AtomicU64>,
        pending_ptt_off: &PendingPttOff,
    ) -> Self {
        pending_ptt_off.settle().await;
        {
            let _ptt_sync = ptt_sync_gate.lock().await;
            // Reserve the relocation interlock even if gain currently blocks
            // hardware key-down. Gain may recover during status publication;
            // send_ptt_command still refuses key-down while gain is blocked.
            ptt_active.store(true, std::sync::atomic::Ordering::Release);
        }
        last_ptt_on_ms.store(
            super::super::now_epoch_ms(),
            std::sync::atomic::Ordering::Release,
        );
        Self {
            message_bus,
            armed: true,
            ptt_active,
            last_ptt_off_ms: last_ptt_off_ms.clone(),
            pending_ptt_off: pending_ptt_off.clone(),
        }
    }

    /// PAN-135: unkey with checked, bounded-retry delivery, and stand the
    /// RAII fallback down only once PTT-off is enqueued on Hamlib's channel.
    /// Hamlib applies that channel in order, so any later PTT-on from this
    /// worker lands after this unkey. An enqueued PTT-off is handed off.
    /// If every attempt drops, the guard stays armed (its `Drop` retries on
    /// scope exit, and the next [`Self::new`] waits for that retry before
    /// keying) and `ptt_active` stays true until then; Hamlib's 14 s
    /// PTT watchdog is the remaining backstop.
    ///
    /// PAN-273: every TX-worker unkey after a key-down goes through here
    /// (Step 9, multi-TX, tune, the clock-step hold guard and supersede),
    /// never through a best-effort `send_message` followed by `disarm()`.
    /// With no Hamlib channel registered nothing was keyed, so the guard
    /// stands down at once and returns [`PttRelease::NoRig`].
    pub(super) async fn release(&mut self, context: &str) -> PttRelease {
        if !self.message_bus.has_rig_route() {
            debug!(
                target: "pancetta::tx.ptt",
                "{context}: no Hamlib channel registered; nothing to unkey"
            );
            self.stand_down();
            return PttRelease::NoRig;
        }
        match self
            .message_bus
            .send_message_retrying(ptt_off_message, &SAFETY_SEND_RETRY_DELAYS)
            .await
        {
            Ok(true) => {
                self.stand_down();
                PttRelease::Delivered
            }
            Ok(false) | Err(_) => {
                error!(
                    target: "pancetta::tx.ptt",
                    "{context}: PTT OFF not delivered to Hamlib after {} attempts; PTT guard stays armed and retries on drop; Hamlib's PTT watchdog is the backstop",
                    SAFETY_SEND_RETRY_DELAYS.len() + 1
                );
                PttRelease::Undelivered
            }
        }
    }

    /// The unkey is handed off (or nothing was keyed): stamp the PTT-off
    /// time, clear the keyed flag, and stand the `Drop` fallback down.
    fn stand_down(&mut self) {
        self.last_ptt_off_ms
            .store(super::super::now_epoch_ms(), Ordering::Release);
        self.ptt_active
            .store(false, std::sync::atomic::Ordering::Release);
        self.armed = false;
    }

    /// Stand the guard down when no PTT-on was enqueued (before the key-down
    /// send, or after a `Denied` one). Every unkey after a key-down uses
    /// [`Self::release`] (PAN-273).
    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }
}

/// PAN-202: the one path every PTT command takes to the rig from the TX
/// worker (single TX, bundle, tune) and the TUI's manual PTT. Synchronous:
/// [`MessageBus::send_rig_command_if`] takes no lock and no await, so the
/// last pre-PTT gate's verdict still holds when the command reaches Hamlib's
/// queue, even while another component registers a bus channel.
/// `authorize` follows `send_message_if`'s synchronous-leaf rule.
pub(super) struct GainTxIntent(std::sync::Arc<std::sync::atomic::AtomicU32>);
impl GainTxIntent {
    pub(super) fn new(counter: std::sync::Arc<std::sync::atomic::AtomicU32>) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self(counter)
    }
}
impl Drop for GainTxIntent {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) fn send_ptt_command(
    message_bus: &MessageBus,
    source: ComponentId,
    state: bool,
    authorize: impl FnOnce() -> bool,
) -> SendIfOutcome {
    send_ptt_command_with_attempt(message_bus, source, state, None, authorize)
}

pub(super) fn send_ptt_command_with_attempt(
    message_bus: &MessageBus,
    source: ComponentId,
    state: bool,
    attempt: Option<std::sync::Arc<super::super::tx_meter::TxDriveAttempt>>,
    authorize: impl FnOnce() -> bool,
) -> SendIfOutcome {
    // One CAS arbitrates gain admission and every key-down enqueue, without
    // introducing a lock or await into the last-gate-to-enqueue path.
    if state {
        match message_bus
            .gain_tx_gate
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                if value & 1 == 0 {
                    value.checked_add(4)
                } else {
                    None
                }
            }) {
            Ok(_) => (),
            Err(_) => return SendIfOutcome::Denied,
        }
    }
    let mut admitted = false;
    let mut command = ComponentMessage::new(
        source,
        ComponentId::Hamlib,
        MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt { state }),
        Instant::now(),
    );
    command.tx_drive_attempt = attempt;
    let outcome = message_bus.send_rig_command_if(command, || {
        if !authorize() {
            return false;
        }
        if !state {
            return true;
        }
        if !message_bus.data_mode.is_ready() {
            return false;
        }
        admitted = message_bus
            .receive_gain
            .admit_ptt(UtcTimestamp::now().as_millisecond());
        admitted
    });
    if admitted && outcome != SendIfOutcome::Enqueued {
        message_bus.receive_gain.ptt_command_consumed();
    }
    if state {
        if outcome == SendIfOutcome::Enqueued {
            message_bus.gain_tx_gate.fetch_or(2, Ordering::AcqRel);
            message_bus.gain_tx_epoch.fetch_add(1, Ordering::AcqRel);
        }
        // Bits 2+ count synchronous enqueue attempts, bit 1 persists until
        // Hamlib acknowledges PTT-off, and bit 0 belongs to gain transitions.
        message_bus.gain_tx_gate.fetch_sub(4, Ordering::AcqRel);
    }
    outcome
}

/// PAN-202 test seam: when armed, parks the worker once between its last
/// pre-PTT gate and the key-down, so a test can hold the bus table across
/// that instant. Unarmed it returns without yielding.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct KeyDownProbe {
    pub(crate) armed: std::sync::atomic::AtomicBool,
    pub(crate) reached: tokio::sync::Notify,
    pub(crate) release: tokio::sync::Notify,
}

#[cfg(test)]
impl KeyDownProbe {
    pub(super) async fn pause_if_armed(&self) {
        if self.armed.swap(false, Ordering::AcqRel) {
            self.reached.notify_one();
            self.release.notified().await;
        }
    }
}

impl Drop for PttGuard {
    fn drop(&mut self) {
        // Always clear keyed state, on every exit path (normal / abort / panic).
        if self.ptt_active.load(Ordering::Acquire) {
            self.last_ptt_off_ms
                .store(super::super::now_epoch_ms(), Ordering::Release);
        }
        self.ptt_active
            .store(false, std::sync::atomic::Ordering::Release);
        if self.armed {
            let bus = self.message_bus.clone();
            // Spawn a task to send PTT-off. This runs even if the parent task
            // was cancelled. PAN-273 (Codex P2, PR #587): its handle goes in
            // the shared slot so the TX worker's next key-down waits for it.
            self.pending_ptt_off.push(tokio::task::spawn(async move {
                match bus
                    .send_message_retrying(ptt_off_message, &SAFETY_SEND_RETRY_DELAYS)
                    .await
                {
                    Ok(true) => {
                        tracing::warn!("PTT GUARD: queued a forced PTT-off for Hamlib after task cancellation")
                    }
                    Ok(false) => tracing::error!(
                        target: "pancetta::tx.ptt",
                        "PTT GUARD: PTT OFF not delivered to Hamlib after {} attempts on drop; Hamlib's PTT watchdog, or at shutdown the direct rigctld PTT-off, is the remaining backstop",
                        SAFETY_SEND_RETRY_DELAYS.len() + 1
                    ),
                    Err(e) => tracing::error!("PTT GUARD: failed to force PTT off on drop: {}", e),
                }
            }));
        }
    }
}
