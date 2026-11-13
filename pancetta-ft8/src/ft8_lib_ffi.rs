//! FFI bindings to ft8_lib — the reference FT8 implementation.
//!
//! Used for cross-validation: comparing our encoder/decoder output against
//! an independent, well-tested C implementation.

#![allow(non_camel_case_types)]

#[cfg(not(ft8lib_stub))]
use std::ffi::CString;
use std::os::raw::c_char;

/// Safely build a `&str` from a fixed C output buffer, requiring an in-bounds
/// NUL terminator.
///
/// `ft8_lib` is trusted to NUL-terminate its decode-text output within the
/// caller-provided fixed buffer, but a deliberately-crafted hostile frame must
/// never be able to make us read past the buffer. Unlike `CStr::from_ptr`
/// (which scans for a NUL with no length bound and would over-read on a
/// non-terminated buffer), this scans only `buf` and returns `None` if no NUL
/// is present — letting the caller discard that decode instead of triggering
/// undefined behavior. A leading NUL yields `Some("")`.
#[cfg(not(ft8lib_stub))]
fn cstr_from_fixed_buf(buf: &[u8]) -> Option<&str> {
    let nul = buf.iter().position(|&b| b == 0)?; // in-bounds NUL required
    std::str::from_utf8(&buf[..nul]).ok()
}

// Compile-time struct size assertions to match C layout
const _: () = assert!(std::mem::size_of::<ftx_waterfall_t>() == 40);
const _: () = assert!(std::mem::size_of::<monitor_t>() == 112);
const _: () = assert!(std::mem::size_of::<ftx_message_t>() == 12);
const _: () = assert!(std::mem::size_of::<ftx_candidate_t>() == 8);
const _: () = assert!(std::mem::size_of::<ftx_decode_status_t>() == 16);
const _: () = assert!(std::mem::size_of::<monitor_config_t>() == 24);
const _: () = assert!(std::mem::size_of::<ftx_callsign_hash_interface_t>() == 16);

// ============================================================================
// Raw FFI bindings
// ============================================================================

#[repr(C)]
pub struct ftx_message_t {
    pub payload: [u8; 10],
    pub hash: u16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ftx_candidate_t {
    pub score: i16,
    pub time_offset: i16,
    pub freq_offset: i16,
    pub time_sub: u8,
    pub freq_sub: u8,
}

/// One ft8_lib sync-search candidate, lifted out of FFI space as plain data.
///
/// PAN-7: [`ft8lib_find_candidate_seeds`] returns these so callers that never
/// touch the C API can still reason about ft8_lib's candidate positions.
///
/// `score` is ft8_lib's own integer sync score in units of **0.5 dB**
/// (`WF_ELEM_MAG_INT`, `vendor/ft8_lib/ft8/decode.h:26`, MIT). It orders seeds
/// among themselves and is useful as a diagnostic, but it is **not** a pancetta
/// `sync_score` and must never be used as one — the two kernels normalize
/// differently. Callers re-score every seed on pancetta's own spectrogram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ft8LibSeed {
    pub score: i16,
    pub time_offset: i16,
    pub freq_offset: i16,
    pub time_sub: u8,
    pub freq_sub: u8,
}

/// A seed set plus the monitor scalars needed to interpret its coordinates.
///
/// `time_offset` / `freq_offset` are meaningless without the waterfall geometry
/// they were computed against, so the scalars are copied out of `monitor_t`
/// **before** `monitor_free` and travel with the seeds. Consumers must check
/// `time_osr` / `freq_osr` / `block_size` against their own grid before
/// translating: the two grids are set by independent constants that happen to
/// agree today, and on mismatch the right move is to reject rather than emit
/// silently-wrong positions.
///
/// Declared outside any `cfg` so the real and stub builds share one signature.
#[derive(Debug, Clone, Default)]
pub struct Ft8LibSeedSet {
    pub seeds: Vec<Ft8LibSeed>,
    pub min_bin: i32,
    pub time_osr: i32,
    pub freq_osr: i32,
    pub num_bins: i32,
    pub num_blocks: i32,
    pub block_size: i32,
    pub symbol_period: f32,
}

#[repr(C)]
#[derive(Debug)]
pub struct ftx_decode_status_t {
    pub freq: f32,
    pub time: f32,
    pub ldpc_errors: i32,
    pub crc_extracted: u16,
    pub crc_calculated: u16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ftx_protocol_t {
    FTX_PROTOCOL_FT4 = 0,
    FTX_PROTOCOL_FT8 = 1,
}

#[repr(C)]
pub struct ftx_waterfall_t {
    pub max_blocks: i32,          // offset 0
    pub num_blocks: i32,          // offset 4
    pub num_bins: i32,            // offset 8
    pub time_osr: i32,            // offset 12
    pub freq_osr: i32,            // offset 16
    _pad0: u32,                   // offset 20 (padding for pointer alignment)
    pub mag: *mut u8,             // offset 24
    pub block_stride: i32,        // offset 32
    pub protocol: ftx_protocol_t, // offset 36
} // total size: 40

#[repr(C)]
pub struct monitor_config_t {
    pub f_min: f32,
    pub f_max: f32,
    pub sample_rate: i32,
    pub time_osr: i32,
    pub freq_osr: i32,
    pub protocol: ftx_protocol_t,
}

// Opaque monitor struct — layout must match C exactly (112 bytes on 64-bit)
#[repr(C)]
pub struct monitor_t {
    pub symbol_period: f32,              // offset 0
    pub min_bin: i32,                    // offset 4
    pub max_bin: i32,                    // offset 8
    pub block_size: i32,                 // offset 12
    pub subblock_size: i32,              // offset 16
    pub nfft: i32,                       // offset 20
    pub fft_norm: f32,                   // offset 24
    _pad0: u32,                          // offset 28 (padding for pointer alignment)
    pub window: *mut f32,                // offset 32
    pub last_frame: *mut f32,            // offset 40
    pub wf: ftx_waterfall_t,             // offset 48 (size 40)
    pub max_mag: f32,                    // offset 88
    _pad1: u32,                          // offset 92 (padding for pointer alignment)
    pub fft_work: *mut std::ffi::c_void, // offset 96
    pub fft_cfg: *mut std::ffi::c_void,  // offset 104
} // total size: 112

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ftx_message_rc_t {
    FTX_MESSAGE_RC_OK = 0,
    FTX_MESSAGE_RC_ERROR_CALLSIGN1 = 1,
    FTX_MESSAGE_RC_ERROR_CALLSIGN2 = 2,
    FTX_MESSAGE_RC_ERROR_SUFFIX = 3,
    FTX_MESSAGE_RC_ERROR_GRID = 4,
    FTX_MESSAGE_RC_ERROR_TYPE = 5,
}

#[repr(C)]
pub struct ftx_callsign_hash_interface_t {
    pub lookup_hash:
        Option<unsafe extern "C" fn(hash_type: i32, hash: u32, callsign: *mut c_char) -> bool>,
    pub save_hash: Option<unsafe extern "C" fn(callsign: *const c_char, n22: u32)>,
}

/// FTX_MAX_MESSAGE_FIELDS = 3
#[repr(C)]
pub struct ftx_message_offsets_t {
    pub types: [i32; 3], // ftx_field_t enum values
    pub offsets: [i16; 3],
}

// Real FFI symbols — only when the C library was actually compiled.
#[cfg(not(ft8lib_stub))]
unsafe extern "C" {
    // Encoding
    pub fn ft8_encode(payload: *const u8, tones: *mut u8);
    pub fn ftx_message_init(msg: *mut ftx_message_t);
    pub fn ftx_message_encode(
        msg: *mut ftx_message_t,
        hash_if: *mut ftx_callsign_hash_interface_t,
        message_text: *const c_char,
    ) -> ftx_message_rc_t;

    // Decoding
    pub fn ftx_find_candidates(
        power: *const ftx_waterfall_t,
        num_candidates: i32,
        heap: *mut ftx_candidate_t,
        min_score: i32,
    ) -> i32;

    pub fn ftx_decode_candidate(
        power: *const ftx_waterfall_t,
        cand: *const ftx_candidate_t,
        max_iterations: i32,
        message: *mut ftx_message_t,
        status: *mut ftx_decode_status_t,
    ) -> bool;

    pub fn ftx_message_decode(
        msg: *const ftx_message_t,
        hash_if: *mut ftx_callsign_hash_interface_t,
        message: *mut c_char,
        offsets: *mut ftx_message_offsets_t,
    ) -> ftx_message_rc_t;

    // Monitor (audio → waterfall)
    pub fn monitor_init(me: *mut monitor_t, cfg: *const monitor_config_t);
    pub fn monitor_process(me: *mut monitor_t, frame: *const f32);
    pub fn monitor_reset(me: *mut monitor_t);
    pub fn monitor_free(me: *mut monitor_t);
}

// The C struct contains heap pointers but no pointers into its own storage.
// Moving it preserves those allocations. Only this owner calls monitor_free.
#[cfg(not(ft8lib_stub))]
struct MonitorGuard(monitor_t);

#[cfg(not(ft8lib_stub))]
impl MonitorGuard {
    fn new(cfg: &monitor_config_t) -> Self {
        // SAFETY: all fields admit zero, including null raw pointers and protocol
        // discriminant 0 (FT4). monitor_init fills the C-owned fields before use.
        let mut mon: monitor_t = unsafe { std::mem::zeroed() };
        // SAFETY: mon is writable, cfg is live for this synchronous call. Callers
        // use 12 kHz, positive OSRs and an in-Nyquist band; C retains no cfg pointer.
        unsafe { monitor_init(&mut mon, cfg) };
        Self(mon)
    }
}

#[cfg(not(ft8lib_stub))]
impl Drop for MonitorGuard {
    fn drop(&mut self) {
        // SAFETY: self uniquely owns a successfully initialized monitor. All C
        // borrows have ended and this is its only free, including during unwind.
        unsafe { monitor_free(&mut self.0) };
        #[cfg(test)]
        MONITOR_RELEASES.set(MONITOR_RELEASES.get() + 1);
    }
}

#[cfg(all(test, not(ft8lib_stub)))]
thread_local! { static MONITOR_RELEASES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

// ============================================================================
// Hash interface callbacks (no-op — we don't maintain a hash table)
// ============================================================================

#[cfg(not(ft8lib_stub))]
unsafe extern "C" fn noop_lookup_hash(_hash_type: i32, _hash: u32, _callsign: *mut c_char) -> bool {
    false
}

#[cfg(not(ft8lib_stub))]
unsafe extern "C" fn noop_save_hash(_callsign: *const c_char, _n22: u32) {}

#[cfg(not(ft8lib_stub))]
fn make_hash_interface() -> ftx_callsign_hash_interface_t {
    ftx_callsign_hash_interface_t {
        lookup_hash: Some(noop_lookup_hash),
        save_hash: Some(noop_save_hash),
    }
}

// ============================================================================
// Safe Rust wrapper
// ============================================================================

/// Encode a message string to 79 FT8 tones using ft8_lib.
#[cfg(not(ft8lib_stub))]
pub fn ft8lib_encode(message: &str) -> Option<[u8; 79]> {
    let c_msg = CString::new(message).ok()?;
    // SAFETY: All fields are integers, floats or integer arrays, for which zero is valid.
    let mut msg: ftx_message_t = unsafe { std::mem::zeroed() };
    // SAFETY: msg is an exclusive writable C-layout message; C retains no pointer.
    unsafe { ftx_message_init(&mut msg) };

    let mut hash_if = make_hash_interface();

    // SAFETY: msg is writable, c_msg is NUL-terminated and live, and the live hash
    // callbacks do not dereference or retain pointers.
    let rc = unsafe { ftx_message_encode(&mut msg, &mut hash_if, c_msg.as_ptr()) };

    if rc != ftx_message_rc_t::FTX_MESSAGE_RC_OK {
        return None;
    }

    let mut tones = [0u8; 79];
    // SAFETY: The live payload has 10 bytes and the exclusive tones buffer has 79 bytes, as
    // required by C; no pointers are retained.
    unsafe { ft8_encode(msg.payload.as_ptr(), tones.as_mut_ptr()) };

    Some(tones)
}

/// Encode a message string to 10-byte payload using ft8_lib.
#[cfg(not(ft8lib_stub))]
pub fn ft8lib_encode_payload(message: &str) -> Option<[u8; 10]> {
    let c_msg = CString::new(message).ok()?;
    // SAFETY: All fields are integers, floats or integer arrays, for which zero is valid.
    let mut msg: ftx_message_t = unsafe { std::mem::zeroed() };
    // SAFETY: msg is an exclusive writable C-layout message; C retains no pointer.
    unsafe { ftx_message_init(&mut msg) };

    let mut hash_if = make_hash_interface();
    // SAFETY: msg is writable, c_msg is NUL-terminated and live, and the live hash
    // callbacks do not dereference or retain pointers.
    let rc = unsafe { ftx_message_encode(&mut msg, &mut hash_if, c_msg.as_ptr()) };

    if rc != ftx_message_rc_t::FTX_MESSAGE_RC_OK {
        return None;
    }

    Some(msg.payload)
}

/// Decode a 10-byte payload to message text using ft8_lib.
#[cfg(not(ft8lib_stub))]
pub fn ft8lib_decode_payload(payload: &[u8; 10]) -> Option<String> {
    let msg = ftx_message_t {
        payload: *payload,
        hash: 0,
    };

    let mut hash_if = make_hash_interface();
    let mut text_buf = [0u8; 35];
    // SAFETY: All fields are integers, floats or integer arrays, for which zero is valid.
    let mut offsets: ftx_message_offsets_t = unsafe { std::mem::zeroed() };

    // SAFETY: msg and no-op hash callbacks are live; text_buf has 35 writable bytes,
    // offsets is a writable C-layout output, and C retains no pointers.
    let rc = unsafe {
        ftx_message_decode(
            &msg,
            &mut hash_if,
            text_buf.as_mut_ptr() as *mut c_char,
            &mut offsets,
        )
    };

    if rc != ftx_message_rc_t::FTX_MESSAGE_RC_OK {
        return None;
    }

    match cstr_from_fixed_buf(&text_buf) {
        Some(s) => Some(s.trim().to_string()),
        None => {
            tracing::warn!(
                target: "ft8.ffi",
                "ft8_lib decode buffer not NUL-terminated within bounds; discarding decode"
            );
            None
        }
    }
}

/// Number of FSK tones in an FT8 symbol.
#[cfg(not(ft8lib_stub))]
const FT8_NUM_TONES: usize = 8;
/// Number of FT8 data symbols (non-Costas).
#[cfg(not(ft8lib_stub))]
const FT8_ND: usize = 58;

/// Read the dB magnitude of one waterfall cell from the `uint8_t` mag buffer.
///
/// ft8_lib's default build stores magnitudes as `uint8_t` and recovers the
/// dB value as `mag * 0.5 - 120.0` (see `WF_ELEM_MAG` in `ft8/decode.h`).
///
/// # Safety
/// `mag` must point into a live initialized u8 allocation, with `idx` selecting
/// an initialized cell in that allocation. Pointer arithmetic must stay within
/// it, and no concurrent mutation or free may occur during this read.
#[cfg(not(ft8lib_stub))]
#[inline]
unsafe fn wf_mag_db(mag: *const u8, idx: usize) -> f64 {
    // SAFETY: the caller guarantees an initialized in-allocation cell and no mutation.
    f64::from(unsafe { *mag.add(idx) }) * 0.5 - 120.0
}

/// Estimate SNR (dB, referenced to a 2500 Hz noise bandwidth, WSJT-X
/// convention) for one decoded candidate, directly from the ft8_lib
/// waterfall magnitudes.
///
/// This intentionally mirrors pancetta's native
/// `Ft8Decoder::estimate_snr_spectrogram` so the two decode paths report
/// comparable numbers: for each of the 58 FT8 data symbols we take the
/// strongest tone bin (`best`) and weakest tone bin (`worst`) across the 8
/// tones, average each across symbols, and form
/// `snr_bin_db = avg_best - avg_worst`. We then subtract the same
/// bandwidth correction `10*log10(2500/6.25)` that the native path uses
/// to reference the per-bin (6.25 Hz) ratio to a 2500 Hz noise bandwidth.
///
/// The waterfall `mag` buffer is laid out as
/// `uint8_t[blocks][time_osr][freq_osr][num_bins]`. The base offset for a
/// candidate is
/// `((((time_offset*time_osr)+time_sub)*freq_osr)+freq_sub)*num_bins + freq_offset`,
/// and consecutive channel symbols are `block_stride = time_osr*freq_osr*num_bins`
/// elements apart (matching `get_cand_mag` / the symbol loop in `ft8/decode.c`).
/// Data symbol `k` (0..58) sits at channel symbol index `k + (k<29 ? 7 : 14)`,
/// skipping the three Costas sync arrays.
///
/// Returns `None` if the candidate's tone bins or symbol blocks fall
/// outside the waterfall (so the caller can fall back rather than read OOB).
///
/// # Safety
/// `wf` must describe a live monitor-owned waterfall with initialized blocks,
/// unmodified C geometry, and a candidate returned by that waterfall's search.
/// No concurrent writes or free are permitted during this call.
#[cfg(not(ft8lib_stub))]
unsafe fn estimate_snr_from_waterfall(wf: &ftx_waterfall_t, cand: &ftx_candidate_t) -> Option<f64> {
    if wf.mag.is_null() {
        return None;
    }
    let num_bins = wf.num_bins as i64;
    let time_osr = wf.time_osr as i64;
    let freq_osr = wf.freq_osr as i64;
    let block_stride = wf.block_stride as i64;
    let num_blocks = wf.num_blocks as i64;
    if num_bins <= 0 || time_osr <= 0 || freq_osr <= 0 || block_stride <= 0 {
        return None;
    }

    // Base offset of the candidate's first channel symbol (block 0).
    let base = ((((cand.time_offset as i64 * time_osr) + cand.time_sub as i64) * freq_osr)
        + cand.freq_sub as i64)
        * num_bins
        + cand.freq_offset as i64;

    // Accumulate per-bin signal/noise *power* (linear), mirroring the native
    // `snr_from_tone_mags_db`: for each symbol, peak tone = signal+noise, mean
    // of the other 7 = per-bin noise floor.
    let mut signal_power = 0.0f64;
    let mut noise_power = 0.0f64;
    let mut count = 0usize;

    for k in 0..FT8_ND {
        // Channel-symbol index of data symbol k (skip the Costas arrays).
        let sym_idx = (k + if k < 29 { 7 } else { 14 }) as i64;
        let block_abs = cand.time_offset as i64 + sym_idx;
        if block_abs < 0 || block_abs >= num_blocks {
            continue;
        }
        let sym_off = base + sym_idx * block_stride;
        // The 8 tone bins of this symbol must be in range.
        if sym_off < 0 || cand.freq_offset as i64 + (FT8_NUM_TONES as i64) > num_bins {
            continue;
        }
        // Read the 8 tone bins as linear power and find the peak (signal) tone.
        let mut lin = [0.0f64; FT8_NUM_TONES];
        let mut peak = 0.0f64;
        let mut peak_idx = 0usize;
        for (t, slot) in lin.iter_mut().enumerate() {
            // SAFETY: The live C monitor allocated max_blocks * block_stride bytes;
            // processed blocks are initialized. C search bounds subcoordinates by
            // OSRs, and the checks above bound block and tone indices.
            let db = unsafe { wf_mag_db(wf.mag, (sym_off as usize) + t) };
            let p = 10.0f64.powf(db / 10.0);
            *slot = p;
            if p > peak {
                peak = p;
                peak_idx = t;
            }
        }
        // Noise floor: mean of the non-peak tone bins.
        let mut floor_sum = 0.0f64;
        for (t, &p) in lin.iter().enumerate() {
            if t != peak_idx {
                floor_sum += p;
            }
        }
        let floor = floor_sum / (FT8_NUM_TONES as f64 - 1.0);
        signal_power += (peak - floor).max(0.0);
        noise_power += floor;
        count += 1;
    }

    if count == 0 || noise_power <= 0.0 {
        return None;
    }
    let snr_bin_linear = signal_power / noise_power;
    if snr_bin_linear <= 0.0 {
        return Some(-24.0);
    }
    let snr_bin_db = 10.0 * snr_bin_linear.log10();
    // Reference the 6.25 Hz bin ratio to a 2500 Hz noise bandwidth (WSJT-X
    // convention).
    let bw_correction = 10.0 * (2500.0f64 / 6.25).log10();
    let raw = snr_bin_db - bw_correction;
    // Linearity correction, mirroring the native `snr_from_tone_mags_db`. The
    // ft8_lib waterfall is a uint8 dB grid with its own dynamic range, so its
    // raw estimate has a *different* compression than the native spectrogram: a
    // least-squares fit over the operational band (true SNR -19..-3 dB,
    // calibrated white noise referenced to 2500 Hz; see
    // `examples/snr_calibration.rs`) gives raw ≈ 0.756*true - 3.73. Inverting
    // (true ≈ (raw - b)/slope) maps it onto the true WSJT-X 2500 Hz SNR and
    // makes this path report the same number as the native path for the same
    // signal. Level- and message-independent (SNR is a ratio).
    const SLOPE: f64 = 0.756;
    const INTERCEPT: f64 = -3.73;
    let snr = (raw - INTERCEPT) / SLOPE;
    Some(snr.clamp(-24.0, 24.0))
}

/// Decode audio samples to FT8 messages using ft8_lib's full pipeline.
/// Returns a vector of (message_text, frequency_hz, time_sec, ldpc_errors, snr_db).
///
/// `snr_db` is computed from the ft8_lib waterfall magnitudes at the decoded
/// candidate's position using [`estimate_snr_from_waterfall`], which mirrors
/// pancetta's native SNR definition (WSJT-X 2500 Hz reference). If the
/// waterfall position is out of range the value falls back to `-24.0` dB (the
/// same floor the native estimator uses).
///
/// FT8-only, byte-identical to every existing caller (tests, benchmark.rs).
/// See [`ft8lib_decode_audio_protocol`] for the FT4-capable entry point.
#[cfg(not(ft8lib_stub))]
pub fn ft8lib_decode_audio(samples: &[f32]) -> Vec<(String, f32, f32, i32, f32)> {
    ft8lib_decode_audio_protocol(samples, ftx_protocol_t::FTX_PROTOCOL_FT8)
}

/// Decode audio samples using ft8_lib's full pipeline for the given protocol.
///
/// The vendored C library's `monitor_init` (`common/monitor.c`) branches on
/// `cfg.protocol` for slot time, symbol period, and block size — it already
/// supports FT4 correctly; only this Rust wrapper previously hardcoded FT8.
/// 2026-07-06: measured ~25x faster than the native Rust decoder on a real
/// off-air recording (5.6ms vs 144.5ms per window, same message count) —
/// added so the coordinator can use the C decoder for FT4, where decode
/// wall-clock time was eating into the tight timing margin and truncating
/// transmissions (see `docs/` FT4 TX-timing investigation).
#[cfg(not(ft8lib_stub))]
pub fn ft8lib_decode_audio_protocol(
    samples: &[f32],
    protocol: ftx_protocol_t,
) -> Vec<(String, f32, f32, i32, f32)> {
    ft8lib_decode_audio_protocol_with_parser(samples, protocol, None)
        .into_iter()
        .map(|(t, f, dt, e, snr, _)| (t, f, dt, e, snr))
        .collect()
}

/// Production wrapper retaining raw type 0.1 payloads for collision-safe
/// parsing. Other message types keep the reference decoder's text behavior.
#[cfg(not(ft8lib_stub))]
pub fn ft8lib_decode_audio_protocol_with_parser(
    samples: &[f32],
    protocol: ftx_protocol_t,
    parser: Option<&crate::message::MessageParser>,
) -> Vec<(String, f32, f32, i32, f32, Option<crate::Ft8Message>)> {
    let cfg = monitor_config_t {
        f_min: 100.0,
        f_max: 3000.0,
        sample_rate: 12000,
        time_osr: 2,
        freq_osr: 2,
        protocol,
    };

    let mut guard = MonitorGuard::new(&cfg);
    let mon = &mut guard.0;

    // Feed audio in block_size chunks
    let block_size = mon.block_size as usize;
    let mut offset = 0;
    while offset + block_size <= samples.len() {
        // SAFETY: mon is exclusively owned by guard; the loop guarantees block_size
        // live f32 samples, consumed synchronously without retaining the input.
        unsafe { monitor_process(mon, samples[offset..].as_ptr()) };
        offset += block_size;
    }

    // SAFETY: the processed waterfall is owned by guard and stays live and
    // unmutated for the whole call; min_bin/symbol_period are its own monitor's.
    unsafe { decode_candidates_from_waterfall(&mon.wf, mon.min_bin, mon.symbol_period, parser) }
}

/// Owned 0–3000 Hz magnitude grid captured from one reference-pass monitor
/// (PAN-318 AC4). The bytes are copied out of the C monitor's `uint8_t`
/// waterfall before it is freed — no C pointer leaves the call — in the
/// original `[block][time_sub][freq_sub][bin]` layout. A cell's dB value is
/// `mag * 0.5 - 120` (same `WF_ELEM_MAG_INT` quantization as the decode
/// path); frequency is `(min_bin + bin + freq_sub/freq_osr) / symbol_period`.
///
/// Declared outside any `cfg` so the real and stub builds share one type;
/// the stub build can never produce an instance (its capture entry returns
/// `None`).
#[derive(Debug, Clone)]
pub struct Ft8LibDisplayGrid {
    /// `[block][time_sub][freq_sub][bin]` magnitudes, `block_stride` bytes
    /// per block, `num_bins` per (freq_sub, block) row.
    pub mag: Vec<u8>,
    /// Number of processed blocks (one per `block_size` input samples).
    pub num_blocks: usize,
    /// Bins per freq-subrow (481 for the 0–3000 Hz FT8 monitor).
    pub num_bins: usize,
    pub time_osr: usize,
    pub freq_osr: usize,
    /// Monitor `min_bin` (0: the capture monitor spans DC up).
    pub min_bin: usize,
    /// FT8: 0.16 s; FT4: 1/12000*576 s. Sets the frequency grid.
    pub symbol_period: f32,
    pub sample_rate: usize,
    /// Samples per block (FT8: 1920).
    pub block_size: usize,
}

impl Ft8LibDisplayGrid {
    /// Convert the captured grid onto the legacy display grid
    /// (`WaterfallData`) without a second FFT, reproducing
    /// `Ft8Decoder::generate_waterfall_data`'s geometry for
    /// `audio_len` samples: 4096-point FFT partitions averaged into 4
    /// target rows plus any trailing partial row, 0–3000 Hz at
    /// `sample_rate/4096` Hz per bin, per-row time labels from the same
    /// `(fft_idx - frames/2) * hop / sr` formula.
    ///
    /// Powers: quantized dB → linear, averaged over the monitor sub-frames
    /// whose analysis-frame centers fall in each legacy row's span
    /// (startup zero-padding excluded), linearly interpolated in power on
    /// the frequency axis `(min_bin + bin + freq_sub/freq_osr) /
    /// symbol_period`, then rescaled to the legacy PSD convention. The C
    /// window carries the FFT's `2/nfft` gain and the legacy one-sided PSD
    /// doubles positive frequencies, giving a coherent-gain ratio of
    /// `nfft²/2` for both tones and noise (Hann sums ≈ N/2 and squares ≈
    /// 3N/8 for either window length). The result is approximate — window
    /// shapes and lengths differ — and must not be read as calibrated RF
    /// power.
    pub fn to_waterfall_data(&self, audio_len: usize) -> crate::decoder::WaterfallData {
        self.to_waterfall_data_with(audio_len, 4096, 4, 3000.0)
    }

    /// Geometry-parameterized core, so the pin against the legacy
    /// generator's constants is one place. `fft_size`/`target_rows`/`f_max`
    /// are `generate_waterfall_data`'s (4096, 4, 3000 Hz).
    fn to_waterfall_data_with(
        &self,
        audio_len: usize,
        fft_size: usize,
        target_rows: usize,
        f_max_hz: f64,
    ) -> crate::decoder::WaterfallData {
        let empty = || crate::decoder::WaterfallData {
            time_bins: Vec::new(),
            frequency_bins: Vec::new(),
            power_matrix: Vec::new(),
            min_power: 0.0,
            max_power: 0.0,
        };
        if self.num_blocks == 0 || self.num_bins == 0 || self.sample_rate == 0 {
            return empty();
        }

        // ---- legacy row partition arithmetic (generate_waterfall_data) ----
        let window_size = fft_size.min(audio_len);
        if window_size == 0 || audio_len < window_size {
            return empty();
        }
        let hop = fft_size / 4; // legacy hop: window/4 (fft/4 for full windows)
        let num_ffts = (audio_len - window_size) / hop + 1;
        let ffts_per_row = (num_ffts / target_rows).max(1);
        let freq_resolution = self.sample_rate as f64 / fft_size as f64;
        let bin_end = ((f_max_hz / freq_resolution).floor() as usize).min(fft_size / 2);
        let display_bins = bin_end + 1; // bin_start = 0, endpoint included

        // One span per emitted legacy row: the row's time label and the
        // centers of the first/last legacy FFT windows it averages (FFT i
        // covers [i*hop, i*hop+window), centered at (i*hop + window/2)/sr).
        let fft_center = |i: usize| (i * hop + window_size / 2) as f64 / self.sample_rate as f64;
        struct RowSpan {
            label_s: f64,
            first_center_s: f64,
            last_center_s: f64,
        }
        let mut rows: Vec<RowSpan> = Vec::new();
        let mut row_first = 0usize;
        let mut count = 0usize;
        for fft_idx in 0..num_ffts {
            count += 1;
            if count >= ffts_per_row || fft_idx == num_ffts - 1 {
                rows.push(RowSpan {
                    label_s: (fft_idx as f64 - count as f64 / 2.0) * hop as f64
                        / self.sample_rate as f64,
                    first_center_s: fft_center(row_first),
                    last_center_s: fft_center(fft_idx),
                });
                row_first = fft_idx + 1;
                count = 0;
            }
        }
        if rows.is_empty() {
            return empty();
        }

        // Row boundaries at span midpoints; rows keep the legacy label.
        let mut bounds: Vec<f64> = Vec::with_capacity(rows.len() + 1);
        bounds.push(f64::NEG_INFINITY);
        for pair in rows.windows(2) {
            bounds.push((pair[0].last_center_s + pair[1].first_center_s) / 2.0);
        }
        bounds.push(f64::INFINITY);

        // ---- monitor sub-frame geometry ----
        // Sub-frame (b, ts) analyzed the trailing nfft samples ending at
        // sample e = b*block_size + (ts+1)*block_size/time_osr (the C
        // rolling buffer), so its center is e - nfft/2. Frames with
        // e < nfft averaged in zero-padding from the calloc'd buffer and
        // are excluded.
        let nfft = self.block_size * self.freq_osr;
        let subblock = self.block_size / self.time_osr.max(1);
        let axis_len = self.num_bins * self.freq_osr; // sub-bin power axis
        let axis_step_hz = 1.0 / (self.symbol_period as f64 * self.freq_osr as f64); // 3.125 Hz (FT8)
        let block_stride = self.time_osr * self.freq_osr * self.num_bins;

        // The C window folds the 2/nfft FFT gain into its coefficients and
        // the legacy one-sided PSD doubles positive-frequency power; see
        // the method doc.
        let psd_scale = (nfft as f64) * (nfft as f64) / 2.0;

        let floor_db = -120.0; // quantized grid floor (mag = 0)
        let mut power_matrix: Vec<Vec<f64>> = Vec::with_capacity(rows.len());
        let mut time_bins: Vec<f64> = Vec::with_capacity(rows.len());
        let mut min_power = f64::MAX;
        let mut max_power = f64::MIN;

        // Per-row accumulators: a row's image is the average of ITS OWN
        // assigned frames only, never the whole window's.
        let mut accum = vec![0.0f64; rows.len() * axis_len];
        let mut counts = vec![0usize; rows.len()];

        for b in 0..self.num_blocks {
            for ts in 0..self.time_osr {
                let end_sample = b * self.block_size + (ts + 1) * subblock;
                if end_sample < nfft {
                    continue; // startup zero-padding
                }
                let center_s = (end_sample - nfft / 2) as f64 / self.sample_rate as f64;
                // Nearest legacy row by span boundaries.
                let row = match bounds.partition_point(|&x| x <= center_s) {
                    0 => 0,
                    idx => idx - 1,
                };
                let base = b * block_stride + ts * self.freq_osr * self.num_bins;
                let row_acc = &mut accum[row * axis_len..(row + 1) * axis_len];
                for bin in 0..self.num_bins {
                    for fs in 0..self.freq_osr {
                        let cell = base + fs * self.num_bins + bin;
                        // SAFETY-free indexing: mag is an owned Vec of
                        // num_blocks * block_stride bytes, cell is in range.
                        let db = f64::from(self.mag[cell]) * 0.5 - 120.0;
                        row_acc[bin * self.freq_osr + fs] += 10f64.powf(db / 10.0);
                    }
                }
                counts[row] += 1;
            }
        }

        for (row_idx, span) in rows.iter().enumerate() {
            time_bins.push(span.label_s);
            if counts[row_idx] == 0 {
                // No monitor frame covered this row (audio much shorter
                // than a block): emit the floor, keeping values finite.
                power_matrix.push(vec![floor_db; display_bins]);
                min_power = min_power.min(floor_db);
                max_power = max_power.max(floor_db);
                continue;
            }
            let n = counts[row_idx] as f64;
            let row_acc = &accum[row_idx * axis_len..(row_idx + 1) * axis_len];
            let mut row = Vec::with_capacity(display_bins);
            for i in 0..display_bins {
                let f_hz = i as f64 * freq_resolution;
                let pos = f_hz / axis_step_hz;
                let lo = pos.floor();
                let power = if lo >= (axis_len - 1) as f64 {
                    row_acc[axis_len - 1]
                } else {
                    let frac = pos - lo;
                    let lo = lo as usize;
                    row_acc[lo] * (1.0 - frac) + row_acc[lo + 1] * frac
                } / n;
                let db = 10.0 * (power * psd_scale + 1e-12).log10();
                min_power = min_power.min(db);
                max_power = max_power.max(db);
                row.push(db);
            }
            power_matrix.push(row);
        }

        crate::decoder::WaterfallData {
            time_bins,
            frequency_bins: (0..=bin_end).map(|i| i as f64 * freq_resolution).collect(),
            power_matrix,
            min_power,
            max_power,
        }
    }
}

/// PAN-318 AC4 capture entry: decode with the reference pipeline AND return
/// the monitor's owned 0–3000 Hz display grid, so one C FFT supplies both
/// the decode band and the waterfall.
///
/// The monitor spans 0–3000 Hz (display coverage); candidate search and
/// decoding run against an OWNED copy of the original 100–3000 Hz bins in
/// the original `[block][time_sub][freq_sub][bin]` layout with the original
/// min-bin offset, dimensions and strides — the expanded monitor must not
/// widen candidate selection or change candidate priority, so the returned
/// messages are byte-identical to
/// [`ft8lib_decode_audio_protocol_with_parser`]'s.
///
/// FT8-only concern of the caller (the coordinator); the C monitor itself
/// is protocol-aware, so the entry accepts the same protocols as the
/// no-capture one.
#[cfg(not(ft8lib_stub))]
pub fn ft8lib_decode_audio_protocol_with_parser_and_capture(
    samples: &[f32],
    protocol: ftx_protocol_t,
    parser: Option<&crate::message::MessageParser>,
) -> (
    Vec<(String, f32, f32, i32, f32, Option<crate::Ft8Message>)>,
    Option<Ft8LibDisplayGrid>,
) {
    let cfg = monitor_config_t {
        f_min: 0.0, // display spans DC up; the decode band is an owned view below
        f_max: 3000.0,
        sample_rate: 12000,
        time_osr: 2,
        freq_osr: 2,
        protocol,
    };

    let mut guard = MonitorGuard::new(&cfg);
    let mon = &mut guard.0;

    // Feed audio in block_size chunks
    let block_size = mon.block_size as usize;
    let mut offset = 0;
    while offset + block_size <= samples.len() {
        // SAFETY: mon is exclusively owned by guard; the loop guarantees block_size
        // live f32 samples, consumed synchronously without retaining the input.
        unsafe { monitor_process(mon, samples[offset..].as_ptr()) };
        offset += block_size;
    }

    // Owned decode-band view: copy ONLY the original 100–3000 Hz bins,
    // keeping the original layout/min-bin/dimensions/strides so the
    // candidate search sees exactly the grid the no-capture monitor had.
    // Same formulas monitor_init uses (f_min/f_max * symbol_period).
    let symbol_period = mon.symbol_period;
    let decode_min_bin = (100.0f32 * symbol_period) as i32; // FT8: 16
    let decode_max_bin = (3000.0f32 * symbol_period) as i32 + 1; // FT8: 481
    let decode_bins = (decode_max_bin - decode_min_bin) as usize;
    let num_blocks = mon.wf.num_blocks as usize;
    let full_bins = mon.wf.num_bins as usize;
    let time_osr = mon.wf.time_osr as usize;
    let freq_osr = mon.wf.freq_osr as usize;
    let full_stride = mon.wf.block_stride as usize;
    let view_stride = time_osr * freq_osr * decode_bins;
    let mut view = vec![0u8; num_blocks * view_stride];
    for b in 0..num_blocks {
        for ts in 0..time_osr {
            for fs in 0..freq_osr {
                let src =
                    b * full_stride + (ts * freq_osr + fs) * full_bins + decode_min_bin as usize;
                let dst = b * view_stride + (ts * freq_osr + fs) * decode_bins;
                // SAFETY: the monitor's mag allocation holds
                // max_blocks * full_stride initialized bytes (processed
                // blocks are initialized by monitor_process); src and the
                // decode_bins-length read stay inside it for every
                // in-range block/subrow.
                view[dst..dst + decode_bins].copy_from_slice(unsafe {
                    std::slice::from_raw_parts(mon.wf.mag.add(src), decode_bins)
                });
            }
        }
    }
    let view_wf = ftx_waterfall_t {
        max_blocks: num_blocks as i32,
        num_blocks: num_blocks as i32,
        num_bins: decode_bins as i32,
        time_osr: mon.wf.time_osr,
        freq_osr: mon.wf.freq_osr,
        _pad0: 0,
        mag: view.as_mut_ptr(),
        block_stride: view_stride as i32,
        protocol,
    };

    // SAFETY: view_wf points into the owned, initialized `view` buffer,
    // unmutated for the whole call; min_bin/symbol_period match the C
    // formulas above.
    let messages = unsafe {
        decode_candidates_from_waterfall(&view_wf, decode_min_bin, symbol_period, parser)
    };

    // Owned display copy of the full 0–3000 Hz grid, taken before the
    // guard drops: no C pointer leaves the call.
    // SAFETY: processed blocks occupy num_blocks * full_stride initialized
    // bytes of the monitor-owned allocation.
    let mag = unsafe { std::slice::from_raw_parts(mon.wf.mag, num_blocks * full_stride).to_vec() };
    let grid = Ft8LibDisplayGrid {
        mag,
        num_blocks,
        num_bins: full_bins,
        time_osr,
        freq_osr,
        min_bin: mon.min_bin as usize, // 0 for the display monitor
        symbol_period,
        sample_rate: 12000,
        block_size,
    };

    (messages, Some(grid))
}

/// Candidate search + decode over an already-processed waterfall view.
///
/// `wf` must describe a live, processed waterfall in the C mag layout
/// `[block][time_sub][freq_sub][bin]`; `min_bin`/`symbol_period` are the
/// monitor scalars that interpret its coordinates (frequency mapping
/// `(min_bin + freq_offset + freq_sub/freq_osr) / symbol_period`). Shared
/// by the no-capture entry (which passes the monitor's own waterfall) and
/// the PAN-318 capture entry (which passes an owned decode-band view with
/// identical geometry), so the two decode byte-identically.
///
/// # Safety
/// `wf` must point into a live initialized magnitude buffer that stays
/// unmutated for the duration of this call.
#[cfg(not(ft8lib_stub))]
unsafe fn decode_candidates_from_waterfall(
    wf: &ftx_waterfall_t,
    min_bin: i32,
    symbol_period: f32,
    parser: Option<&crate::message::MessageParser>,
) -> Vec<(String, f32, f32, i32, f32, Option<crate::Ft8Message>)> {
    // Find candidates
    let num_candidates = 50;
    let mut candidates = vec![
        ftx_candidate_t {
            score: 0,
            time_offset: 0,
            freq_offset: 0,
            time_sub: 0,
            freq_sub: 0,
        };
        num_candidates
    ];

    let n_found =
        // SAFETY: wf points into a live processed magnitude buffer owned by the
        // caller; the exclusive heap has num_candidates writable entries. C bounds
        // its writes and returned count by that capacity.
        unsafe { ftx_find_candidates(wf, num_candidates as i32, candidates.as_mut_ptr(), 0) };

    // Decode each candidate
    let mut messages = Vec::new();
    let mut hash_if = make_hash_interface();
    // rationale: `i` indexes the C-returned `candidates` buffer up to `n_found`
    // and is used to take `&candidates[i]` for the FFI decode call; the index
    // form mirrors the ft8_lib C API and keeps this hot FFI loop unambiguous.
    #[allow(clippy::needless_range_loop)]
    for i in 0..n_found as usize {
        // SAFETY: All fields are integers, floats or integer arrays, for which zero is valid.
        let mut msg: ftx_message_t = unsafe { std::mem::zeroed() };
        // SAFETY: status contains only floats and integers, all valid when zero.
        let mut status: ftx_decode_status_t = unsafe { std::mem::zeroed() };

        let ok =
            // SAFETY: wf points into a live processed magnitude buffer, candidate
            // came from its search, and msg/status are exclusive C-layout output
            // buffers; C retains no pointers.
            unsafe { ftx_decode_candidate(wf, &candidates[i], 25, &mut msg, &mut status) };

        if ok {
            let mut text_buf = [0u8; 35];
            // SAFETY: All fields are integers, floats or integer arrays, for which zero is valid.
            let mut offsets: ftx_message_offsets_t = unsafe { std::mem::zeroed() };
            // SAFETY: msg and no-op callbacks are live; the exclusive output buffers
            // have the required 35-byte text capacity and C offsets layout.
            let rc = unsafe {
                ftx_message_decode(
                    &msg,
                    &mut hash_if,
                    text_buf.as_mut_ptr() as *mut c_char,
                    &mut offsets,
                )
            };

            let is_dxpedition = ((msg.payload[9] >> 3) & 7) == 0
                && (((msg.payload[8] & 1) << 2) | (msg.payload[9] >> 6)) == 1;
            let dxpedition_message = if is_dxpedition {
                let Some(parser) = parser else {
                    continue;
                };
                let bits: bitvec::vec::BitVec = (0..77)
                    .map(|i| msg.payload[i / 8] & (0x80 >> (i % 8)) != 0)
                    .collect();
                let Ok(decoded) = parser.parse_payload(&bits) else {
                    continue;
                };
                if !parser.is_plausible(&decoded) {
                    continue;
                }
                Some(decoded)
            } else {
                None
            };

            if rc == ftx_message_rc_t::FTX_MESSAGE_RC_OK || dxpedition_message.is_some() {
                let text = if let Some(message) = &dxpedition_message {
                    message.to_string()
                } else {
                    match cstr_from_fixed_buf(&text_buf) {
                        Some(s) => s.trim().to_string(),
                        None => {
                            tracing::warn!(target: "ft8.ffi",
                                "ft8_lib decode buffer not NUL-terminated within bounds; skipping candidate");
                            continue;
                        }
                    }
                };

                // ft8_lib's ftx_decode_candidate never populates
                // status.freq / status.time (upstream behavior — only
                // ldpc_errors and the CRCs are written). Derive both
                // from the candidate exactly like the upstream demo
                // (demo/decode_ft8.c):
                //   freq_hz  = (min_bin + freq_offset + freq_sub/freq_osr) / symbol_period
                //   time_sec = (time_offset + time_sub/time_osr) * symbol_period
                let cand = &candidates[i];
                let freq_hz = (min_bin as f32
                    + cand.freq_offset as f32
                    + cand.freq_sub as f32 / wf.freq_osr as f32)
                    / symbol_period;
                let time_sec = (cand.time_offset as f32
                    + cand.time_sub as f32 / wf.time_osr as f32)
                    * symbol_period;

                // Real SNR from the waterfall at the decoded position; fall
                // back to the native estimator's -24 dB floor when the
                // candidate position is out of the waterfall's range.
                // SAFETY: wf points into the processed magnitude buffer the
                // candidate came from, unchanged since search; no C mutation
                // occurs during SNR reads.
                let snr_db =
                    unsafe { estimate_snr_from_waterfall(wf, cand) }.unwrap_or(-24.0) as f32;

                // Deduplicate
                if !messages.iter().any(
                    |(t, _, _, _, _, _): &(
                        String,
                        f32,
                        f32,
                        i32,
                        f32,
                        Option<crate::Ft8Message>,
                    )| *t == text,
                ) {
                    messages.push((
                        text,
                        freq_hz,
                        time_sec,
                        status.ldpc_errors,
                        snr_db,
                        dxpedition_message,
                    ));
                }
            }
        }
    }

    messages
}

/// Run ft8_lib's sync search over `samples` and return the raw candidate
/// positions, without decoding any of them.
///
/// PAN-7: this is [`ft8lib_decode_audio_protocol`]'s
/// `monitor_init → monitor_process → ftx_find_candidates` sequence
/// (`vendor/ft8_lib/common/monitor.c`, `vendor/ft8_lib/ft8/decode.c`, MIT) with
/// the LDPC decode loop removed. It exists so pancetta's own Costas sweep can
/// union ft8_lib's candidate *locations* into its candidate list; every seed is
/// re-scored on pancetta's spectrogram by the caller, so ft8_lib never
/// contributes a score to a pancetta decode.
///
/// `num_candidates` is the heap budget handed to `ftx_find_candidates`. A
/// budget of 0 short-circuits — never hand the C function a dangling heap
/// pointer.
///
/// `min_score` is fixed at 0, making the returned set a strict superset of what
/// ft8_lib's own demo would try (`vendor/ft8_lib/demo/decode_ft8.c:21` gates at
/// `kMin_score = 10` ≡ 5.0 dB).
///
/// The `monitor_config_t` is byte-identical to
/// [`ft8lib_decode_audio_protocol`]'s. Widening the band would change `min_bin`
/// and `num_bins` and therefore every seed coordinate, and would depart from the
/// already-measured ft8_lib configuration. Consequence worth knowing: ft8_lib
/// covers bins 16–481 (100–3006 Hz), so seeds add no coverage outside that band.
#[cfg(not(ft8lib_stub))]
pub fn ft8lib_find_candidate_seeds(
    samples: &[f32],
    protocol: ftx_protocol_t,
    num_candidates: usize,
) -> Ft8LibSeedSet {
    if num_candidates == 0 {
        return Ft8LibSeedSet::default();
    }

    let cfg = monitor_config_t {
        f_min: 100.0,
        f_max: 3000.0,
        sample_rate: 12000,
        time_osr: 2,
        freq_osr: 2,
        protocol,
    };

    let mut guard = MonitorGuard::new(&cfg);
    let mon = &mut guard.0;

    let block_size = mon.block_size as usize;
    let mut offset = 0;
    while offset + block_size <= samples.len() {
        // SAFETY: mon is exclusively owned by guard; the loop guarantees block_size
        // live f32 samples, consumed synchronously without retaining the input.
        unsafe { monitor_process(mon, samples[offset..].as_ptr()) };
        offset += block_size;
    }

    let mut heap = vec![
        ftx_candidate_t {
            score: 0,
            time_offset: 0,
            freq_offset: 0,
            time_sub: 0,
            freq_sub: 0,
        };
        num_candidates
    ];

    let n_found =
        // SAFETY: guard owns the processed waterfall; the exclusive heap has
        // num_candidates writable entries. C bounds its writes and returned count by
        // that capacity.
        unsafe { ftx_find_candidates(&mon.wf, num_candidates as i32, heap.as_mut_ptr(), 0) };
    let n_found = (n_found.max(0) as usize).min(num_candidates);

    // Copy the monitor scalars out BEFORE the free — they are what makes the
    // returned coordinates interpretable. `min_bin`, `block_size` and
    // `symbol_period` live on `monitor_t`; the OSR/geometry fields live on its
    // embedded waterfall.
    let set = Ft8LibSeedSet {
        seeds: heap[..n_found]
            .iter()
            .map(|c| Ft8LibSeed {
                score: c.score,
                time_offset: c.time_offset,
                freq_offset: c.freq_offset,
                time_sub: c.time_sub,
                freq_sub: c.freq_sub,
            })
            .collect(),
        min_bin: mon.min_bin,
        time_osr: mon.wf.time_osr,
        freq_osr: mon.wf.freq_osr,
        num_bins: mon.wf.num_bins,
        num_blocks: mon.wf.num_blocks,
        block_size: mon.block_size,
        symbol_period: mon.symbol_period,
    };

    set
}

// ============================================================================
// Stub implementations — used when ft8_lib C library is not compiled.
// These return empty/None so code compiles and unit tests can run without
// the C dependency present (e.g. in CI or fresh checkouts without submodules).
// ============================================================================

#[cfg(ft8lib_stub)]
pub fn ft8lib_encode(_message: &str) -> Option<[u8; 79]> {
    None
}

#[cfg(ft8lib_stub)]
pub fn ft8lib_encode_payload(_message: &str) -> Option<[u8; 10]> {
    None
}

#[cfg(ft8lib_stub)]
pub fn ft8lib_decode_payload(_payload: &[u8; 10]) -> Option<String> {
    None
}

/// Stub: returns empty results when ft8_lib C library is not available.
#[cfg(ft8lib_stub)]
pub fn ft8lib_decode_audio(_samples: &[f32]) -> Vec<(String, f32, f32, i32, f32)> {
    Vec::new()
}

/// Stub: returns empty results when ft8_lib C library is not available.
#[cfg(ft8lib_stub)]
pub fn ft8lib_decode_audio_protocol(
    _samples: &[f32],
    _protocol: ftx_protocol_t,
) -> Vec<(String, f32, f32, i32, f32)> {
    Vec::new()
}

/// Stub: returns an empty seed set when the ft8_lib C library is not available.
///
/// PAN-7: this is the runtime no-op that lets the `decoder.rs` call site stay
/// **un-cfg-gated**, so `Ft8Config::ft8lib_sync_seeds_enabled` is the single
/// gate and the flag-off path cannot diverge between real and stub builds.
#[cfg(ft8lib_stub)]
pub fn ft8lib_find_candidate_seeds(
    _samples: &[f32],
    _protocol: ftx_protocol_t,
    _num_candidates: usize,
) -> Ft8LibSeedSet {
    Ft8LibSeedSet::default()
}

// ============================================================================
// Availability detection
// ============================================================================

/// Returns `true` when the real ft8_lib C library is compiled in,
/// `false` when using the pure-Rust stub fallback.
pub fn ft8lib_is_available() -> bool {
    cfg!(not(ft8lib_stub))
}

#[cfg(all(test, not(ft8lib_stub)))]
mod tests {
    use super::*;

    fn config(protocol: ftx_protocol_t) -> monitor_config_t {
        monitor_config_t {
            f_min: 100.0,
            f_max: 3000.0,
            sample_rate: 12000,
            time_osr: 2,
            freq_osr: 2,
            protocol,
        }
    }

    fn protocols() -> [ftx_protocol_t; 2] {
        [
            ftx_protocol_t::FTX_PROTOCOL_FT8,
            ftx_protocol_t::FTX_PROTOCOL_FT4,
        ]
    }

    #[test]
    fn monitor_guard_releases_on_return() {
        for protocol in protocols() {
            let before = MONITOR_RELEASES.get();
            {
                let _guard = MonitorGuard::new(&config(protocol));
            }
            assert_eq!(MONITOR_RELEASES.get(), before + 1);
        }
    }

    #[test]
    fn monitor_guard_releases_on_early_return() {
        fn early(protocol: ftx_protocol_t) -> bool {
            let _guard = MonitorGuard::new(&config(protocol));
            if true {
                return true;
            }
            false
        }
        for protocol in protocols() {
            let before = MONITOR_RELEASES.get();
            assert!(early(protocol));
            assert_eq!(MONITOR_RELEASES.get(), before + 1);
        }
    }

    #[test]
    fn monitor_guard_releases_on_unwind() {
        for protocol in protocols() {
            let before = MONITOR_RELEASES.get();
            let result = std::panic::catch_unwind(|| {
                let _guard = MonitorGuard::new(&config(protocol));
                panic!("exercise Rust unwind");
            });
            assert!(result.is_err());
            assert_eq!(MONITOR_RELEASES.get(), before + 1);
        }
    }

    #[test]
    fn wrapper_paths_release_monitors() {
        for protocol in protocols() {
            let samples = vec![0.0; 12000 * 15];
            let before = MONITOR_RELEASES.get();
            let empty = ft8lib_find_candidate_seeds(&samples, protocol, 0);
            assert!(empty.seeds.is_empty());
            assert_eq!(MONITOR_RELEASES.get(), before);
            assert!(ft8lib_decode_audio_protocol(&samples, protocol).is_empty());
            assert_eq!(MONITOR_RELEASES.get(), before + 1);
            let set = ft8lib_find_candidate_seeds(&samples, protocol, 10);
            assert_eq!(MONITOR_RELEASES.get(), before + 2);
            assert_eq!(set.time_osr, 2);
            assert_eq!(set.freq_osr, 2);
            assert_eq!(
                set.block_size,
                if protocol == ftx_protocol_t::FTX_PROTOCOL_FT8 {
                    1920
                } else {
                    576
                }
            );
        }
    }

    #[test]
    fn wf_mag_db_reads_owned_cells() {
        let cells = [0u8, 240, 255];
        for (idx, expected) in [-120.0, 0.0, 7.5].into_iter().enumerate() {
            // SAFETY: idx is in this live initialized array, which is not mutated.
            assert_eq!(unsafe { wf_mag_db(cells.as_ptr(), idx) }, expected);
        }
    }

    #[test]
    fn fixed_buf_with_in_bounds_nul_returns_prefix() {
        // 35-byte buffer mirroring the FFI text_buf; NUL after "CQ K5ARH".
        let mut buf = [0u8; 35];
        buf[..8].copy_from_slice(b"CQ K5ARH");
        assert_eq!(cstr_from_fixed_buf(&buf), Some("CQ K5ARH"));
    }

    #[test]
    fn fixed_buf_without_nul_returns_none() {
        // No NUL anywhere in bounds: must not over-read, must not panic.
        let buf = [b'A'; 35];
        assert_eq!(cstr_from_fixed_buf(&buf), None);
    }

    #[test]
    fn fixed_buf_leading_nul_returns_empty() {
        let buf = [0u8; 35];
        assert_eq!(cstr_from_fixed_buf(&buf), Some(""));
    }
}

/// PAN-318 AC4 FFI unit tests: the capture entry reuses ONE monitor FFT
/// for both the decode band and the display grid, and the expanded
/// 0–3000 Hz monitor does not change the reference decode.
#[cfg(all(test, not(ft8lib_stub)))]
mod display_capture_tests {
    use super::*;

    /// The PAN-318 qualification fixture (12 kHz mono vendor WAV,
    /// truncated to the production window length exactly as the
    /// coordinator shapes windows).
    fn production_window(name: &str) -> Vec<f32> {
        let path = format!(
            "{}/vendor/ft8_lib/test/wav/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let reader = hound::WavReader::open(&path).unwrap_or_else(|e| {
            panic!("fixture {path} must be present (vendored ft8_lib test WAV): {e}")
        });
        assert_eq!(reader.spec().sample_rate, 12000, "fixture must be 12 kHz");
        assert_eq!(reader.spec().channels, 1, "fixture must be mono");
        let mut samples: Vec<f32> = reader
            .into_samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32768.0)
            .collect();
        samples.truncate(crate::WINDOW_SAMPLES);
        samples
    }

    type DecodeTuple = (String, f32, f32, i32, f32, Option<crate::Ft8Message>);

    fn sorted(mut v: Vec<DecodeTuple>) -> Vec<(String, f32, f32, i32, f32)> {
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v.into_iter()
            .map(|(t, f, dt, e, s, _)| (t, f, dt, e, s))
            .collect()
    }

    /// AC4 `display_capture_reuses_monitor`: the capture route decodes the
    /// SAME messages as the plain no-capture route on a real off-air
    /// fixture — one monitor FFT supplied both the owned decode-band view
    /// and the display grid — and it costs exactly one C monitor (no
    /// second FFT pass hidden behind the capture).
    #[test]
    fn display_capture_reuses_monitor() {
        let samples = production_window("191111_110200.wav");
        let before = MONITOR_RELEASES.get();

        let plain = ft8lib_decode_audio_protocol_with_parser(
            &samples,
            ftx_protocol_t::FTX_PROTOCOL_FT8,
            None,
        );
        assert_eq!(MONITOR_RELEASES.get(), before + 1);

        let (captured, grid) = ft8lib_decode_audio_protocol_with_parser_and_capture(
            &samples,
            ftx_protocol_t::FTX_PROTOCOL_FT8,
            None,
        );
        assert_eq!(
            MONITOR_RELEASES.get(),
            before + 2,
            "the capture entry must reuse ONE monitor, not add a second FFT pass"
        );

        assert!(!plain.is_empty(), "fixture must decode for this to bite");
        assert_eq!(
            sorted(captured.clone()),
            sorted(plain),
            "capture route messages must be byte-identical (text/freq/time/errors/snr)"
        );

        // The grid is owned Rust memory usable after the monitor was freed.
        let grid = grid.expect("real build returns a display grid");
        assert!(grid.num_blocks > 0);
        let wf = grid.to_waterfall_data(samples.len());
        assert!(!wf.power_matrix.is_empty());
        assert!(wf
            .power_matrix
            .iter()
            .all(|r| r.iter().all(|v| v.is_finite())));
    }

    /// AC4 `display_capture_preserves_decode_band`: the expanded 0–3000 Hz
    /// monitor does not widen candidate selection — the owned view keeps
    /// the original 100–3000 Hz decode band (every decoded frequency
    /// stays inside it, no candidate appears in the 0–100 Hz display-only
    /// region) and the display grid itself spans 0–3000 Hz.
    #[test]
    fn display_capture_preserves_decode_band() {
        let samples = production_window("websdr_test2.wav");
        let (captured, grid) = ft8lib_decode_audio_protocol_with_parser_and_capture(
            &samples,
            ftx_protocol_t::FTX_PROTOCOL_FT8,
            None,
        );
        assert!(!captured.is_empty(), "fixture must decode for this to bite");
        for (text, freq_hz, ..) in &captured {
            assert!(
                *freq_hz >= 100.0 && *freq_hz <= 3000.0,
                "decode {text} at {freq_hz} Hz escaped the original 100-3000 Hz band"
            );
        }

        let grid = grid.expect("real build returns a display grid");
        // FT8 display-monitor geometry: min_bin 0, 481 bins (0–3000.25 Hz),
        // 3.125 Hz sub-bin axis, block_size 1920.
        assert_eq!(grid.min_bin, 0);
        assert_eq!(grid.num_bins, 481);
        assert_eq!(grid.freq_osr, 2);
        assert_eq!(grid.block_size, 1920);
        assert_eq!(
            grid.mag.len(),
            grid.num_blocks * grid.time_osr * grid.freq_osr * grid.num_bins,
            "owned grid must hold the full [block][time_sub][freq_sub][bin] layout"
        );

        let wf = grid.to_waterfall_data(samples.len());
        let bin_width = wf.frequency_bins[1] - wf.frequency_bins[0];
        assert!(
            wf.frequency_bins.first() == Some(&0.0)
                && (wf.frequency_bins.last().unwrap() - 3000.0).abs() < bin_width,
            "display must cover 0-3000 Hz, got {:?}..{:?}",
            wf.frequency_bins.first(),
            wf.frequency_bins.last()
        );
    }

    /// AC4 tone placement: pure tones near the band edges and center land
    /// on the right display frequency (no missing low-frequency coverage,
    /// no endpoint mistakes), and silence renders finite uniform rows.
    #[test]
    fn display_capture_tone_placement_and_silence() {
        let tone = |freq_hz: f32| {
            let samples: Vec<f32> = (0..crate::WINDOW_SAMPLES)
                .map(|i| 0.25 * (2.0 * std::f32::consts::PI * freq_hz * i as f32 / 12000.0).sin())
                .collect();
            let (_, grid) = ft8lib_decode_audio_protocol_with_parser_and_capture(
                &samples,
                ftx_protocol_t::FTX_PROTOCOL_FT8,
                None,
            );
            let grid = grid.expect("real build returns a display grid");
            let wf = grid.to_waterfall_data(samples.len());
            let bin_width = wf.frequency_bins[1] - wf.frequency_bins[0];
            // Peak bin of the loudest row must sit within one source
            // frequency step (3.125 Hz) + one display bin (2.93 Hz) of the
            // tone; 10 Hz covers both with margin.
            let loudest = wf
                .power_matrix
                .iter()
                .max_by(|a, b| {
                    a.iter()
                        .cloned()
                        .fold(f64::MIN, f64::max)
                        .total_cmp(&b.iter().cloned().fold(f64::MIN, f64::max))
                })
                .expect("rows exist");
            let peak_freq = wf.frequency_bins[loudest
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.total_cmp(b))
                .map(|(i, _)| i)
                .expect("bins exist")];
            assert!(
                (peak_freq - freq_hz as f64).abs() <= 10.0,
                "tone at {freq_hz} Hz placed at {peak_freq} Hz"
            );
        };
        for freq_hz in [50.0f32, 1500.0, 2990.0] {
            tone(freq_hz);
        }

        // Silence: finite, uniform rows (the quantized floor is flat).
        let silence = vec![0.0f32; crate::WINDOW_SAMPLES];
        let (_, grid) = ft8lib_decode_audio_protocol_with_parser_and_capture(
            &silence,
            ftx_protocol_t::FTX_PROTOCOL_FT8,
            None,
        );
        let wf = grid.expect("grid").to_waterfall_data(silence.len());
        assert!(!wf.power_matrix.is_empty());
        for row in &wf.power_matrix {
            assert!(row.iter().all(|v| v.is_finite()));
            let spread = row.iter().cloned().fold(f64::MIN, f64::max)
                - row.iter().cloned().fold(f64::MAX, f64::min);
            assert!(
                spread <= 1e-6,
                "silence must be uniform, spread {spread} dB"
            );
        }
    }
}

/// PAN-7: the real and stub `ft8lib_find_candidate_seeds` must keep identical
/// signatures. No CI job ever type-checks the stub arm — all four compiling
/// jobs use `submodules: recursive` — so this coercion into a typed fn pointer,
/// which compiles under **both** cfgs, is the only mechanical guard against the
/// stub rotting out of sync with the real one.
#[cfg(test)]
mod seed_signature_tests {
    use super::*;

    #[test]
    fn seed_helper_signature_is_cfg_invariant() {
        let f: fn(&[f32], ftx_protocol_t, usize) -> Ft8LibSeedSet = ft8lib_find_candidate_seeds;

        // The zero-budget short-circuit is the one behavior that is identical
        // under both cfgs: the real helper early-returns before touching C, the
        // stub always returns empty.
        let set = f(&[], ftx_protocol_t::FTX_PROTOCOL_FT8, 0);
        assert!(set.seeds.is_empty());
        assert_eq!(set.min_bin, 0);
    }
}

#[cfg(ft8lib_stub)]
pub fn ft8lib_decode_audio_protocol_with_parser(
    _samples: &[f32],
    _protocol: ftx_protocol_t,
    _parser: Option<&crate::message::MessageParser>,
) -> Vec<(String, f32, f32, i32, f32, Option<crate::Ft8Message>)> {
    Vec::new()
}

/// Stub: no C monitor exists to capture, so the display grid is `None`
/// (the caller falls back to the legacy waterfall generator).
#[cfg(ft8lib_stub)]
pub fn ft8lib_decode_audio_protocol_with_parser_and_capture(
    _samples: &[f32],
    _protocol: ftx_protocol_t,
    _parser: Option<&crate::message::MessageParser>,
) -> (
    Vec<(String, f32, f32, i32, f32, Option<crate::Ft8Message>)>,
    Option<Ft8LibDisplayGrid>,
) {
    (Vec::new(), None)
}

/// The real and stub capture entries must keep identical signatures (same
/// rationale as [`seed_signature_tests`]): no CI job type-checks the stub
/// arm, so this coercion is the mechanical guard.
#[cfg(test)]
mod capture_signature_tests {
    use super::*;

    #[test]
    fn capture_helper_signature_is_cfg_invariant() {
        let f: fn(
            &[f32],
            ftx_protocol_t,
            Option<&crate::message::MessageParser>,
        ) -> (
            Vec<(String, f32, f32, i32, f32, Option<crate::Ft8Message>)>,
            Option<Ft8LibDisplayGrid>,
        ) = ft8lib_decode_audio_protocol_with_parser_and_capture;

        // Empty audio: the real build processes zero blocks and returns an
        // empty-but-present grid; the stub returns None. Both return no
        // messages and must not panic — that is the shared contract.
        let (messages, grid) = f(&[], ftx_protocol_t::FTX_PROTOCOL_FT8, None);
        assert!(messages.is_empty());
        assert!(grid.as_ref().map_or(true, |g| g.num_blocks == 0));
    }
}
