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

/// Decode audio samples to FT8 messages using ft8_lib's full pipeline.
/// Returns a vector of (message_text, frequency_hz, time_sec, ldpc_errors).
#[cfg(not(ft8lib_stub))]
pub fn ft8lib_decode_audio(samples: &[f32]) -> Vec<(String, f32, f32, i32)> {
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

                // Deduplicate
                if !messages
                    .iter()
                    .any(|(t, _, _, _): &(String, f32, f32, i32)| *t == text)
                {
                    messages.push((text, freq_hz, time_sec, status.ldpc_errors));
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
pub fn ft8lib_decode_audio(_samples: &[f32]) -> Vec<(String, f32, f32, i32)> {
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
