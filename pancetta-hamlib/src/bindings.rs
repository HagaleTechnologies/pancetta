//! FFI bindings to hamlib
//!
//! This module provides low-level FFI bindings to the hamlib C library.
//! These bindings are wrapped by higher-level safe interfaces in other modules.

#![allow(missing_docs)]
#![allow(clippy::missing_safety_doc)]

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_long, c_longlong, c_uint, c_void};

/// Hamlib RIG handle wrapper that is Send + Sync safe
#[derive(Debug, Clone, Copy)]
pub struct RigHandle(*mut c_void);

impl RigHandle {
    /// Create new RigHandle from raw pointer
    pub fn new(ptr: *mut c_void) -> Self {
        Self(ptr)
    }

    /// Get raw pointer
    pub fn as_ptr(&self) -> *mut c_void {
        self.0
    }

    /// Check if handle is null
    pub fn is_null(&self) -> bool {
        self.0.is_null()
    }
}

// Safety: RigHandle is a raw C pointer that we protect with proper synchronization
// We ensure it's only accessed from one thread at a time via Mutex
unsafe impl Send for RigHandle {}
unsafe impl Sync for RigHandle {}

/// Hamlib rig model type
pub type RigModel = c_int;

/// Frequency type (Hz)
pub type Frequency = c_longlong;

/// Power level type (0.0-1.0)
pub type PowerLevel = f32;

/// S-meter reading type
pub type SMeter = c_int;

/// SWR reading type  
pub type SwrReading = f32;

// Hamlib constants
pub const RIG_MODEL_DUMMY: RigModel = 1;
pub const RIG_MODEL_NETRIGCTL: RigModel = 2;

// VFO constants
pub const RIG_VFO_CURR: c_uint = 0;
pub const RIG_VFO_A: c_uint = 1;
pub const RIG_VFO_B: c_uint = 2;
pub const RIG_VFO_MEM: c_uint = 3;

// Mode constants
pub const RIG_MODE_NONE: c_uint = 0;
pub const RIG_MODE_AM: c_uint = 1 << 0;
pub const RIG_MODE_CW: c_uint = 1 << 1;
pub const RIG_MODE_USB: c_uint = 1 << 2;
pub const RIG_MODE_LSB: c_uint = 1 << 3;
pub const RIG_MODE_RTTY: c_uint = 1 << 4;
pub const RIG_MODE_FM: c_uint = 1 << 5;
pub const RIG_MODE_WFM: c_uint = 1 << 6;
pub const RIG_MODE_CWR: c_uint = 1 << 7;
pub const RIG_MODE_RTTYR: c_uint = 1 << 8;
pub const RIG_MODE_AMS: c_uint = 1 << 9;
pub const RIG_MODE_PKTLSB: c_uint = 1 << 10;
pub const RIG_MODE_PKTUSB: c_uint = 1 << 11;
pub const RIG_MODE_PKTFM: c_uint = 1 << 12;
pub const RIG_MODE_ECSSUSB: c_uint = 1 << 13;
pub const RIG_MODE_ECSSLSB: c_uint = 1 << 14;
pub const RIG_MODE_FT8: c_uint = 1 << 15;
pub const RIG_MODE_FT4: c_uint = 1 << 16;

// PTT constants
pub const RIG_PTT_OFF: c_uint = 0;
pub const RIG_PTT_ON: c_uint = 1;
pub const RIG_PTT_ON_MIC: c_uint = 2;
pub const RIG_PTT_ON_DATA: c_uint = 3;

// Return codes
pub const RIG_OK: c_int = 0;
pub const RIG_EINVAL: c_int = -1;
pub const RIG_ECONF: c_int = -2;
pub const RIG_ENOMEM: c_int = -3;
pub const RIG_ENIMPL: c_int = -4;
pub const RIG_ETIMEOUT: c_int = -5;
pub const RIG_EIO: c_int = -6;
pub const RIG_EINTERNAL: c_int = -7;
pub const RIG_EPROTO: c_int = -8;
pub const RIG_ERJCTED: c_int = -9;
pub const RIG_ETRUNC: c_int = -10;
pub const RIG_ENAVAIL: c_int = -11;
pub const RIG_ENTARGET: c_int = -12;
pub const RIG_BUSERROR: c_int = -13;
pub const RIG_BUSBUSY: c_int = -14;
pub const RIG_EARG: c_int = -15;
pub const RIG_EVFO: c_int = -16;
pub const RIG_EDOM: c_int = -17;

/// Hamlib port type
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct HamLibPort {
    pub typ: c_uint,
    pub fd: c_int,
    pub timeout: c_int,
    pub retry: c_int,
    pub pathname: [c_char; 1024],
    pub write_delay: c_int,
    pub post_write_delay: c_int,
}

/// Hamlib rig state structure (partial)
///
/// # Safety
///
/// This is a partial layout of hamlib's `rig_state` C struct. DO NOT construct
/// instances of this struct directly -- it is only valid when cast from a pointer
/// returned by hamlib. The real struct has many more fields; using this as a
/// value type will cause undefined behavior.
#[repr(C)]
#[allow(dead_code)]
pub(crate) struct RigState {
    pub port: HamLibPort,
    pub comm_state: c_int,
    pub itu_region: c_int,
    pub freq: Frequency,
    pub mode: c_uint,
    pub width: c_long,
    pub vfo: c_uint,
    pub ptt: c_uint,
    pub dcd: c_uint,
}

// Hamlib function declarations - only available when hamlib is installed
#[cfg(hamlib_found)]
extern "C" {
    /// Initialize hamlib
    pub fn rig_init(debug_level: c_int);

    /// Create a new rig handle
    pub fn rig_init_rig(rig_model: RigModel) -> *mut c_void;

    /// Open connection to rig
    pub fn rig_open(rig: *mut c_void) -> c_int;

    /// Close connection to rig
    pub fn rig_close(rig: *mut c_void) -> c_int;

    /// Cleanup rig handle
    pub fn rig_cleanup(rig: *mut c_void) -> c_int;

    /// Set rig parameter (device path, baud rate, etc.)
    pub fn rig_set_conf(rig: *mut c_void, token: c_uint, val: *const c_char) -> c_int;

    /// Get rig parameter
    pub fn rig_get_conf(rig: *mut c_void, token: c_uint, val: *mut c_char) -> c_int;

    /// Set frequency
    pub fn rig_set_freq(rig: *mut c_void, vfo: c_uint, freq: Frequency) -> c_int;

    /// Get frequency
    pub fn rig_get_freq(rig: *mut c_void, vfo: c_uint, freq: *mut Frequency) -> c_int;

    /// Set mode and passband
    pub fn rig_set_mode(rig: *mut c_void, vfo: c_uint, mode: c_uint, width: c_long) -> c_int;

    /// Get mode and passband
    pub fn rig_get_mode(
        rig: *mut c_void,
        vfo: c_uint,
        mode: *mut c_uint,
        width: *mut c_long,
    ) -> c_int;

    /// Set VFO
    pub fn rig_set_vfo(rig: *mut c_void, vfo: c_uint) -> c_int;

    /// Get VFO
    pub fn rig_get_vfo(rig: *mut c_void, vfo: *mut c_uint) -> c_int;

    /// Set PTT
    pub fn rig_set_ptt(rig: *mut c_void, vfo: c_uint, ptt: c_uint) -> c_int;

    /// Get PTT
    pub fn rig_get_ptt(rig: *mut c_void, vfo: c_uint, ptt: *mut c_uint) -> c_int;

    /// Set power level
    pub fn rig_set_level(
        rig: *mut c_void,
        vfo: c_uint,
        level_type: c_uint,
        val: *const c_void,
    ) -> c_int;

    /// Get power level, S-meter, SWR, etc.
    pub fn rig_get_level(
        rig: *mut c_void,
        vfo: c_uint,
        level_type: c_uint,
        val: *mut c_void,
    ) -> c_int;

    /// Set memory channel
    pub fn rig_set_mem(rig: *mut c_void, vfo: c_uint, ch: c_int) -> c_int;

    /// Get memory channel
    pub fn rig_get_mem(rig: *mut c_void, vfo: c_uint, ch: *mut c_int) -> c_int;

    /// Start/stop scanning
    pub fn rig_scan(rig: *mut c_void, vfo: c_uint, scan: c_uint, ch: c_int) -> c_int;

    /// Get rig info string
    pub fn rig_get_info(rig: *mut c_void) -> *const c_char;

    /// Get error string for error code
    pub fn rigerror(errnum: c_int) -> *const c_char;

    /// Set timeout for operations
    pub fn rig_set_timeout(rig: *mut c_void, timeout: c_int) -> c_int;

    /// Get timeout
    pub fn rig_get_timeout(rig: *mut c_void, timeout: *mut c_int) -> c_int;
}

// Stub implementations when hamlib is not installed.
// These allow the crate to compile and use MockRig without requiring libhamlib.
#[cfg(not(hamlib_found))]
/// Initialize hamlib (stub - hamlib not installed)
pub unsafe fn rig_init(_debug_level: c_int) {}

#[cfg(not(hamlib_found))]
/// Create a new rig handle (stub - hamlib not installed)
pub unsafe fn rig_init_rig(_rig_model: RigModel) -> *mut c_void {
    std::ptr::null_mut()
}

#[cfg(not(hamlib_found))]
/// Open connection to rig (stub - hamlib not installed)
pub unsafe fn rig_open(_rig: *mut c_void) -> c_int {
    RIG_EINVAL
}

#[cfg(not(hamlib_found))]
/// Close connection to rig (stub - hamlib not installed)
pub unsafe fn rig_close(_rig: *mut c_void) -> c_int {
    RIG_OK
}

#[cfg(not(hamlib_found))]
/// Cleanup rig handle (stub - hamlib not installed)
pub unsafe fn rig_cleanup(_rig: *mut c_void) -> c_int {
    RIG_OK
}

#[cfg(not(hamlib_found))]
/// Set rig parameter (stub - hamlib not installed)
pub unsafe fn rig_set_conf(_rig: *mut c_void, _token: c_uint, _val: *const c_char) -> c_int {
    RIG_EINVAL
}

#[cfg(not(hamlib_found))]
/// Get rig parameter (stub - hamlib not installed)
pub unsafe fn rig_get_conf(_rig: *mut c_void, _token: c_uint, _val: *mut c_char) -> c_int {
    RIG_EINVAL
}

#[cfg(not(hamlib_found))]
/// Set frequency (stub - hamlib not installed)
pub unsafe fn rig_set_freq(_rig: *mut c_void, _vfo: c_uint, _freq: Frequency) -> c_int {
    RIG_EINVAL
}

#[cfg(not(hamlib_found))]
/// Get frequency (stub - hamlib not installed)
pub unsafe fn rig_get_freq(_rig: *mut c_void, _vfo: c_uint, _freq: *mut Frequency) -> c_int {
    RIG_EINVAL
}

#[cfg(not(hamlib_found))]
/// Set mode and passband (stub - hamlib not installed)
pub unsafe fn rig_set_mode(
    _rig: *mut c_void,
    _vfo: c_uint,
    _mode: c_uint,
    _width: c_long,
) -> c_int {
    RIG_EINVAL
}

#[cfg(not(hamlib_found))]
/// Get mode and passband (stub - hamlib not installed)
pub unsafe fn rig_get_mode(
    _rig: *mut c_void,
    _vfo: c_uint,
    _mode: *mut c_uint,
    _width: *mut c_long,
) -> c_int {
    RIG_EINVAL
}

#[cfg(not(hamlib_found))]
/// Set VFO (stub - hamlib not installed)
pub unsafe fn rig_set_vfo(_rig: *mut c_void, _vfo: c_uint) -> c_int {
    RIG_EINVAL
}

#[cfg(not(hamlib_found))]
/// Get VFO (stub - hamlib not installed)
pub unsafe fn rig_get_vfo(_rig: *mut c_void, _vfo: *mut c_uint) -> c_int {
    RIG_EINVAL
}

#[cfg(not(hamlib_found))]
/// Set PTT (stub - hamlib not installed)
pub unsafe fn rig_set_ptt(_rig: *mut c_void, _vfo: c_uint, _ptt: c_uint) -> c_int {
    RIG_EINVAL
}

#[cfg(not(hamlib_found))]
/// Get PTT (stub - hamlib not installed)
pub unsafe fn rig_get_ptt(_rig: *mut c_void, _vfo: c_uint, _ptt: *mut c_uint) -> c_int {
    RIG_EINVAL
}

#[cfg(not(hamlib_found))]
/// Set power level (stub - hamlib not installed)
pub unsafe fn rig_set_level(
    _rig: *mut c_void,
    _vfo: c_uint,
    _level_type: c_uint,
    _val: *const c_void,
) -> c_int {
    RIG_EINVAL
}

#[cfg(not(hamlib_found))]
/// Get power level, S-meter, SWR, etc. (stub - hamlib not installed)
pub unsafe fn rig_get_level(
    _rig: *mut c_void,
    _vfo: c_uint,
    _level_type: c_uint,
    _val: *mut c_void,
) -> c_int {
    RIG_EINVAL
}

#[cfg(not(hamlib_found))]
/// Set memory channel (stub - hamlib not installed)
pub unsafe fn rig_set_mem(_rig: *mut c_void, _vfo: c_uint, _ch: c_int) -> c_int {
    RIG_EINVAL
}

#[cfg(not(hamlib_found))]
/// Get memory channel (stub - hamlib not installed)
pub unsafe fn rig_get_mem(_rig: *mut c_void, _vfo: c_uint, _ch: *mut c_int) -> c_int {
    RIG_EINVAL
}

#[cfg(not(hamlib_found))]
/// Start/stop scanning (stub - hamlib not installed)
pub unsafe fn rig_scan(_rig: *mut c_void, _vfo: c_uint, _scan: c_uint, _ch: c_int) -> c_int {
    RIG_EINVAL
}

#[cfg(not(hamlib_found))]
/// Get rig info string (stub - hamlib not installed)
pub unsafe fn rig_get_info(_rig: *mut c_void) -> *const c_char {
    std::ptr::null()
}

#[cfg(not(hamlib_found))]
/// Get error string for error code (stub - hamlib not installed)
pub unsafe fn rigerror(_errnum: c_int) -> *const c_char {
    b"hamlib not installed\0".as_ptr() as *const c_char
}

#[cfg(not(hamlib_found))]
/// Set timeout for operations (stub - hamlib not installed)
pub unsafe fn rig_set_timeout(_rig: *mut c_void, _timeout: c_int) -> c_int {
    RIG_EINVAL
}

#[cfg(not(hamlib_found))]
/// Get timeout (stub - hamlib not installed)
pub unsafe fn rig_get_timeout(_rig: *mut c_void, _timeout: *mut c_int) -> c_int {
    RIG_EINVAL
}

// Level types for rig_set_level/rig_get_level
pub const RIG_LEVEL_RF: c_uint = 1 << 0;
pub const RIG_LEVEL_RFPOWER: c_uint = 1 << 1;
pub const RIG_LEVEL_AF: c_uint = 1 << 2;
pub const RIG_LEVEL_SQL: c_uint = 1 << 3;
pub const RIG_LEVEL_IF: c_uint = 1 << 4;
pub const RIG_LEVEL_APF: c_uint = 1 << 5;
pub const RIG_LEVEL_NR: c_uint = 1 << 6;
pub const RIG_LEVEL_PBT_IN: c_uint = 1 << 7;
pub const RIG_LEVEL_PBT_OUT: c_uint = 1 << 8;
pub const RIG_LEVEL_CWPITCH: c_uint = 1 << 9;
pub const RIG_LEVEL_KEYSPD: c_uint = 1 << 10;
pub const RIG_LEVEL_NOTCHF: c_uint = 1 << 11;
pub const RIG_LEVEL_COMP: c_uint = 1 << 12;
pub const RIG_LEVEL_AGC: c_uint = 1 << 13;
pub const RIG_LEVEL_BKINDL: c_uint = 1 << 14;
pub const RIG_LEVEL_BALANCE: c_uint = 1 << 15;
pub const RIG_LEVEL_METER: c_uint = 1 << 16;
pub const RIG_LEVEL_VOXGAIN: c_uint = 1 << 17;
pub const RIG_LEVEL_VOXDELAY: c_uint = 1 << 18;
pub const RIG_LEVEL_ANTIVOX: c_uint = 1 << 19;
pub const RIG_LEVEL_SLOPE_LOW: c_uint = 1 << 20;
pub const RIG_LEVEL_SLOPE_HIGH: c_uint = 1 << 21;
pub const RIG_LEVEL_BKIN_DLYMS: c_uint = 1 << 22;
pub const RIG_LEVEL_RAWSTR: c_uint = 1 << 23;
pub const RIG_LEVEL_SWR: c_uint = 1 << 24;
pub const RIG_LEVEL_ALC: c_uint = 1 << 25;
pub const RIG_LEVEL_STRENGTH: c_uint = 1 << 26;

// Scan types
pub const RIG_SCAN_STOP: c_uint = 0;
pub const RIG_SCAN_MEM: c_uint = 1;
pub const RIG_SCAN_SLCT: c_uint = 2;
pub const RIG_SCAN_PRIO: c_uint = 3;
pub const RIG_SCAN_PROG: c_uint = 4;
pub const RIG_SCAN_DELTA: c_uint = 5;
pub const RIG_SCAN_VFO: c_uint = 6;
pub const RIG_SCAN_PLT: c_uint = 7;

/// Safe wrapper to convert C string to Rust string
pub fn c_str_to_string(c_str: *const c_char) -> Result<String, std::str::Utf8Error> {
    if c_str.is_null() {
        return Ok(String::new());
    }

    unsafe {
        let cstr = CStr::from_ptr(c_str);
        cstr.to_str().map(|s| s.to_owned())
    }
}

/// Safe wrapper to convert Rust string to C string
pub fn string_to_c_str(s: &str) -> Result<CString, std::ffi::NulError> {
    CString::new(s)
}

/// Check if hamlib return code indicates success
pub fn is_hamlib_success(code: c_int) -> bool {
    code == RIG_OK
}

/// Convert hamlib error code to error message
pub fn hamlib_error_message(code: c_int) -> String {
    unsafe {
        let c_str = rigerror(code);
        c_str_to_string(c_str).unwrap_or_else(|_| format!("Unknown error code: {}", code))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_c_str_conversion() {
        let test_str = "test string";
        let c_str = string_to_c_str(test_str).unwrap();
        let ptr = c_str.as_ptr();
        let result = c_str_to_string(ptr).unwrap();
        assert_eq!(result, test_str);
    }

    #[test]
    fn test_error_checking() {
        assert!(is_hamlib_success(RIG_OK));
        assert!(!is_hamlib_success(RIG_EINVAL));
        assert!(!is_hamlib_success(RIG_ETIMEOUT));
    }

    #[test]
    fn test_constants() {
        // Verify some key constants are defined correctly
        assert_eq!(RIG_VFO_A, 1);
        assert_eq!(RIG_VFO_B, 2);
        assert_eq!(RIG_PTT_OFF, 0);
        assert_eq!(RIG_PTT_ON, 1);
        assert_ne!(RIG_MODE_USB, RIG_MODE_LSB);
    }
}
