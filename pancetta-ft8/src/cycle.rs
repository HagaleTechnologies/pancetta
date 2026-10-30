//! Cycle-level FT8/FT4 decoding — the standalone entry point (PAN-176).
//!
//! [`decode_cycle`] accepts one full protocol cycle of 12 kHz mono audio —
//! FT8: 180 000 samples (15.0 s), FT4: 90 000 samples (7.5 s) — with sample 0
//! at the slot boundary, and returns the same decode set pancetta's station
//! gets for that audio. It is a composition of pieces that already live in
//! this crate; no station, audio-device, rig-control or network code is
//! involved.
//!
//! # Composition (station parity)
//!
//! The station coordinator (`pancetta/src/coordinator/ft8.rs`, primary
//! decode path around lines 1196–1362) runs, per window:
//!
//! 1. the ft8_lib C reference pass —
//!    `Ft8Decoder::decode_window_ft8lib_protocol(&window, protocol)`
//!    (protocol-aware: FT4 slots pass `Protocol::Ft4`);
//! 2. the native AP pass —
//!    `decode_window_with_ap_scoped_partner_budgeted(&window, &ap_context,
//!    narrow_filter_bins, partner_freq, decode_budget)`;
//! 3. the PAN-110 merge — `merge_ft8lib_first`;
//! 4. the station's post-merge unresolved-hash twin suppression —
//!    `drop_unresolved_hash_twins` (the coordinator applies the same pass
//!    to its delivered set, `pancetta/src/coordinator/ft8.rs`).
//!
//! Everything station-specific in step 2 (`ApContext` from live QSO state,
//! the hb-229/230 narrow filter, the wall-clock `DecodeBudget`) is QSO- and
//! load-derived. A consumer with no QSO state gets the station's quiet-band
//! behaviour: neutral `ApContext::default()`, no filter (`None`), unlimited
//! budget (the plain `decode_window_with_ap_scoped` entry point). The FT4
//! native pass mirrors the coordinator: the decoder is (re)built from a
//! config whose `protocol` matches the slot (`DecoderRebuildKey::protocol`
//! triggers the rebuild), so both passes run for FT4 exactly as for FT8.
//!
//! # Window geometry
//!
//! The two passes read differently sized views of the same cycle:
//!
//! * the C reference pass reads the **full cycle** (FT8 180 000 / FT4
//!   90 000 samples). It is a sliding-frame monitor that handles any
//!   length, and the tail of the cycle is real signal: a transmission
//!   starting late in the slot extends past 12.64 s, and the ft8_lib C
//!   decode of the committed `basicft8/live_now.wav` fixture demonstrably
//!   loses `VO1AE KA3KQO FM29` when the cycle is truncated at
//!   [`WINDOW_SAMPLES`] (the station, likewise, retains audio through
//!   emission time — its `boundary_anchored_slice` window ends at the
//!   decode trigger, not at the 12.64 s message span);
//! * the native AP pass reads the first [`WINDOW_SAMPLES`] samples
//!   (zero-padded when short). `decode_window` documents a 4–5x over-cost
//!   when fed a longer buffer, so the cycle API keeps that pass bounded.
//!
//! `time_offset` in the returned messages is slot-boundary-relative
//! (sample 0 = boundary), matching the station's lead-corrected DT.
//!
//! Input shorter than one cycle is zero-padded (documented, no panic); empty
//! input is an error; longer input is truncated to the first full cycle.

use crate::ap::ApContext;
use crate::decoder::{Ft8Config, Ft8Decoder};
use crate::merge::{drop_unresolved_hash_twins, merge_ft8lib_first};
use crate::message::DecodedMessage;
use crate::protocol::{Protocol, ProtocolParams};
use crate::{Ft8Error, Ft8Result, SAMPLE_RATE, WINDOW_SAMPLES};

/// Trim or zero-pad `samples` to exactly `len` samples.
fn fit_to_len(samples: &[f32], len: usize) -> Vec<f32> {
    if samples.len() >= len {
        samples[..len].to_vec()
    } else {
        let mut padded = samples.to_vec();
        padded.resize(len, 0.0);
        padded
    }
}

/// Reusable cycle decoder: decode consecutive cycles through one instance so
/// internal buffers are reused across slots.
///
/// One cross-cycle carry exists by design: the native pass's callsign-hash
/// table learns from every decode (`decoder.rs`), so a later cycle through
/// the same instance can resolve a hashed callsign that a fresh instance
/// renders as `<...>` — the same learned-table behaviour the station's
/// long-lived decoder has. The post-merge unresolved-hash twin suppression
/// (see [`Self::decode_cycle`]) keeps that difference from ever surfacing as
/// duplicate decodes of one physical signal.
///
/// # Examples
///
/// ```
/// use pancetta_ft8::{CycleDecoder, Protocol};
///
/// let mut decoder = CycleDecoder::new(Protocol::Ft8)?;
/// let silence: Vec<f32> = vec![0.0; 180_000];
/// let messages = decoder.decode_cycle(&silence)?;
/// assert!(messages.is_empty());
/// # Ok::<(), pancetta_ft8::Ft8Error>(())
/// ```
pub struct CycleDecoder {
    protocol: Protocol,
    decoder: Ft8Decoder,
}

impl CycleDecoder {
    /// Build a cycle decoder for one protocol (default features of this
    /// crate only; see the README's decoder contract for the dependency
    /// guarantee).
    pub fn new(protocol: Protocol) -> Ft8Result<Self> {
        let config = Ft8Config {
            protocol,
            ..Ft8Config::default()
        };
        let decoder = Ft8Decoder::new(config)?;
        Ok(Self { protocol, decoder })
    }

    /// Decode one full cycle of 12 kHz mono audio and return the
    /// station-equivalent decode set (see the module docs for the exact
    /// composition and tolerances).
    ///
    /// `samples[0]` is the slot boundary; the FT8 cycle is 180 000 samples
    /// (15.0 s) and the FT4 cycle 90 000 samples (7.5 s). Shorter input is
    /// zero-padded to the cycle length; longer input is truncated to the
    /// first full cycle; empty input is an error.
    pub fn decode_cycle(&mut self, samples: &[f32]) -> Ft8Result<Vec<DecodedMessage>> {
        if samples.is_empty() {
            return Err(Ft8Error::InsufficientData {
                needed: 1,
                available: 0,
            });
        }

        // Pass 1 view: the FULL cycle. The ft8_lib C reference decoder is a
        // sliding-frame monitor with no fixed-window contract, and the tail
        // of the cycle (samples past WINDOW_SAMPLES) is real signal for
        // transmissions that start late in the slot — truncating before
        // this pass loses recall (basicft8/live_now.wav loses
        // `VO1AE KA3KQO FM29`).
        let cycle_len = ProtocolParams::from_protocol(self.protocol).window_samples(SAMPLE_RATE);
        let cycle = fit_to_len(samples, cycle_len);

        // Pass 2 view: the native decoder's exact window — the message
        // spans the first WINDOW_SAMPLES of a boundary-anchored cycle and
        // the Costas t0 sweep over-costs on longer buffers (decoder.rs
        // docs), so only this pass stays bounded. Derive it from the
        // selected cycle so FT4 never reads audio from the next slot.
        let window = fit_to_len(&cycle, WINDOW_SAMPLES);

        // Pass 1: ft8_lib C reference decoder (protocol-aware, so the same
        // call covers FT4 cycles).
        let ft8lib_messages = Ft8Decoder::decode_window_ft8lib_protocol(&cycle, self.protocol);

        // Pass 2: native AP decoder with the neutral context —
        // `ApContext::default()`, no narrow filter, and an unlimited budget
        // (the plain scoped entry point delegates with
        // `DecodeBudget::unlimited()`, matching the station's quiet-band
        // behaviour with no active QSOs).
        let native_messages =
            self.decoder
                .decode_window_with_ap_scoped(&window, &ApContext::default(), None)?;

        // Pass 3: the production merge — ft8_lib's set first, then any
        // native-only (e.g. AP-injected) decode it missed.
        let merged = merge_ft8lib_first(ft8lib_messages, native_messages).messages;

        // Pass 4: the station's post-merge unresolved-hash twin suppression
        // (the coordinator runs the same pass on its delivered set). The
        // C pass resolves no hashed callsigns (its FFI hash callbacks are
        // no-ops), while the native pass resolves callsigns its hash table
        // has learned — including from earlier cycles on a reused
        // `CycleDecoder` — so the exact-text merge above can deliver both
        // the `<...>` and the resolved rendering of one physical signal.
        // The cycle API has no AP-eval partition, so running directly on
        // the merged set matches the station's delivered-set semantics.
        let (kept, _dropped) = drop_unresolved_hash_twins(merged, self.protocol);
        Ok(kept)
    }
}

/// Decode one full cycle of 12 kHz mono audio (FT8: 180 000 samples / 15 s,
/// FT4: 90 000 samples / 7.5 s, sample 0 at the slot boundary) and return the
/// same decode set pancetta's station gets for that audio.
///
/// Convenience wrapper: it constructs a fresh [`CycleDecoder`] per call.
/// Decode continuously through a held [`CycleDecoder`] instead to reuse
/// buffers across slots.
///
/// # Examples
///
/// ```
/// use pancetta_ft8::{decode_cycle, Protocol};
///
/// // One 15-second FT8 cycle of 12 kHz mono f32 samples, sample 0 at the
/// // slot boundary.
/// let samples: Vec<f32> = vec![0.0; 180_000];
/// for m in decode_cycle(&samples, Protocol::Ft8)? {
///     println!("[{:>5.1} dB] {} ({} Hz, dt {:.2}s)",
///              m.snr_db, m.text, m.frequency_offset, m.time_offset);
/// }
/// # Ok::<(), pancetta_ft8::Ft8Error>(())
/// ```
pub fn decode_cycle(samples: &[f32], protocol: Protocol) -> Ft8Result<Vec<DecodedMessage>> {
    CycleDecoder::new(protocol)?.decode_cycle(samples)
}
