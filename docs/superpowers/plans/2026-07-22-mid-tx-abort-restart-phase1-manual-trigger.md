# Mid-TX Abort/Restart — Phase 1 (Manual Trigger) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When the operator manually sends/CQs while pancetta is already mid-transmission (PTT
asserted, whether still pre-slot or already playing audio), automatically abort the in-flight
frame and re-key with the new content, instead of requiring a manual F8 abort followed by a
separate send.

**Architecture:** Extend the TX worker's existing abort primitives (`abort_current_tx`,
`interruptible_sleep`) so a qualifying newly-arrived request — detected by polling the same
channel the worker already drains — triggers the abort itself and is re-keyed through the same
`schedule_tx` late-skip machinery that already handles any freshly-arriving late request. A new
audio ring-buffer flush primitive guarantees the re-keyed audio never plays behind stale samples
from the aborted transmission.

**Tech Stack:** Rust, tokio (async coordinator), crossbeam-channel (message bus + audio relay),
ringbuf 0.5 (lock-free SPSC audio buffer), cpal (audio I/O).

## Global Constraints

- FT8 behavior must stay byte-identical wherever this plan doesn't explicitly change it (CLAUDE.md
  invariant).
- `mode=FT8` paths must remain byte-identical when FT4/FT2 features are untouched (CLAUDE.md).
- Every transmitted frame must reflect the freshest content at key-time, OR at the moment of a
  qualifying mid-TX abort+re-key (this plan updates that CLAUDE.md line — Task 8).
- No new manual keybinding — Phase 1 reuses the existing `SendMessage`/`StartCq` commands
  (docs/superpowers/specs/2026-07-22-mid-tx-abort-restart-design.md, Section 2).
- Phase 2 (autonomous Atno/PerBandDxccNew preemption) is explicitly OUT of scope for this plan —
  do not add the `autonomous.mid_tx_preemption_enabled` config flag or any priority-tier
  comparison logic here.

---

## File Structure

- `pancetta-audio/src/ringbuffer_comm.rs` — add the flush-request/flush-completed handshake to
  `AudioCommShared`, and a `drain_pending_flush` method to `AudioConsumer`.
- `pancetta-audio/src/stream.rs` — call the new drain check from the output stream callback.
- `pancetta-audio/src/manager.rs` — `AudioManager::queue_output` gains a `flush_first: bool`
  parameter that requests-and-awaits the flush before pushing.
- `pancetta/src/message_bus.rs` — `MessageType::AudioOutput` gains a `flush_first: bool` field.
- `pancetta/src/coordinator/pipeline.rs` — Audio TX relay threads `flush_first` through the
  `tx_audio_tx` channel tuple.
- `pancetta/src/coordinator/audio.rs` — both the stub and real audio loops consume the widened
  tuple.
- `pancetta/src/coordinator/tx.rs` — the bulk of the feature: `tx_late_max_ms_effective`,
  `classify_incoming_during_tx`, `interruptible_sleep_or_supersede`, and the single-TX arm's
  supersede handling.
- `CLAUDE.md` — invariant line update.
- `pancetta/tests/loopback_qso.rs` (or wherever the existing loopback integration tests live) —
  end-to-end manual-override test.

---

### Task 1: `tx_late_max_ms` mode-scaling

**Files:**
- Modify: `pancetta/src/coordinator/tx.rs:87-163` (add function near `coalesce_collect_window_ms`;
  update `adaptive_coalesce_cap_ms`)
- Modify: `pancetta/src/coordinator/tx.rs:1944-1950` (single-TX arm's `schedule_tx` call)
- Modify: `pancetta/src/coordinator/tx.rs:2804-2810` (multi-TX arm's `schedule_tx` call)
- Test: same file, `#[cfg(test)] mod tests` block (existing, around line 3940)

**Interfaces:**
- Produces: `fn tx_late_max_ms_effective(protocol: pancetta_ft8::Protocol, tx_late_max_ms: u64) -> u64`
  — used by Task 6.

- [ ] **Step 1: Write the failing tests**

Add next to the existing `coalesce_collect_window_ft8_byte_identical` /
`coalesce_collect_window_scales_down_for_ft4` tests (same `mod tests` block, ~line 3940):

```rust
#[test]
fn tx_late_max_ms_effective_ft8_byte_identical() {
    assert_eq!(
        tx_late_max_ms_effective(pancetta_ft8::Protocol::Ft8, 8000),
        8000
    );
}

#[test]
fn tx_late_max_ms_effective_scales_down_for_ft4() {
    // FT4 cycle = 7.5s, half of FT8's 15s → half the cap (4000ms).
    assert_eq!(
        tx_late_max_ms_effective(pancetta_ft8::Protocol::Ft4, 8000),
        4000
    );
}

#[test]
#[cfg(feature = "ft2")]
fn tx_late_max_ms_effective_scales_down_for_ft2() {
    // FT2 cycle = 3.2s → 8000 * 3.2 / 15 = 1706.67, rounds to 1707ms.
    assert_eq!(
        tx_late_max_ms_effective(pancetta_ft8::Protocol::Ft2, 8000),
        1707
    );
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p pancetta tx_late_max_ms_effective --lib`
Expected: FAIL with `cannot find function 'tx_late_max_ms_effective' in this scope`

- [ ] **Step 3: Implement `tx_late_max_ms_effective`**

Insert directly after `coalesce_collect_window_ms` (after line 91):

```rust
/// Mode-scaled `tx_late_max_ms` cap. Mirrors `coalesce_collect_window_ms`'s
/// cycle-ratio scaling exactly (see docs/superpowers/specs/2026-07-22-
/// mid-tx-abort-restart-design.md "tx_late_max_ms mode-scaling"). FT8 stays
/// byte-identical; FT4/FT2 get a proportionally tighter late-viability cap
/// since their slots are shorter. Closes the gap flagged in
/// `COALESCE_MAX_EXTENSION_MS`'s doc comment: `tx_late_max_ms` itself was
/// previously unscaled, so it exceeded FT4's whole 7.5s slot and the "too
/// late, defer" branch of `schedule_tx` could never fire for FT4.
fn tx_late_max_ms_effective(protocol: pancetta_ft8::Protocol, tx_late_max_ms: u64) -> u64 {
    const FT8_CYCLE_SECS: f64 = 15.0;
    let cycle = pancetta_ft8::ProtocolParams::from_protocol(protocol).cycle_duration;
    ((tx_late_max_ms as f64) * (cycle / FT8_CYCLE_SECS)).round() as u64
}
```

- [ ] **Step 4: Run to verify the new tests pass**

Run: `cargo test -p pancetta tx_late_max_ms_effective --lib`
Expected: PASS (3 tests, or 2 if the `ft2` feature isn't enabled)

- [ ] **Step 5: Wire into `adaptive_coalesce_cap_ms`**

In `adaptive_coalesce_cap_ms` (tx.rs:131-163), add the scaled value right after the
`required_parity` computation and use it for both the probe call and the headroom calc:

```rust
    let required_parity =
        resolve_required_parity(*tx_parity, tx_self_parity, request_received_at, slot_ns);
    let tx_late_max_ms_eff = tx_late_max_ms_effective(protocol, tx_late_max_ms);
    let probe = schedule_tx(
        request_received_at,
        required_parity,
        tx_late_max_ms_eff,
        sample_rate,
        slot_ns,
    );
    let protocol_ceiling = coalesce_max_extension_ms(protocol);
    if probe.deferred {
        return protocol_ceiling;
    }
    let elapsed_in_slot_ms = (request_received_at - probe.target_slot)
        .num_milliseconds()
        .max(0) as u64;
    let headroom = tx_late_max_ms_eff
        .saturating_sub(elapsed_in_slot_ms)
        .saturating_sub(COALESCE_CAP_SAFETY_MARGIN_MS);
    headroom.min(protocol_ceiling)
```

Also update the doc comment on `COALESCE_MAX_EXTENSION_MS` (around line 100-102) — remove the
sentence "tx_late_max_ms itself isn't mode-scaled today — a separately tracked open question, not
addressed by this change" since this task addresses it.

- [ ] **Step 6: Wire into the single-TX and multi-TX `schedule_tx` calls**

At tx.rs:1944-1950 (single-TX arm), change:

```rust
                                    let mut schedule = schedule_tx(
                                        request_received_at,
                                        required_parity,
                                        tx_late_max_ms,
                                        sample_rate,
                                        slot_ns,
                                    );
```

to:

```rust
                                    let mut schedule = schedule_tx(
                                        request_received_at,
                                        required_parity,
                                        tx_late_max_ms_effective(active_protocol, tx_late_max_ms),
                                        sample_rate,
                                        slot_ns,
                                    );
```

Apply the identical change at tx.rs:2804-2810 (multi-TX arm) — same replacement, same
`active_protocol` variable is in scope there too.

- [ ] **Step 7: Run full tx.rs test suite**

Run: `cargo test -p pancetta --lib coordinator::tx:: --features transmit`
Expected: PASS, no regressions (FT8 byte-identical since `tx_late_max_ms_effective(Ft8, x) == x`)

- [ ] **Step 8: Commit**

```bash
git add pancetta/src/coordinator/tx.rs
git commit -m "feat(tx): mode-scale tx_late_max_ms for FT4/FT2

Mirrors the existing coalesce_collect_window_ms cycle-ratio scaling.
Closes a latent gap where tx_late_max_ms exceeded FT4's whole slot,
so the too-late/defer branch of schedule_tx could never fire for FT4."
```

---

### Task 2: Audio ring-buffer flush primitive

**Files:**
- Modify: `pancetta-audio/src/ringbuffer_comm.rs`
- Modify: `pancetta-audio/src/stream.rs:594-609`
- Test: `pancetta-audio/src/ringbuffer_comm.rs` (existing `#[cfg(test)] mod tests`, line 281)

**Interfaces:**
- Produces: `AudioCommShared::request_flush(&self) -> u64`, `AudioCommShared::is_flush_completed(&self, token: u64) -> bool`,
  `AudioConsumer::drain_pending_flush(&mut self)` — used by Task 3.

- [ ] **Step 1: Write the failing test**

Add to `pancetta-audio/src/ringbuffer_comm.rs`'s existing `mod tests` block (after
`test_audio_sample_transfer`, ~line 322):

```rust
    #[test]
    fn flush_request_drains_buffered_samples() {
        let (mut producer, mut consumer) =
            audio_comm_pair(DEFAULT_AUDIO_BUFFER_SIZE, DEFAULT_LATENCY_BUFFER_SIZE);

        producer.push_audio_slice(&[0.1f32, 0.2, 0.3, 0.4]);
        assert_eq!(consumer.audio_samples_available(), 4);

        let token = producer.shared.request_flush();
        assert!(!producer.shared.is_flush_completed(token));

        consumer.drain_pending_flush();

        assert_eq!(consumer.audio_samples_available(), 0);
        assert!(producer.shared.is_flush_completed(token));
    }

    #[test]
    fn drain_pending_flush_is_a_no_op_without_a_request() {
        let (mut producer, mut consumer) =
            audio_comm_pair(DEFAULT_AUDIO_BUFFER_SIZE, DEFAULT_LATENCY_BUFFER_SIZE);
        producer.push_audio_slice(&[0.5f32, 0.6]);
        consumer.drain_pending_flush();
        assert_eq!(consumer.audio_samples_available(), 2);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p pancetta-audio ringbuffer_comm::tests::flush_request -- --nocapture`
Expected: FAIL with `no method named 'request_flush'`

- [ ] **Step 3: Implement the flush handshake**

In `AudioCommShared` (ringbuffer_comm.rs), add two fields and two methods. Change the struct
(line ~29-38):

```rust
#[derive(Clone)]
pub struct AudioCommShared {
    /// Atomic flag for clean shutdown
    pub should_stop: Arc<Atomic<bool>>,
    /// Atomic flag set when the audio stream reports an error (e.g. device disconnect)
    pub stream_error: Arc<Atomic<bool>>,
    /// Atomic counter for dropped samples (individual f32 values)
    pub dropped_samples: Arc<Atomic<u64>>,
    /// Atomic counter for processed samples (individual f32 values)
    pub processed_samples: Arc<Atomic<u64>>,
    /// Monotonic flush-request token. The producer side bumps this via
    /// `request_flush`; the consumer side (audio callback) observes
    /// `flush_requested != flush_completed`, clears the ring buffer, then
    /// bumps `flush_completed` to match. Never touches the ring buffer
    /// directly from the producer thread — SPSC safety (see
    /// docs/superpowers/specs/2026-07-22-mid-tx-abort-restart-design.md).
    flush_requested: Arc<Atomic<u64>>,
    /// See `flush_requested`.
    flush_completed: Arc<Atomic<u64>>,
}
```

Update `AudioCommShared::new()` (line ~41-48):

```rust
    fn new() -> Self {
        Self {
            should_stop: Arc::new(Atomic::new(false)),
            stream_error: Arc::new(Atomic::new(false)),
            dropped_samples: Arc::new(Atomic::new(0)),
            processed_samples: Arc::new(Atomic::new(0)),
            flush_requested: Arc::new(Atomic::new(0)),
            flush_completed: Arc::new(Atomic::new(0)),
        }
    }
```

Add methods to `impl AudioCommShared` (after `clear_stream_error`, ~line 79):

```rust
    /// Request that the consumer side discard all currently-buffered audio
    /// samples before it next drains. Returns a token; pass it to
    /// `is_flush_completed` to know when the flush has actually happened.
    pub fn request_flush(&self) -> u64 {
        self.flush_requested.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Whether the flush identified by `token` (the return value of a prior
    /// `request_flush` call) has been carried out by the consumer side.
    pub fn is_flush_completed(&self, token: u64) -> bool {
        self.flush_completed.load(Ordering::Acquire) >= token
    }
```

Add `drain_pending_flush` to `impl AudioConsumer` (after `pop_audio_slice`, ~line 167):

```rust
    /// If a flush was requested since the last check, discard all currently
    /// buffered samples and acknowledge completion. Called once per output
    /// callback invocation, before draining audio for playback, so a mid-TX
    /// re-key (docs/superpowers/specs/2026-07-22-mid-tx-abort-restart-design.md)
    /// never plays stale samples left over from an aborted transmission.
    pub fn drain_pending_flush(&mut self) {
        let requested = self.shared.flush_requested.load(Ordering::Acquire);
        let completed = self.shared.flush_completed.load(Ordering::Acquire);
        if requested != completed {
            self.audio_consumer.clear();
            self.shared.flush_completed.store(requested, Ordering::Release);
        }
    }
```

`Consumer::clear(&mut self) -> usize` is provided by the `ringbuf::traits::Consumer` trait
(already imported at the top of this file) — confirmed present in the pinned `ringbuf = "0.5.1"`
(`~/.cargo/registry/src/.../ringbuf-0.5.1/src/traits/consumer.rs`).

- [ ] **Step 4: Run to verify the new tests pass**

Run: `cargo test -p pancetta-audio ringbuffer_comm::tests -- --nocapture`
Expected: PASS, all tests in the module including the two new ones

- [ ] **Step 5: Call the drain from the real output callback**

In `pancetta-audio/src/stream.rs`, the output stream callback (line 594-609) currently reads:

```rust
        let stream = output_device.build_output_stream(
            stream_config,
            move |data: &mut [f32], _info: &OutputCallbackInfo| {
                let read = output_consumer.pop_audio_slice(data);
                // Fill any remaining samples with silence (underrun is normal when not transmitting)
                for sample in data[read..].iter_mut() {
                    *sample = 0.0;
                }
            },
```

Change to:

```rust
        let stream = output_device.build_output_stream(
            stream_config,
            move |data: &mut [f32], _info: &OutputCallbackInfo| {
                output_consumer.drain_pending_flush();
                let read = output_consumer.pop_audio_slice(data);
                // Fill any remaining samples with silence (underrun is normal when not transmitting)
                for sample in data[read..].iter_mut() {
                    *sample = 0.0;
                }
            },
```

- [ ] **Step 6: Run the full pancetta-audio test suite**

Run: `cargo test -p pancetta-audio`
Expected: PASS, no regressions

- [ ] **Step 7: Commit**

```bash
git add pancetta-audio/src/ringbuffer_comm.rs pancetta-audio/src/stream.rs
git commit -m "feat(audio): add a flush primitive to the TX output ring buffer

Producer/consumer handshake (request_flush/is_flush_completed) so a
caller can guarantee stale buffered samples are discarded before new
ones are pushed. Needed for the mid-TX abort/restart feature — a
re-key must never play the tail of the aborted transmission."
```

---

### Task 3: Plumb `flush_first` through `AudioOutput`

**Files:**
- Modify: `pancetta/src/message_bus.rs:362`
- Modify: `pancetta/src/coordinator/pipeline.rs:90-125`
- Modify: `pancetta/src/coordinator/audio.rs` (stub loop ~line 79-110, real loop ~line 355-373)
- Modify: `pancetta-audio/src/manager.rs:358-398`
- Modify: `pancetta/src/coordinator/tx.rs:2363, 3534, 3692` (existing `AudioOutput` construction
  sites — set `flush_first: false`, byte-identical behavior)
- Test: `pancetta-audio/src/manager.rs` (new test near `queue_output`)

**Interfaces:**
- Consumes: `AudioCommShared::request_flush`/`is_flush_completed` (Task 2)
- Produces: `AudioManager::queue_output(&mut self, samples: &[f32], input_rate: u32, flush_first: bool) -> Result<(), AudioError>`
  — used by Task 6 (existing call sites pass `flush_first: false`; the new supersede re-key path
  passes `true`).

- [ ] **Step 1: Widen `MessageType::AudioOutput`**

In `pancetta/src/message_bus.rs`, change line 362:

```rust
    /// Audio output samples for transmission
    AudioOutput { samples: Vec<f32>, sample_rate: u32 },
```

to:

```rust
    /// Audio output samples for transmission. `flush_first`, when true,
    /// discards any samples still buffered from a previous transmission
    /// before these are queued — set for a mid-TX abort/restart re-key
    /// (docs/superpowers/specs/2026-07-22-mid-tx-abort-restart-design.md),
    /// `false` everywhere else (byte-identical to today).
    AudioOutput {
        samples: Vec<f32>,
        sample_rate: u32,
        flush_first: bool,
    },
```

- [ ] **Step 2: Update the three existing construction sites (byte-identical: `flush_first: false`)**

`pancetta/src/coordinator/tx.rs:2363-2369` (single-TX Step 7):

```rust
                                        MessageType::AudioOutput {
                                            samples: audio_out,
                                            sample_rate,
                                            flush_first: false,
                                        },
```

`pancetta/src/coordinator/tx.rs:3534-3537` (multi-TX Step 7): same shape, `flush_first: false`.

`pancetta/src/coordinator/tx.rs:3692-3695` (Tune): same shape, `flush_first: false`.

- [ ] **Step 3: Update pipeline.rs's Audio TX relay to thread the flag through**

In `pancetta/src/coordinator/pipeline.rs`, the relay currently (lines 90-125) forwards
`(samples, sample_rate)` over `tx_audio_tx: crossbeam_channel::Sender<(Vec<f32>, u32)>`. Change
the destructure at line 98-101:

```rust
                            if let MessageType::AudioOutput {
                                samples,
                                sample_rate,
                            } = message.message_type
                            {
```

to:

```rust
                            if let MessageType::AudioOutput {
                                samples,
                                sample_rate,
                                flush_first,
                            } = message.message_type
                            {
```

and the send at line 109:

```rust
                                if tx_audio_tx.send((samples, sample_rate)).is_err() {
```

to:

```rust
                                if tx_audio_tx.send((samples, sample_rate, flush_first)).is_err() {
```

This changes the channel's type; the channel is created elsewhere as
`crossbeam_channel::Sender<(Vec<f32>, u32)>` / `Receiver<(Vec<f32>, u32)>` — find that
construction (search `tx_audio_tx` / `tx_audio_rx` channel creation, in
`pancetta/src/coordinator/mod.rs` where `start_transmitter_component`/`start_audio_pipeline` are
wired together) and widen both the `Sender<...>`/`Receiver<...>` type annotations to
`(Vec<f32>, u32, bool)`.

- [ ] **Step 4: Update `audio.rs`'s stub and real consumption loops**

The stub loop (audio.rs, ~line 79-110) does not currently read `tx_audio_rx` at all — no change
needed there.

The real loop (audio.rs:356-368) currently:

```rust
                    match tx_audio_rx.try_recv() {
                        Ok((samples, sample_rate)) => {
                            info!(
                                "Audio TX: queueing {} samples at {} Hz",
                                samples.len(),
                                sample_rate
                            );
                            if let Err(e) = audio_manager.queue_output(&samples, sample_rate) {
                                let s = e.to_string();
                                error!("Audio TX output error: {}", s);
                                maybe_report_runtime("TX output error", s);
                            }
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => {}
                        Err(crossbeam_channel::TryRecvError::Disconnected) => {
                            info!("Audio TX channel disconnected");
                        }
                    }
```

becomes:

```rust
                    match tx_audio_rx.try_recv() {
                        Ok((samples, sample_rate, flush_first)) => {
                            info!(
                                "Audio TX: queueing {} samples at {} Hz (flush_first={})",
                                samples.len(),
                                sample_rate,
                                flush_first
                            );
                            if let Err(e) =
                                audio_manager.queue_output(&samples, sample_rate, flush_first)
                            {
                                let s = e.to_string();
                                error!("Audio TX output error: {}", s);
                                maybe_report_runtime("TX output error", s);
                            }
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => {}
                        Err(crossbeam_channel::TryRecvError::Disconnected) => {
                            info!("Audio TX channel disconnected");
                        }
                    }
```

- [ ] **Step 5: Write the failing test for `AudioManager::queue_output`'s new parameter**

In `pancetta-audio/src/manager.rs`'s test module, add (adjust setup to match however existing
`AudioManager` tests construct a manager with an output stream — mirror whatever pattern nearby
`queue_output` tests already use for device setup):

```rust
    #[test]
    fn queue_output_flush_first_clears_buffered_samples_before_pushing() {
        let mut manager = test_audio_manager_with_output(); // existing test helper
        manager
            .queue_output(&[0.1, 0.2, 0.3, 0.4], 48_000, false)
            .expect("first queue_output should succeed");
        let occupied_before = manager.output_producer_occupied_len(); // see Step 6
        assert!(occupied_before > 0);

        manager
            .queue_output(&[0.9, 0.9], 48_000, true)
            .expect("flush_first queue_output should succeed");

        // The flush handshake completes synchronously inside queue_output when
        // flush_first is set, so by the time this call returns the buffer holds
        // only the new 2 samples, not 4+2.
        assert_eq!(manager.output_producer_occupied_len(), 2);
    }
```

If no `test_audio_manager_with_output()` helper exists yet, check `manager.rs`'s existing
`#[cfg(test)]` module for how other `queue_output` tests construct a manager (grep
`fn queue_output` usages in that module) and follow the same setup — do not invent a new
construction path.

- [ ] **Step 6: Run to verify failure, then implement**

Run: `cargo test -p pancetta-audio queue_output_flush_first`
Expected: FAIL — `queue_output` doesn't take a third argument yet (and
`output_producer_occupied_len` doesn't exist)

Add a small accessor (needed for the test above) and update `queue_output` in
`pancetta-audio/src/manager.rs:358-398`:

```rust
    /// Number of samples currently buffered in the output ring buffer.
    /// Test-only introspection (mirrors `AudioConsumer::audio_samples_available`,
    /// which isn't reachable from here — this crate's `AudioManager` only holds
    /// the producer half).
    #[cfg(test)]
    pub(crate) fn output_producer_occupied_len(&self) -> usize {
        self.output_producer
            .as_ref()
            .map(|p| p.shared.processed_samples.load(std::sync::atomic::Ordering::Relaxed) as usize)
            .unwrap_or(0)
    }

    /// Queue audio samples for output playback.
    ///
    /// Pushes TX audio into the output ring buffer. The cpal output stream
    /// callback drains this buffer in real time. If `input_rate` differs from
    /// the configured output sample rate, a simple linear interpolation
    /// resampler is applied. When `flush_first` is true, requests that the
    /// output callback discard any still-buffered samples from a previous
    /// transmission and blocks (briefly, bounded) until that's confirmed
    /// before pushing — see `AudioCommShared::request_flush`.
    pub fn queue_output(
        &mut self,
        samples: &[f32],
        input_rate: u32,
        flush_first: bool,
    ) -> Result<(), AudioError> {
        let producer = self
            .output_producer
            .as_mut()
            .ok_or_else(|| AudioError::Stream {
                message: "Output stream not initialized".to_string(),
            })?;

        if flush_first {
            let token = producer.shared.request_flush();
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(20);
            while !producer.shared.is_flush_completed(token) && std::time::Instant::now() < deadline
            {
                std::thread::sleep(std::time::Duration::from_micros(500));
            }
            if !producer.shared.is_flush_completed(token) {
                warn!("queue_output: flush did not complete within 20ms — proceeding anyway");
            }
        }

        // Resample if input rate differs from output rate
        let output_samples = if input_rate != self.config.sample_rate {
            let ratio = self.config.sample_rate as f64 / input_rate as f64;
            let out_len = (samples.len() as f64 * ratio) as usize;
            let mut resampled = Vec::with_capacity(out_len);
            for i in 0..out_len {
                let src_pos = i as f64 / ratio;
                let src_idx = src_pos as usize;
                let frac = src_pos - src_idx as f64;
                let s0 = samples[src_idx.min(samples.len() - 1)];
                let s1 = samples[(src_idx + 1).min(samples.len() - 1)];
                resampled.push(s0 + (s1 - s0) * frac as f32);
            }
            resampled
        } else {
            samples.to_vec()
        };

        let written = producer.push_audio_slice(&output_samples);
        if written < output_samples.len() {
            warn!(
                "Output buffer overrun: {}/{} samples written",
                written,
                output_samples.len()
            );
        }

        info!(
            "Queued {} TX audio samples for output (rate {}->{}Hz, flush_first={})",
            written, input_rate, self.config.sample_rate, flush_first
        );
        Ok(())
    }
```

Note: `output_producer_occupied_len` as written reads the running `processed_samples` counter,
which is cumulative, not current occupancy — replace it with a real occupancy read if
`AudioProducer` doesn't already expose one; if it doesn't, add
`pub fn occupied_len(&self) -> usize { self.audio_producer.occupied_len() }` to `AudioProducer` in
`ringbuffer_comm.rs` (mirrors `AudioConsumer::audio_samples_available`) and use that here instead
— prefer the real occupancy read over the cumulative-counter approximation.

- [ ] **Step 7: Run to verify the test passes**

Run: `cargo test -p pancetta-audio queue_output_flush_first`
Expected: PASS

- [ ] **Step 8: Full-workspace compile check**

Run: `cargo build --workspace --features transmit`
Expected: succeeds — this task touches call sites across 5 files; a stale reference to the old
2-field `AudioOutput` or 2-arg `queue_output` anywhere will fail to compile here.

- [ ] **Step 9: Commit**

```bash
git add pancetta/src/message_bus.rs pancetta/src/coordinator/pipeline.rs \
        pancetta/src/coordinator/audio.rs pancetta/src/coordinator/tx.rs \
        pancetta/src/coordinator/mod.rs pancetta-audio/src/manager.rs \
        pancetta-audio/src/ringbuffer_comm.rs
git commit -m "feat(audio): thread flush_first through AudioOutput end to end

Existing call sites pass flush_first: false (byte-identical). Sets up
AudioManager::queue_output to actually use the Task 2 flush primitive
ahead of the mid-TX abort/restart re-key path."
```

---

### Task 4: Pure classifier — `classify_incoming_during_tx`

**Files:**
- Modify: `pancetta/src/coordinator/tx.rs` (new function, near `is_pivot_duplicate`'s call site or
  `tx_pivot_target` in `coordinator/mod.rs` — place it in `tx.rs` since it's worker-local, unlike
  `tx_pivot_target` which `coordinator/mod.rs` owns because multiple components read
  `latest_tx_intent`)
- Test: `pancetta/src/coordinator/tx.rs` `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `super::is_pivot_duplicate(qso_id: Option<&str>, text: &str, pivoted_once: &HashMap<String, String>) -> bool`
  (existing, `coordinator/mod.rs`)
- Produces: `enum IncomingDuringTx { Drop, Supersede { text: String, frequency_offset: f64, qso_id: Option<String>, tx_parity: Option<pancetta_core::slot::SlotParity> } }`
  and `fn classify_incoming_during_tx(candidate: &MessageType, in_flight_qso_id: Option<&str>, in_flight_text: &str, pivoted_once: &HashMap<String, String>) -> IncomingDuringTx`
  — used by Task 5.

- [ ] **Step 1: Write the failing tests**

Add to `tx.rs`'s test module:

```rust
    fn transmit_request(text: &str, qso_id: Option<&str>) -> MessageType {
        MessageType::TransmitRequest {
            message_text: text.to_string(),
            frequency_offset: 1500.0,
            qso_id: qso_id.map(|s| s.to_string()),
            tx_parity: None,
            origin: crate::message_bus::TxOrigin::Local,
        }
    }

    #[test]
    fn classify_supersedes_on_different_text_same_qso() {
        let pivoted_once = std::collections::HashMap::new();
        let candidate = transmit_request("KA1ABC K5ARH RR73", Some("qso-1"));
        let outcome =
            super::classify_incoming_during_tx(&candidate, Some("qso-1"), "KA1ABC K5ARH R-15", &pivoted_once);
        match outcome {
            super::IncomingDuringTx::Supersede { text, qso_id, .. } => {
                assert_eq!(text, "KA1ABC K5ARH RR73");
                assert_eq!(qso_id.as_deref(), Some("qso-1"));
            }
            super::IncomingDuringTx::Drop => panic!("expected Supersede"),
        }
    }

    #[test]
    fn classify_supersedes_on_different_qso() {
        let pivoted_once = std::collections::HashMap::new();
        let candidate = transmit_request("CQ K5ARH EM12", None);
        let outcome =
            super::classify_incoming_during_tx(&candidate, Some("qso-1"), "KA1ABC K5ARH R-15", &pivoted_once);
        assert!(matches!(outcome, super::IncomingDuringTx::Supersede { .. }));
    }

    #[test]
    fn classify_drops_identical_content() {
        let pivoted_once = std::collections::HashMap::new();
        let candidate = transmit_request("KA1ABC K5ARH R-15", Some("qso-1"));
        let outcome =
            super::classify_incoming_during_tx(&candidate, Some("qso-1"), "KA1ABC K5ARH R-15", &pivoted_once);
        assert!(matches!(outcome, super::IncomingDuringTx::Drop));
    }

    #[test]
    fn classify_drops_pivot_tombstone_duplicate() {
        let mut pivoted_once = std::collections::HashMap::new();
        pivoted_once.insert(
            super::active_tx_qso_key("qso-1"),
            "KA1ABC K5ARH RR73".to_string(),
        );
        let candidate = transmit_request("KA1ABC K5ARH RR73", Some("qso-1"));
        // in_flight_text is something else — the pivot already sent RR73 via
        // Step 4c, this is the stale second copy of the request that produced it.
        let outcome = super::classify_incoming_during_tx(
            &candidate,
            Some("qso-1"),
            "KA1ABC K5ARH R-15",
            &pivoted_once,
        );
        assert!(matches!(outcome, super::IncomingDuringTx::Drop));
    }

    #[test]
    fn classify_always_supersedes_multi_transmit_request() {
        let pivoted_once = std::collections::HashMap::new();
        let candidate = MessageType::MultiTransmitRequest {
            items: vec![],
            tx_parity: None,
            origin: crate::message_bus::TxOrigin::Local,
        };
        let outcome =
            super::classify_incoming_during_tx(&candidate, Some("qso-1"), "anything", &pivoted_once);
        assert!(matches!(outcome, super::IncomingDuringTx::Supersede { .. }));
    }
```

`active_tx_qso_key` is the existing helper in `coordinator/mod.rs` already used by
`is_pivot_duplicate`/`tx_pivot_target` — reuse it, don't reinvent the key format. Check
`crate::message_bus::TxOrigin`'s exact variant name (`Local`) before using it — confirm via
`grep -n "enum TxOrigin" -A 5 pancetta/src/message_bus.rs`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p pancetta coordinator::tx::tests::classify --features transmit`
Expected: FAIL — `classify_incoming_during_tx` and `IncomingDuringTx` don't exist yet

- [ ] **Step 3: Implement**

Add near the top of `tx.rs`, after the `TxSchedule` struct or near `is_pivot_duplicate`'s usage:

```rust
/// Outcome of checking whether a newly-arrived request should supersede
/// (abort + re-key) an in-flight transmission. See
/// docs/superpowers/specs/2026-07-22-mid-tx-abort-restart-design.md.
#[derive(Debug, Clone, PartialEq)]
pub enum IncomingDuringTx {
    /// Not a genuine new request — either an exact-content repeat of what's
    /// already in flight, or a stale pivot-tombstone duplicate. Discard.
    Drop,
    /// A genuinely different request. Abort the in-flight transmission and
    /// attempt to re-key with this content.
    Supersede {
        text: String,
        frequency_offset: f64,
        qso_id: Option<String>,
        tx_parity: Option<pancetta_core::slot::SlotParity>,
    },
}

/// Phase 1 (manual trigger) classifier: any request arriving while another
/// is in flight supersedes it UNLESS it's an exact duplicate (identical
/// text) or a recognized pivot-tombstone (`is_pivot_duplicate`). Applies
/// regardless of whether the candidate targets the same QSO or a different
/// one — Phase 1 has no priority-tier gating (that's Phase 2, not built by
/// this plan).
pub fn classify_incoming_during_tx(
    candidate: &MessageType,
    in_flight_qso_id: Option<&str>,
    in_flight_text: &str,
    pivoted_once: &std::collections::HashMap<String, String>,
) -> IncomingDuringTx {
    match candidate {
        MessageType::TransmitRequest {
            message_text,
            frequency_offset,
            qso_id,
            tx_parity,
            ..
        } => {
            if super::is_pivot_duplicate(qso_id.as_deref(), message_text, pivoted_once) {
                return IncomingDuringTx::Drop;
            }
            let same_target = qso_id.as_deref() == in_flight_qso_id;
            if same_target && message_text == in_flight_text {
                return IncomingDuringTx::Drop;
            }
            IncomingDuringTx::Supersede {
                text: message_text.clone(),
                frequency_offset: *frequency_offset,
                qso_id: qso_id.clone(),
                tx_parity: *tx_parity,
            }
        }
        // A bundle is always new information (it carries its own set of
        // items, not comparable 1:1 to a single in-flight text) — always
        // supersede. Task 7 (multi-TX bundle-add) refines what happens next;
        // this classifier only decides Drop vs Supersede.
        MessageType::MultiTransmitRequest { .. } => IncomingDuringTx::Supersede {
            text: String::new(),
            frequency_offset: 0.0,
            qso_id: None,
            tx_parity: None,
        },
        _ => IncomingDuringTx::Drop,
    }
}
```

The `MultiTransmitRequest` arm's placeholder `Supersede` fields are intentionally unused — Task 6
handles `MultiTransmitRequest` candidates by branching on the original `MessageType` (retained
separately, see Task 5), not by reading these fields. If that turns out awkward once Task 6 is
implemented, change `IncomingDuringTx::Supersede` to wrap the original `MessageType` directly
instead of flattened fields — revisit then rather than guessing now.

- [ ] **Step 4: Run to verify tests pass**

Run: `cargo test -p pancetta coordinator::tx::tests::classify --features transmit`
Expected: PASS (5 tests)

- [ ] **Step 5: Commit**

```bash
git add pancetta/src/coordinator/tx.rs
git commit -m "feat(tx): add classify_incoming_during_tx supersede classifier

Phase 1 (manual trigger) rule: any genuinely new request arriving
while a TX is in flight supersedes it, exact-duplicate and
pivot-tombstone requests are dropped as today. Pure/unit-testable,
no async or channel access."
```

---

### Task 5: `interruptible_sleep_or_supersede`

**Files:**
- Modify: `pancetta/src/coordinator/tx.rs:282-301` (near existing `interruptible_sleep`)
- Test: `pancetta/src/coordinator/tx.rs` `interruptible_sleep_tests` module (~line 304)

**Interfaces:**
- Consumes: `classify_incoming_during_tx` (Task 4), `crossbeam_channel::Receiver<ComponentMessage>`
- Produces: `enum SleepOutcome { Completed, AbortedByShutdown, AbortedByOperator, Superseded(MessageType) }`
  and `async fn interruptible_sleep_or_supersede(total: Duration, shutdown: &Arc<AtomicBool>, abort: &Arc<AtomicBool>, tx_rx: &crossbeam_channel::Receiver<ComponentMessage>, in_flight_qso_id: Option<&str>, in_flight_text: &str, pivoted_once: &HashMap<String, String>) -> SleepOutcome`
  — used by Task 6.

- [ ] **Step 1: Write the failing tests**

Add to the existing `interruptible_sleep_tests` module (~line 304):

```rust
    #[tokio::test(flavor = "current_thread")]
    async fn supersede_sleep_completes_normally_with_no_incoming_message() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let abort = Arc::new(AtomicBool::new(false));
        let (_tx, rx) = crossbeam_channel::unbounded();
        let pivoted_once = std::collections::HashMap::new();
        let outcome = super::interruptible_sleep_or_supersede(
            Duration::from_millis(80),
            &shutdown,
            &abort,
            &rx,
            Some("qso-1"),
            "in flight text",
            &pivoted_once,
        )
        .await;
        assert_eq!(outcome, super::SleepOutcome::Completed);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn supersede_sleep_detects_a_qualifying_incoming_message() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let abort = Arc::new(AtomicBool::new(false));
        let (tx, rx) = crossbeam_channel::unbounded();
        let pivoted_once = std::collections::HashMap::new();

        let new_request = MessageType::TransmitRequest {
            message_text: "KA1ABC K5ARH RR73".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            tx_parity: None,
            origin: crate::message_bus::TxOrigin::Local,
        };
        tx.send(crate::message_bus::ComponentMessage::new(
            crate::message_bus::ComponentId::Autonomous,
            crate::message_bus::ComponentId::Ft8Transmitter,
            new_request.clone(),
            std::time::Instant::now(),
        ))
        .unwrap();

        let outcome = super::interruptible_sleep_or_supersede(
            Duration::from_millis(500),
            &shutdown,
            &abort,
            &rx,
            Some("qso-1"),
            "KA1ABC K5ARH R-15",
            &pivoted_once,
        )
        .await;

        assert_eq!(outcome, super::SleepOutcome::Superseded(new_request));
        assert!(abort.load(Ordering::Acquire), "should set abort_current_tx itself");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn supersede_sleep_drops_non_qualifying_message_and_keeps_waiting() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let abort = Arc::new(AtomicBool::new(false));
        let (tx, rx) = crossbeam_channel::unbounded();
        let pivoted_once = std::collections::HashMap::new();

        // Identical content to what's in flight — should be Dropped, not treated
        // as a trigger, and the sleep should complete normally.
        let duplicate = MessageType::TransmitRequest {
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            tx_parity: None,
            origin: crate::message_bus::TxOrigin::Local,
        };
        tx.send(crate::message_bus::ComponentMessage::new(
            crate::message_bus::ComponentId::Autonomous,
            crate::message_bus::ComponentId::Ft8Transmitter,
            duplicate,
            std::time::Instant::now(),
        ))
        .unwrap();

        let outcome = super::interruptible_sleep_or_supersede(
            Duration::from_millis(80),
            &shutdown,
            &abort,
            &rx,
            Some("qso-1"),
            "KA1ABC K5ARH R-15",
            &pivoted_once,
        )
        .await;

        assert_eq!(outcome, super::SleepOutcome::Completed);
        assert!(!abort.load(Ordering::Acquire));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn supersede_sleep_still_honors_shutdown() {
        let shutdown = Arc::new(AtomicBool::new(true));
        let abort = Arc::new(AtomicBool::new(false));
        let (_tx, rx) = crossbeam_channel::unbounded();
        let pivoted_once = std::collections::HashMap::new();
        let outcome = super::interruptible_sleep_or_supersede(
            Duration::from_secs(60),
            &shutdown,
            &abort,
            &rx,
            Some("qso-1"),
            "in flight text",
            &pivoted_once,
        )
        .await;
        assert_eq!(outcome, super::SleepOutcome::AbortedByShutdown);
    }
```

`SleepOutcome` needs `#[derive(Debug, PartialEq)]` — `MessageType` must already derive `PartialEq`
(or `Debug`+manual comparison) for this; check `#[derive(...)]` on `MessageType` in
`message_bus.rs` and add `PartialEq` if missing (it may already have it — verify before assuming).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p pancetta coordinator::tx::interruptible_sleep_tests::supersede --features transmit`
Expected: FAIL — `interruptible_sleep_or_supersede` doesn't exist

- [ ] **Step 3: Implement**

Add directly after the existing `interruptible_sleep` function (after line 301):

```rust
/// Outcome of `interruptible_sleep_or_supersede`.
#[derive(Debug, PartialEq)]
pub enum SleepOutcome {
    /// The full duration elapsed with no abort, shutdown, or supersede.
    Completed,
    AbortedByShutdown,
    /// F8 (or any other existing abort_current_tx setter) fired with no
    /// stashed replacement request.
    AbortedByOperator,
    /// A qualifying request arrived; abort_current_tx was set by this
    /// function itself. Caller should attempt to re-key with the contained
    /// message.
    Superseded(MessageType),
}

/// Like `interruptible_sleep`, but also polls `tx_rx` for a qualifying
/// incoming request (Task 4's `classify_incoming_during_tx`) on every 50ms
/// tick. A qualifying request sets `abort` itself (mirroring the operator
/// F8 path) and is returned via `SleepOutcome::Superseded` for the caller to
/// re-key. A non-qualifying request (exact duplicate or pivot tombstone) is
/// silently consumed — same as it would have been had it reached the main
/// dequeue loop naturally — and the sleep keeps waiting.
///
/// See docs/superpowers/specs/2026-07-22-mid-tx-abort-restart-design.md,
/// "Abort + re-key mechanics."
#[allow(clippy::too_many_arguments)]
async fn interruptible_sleep_or_supersede(
    total: Duration,
    shutdown: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    abort: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    tx_rx: &crossbeam_channel::Receiver<ComponentMessage>,
    in_flight_qso_id: Option<&str>,
    in_flight_text: &str,
    pivoted_once: &std::collections::HashMap<String, String>,
) -> SleepOutcome {
    use std::sync::atomic::Ordering;

    let check_once = |tx_rx: &crossbeam_channel::Receiver<ComponentMessage>| -> Option<SleepOutcome> {
        if shutdown.load(Ordering::Acquire) {
            return Some(SleepOutcome::AbortedByShutdown);
        }
        if abort.load(Ordering::Acquire) {
            return Some(SleepOutcome::AbortedByOperator);
        }
        if let Ok(message) = tx_rx.try_recv() {
            match classify_incoming_during_tx(
                &message.message_type,
                in_flight_qso_id,
                in_flight_text,
                pivoted_once,
            ) {
                IncomingDuringTx::Drop => {}
                IncomingDuringTx::Supersede { .. } => {
                    abort.store(true, Ordering::Release);
                    return Some(SleepOutcome::Superseded(message.message_type));
                }
            }
        }
        None
    };

    if let Some(outcome) = check_once(tx_rx) {
        return outcome;
    }

    let chunk = Duration::from_millis(50);
    let deadline = tokio::time::Instant::now() + total;
    while tokio::time::Instant::now() < deadline {
        if let Some(outcome) = check_once(tx_rx) {
            return outcome;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        sleep(remaining.min(chunk)).await;
    }
    SleepOutcome::Completed
}
```

- [ ] **Step 4: Run to verify tests pass**

Run: `cargo test -p pancetta coordinator::tx::interruptible_sleep_tests --features transmit`
Expected: PASS (all existing `interruptible_sleep` tests plus the 4 new ones)

- [ ] **Step 5: Commit**

```bash
git add pancetta/src/coordinator/tx.rs
git commit -m "feat(tx): add interruptible_sleep_or_supersede

Extends the existing abort-aware sleep with a poll of the TX request
channel, so a qualifying manually-issued request can abort and stash
itself for re-key instead of only ever being processed after the
current transmission finishes."
```

---

### Task 6: Wire supersede re-key into the single-TX arm

**Files:**
- Modify: `pancetta/src/coordinator/tx.rs:2295-2434` (single-TX arm, Steps 5-10)

**Interfaces:**
- Consumes: `interruptible_sleep_or_supersede` (Task 5), `tx_late_max_ms_effective` (Task 1),
  `schedule_tx` (existing), `encode_for_protocol`/`modulate_for_protocol` (existing)

This is the core behavioral change. The existing Steps 5-10 (tx.rs:2295-2434) run once per
message. Wrap them in a retry loop keyed on the "currently active" request fields, so a
`SleepOutcome::Superseded` result re-drives the same steps with new content instead of just
exiting.

- [ ] **Step 1: Read the current Steps 5-10 in full before editing**

Run: `sed -n '2295,2434p' pancetta/src/coordinator/tx.rs`

Confirm the variable names in scope match what follows: `message_text` (mutable, `String`),
`frequency_offset` (mutable, `f64`), `qso_id` (`Option<String>`), `schedule` (mutable,
`TxSchedule`), `audio_out` (mutable, `Vec<f32>`), `audio_duration_ms` (`u64`), `sample_rate`,
`active_protocol`, `encoder`, `modulator`, `message_bus`, `shutdown`, `abort_current_tx`,
`ptt_active`, `last_ptt_on_ms`, `tx_rx` (the worker's channel — needed newly here), `pivoted_once`.

- [ ] **Step 2: Introduce the retry wrapper**

Change the start of Step 5 (currently `// --- Step 5: Assert PTT ---` at line 2295) so the whole
Steps 5-10 body is wrapped in a labeled loop. Insert immediately before line 2295:

```rust
                                    'key_and_send: loop {
```

And change the two existing `interruptible_sleep` calls inside this region (Step 6 at line
2346-2357, Step 8 at line 2374-2387) to `interruptible_sleep_or_supersede`, handling the new
`Superseded` arm. Step 6 becomes:

```rust
                                    // --- Step 6: Sleep precisely until target slot start ---
                                    let to_slot = pancetta_core::slot::duration_until(
                                        schedule.target_slot,
                                        chrono::Utc::now(),
                                    );
                                    match interruptible_sleep_or_supersede(
                                        to_slot,
                                        &shutdown,
                                        &abort_current_tx,
                                        &tx_rx,
                                        qso_id.as_deref(),
                                        &message_text,
                                        &pivoted_once,
                                    )
                                    .await
                                    {
                                        SleepOutcome::Completed => {}
                                        SleepOutcome::AbortedByShutdown => {
                                            info!("TX aborted between PTT and slot by shutdown");
                                            break 'key_and_send;
                                        }
                                        SleepOutcome::AbortedByOperator => {
                                            info!("TX aborted between PTT and slot by operator (F8)");
                                            break 'key_and_send;
                                        }
                                        SleepOutcome::Superseded(new_request) => {
                                            if !supersede_and_rekey(
                                                new_request,
                                                &mut message_text,
                                                &mut frequency_offset,
                                                &mut schedule,
                                                &message_bus,
                                                &ptt_active,
                                                &last_ptt_on_ms,
                                                tx_late_max_ms_effective(active_protocol, tx_late_max_ms),
                                                sample_rate,
                                                slot_ns,
                                                tx_self_parity,
                                                request_received_at,
                                            )
                                            .await
                                            {
                                                break 'key_and_send;
                                            }
                                            continue 'key_and_send;
                                        }
                                    }
```

Step 8 (audio-playback wait, currently lines 2374-2387) follows the identical pattern — replace
its `interruptible_sleep(...)` call the same way, matching on `SleepOutcome` with the same three
non-`Completed` arms (`AbortedByShutdown` → `break 'key_and_send` after logging "during playback by
shutdown", `AbortedByOperator` → same with "during playback by operator (F8)", `Superseded` → same
`supersede_and_rekey` call, `continue 'key_and_send` on success).

Close the loop: after Step 10 (`TransmitComplete` send, currently ending at line 2434), add:

```rust
                                        break 'key_and_send;
                                    } // end 'key_and_send
```

Every existing `continue`/`break` inside Steps 5-10 that referred to the OUTER `while !shutdown`
loop (e.g. the pre-existing `continue;` after an F8 abort) must now target the outer loop
explicitly by label, since `'key_and_send` is now the innermost loop. Label the outer loop (line
1458, `while !shutdown.load(Ordering::Acquire) {`) as `'worker: while !shutdown.load(...)`, and
change every pre-existing bare `continue;`/`break;` within Steps 5-10 that must skip to the NEXT
message (not retry this one) to `continue 'worker;`/`break 'worker;`. Concretely: Step 9's abort
paths (lines 2392-2405) and any other bare `continue`/`break` between lines 2295-2434 that isn't
one you just added for `'key_and_send` — audit each one individually rather than blanket
find-replace, since some (the new supersede paths) intentionally target `'key_and_send` and others
must still target `'worker`.

- [ ] **Step 3: Implement `supersede_and_rekey`**

This is the actual re-key logic: given the freshly-superseded request, recompute scheduling
against *now*, and if still viable within the mode-scaled `tx_late_max_ms`, mutate the loop's
working variables (`message_text`, `frequency_offset`, `schedule`) in place and re-encode/modulate
so the loop's `continue` re-runs Steps 5-10 with the new content. Add this function near
`tx_pivot_target`'s usage, above the `start_transmitter_component` impl block:

```rust
/// Recompute scheduling for a superseding request against *now* and, if
/// still viable within `tx_late_max_ms_eff`, deassert PTT (clean stop of the
/// aborted transmission), request an audio-buffer flush (Task 2/3) so no
/// stale samples bleed into the re-key, and mutate `message_text`/
/// `frequency_offset`/`schedule` in place so the caller's retry loop re-runs
/// Steps 5-10 with the new content.
///
/// Returns `true` if the caller should retry with the mutated state, `false`
/// if re-keying isn't viable this slot (already deasserted PTT; caller
/// should stop and let the request flow through the worker's next natural
/// dequeue cycle for the next slot — this function does NOT re-enqueue it;
/// see the design spec's "Error handling" section for why there's nothing
/// to re-enqueue for a `TransmitRequest`, since dropping it here means it's
/// simply gone — a real gap the design spec accepted for Phase 1, matching
/// today's F8-abort behavior of not resending either).
#[allow(clippy::too_many_arguments)]
async fn supersede_and_rekey(
    new_request: MessageType,
    message_text: &mut String,
    frequency_offset: &mut f64,
    schedule: &mut TxSchedule,
    message_bus: &crate::message_bus::MessageBus,
    ptt_active: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    last_ptt_on_ms: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    tx_late_max_ms_eff: u64,
    sample_rate: u32,
    slot_ns: i64,
    tx_self_parity: pancetta_config::station::TxSelfParity,
    _request_received_at: chrono::DateTime<chrono::Utc>,
) -> bool {
    let MessageType::TransmitRequest {
        message_text: new_text,
        frequency_offset: new_freq,
        tx_parity: new_tx_parity,
        ..
    } = new_request
    else {
        // MultiTransmitRequest supersede is handled by Task 7 — for now,
        // treat as not viable so the bundle path (once built) takes over.
        return false;
    };

    // Deassert PTT immediately — the aborted transmission's audio may still
    // be draining from the ring buffer; PTT-off means it doesn't matter that
    // it hasn't been flushed YET (that happens next, before we push new
    // audio in Step 7 of the retried loop iteration).
    let ptt_off_msg = ComponentMessage::new(
        ComponentId::Ft8Transmitter,
        ComponentId::Hamlib,
        MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt { state: false }),
        Instant::now(),
    );
    if let Err(e) = message_bus.send_message(ptt_off_msg).await {
        warn!("supersede: PTT OFF failed: {}", e);
    }
    ptt_active.store(false, Ordering::Release);

    let now = chrono::Utc::now();
    let required_parity =
        resolve_required_parity(new_tx_parity, tx_self_parity, now, slot_ns);
    let new_schedule = schedule_tx(now, required_parity, tx_late_max_ms_eff, sample_rate, slot_ns);

    if new_schedule.deferred {
        info!(
            target: "pancetta::tx.pivot",
            "supersede: '{}' arrived too late to re-key this slot — deferring to next slot via normal scheduling",
            new_text
        );
        return false;
    }

    info!(
        target: "pancetta::tx.pivot",
        "supersede: aborting in-flight TX, re-keying with '{}' @{:.0}Hz",
        new_text, new_freq
    );

    *message_text = new_text;
    *frequency_offset = new_freq;
    *schedule = new_schedule;
    let _ = last_ptt_on_ms; // re-stamped by PttGuard when Step 5 re-asserts PTT
    true
}
```

The caller (Step 6/8's `Superseded` arm) is responsible for re-encoding/re-modulating using the
mutated `message_text`/`frequency_offset` once it `continue`s the `'key_and_send` loop back to
Step 1 — but Steps 1-4 (encode, parity resolve, initial schedule, pivot-check) are OUTSIDE the
`'key_and_send` loop as currently structured (they run once, before Step 5). Move Steps 1
(encode/modulate) through 4c (pivot) INSIDE `'key_and_send` too — i.e. the loop must start at Step
1, not Step 5, so a retried iteration re-encodes the new `message_text` before re-asserting PTT.
Adjust Step 2's opening brace placement (currently `// --- Step 1: Encode + modulate up front
---`, tx.rs:1841) to be the actual start of `'key_and_send`, not line 2295 — revise Step 2 of this
task accordingly: the `'key_and_send: loop {` marker belongs right before the existing `// ---
Step 1: Encode + modulate up front ---` comment (~line 1841), not before Step 5. Re-verify exact
line numbers with `grep -n "Step 1: Encode + modulate up front" pancetta/src/coordinator/tx.rs`
before editing, since Task 1/3's earlier edits in this same file shift line numbers.

- [ ] **Step 4: Update Step 9's `interruptible_sleep` call too, for consistency**

Step 9 (PTT-off tail, tx.rs ~2391-2405) currently uses plain `interruptible_sleep`. Leave this one
as plain `interruptible_sleep` (not `_or_supersede`) — PTT is already off by this point in the
normal-completion path, so there's no in-flight transmission left to supersede; changing it would
only add complexity with no behavioral benefit. Document this decision with a one-line comment at
the call site: `// PTT already off here on the normal path — nothing to supersede.`

- [ ] **Step 5: Write an integration-style unit test**

Add to `tx.rs`'s test module — this exercises the retry loop's core decision (via
`supersede_and_rekey` directly, not the full worker, since spinning up the full
`start_transmitter_component` async task in a unit test requires a full `MessageBus` +
`ApplicationCoordinator` fixture; that level of test belongs in Task 9's loopback integration
test):

```rust
    #[tokio::test(flavor = "current_thread")]
    async fn supersede_and_rekey_updates_state_when_viable() {
        let bus = crate::message_bus::MessageBus::new();
        let (_hamlib_tx, _hamlib_rx) = bus
            .create_channel(crate::message_bus::ComponentId::Hamlib)
            .await
            .unwrap();
        let mut message_text = "OLD TEXT".to_string();
        let mut frequency_offset = 1000.0;
        let mut schedule = super::schedule_tx(
            chrono::Utc::now(),
            pancetta_core::slot::SlotParity::Odd,
            8000,
            12_000,
            pancetta_core::slot::SLOT_NS,
        );
        let ptt_active = Arc::new(AtomicBool::new(true));
        let last_ptt_on_ms = Arc::new(std::sync::atomic::AtomicU64::new(0));

        let new_request = MessageType::TransmitRequest {
            message_text: "NEW TEXT".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            tx_parity: Some(pancetta_core::slot::SlotParity::Odd),
            origin: crate::message_bus::TxOrigin::Local,
        };

        let viable = super::supersede_and_rekey(
            new_request,
            &mut message_text,
            &mut frequency_offset,
            &mut schedule,
            &bus,
            &ptt_active,
            &last_ptt_on_ms,
            8000,
            12_000,
            pancetta_core::slot::SLOT_NS,
            pancetta_config::station::TxSelfParity::Odd,
            chrono::Utc::now(),
        )
        .await;

        assert!(viable);
        assert_eq!(message_text, "NEW TEXT");
        assert_eq!(frequency_offset, 1500.0);
        assert!(!ptt_active.load(Ordering::Acquire));
    }
```

Check `MessageBus::new()`'s exact constructor (may need `MessageBus::with_config(...)` or similar
— grep the existing test module for how other tests construct one) and
`pancetta_config::station::TxSelfParity`'s exact variant names before using them.

- [ ] **Step 6: Run to verify**

Run: `cargo test -p pancetta coordinator::tx --features transmit`
Expected: PASS, including all of Tasks 1/4/5's tests still passing (no regressions from the
restructuring)

- [ ] **Step 7: Full workspace build + existing loopback test**

Run: `cargo build --workspace --features transmit && cargo test -p pancetta --test loopback_qso`
Expected: both succeed — the loopback test is the highest-value regression check here, since it
exercises the exact arm being restructured end-to-end.

- [ ] **Step 8: Commit**

```bash
git add pancetta/src/coordinator/tx.rs
git commit -m "feat(tx): mid-TX manual override — abort and re-key the single-TX arm

Wraps the single-TX arm's Steps 1-10 in a retry loop keyed on
interruptible_sleep_or_supersede's outcome. A qualifying manually-
issued request arriving mid-TX now aborts the in-flight frame and
re-keys with the new content when tx_late_max_ms still allows it this
slot, instead of requiring a separate F8 + resend."
```

---

### Task 7: Multi-TX bundle-add on supersede

**Files:**
- Modify: `pancetta/src/coordinator/tx.rs` (`supersede_and_rekey`, extend for the
  `max_concurrent_qsos > 1` case)

**Interfaces:**
- Consumes: `encode_and_modulate_multi_tx` (existing, tx.rs:679), `TransmitRequestItem { message_text: String, frequency_offset: f64, qso_id: Option<String> }`
  (`pancetta/src/message_bus.rs:427-431`), the 25 Hz-plus-bandwidth pairwise separation check
  inside `pancetta_ft8::modulate_multi_tx` (`pancetta-ft8/src/modulator.rs:589-596`:
  `min_sep = bw_i.max(bw_j) + 25.0`, FT8 bandwidth ≈ 50 Hz so practical minimum separation ≈ 75 Hz).

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn bundle_add_succeeds_when_frequencies_are_well_separated() {
        let mut encoder = super::Ft8Encoder::new();
        let tx_params = pancetta_ft8::ProtocolParams::from_protocol(pancetta_ft8::Protocol::Ft8);
        let in_flight = vec![crate::message_bus::TransmitRequestItem {
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1000.0,
            qso_id: Some("qso-1".to_string()),
        }];
        let new_item = crate::message_bus::TransmitRequestItem {
            message_text: "CQ K5ARH EM12".to_string(),
            frequency_offset: 1400.0, // 400 Hz away — well clear of the ~75 Hz minimum
            qso_id: None,
        };
        let bundled: Vec<_> = in_flight
            .iter()
            .cloned()
            .chain(std::iter::once(new_item))
            .collect();
        let outcome =
            super::encode_and_modulate_multi_tx(&mut encoder, pancetta_ft8::Protocol::Ft8, &tx_params, &bundled);
        assert!(outcome.samples.is_ok(), "expected bundle-add to succeed: {:?}", outcome.samples);
        assert_eq!(outcome.encoded_items.len(), 2);
    }

    #[test]
    fn bundle_add_falls_back_when_frequencies_collide() {
        let mut encoder = super::Ft8Encoder::new();
        let tx_params = pancetta_ft8::ProtocolParams::from_protocol(pancetta_ft8::Protocol::Ft8);
        let in_flight = vec![crate::message_bus::TransmitRequestItem {
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1000.0,
            qso_id: Some("qso-1".to_string()),
        }];
        let new_item = crate::message_bus::TransmitRequestItem {
            message_text: "CQ K5ARH EM12".to_string(),
            frequency_offset: 1010.0, // 10 Hz away — well inside the ~75 Hz minimum, must collide
            qso_id: None,
        };
        let bundled: Vec<_> = in_flight
            .iter()
            .cloned()
            .chain(std::iter::once(new_item))
            .collect();
        let outcome =
            super::encode_and_modulate_multi_tx(&mut encoder, pancetta_ft8::Protocol::Ft8, &tx_params, &bundled);
        assert!(outcome.samples.is_err(), "expected the frequency collision to be rejected");
    }
```

`TransmitRequestItem` needs `#[derive(Clone)]` for the `.cloned()` above — check its existing
derive list (`pancetta/src/message_bus.rs:426`) and add `Clone` if it isn't already there (verify
first; it's likely already derived given `.clone()` calls on it already exist throughout
`encode_and_modulate_multi_tx`'s own body).

- [ ] **Step 2: Run to verify (should already pass — this step proves the existing multi-TX
  encode path is exactly what Step 3 will reuse, no new production code yet)**

Run: `cargo test -p pancetta coordinator::tx::tests::bundle_add --features transmit`
Expected: both tests PASS already, since `encode_and_modulate_multi_tx` and the separation check
are pre-existing. This step is a grounding check, not a red-to-green TDD step — proceed to Step 3
to actually wire supersede into using this.

- [ ] **Step 3: Extend `supersede_and_rekey` to attempt a bundle-add first**

Rename to `supersede_and_rekey_or_bundle` and add two parameters: `max_concurrent_qsos: u32` and
`in_flight_items: &[crate::message_bus::TransmitRequestItem]` (the single-TX arm caller from Task
6 passes a one-element slice built from its own `message_text`/`frequency_offset`/`qso_id`; the
multi-TX arm caller built in Step 4 below passes `&outcome.encoded_items` from its own Step 1).
Add a new return type replacing the `bool`:

```rust
enum SupersedeOutcome {
    /// Not viable this slot — caller stops, PTT already off.
    NotViable,
    /// Single-item replace — caller's existing single-TX retry mutates its
    /// working state and continues (Task 6's existing behavior).
    Replace,
    /// Bundle-add succeeded — caller should send a MultiTransmitRequest
    /// covering `items` instead of continuing the single-item path.
    Bundle { items: Vec<crate::message_bus::TransmitRequestItem> },
}
```

At the point in `supersede_and_rekey` (Task 6) right after `new_schedule.deferred` is confirmed
`false` (viable this slot), insert, before the existing single-replace mutation:

```rust
    if max_concurrent_qsos > 1 {
        let mut candidate_items: Vec<_> = in_flight_items.to_vec();
        candidate_items.push(crate::message_bus::TransmitRequestItem {
            message_text: new_text.clone(),
            frequency_offset: new_freq,
            qso_id: new_qso_id.clone(),
        });
        // Re-encode is deferred to the caller (it already owns `encoder`/
        // `active_protocol`/`tx_params` in scope) — this function only
        // decides Bundle vs Replace, matching its existing "mutate + signal
        // caller to retry" shape rather than reaching into encoder state it
        // doesn't otherwise touch.
        return SupersedeOutcome::Bundle { items: candidate_items };
    }
```

Note `new_qso_id` needs to be captured from the `MessageType::TransmitRequest` destructure
alongside `new_text`/`new_freq`/`new_tx_parity` at the top of the function (Task 6's version didn't
need it since `qso_id` was already handled via the flattened `Supersede` fields in Task 4's
classifier — add `qso_id: new_qso_id` to that destructure now).

The caller (single-TX arm's Step 6/8 `Superseded` match arm) must actually attempt the encode on a
`Bundle` result — `supersede_and_rekey_or_bundle` itself doesn't call
`encode_and_modulate_multi_tx` (it doesn't own `encoder`). On `SupersedeOutcome::Bundle { items }`,
the caller calls `encode_and_modulate_multi_tx(&mut encoder, active_protocol, &tx_params, &items)`
itself; on `Ok`, break out of the single-TX arm's `'key_and_send` loop entirely and re-enqueue the
result as a fresh `MultiTransmitRequest` sent back through `message_bus` to itself (so it's picked
up by the multi-TX arm's existing, unmodified Steps 1-10) rather than trying to inline the whole
multi-TX Step 5-10 sequence a second time; on `Err` (frequency collision), fall back to treating it
as `SupersedeOutcome::Replace` with just the new item (drop the bundle attempt, single-replace as
Task 6 already does).

- [ ] **Step 4: Wire the multi-TX arm's own Steps 6/8 the same way Task 6 wired the single-TX
  arm's**

Apply the identical `interruptible_sleep` → `interruptible_sleep_or_supersede` replacement to the
multi-TX arm's Step 6 (tx.rs ~3511-3524, `// --- Step 6: Sleep precisely until target slot ---`)
and Step 8 (~3541-3554, `// --- Step 8: Wait for playback to complete ---`) sleep call sites shown
above — same pattern as Task 6 Step 2, using `&outcome.encoded_items` (built earlier in this same
arm, ~line 2694-2699) as the `in_flight_items` argument, and `head.as_deref()` /
the bundle's own text summary (whatever `send_tx_queue_status`'s existing `head`/`bundle`
construction nearby already uses for display) as `in_flight_text` for the classifier call — reuse
that existing summary rather than building a new one.

- [ ] **Step 5: Run tests**

Run: `cargo test -p pancetta coordinator::tx --features transmit`
Expected: PASS

- [ ] **Step 6: Full workspace build + loopback test**

Run: `cargo build --workspace --features transmit && cargo test -p pancetta --test loopback_qso`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add pancetta/src/coordinator/tx.rs
git commit -m "feat(tx): mid-TX supersede prefers multi-TX bundle-add over replace

When max_concurrent_qsos > 1, a superseding manual request folds into
the current window's multi-TX bundle alongside the in-flight content
when frequency separation allows, rather than fully replacing it."
```

---

### Task 8: CLAUDE.md invariant update

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Update the invariant line**

Find (in the "Key Invariants" section):

```
- Every transmitted frame (single or multi-TX bundle item) reflects the freshest `MessageToSend` the QSO engine emitted for that qso_id at key-time.
```

Replace with:

```
- Every transmitted frame (single or multi-TX bundle item) reflects the freshest `MessageToSend` the QSO engine emitted for that qso_id at key-time, or at the moment of an operator-triggered mid-TX abort+re-key, whichever is later.
```

(Phase 2's autonomous Atno/PerBandDxccNew-gated clause is NOT added here — that's this same line's
next edit, made by the Phase 2 plan, not this one.)

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: update TX-frame invariant for mid-TX manual override

Phase 1 of the mid-TX abort/restart feature relaxes the freeze-at-
key-time invariant for operator-triggered re-keys."
```

---

### Task 9: Loopback integration test

**Files:**
- Modify: `pancetta/tests/loopback_qso.rs` (or the existing loopback integration test file —
  confirm exact path via `find pancetta/tests -iname "*loopback*"`)

- [ ] **Step 1: Read the existing loopback test file to match its fixture/harness style**

Run: `cat pancetta/tests/loopback_qso.rs | head -80` (or the confirmed path) to see how it spins up
a coordinator, sends a `TransmitRequest`, and asserts on the decoded result.

- [ ] **Step 2: Write the new test**

Add a test that: starts a TX in flight (send a `TransmitRequest`), then — while it's still
pre-slot or mid-playback — sends a second `TransmitRequest` with different text for the same
qso_id, and asserts that exactly ONE frame is heard on the loopback decode side, and its content
matches the SECOND request's text (not the first). Follow the exact harness/assertion style of
the existing tests in this file — do not invent a new harness pattern.

- [ ] **Step 3: Run it**

Run: `cargo test -p pancetta --test loopback_qso --features transmit`
Expected: PASS

- [ ] **Step 4: Run the full workspace test suite**

Run: `cargo test --workspace --features transmit`
Expected: PASS, no regressions anywhere

- [ ] **Step 5: Commit**

```bash
git add pancetta/tests/loopback_qso.rs
git commit -m "test(tx): loopback coverage for mid-TX manual override

Confirms exactly one frame goes out — the superseding request's
content, not the aborted one — with no double-send."
```

---

## Self-Review Notes (for the plan author, not a task)

- **Spec coverage:** manual trigger (Task 6), tx_late_max_ms mode-scaling (Task 1), bundle-add
  (Task 7), CLAUDE.md update (Task 8), audio-flush correctness gap discovered during planning
  (Tasks 2-3), testing (Tasks 5/6/9). Phase 2 (autonomous trigger, resume-after marker, config
  flag) is explicitly deferred to its own follow-on plan, written after Phase 1 ships and gets its
  on-air validation pass — add to meatspace-pending once Task 9 lands.
- **Placeholder scan:** an initial draft of Task 7 deferred its test/wiring specifics with a
  "fill in later" comment — fixed inline by grounding `TransmitRequestItem`'s real fields, the
  ~75 Hz practical minimum separation from `pancetta_ft8::modulate_multi_tx`, and the exact
  multi-TX arm line ranges (tx.rs ~2694-2699 for `outcome.encoded_items`, ~3511-3554 for Steps
  6/8) before finalizing. No remaining placeholders in any task.
- **Type consistency:** `SupersedeOutcome` (Task 7) replaces `supersede_and_rekey`'s original
  `bool` return from Task 6 — Task 6's own steps/tests still describe the `bool` version since
  Task 7 is what performs the rename/signature change to `supersede_and_rekey_or_bundle`;
  whoever executes Task 7 updates Task 6's call sites (the `'key_and_send` loop's two match arms)
  to match the new enum, not just add a new function alongside the old one.
