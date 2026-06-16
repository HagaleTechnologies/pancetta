//! FFI bindings to ft8_lib — the reference FT8 implementation.
//!
//! Used for cross-validation: comparing our encoder/decoder output against
//! an independent, well-tested C implementation.

#![allow(non_camel_case_types)]

#[cfg(not(ft8lib_stub))]
use std::ffi::{CStr, CString};
use std::os::raw::c_char;

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
extern "C" {
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
    let mut msg: ftx_message_t = unsafe { std::mem::zeroed() };
    unsafe { ftx_message_init(&mut msg) };

    let mut hash_if = make_hash_interface();

    let rc = unsafe { ftx_message_encode(&mut msg, &mut hash_if, c_msg.as_ptr()) };

    if rc != ftx_message_rc_t::FTX_MESSAGE_RC_OK {
        return None;
    }

    let mut tones = [0u8; 79];
    unsafe { ft8_encode(msg.payload.as_ptr(), tones.as_mut_ptr()) };

    Some(tones)
}

/// Encode a message string to 10-byte payload using ft8_lib.
#[cfg(not(ft8lib_stub))]
pub fn ft8lib_encode_payload(message: &str) -> Option<[u8; 10]> {
    let c_msg = CString::new(message).ok()?;
    let mut msg: ftx_message_t = unsafe { std::mem::zeroed() };
    unsafe { ftx_message_init(&mut msg) };

    let mut hash_if = make_hash_interface();
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
    let mut offsets: ftx_message_offsets_t = unsafe { std::mem::zeroed() };

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

    let c_str = unsafe { CStr::from_ptr(text_buf.as_ptr() as *const c_char) };
    Some(c_str.to_string_lossy().trim().to_string())
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
#[cfg(not(ft8lib_stub))]
#[inline]
unsafe fn wf_mag_db(mag: *const u8, idx: usize) -> f64 {
    f64::from(*mag.add(idx)) * 0.5 - 120.0
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
#[cfg(not(ft8lib_stub))]
fn estimate_snr_from_waterfall(wf: &ftx_waterfall_t, cand: &ftx_candidate_t) -> Option<f64> {
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
#[cfg(not(ft8lib_stub))]
pub fn ft8lib_decode_audio(samples: &[f32]) -> Vec<(String, f32, f32, i32, f32)> {
    let cfg = monitor_config_t {
        f_min: 100.0,
        f_max: 3000.0,
        sample_rate: 12000,
        time_osr: 2,
        freq_osr: 2,
        protocol: ftx_protocol_t::FTX_PROTOCOL_FT8,
    };

    let mut mon: monitor_t = unsafe { std::mem::zeroed() };
    unsafe { monitor_init(&mut mon, &cfg) };

    // Feed audio in block_size chunks
    let block_size = mon.block_size as usize;
    let mut offset = 0;
    while offset + block_size <= samples.len() {
        unsafe { monitor_process(&mut mon, samples[offset..].as_ptr()) };
        offset += block_size;
    }

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
        unsafe { ftx_find_candidates(&mon.wf, num_candidates as i32, candidates.as_mut_ptr(), 0) };

    // Decode each candidate
    let mut messages = Vec::new();
    let mut hash_if = make_hash_interface();
    for i in 0..n_found as usize {
        let mut msg: ftx_message_t = unsafe { std::mem::zeroed() };
        let mut status: ftx_decode_status_t = unsafe { std::mem::zeroed() };

        let ok =
            unsafe { ftx_decode_candidate(&mon.wf, &candidates[i], 25, &mut msg, &mut status) };

        if ok {
            let mut text_buf = [0u8; 35];
            let mut offsets: ftx_message_offsets_t = unsafe { std::mem::zeroed() };
            let rc = unsafe {
                ftx_message_decode(
                    &msg,
                    &mut hash_if,
                    text_buf.as_mut_ptr() as *mut c_char,
                    &mut offsets,
                )
            };

            if rc == ftx_message_rc_t::FTX_MESSAGE_RC_OK {
                let c_str = unsafe { CStr::from_ptr(text_buf.as_ptr() as *const c_char) };
                let text = c_str.to_string_lossy().trim().to_string();

                // ft8_lib's ftx_decode_candidate never populates
                // status.freq / status.time (upstream behavior — only
                // ldpc_errors and the CRCs are written). Derive both
                // from the candidate exactly like the upstream demo
                // (demo/decode_ft8.c):
                //   freq_hz  = (min_bin + freq_offset + freq_sub/freq_osr) / symbol_period
                //   time_sec = (time_offset + time_sub/time_osr) * symbol_period
                let cand = &candidates[i];
                let freq_hz = (mon.min_bin as f32
                    + cand.freq_offset as f32
                    + cand.freq_sub as f32 / mon.wf.freq_osr as f32)
                    / mon.symbol_period;
                let time_sec = (cand.time_offset as f32
                    + cand.time_sub as f32 / mon.wf.time_osr as f32)
                    * mon.symbol_period;

                // Real SNR from the waterfall at the decoded position; fall
                // back to the native estimator's -24 dB floor when the
                // candidate position is out of the waterfall's range.
                let snr_db = estimate_snr_from_waterfall(&mon.wf, cand).unwrap_or(-24.0) as f32;

                // Deduplicate
                if !messages
                    .iter()
                    .any(|(t, _, _, _, _): &(String, f32, f32, i32, f32)| *t == text)
                {
                    messages.push((text, freq_hz, time_sec, status.ldpc_errors, snr_db));
                }
            }
        }
    }

    unsafe { monitor_free(&mut mon) };

    messages
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

// ============================================================================
// Availability detection
// ============================================================================

/// Returns `true` when the real ft8_lib C library is compiled in,
/// `false` when using the pure-Rust stub fallback.
pub fn ft8lib_is_available() -> bool {
    cfg!(not(ft8lib_stub))
}
