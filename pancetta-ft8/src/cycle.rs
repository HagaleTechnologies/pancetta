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
//! 3. the PAN-110 merge — `merge_ft8lib_first`.
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
//! The message spans the first 12.64 s (FT8) / 5.04 s (FT4) of a
//! boundary-anchored cycle. The native decoder wants exactly
//! [`WINDOW_SAMPLES`] samples (`decoder.rs::decode_window` documents a 4–5x
//! over-cost when fed a full 15 s buffer), so the cycle API trims or zero-pads
//! to that length before dispatch — the station knowledge an outsider should
//! not need. `time_offset` in the returned messages is slot-boundary-relative
//! (sample 0 = boundary), matching the station's lead-corrected DT.
//!
//! Input shorter than one cycle is zero-padded (documented, no panic); empty
//! input is an error; longer input is truncated to the first
//! [`WINDOW_SAMPLES`] samples.

use crate::ap::ApContext;
use crate::decoder::{Ft8Config, Ft8Decoder};
use crate::merge::merge_ft8lib_first;
use crate::message::DecodedMessage;
use crate::protocol::Protocol;
use crate::{Ft8Error, Ft8Result, WINDOW_SAMPLES};

/// Reusable cycle decoder: decode consecutive cycles through one instance so
/// internal buffers are reused across slots.
///
/// Stateless across cycles by construction: every [`Self::decode_cycle`] call
/// runs the full two-pass composition on the caller's samples alone; nothing
/// from a previous cycle influences the next (see
/// `cycle_decoder_is_stateless_across_cycles`).
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
    /// zero-padded to [`WINDOW_SAMPLES`]; longer input is truncated to the
    /// first [`WINDOW_SAMPLES`] samples; empty input is an error.
    pub fn decode_cycle(&mut self, samples: &[f32]) -> Ft8Result<Vec<DecodedMessage>> {
        if samples.is_empty() {
            return Err(Ft8Error::InsufficientData {
                needed: 1,
                available: 0,
            });
        }

        // Trim / zero-pad to the native decoder's exact window: the message
        // spans the first WINDOW_SAMPLES of a boundary-anchored cycle and the
        // Costas t0 sweep over-costs on longer buffers (decoder.rs docs).
        let window: Vec<f32> = if samples.len() >= WINDOW_SAMPLES {
            samples[..WINDOW_SAMPLES].to_vec()
        } else {
            let mut padded = samples.to_vec();
            padded.resize(WINDOW_SAMPLES, 0.0);
            padded
        };

        // Pass 1: ft8_lib C reference decoder (protocol-aware, so the same
        // call covers FT4 cycles).
        let ft8lib_messages = Ft8Decoder::decode_window_ft8lib_protocol(&window, self.protocol);

        // Pass 2: native AP decoder with the neutral context —
        // `ApContext::default()`, no narrow filter, and an unlimited budget
        // (the plain scoped entry point delegates with
        // `DecodeBudget::unlimited()`, matching the station's quiet-band
        // behaviour with no active QSOs).
        let native_messages = self.decoder.decode_window_with_ap_scoped(
            &window,
            &ApContext::default(),
            None,
        )?;

        // Pass 3: the production merge — ft8_lib's set first, then any
        // native-only (e.g. AP-injected) decode it missed.
        Ok(merge_ft8lib_first(ft8lib_messages, native_messages).messages)
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
