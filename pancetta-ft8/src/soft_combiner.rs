//! hb-244: JS8Call-Improved soft combiner across repeated receptions.
//!
//! Inspired by the JS8Call-Improved C++ soft combiner mechanism
//! (`JS8_Mode/soft_combiner.h`, GPL-3.0). pancetta's port is a clean-room
//! Rust implementation written from the prose-only algorithm spec at
//! `research/specs/spec-js8call-soft-combiner.md` — no peer source was
//! read while authoring this module.
//!
//! # Mechanism
//!
//! When the same FT8 transmission is heard more than once (because the
//! operator re-keys it, or a CQ machine cycles the same call across
//! slots), each reception independently produces a set of soft LLRs from
//! the symbol demapper. None of those individual receptions may have
//! enough SNR to LDPC-decode on its own. But the *sum* of the LLRs from
//! multiple receptions is mathematically equivalent to coherent
//! averaging in the soft-decision domain — and often does decode.
//!
//! The soft combiner identifies repeat candidates without operator help
//! and accumulates their LLRs into a single combined LLR stream that
//! the LDPC stage sees as a higher-SNR version of the same signal.
//! Transparent time-diversity boost.
//!
//! # Algorithm
//!
//! 1. **Key construction.** Coarse key = `(mode, freq_bin, time_bin)`.
//!    `time_bin` is the candidate's DT offset rounded to a coarse grid.
//! 2. **Signature.** 32-bit fingerprint built from the sign bits of the
//!    LLR array at 32 fixed positions. Same payload → mostly-same
//!    signature; different payload → very different signature.
//! 3. **Lookup.** Search the cache for entries whose coarse key matches
//!    exactly. Among those, find any whose 32-bit signature differs from
//!    the new candidate's signature by ≤ 4 bits (Hamming distance). The
//!    Hamming tolerance accommodates per-symbol noise that flips a
//!    handful of LLR signs between receptions without the underlying
//!    payload actually changing.
//! 4. **Combine or insert.** On match: element-wise add the new LLRs to
//!    the cached LLR buffer, increment the repeat counter, refresh the
//!    timestamp, and return the combined LLRs. On miss: insert a new
//!    entry and return the input unchanged with `repeat_count = 1`.
//! 5. **Cleanup.** On each insert (and externally callable), drop
//!    entries older than the configured TTL. `mark_decoded` evicts an
//!    entry whose payload has cleared CRC downstream.
//!
//! # Default state
//!
//! `SoftCombiner` is OFF by default at the configuration level
//! (`Ft8Config::soft_combiner_enabled = false`). The mechanism is
//! shipped with the module wired but disabled until corpus measurement
//! validates a net recall gain on a repeat-heavy WAV corpus. pancetta's
//! existing hard-200 corpus is largely single-reception per signal and
//! may not exercise this mechanism.
//!
//! # Threading
//!
//! The combiner uses interior `&mut self` and is intended to live inside
//! a `Mutex` when shared across rayon workers. The combine operation is
//! O(small) — a key hash + linear walk of a small bucket + one 174-f32
//! copy — so the critical section is short.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// FT8 codeword length in bits — number of LLRs combined per reception.
pub const LLR_LEN: usize = 174;

/// Number of bits used in the approximate-match signature. 32 matches
/// the JS8Call-Improved reference value and packs into a single `u32`
/// for cheap Hamming-distance computation via `count_ones`.
pub const SIGNATURE_WIDTH: usize = 32;

/// Default Hamming tolerance for signature matching. Two candidates
/// with signatures differing by ≤ this many bits are treated as the
/// same payload for combining purposes.
pub const DEFAULT_HAMMING_TOLERANCE: u32 = 4;

/// Default cache capacity (total entries across all coarse-key buckets).
/// Sized for a busy FT8 band where ~100 candidates per slot ×
/// TTL/slot_period entries can coexist. LRU eviction caps memory.
pub const DEFAULT_CAPACITY: usize = 256;

/// Default TTL in seconds. Sized for FT8's 15-second slot — three minutes
/// covers a typical CQ cycle without unbounded growth.
pub const DEFAULT_TTL_SECONDS: u64 = 180;

/// Mode discriminator. Currently FT8-only on the pancetta side, but the
/// type is an enum to leave room for future JS8 submode support without
/// breaking the key layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    /// FT8 protocol, 15-second slot, 8-GFSK at 6.25 baud.
    Ft8,
    /// FT4 protocol, 7.5-second slot, 4-GFSK at 20.8 baud.
    Ft4,
}

/// Coarse cache key. Two receptions with the same `CombinerKey` are
/// *candidates* for combining; the signature then filters by approximate
/// payload identity.
///
/// `freq_bin` and `time_bin` are deliberately coarse — the bin grid must
/// be wide enough to absorb per-reception jitter (one symbol period in
/// the time axis works in practice for FT8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CombinerKey {
    /// Protocol / submode identifier.
    pub mode: Mode,
    /// Frequency bin (coarse — typically the candidate's integer
    /// 6.25 Hz bin from the spectrogram).
    pub freq_bin: u32,
    /// Time bin (coarse — typically the candidate's DT in tenths of a
    /// second, or one-symbol-period units).
    pub time_bin: i32,
}

impl CombinerKey {
    /// Construct a new combiner key.
    pub fn new(mode: Mode, freq_bin: u32, time_bin: i32) -> Self {
        Self {
            mode,
            freq_bin,
            time_bin,
        }
    }
}

/// One cached reception's combined LLR state.
#[derive(Debug, Clone)]
struct Entry {
    /// 32-bit approximate-match signature derived from LLR sign bits.
    signature: u32,
    /// Combined LLR buffer (sum of all contributing receptions).
    llrs: [f32; LLR_LEN],
    /// Number of receptions that have contributed to `llrs`.
    repeat_count: u32,
    /// When this entry was last touched (combined into or inserted).
    last_touched: Instant,
}

/// Configuration knobs for the soft combiner.
#[derive(Debug, Clone)]
pub struct SoftCombinerConfig {
    /// Maximum number of entries retained across all coarse-key buckets.
    /// Excess entries are evicted oldest-first (LRU).
    pub capacity: usize,
    /// Time-to-live before an unused entry is evicted on the next
    /// cleanup pass.
    pub ttl: Duration,
    /// Maximum Hamming distance between signatures that still counts as
    /// the same payload for combining purposes.
    pub hamming_tolerance: u32,
}

impl Default for SoftCombinerConfig {
    fn default() -> Self {
        Self {
            capacity: DEFAULT_CAPACITY,
            ttl: Duration::from_secs(DEFAULT_TTL_SECONDS),
            hamming_tolerance: DEFAULT_HAMMING_TOLERANCE,
        }
    }
}

/// Outcome of a `combine` call.
#[derive(Debug, Clone)]
pub struct CombineResult {
    /// The LLR stream the LDPC stage should consume. Equals the input
    /// LLRs on a cache miss; equals the element-wise sum of the input
    /// and the cached entry on a cache hit.
    pub llrs: [f32; LLR_LEN],
    /// Number of receptions that contributed to `llrs` (always ≥ 1).
    /// 1 = first reception (no combining occurred). ≥ 2 = combined.
    pub repeat_count: u32,
}

/// Soft combiner cache.
///
/// See module-level docs for the algorithm. The cache is intended to
/// live across decode slots — wrap in a `Mutex` when sharing across
/// rayon workers.
#[derive(Debug)]
pub struct SoftCombiner {
    cfg: SoftCombinerConfig,
    cache: HashMap<CombinerKey, Vec<Entry>>,
    /// Total number of entries currently held (across all buckets).
    /// Maintained incrementally to avoid summing bucket lengths.
    total_entries: usize,
}

impl SoftCombiner {
    /// Construct a new soft combiner with the given configuration.
    pub fn new(cfg: SoftCombinerConfig) -> Self {
        Self {
            cfg,
            cache: HashMap::new(),
            total_entries: 0,
        }
    }

    /// Construct a soft combiner with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(SoftCombinerConfig::default())
    }

    /// Number of entries currently held (across all coarse-key buckets).
    /// Mostly useful for telemetry and tests.
    pub fn len(&self) -> usize {
        self.total_entries
    }

    /// True when the cache holds no entries.
    pub fn is_empty(&self) -> bool {
        self.total_entries == 0
    }

    /// Construct the 32-bit signature for an LLR array. The signature
    /// is deterministic across runs — it samples the sign bits at the
    /// first 32 fixed positions and packs them MSB-first. Same payload
    /// → mostly-same signature; different payload → very different
    /// signature.
    ///
    /// We sample positions `0, 1, 2, ..., 31` rather than spreading
    /// across all 174 bits because contiguous early bits give a tight
    /// constant-fold for the hot path and the spec only requires that
    /// the position set be deterministic. The first 32 bits of an FT8
    /// codeword span the first ~11 symbols which carry message-type +
    /// callsign-1 — high information content per bit.
    pub fn compute_signature(llrs: &[f32; LLR_LEN]) -> u32 {
        let mut sig: u32 = 0;
        for i in 0..SIGNATURE_WIDTH {
            // Pack sign(llrs[i]) into bit (SIGNATURE_WIDTH - 1 - i) so
            // the signature reads MSB-first.
            let bit = (llrs[i] < 0.0) as u32;
            sig |= bit << (SIGNATURE_WIDTH - 1 - i);
        }
        sig
    }

    /// Combine an incoming LLR stream with any cached prior reception
    /// at the same coarse key (within the configured Hamming tolerance).
    ///
    /// On a cache hit: the cached entry's LLRs are element-wise added to
    /// the input, the cached entry is refreshed in place, and the
    /// combined LLR stream is returned (a copy — the caller, the LDPC
    /// decoder, mutates it).
    ///
    /// On a cache miss: a new entry is inserted carrying the input LLRs
    /// and a fresh timestamp; the input LLRs are returned unchanged.
    pub fn combine(&mut self, key: CombinerKey, llrs: &[f32; LLR_LEN]) -> CombineResult {
        let signature = Self::compute_signature(llrs);
        let now = Instant::now();

        // Look up the bucket; find the closest match within Hamming
        // tolerance. We pick the *closest* match (smallest Hamming
        // distance) so deterministic signatures cluster correctly.
        let mut best_match: Option<usize> = None;
        let mut best_distance: u32 = u32::MAX;
        if let Some(bucket) = self.cache.get(&key) {
            for (idx, entry) in bucket.iter().enumerate() {
                let xor = entry.signature ^ signature;
                let distance = xor.count_ones();
                if distance <= self.cfg.hamming_tolerance && distance < best_distance {
                    best_distance = distance;
                    best_match = Some(idx);
                }
            }
        }

        if let Some(match_idx) = best_match {
            // Cache hit: combine in place and return a copy.
            let bucket = self
                .cache
                .get_mut(&key)
                .expect("bucket existed during lookup");
            let entry = &mut bucket[match_idx];
            for i in 0..LLR_LEN {
                entry.llrs[i] += llrs[i];
            }
            entry.repeat_count = entry.repeat_count.saturating_add(1);
            entry.last_touched = now;
            // Update the signature toward the most recent reception's
            // sign bits. This is a *moving fingerprint* — the next
            // reception is compared against the latest signature, not
            // the original first-reception signature. Equivalent to
            // re-signing the accumulated buffer.
            entry.signature = Self::compute_signature(&entry.llrs);
            CombineResult {
                llrs: entry.llrs,
                repeat_count: entry.repeat_count,
            }
        } else {
            // Cache miss: insert and return unchanged.
            self.insert(key, signature, llrs, now);
            CombineResult {
                llrs: *llrs,
                repeat_count: 1,
            }
        }
    }

    /// Insert a fresh entry at `(key, signature)`. Runs TTL cleanup and
    /// LRU eviction inline to bound cache size on bursty traffic.
    fn insert(&mut self, key: CombinerKey, signature: u32, llrs: &[f32; LLR_LEN], now: Instant) {
        // Drop TTL-expired entries before considering the new arrival.
        self.cleanup_at(now);

        // If at capacity, evict the oldest entry across all buckets
        // before inserting. We pay one O(n) scan per over-capacity
        // insert; for capacity ≤ 1024 this is negligible.
        while self.total_entries >= self.cfg.capacity {
            if !self.evict_oldest() {
                break;
            }
        }

        let entry = Entry {
            signature,
            llrs: *llrs,
            repeat_count: 1,
            last_touched: now,
        };
        self.cache.entry(key).or_default().push(entry);
        self.total_entries += 1;
    }

    /// Mark a payload at this coarse key as successfully decoded
    /// downstream (e.g. CRC passed). Drops all entries in the bucket —
    /// once a payload has cleared CRC there is no value in continuing
    /// to accumulate softness for it, and we don't want a stale
    /// signature to pollute future combines at the same coarse key.
    pub fn mark_decoded(&mut self, key: CombinerKey) {
        if let Some(bucket) = self.cache.remove(&key) {
            self.total_entries -= bucket.len();
        }
    }

    /// Drop all entries older than the configured TTL. Safe to call
    /// externally; also runs inline on every `insert`.
    pub fn cleanup(&mut self) {
        self.cleanup_at(Instant::now());
    }

    fn cleanup_at(&mut self, now: Instant) {
        let ttl = self.cfg.ttl;
        // First pass: drop expired entries within each bucket.
        for bucket in self.cache.values_mut() {
            bucket.retain(|entry| now.duration_since(entry.last_touched) <= ttl);
        }
        // Second pass: drop newly-empty buckets and recount.
        let mut new_total = 0usize;
        self.cache.retain(|_, bucket| {
            if bucket.is_empty() {
                false
            } else {
                new_total += bucket.len();
                true
            }
        });
        self.total_entries = new_total;
    }

    /// Evict the single oldest entry across all buckets. Returns `true`
    /// if an entry was removed.
    fn evict_oldest(&mut self) -> bool {
        let mut oldest_key: Option<CombinerKey> = None;
        let mut oldest_idx: usize = 0;
        let mut oldest_time: Option<Instant> = None;
        for (k, bucket) in self.cache.iter() {
            for (idx, entry) in bucket.iter().enumerate() {
                let should_replace = match oldest_time {
                    None => true,
                    Some(t) => entry.last_touched < t,
                };
                if should_replace {
                    oldest_key = Some(*k);
                    oldest_idx = idx;
                    oldest_time = Some(entry.last_touched);
                }
            }
        }
        if let Some(k) = oldest_key {
            if let Some(bucket) = self.cache.get_mut(&k) {
                bucket.swap_remove(oldest_idx);
                if bucket.is_empty() {
                    self.cache.remove(&k);
                }
                self.total_entries -= 1;
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construct a deterministic LLR array where the first `n` bits
    /// have the given sign pattern (1 = negative, 0 = positive) and the
    /// rest are zero. Used for crafting signature edge cases.
    fn llrs_from_bits(bits: &[u8]) -> [f32; LLR_LEN] {
        let mut llrs = [0.5_f32; LLR_LEN];
        for (i, &b) in bits.iter().enumerate().take(LLR_LEN) {
            llrs[i] = if b == 1 { -1.0 } else { 1.0 };
        }
        llrs
    }

    #[test]
    fn signature_packs_sign_bits_msb_first() {
        // First 8 bits negative (sign bit 1), next 24 positive.
        let mut bits = vec![1u8; 8];
        bits.extend(vec![0u8; 24]);
        let llrs = llrs_from_bits(&bits);
        let sig = SoftCombiner::compute_signature(&llrs);
        // High 8 bits are 1, low 24 bits are 0 → 0xFF00_0000.
        assert_eq!(sig, 0xFF00_0000, "got 0x{:08x}", sig);
    }

    #[test]
    fn signature_distinguishes_payloads() {
        let mut bits_a = vec![0u8; 32];
        bits_a[0] = 1;
        let mut bits_b = vec![0u8; 32];
        bits_b[1] = 1;
        let sig_a = SoftCombiner::compute_signature(&llrs_from_bits(&bits_a));
        let sig_b = SoftCombiner::compute_signature(&llrs_from_bits(&bits_b));
        assert_ne!(sig_a, sig_b);
        assert_eq!((sig_a ^ sig_b).count_ones(), 2);
    }

    #[test]
    fn first_reception_returns_unchanged_and_inserts() {
        let mut combiner = SoftCombiner::with_defaults();
        let llrs = llrs_from_bits(&[0, 1, 0, 1, 0, 1]);
        let key = CombinerKey::new(Mode::Ft8, 100, 0);
        let result = combiner.combine(key, &llrs);
        assert_eq!(result.repeat_count, 1);
        assert_eq!(result.llrs, llrs);
        assert_eq!(combiner.len(), 1);
    }

    #[test]
    fn second_reception_with_exact_signature_combines_additively() {
        let mut combiner = SoftCombiner::with_defaults();
        let llrs = llrs_from_bits(&[0, 1, 0, 1, 0, 1]);
        let key = CombinerKey::new(Mode::Ft8, 100, 0);

        let first = combiner.combine(key, &llrs);
        assert_eq!(first.repeat_count, 1);

        let second = combiner.combine(key, &llrs);
        assert_eq!(second.repeat_count, 2);
        for i in 0..LLR_LEN {
            assert!(
                (second.llrs[i] - 2.0 * llrs[i]).abs() < 1e-5,
                "bit {}: expected {}, got {}",
                i,
                2.0 * llrs[i],
                second.llrs[i]
            );
        }
        // Cache still holds exactly one entry (combine, not insert).
        assert_eq!(combiner.len(), 1);
    }

    #[test]
    fn third_reception_accumulates_to_3x() {
        let mut combiner = SoftCombiner::with_defaults();
        let llrs = llrs_from_bits(&[0, 1, 0, 1, 0, 1]);
        let key = CombinerKey::new(Mode::Ft8, 100, 0);
        combiner.combine(key, &llrs);
        combiner.combine(key, &llrs);
        let third = combiner.combine(key, &llrs);
        assert_eq!(third.repeat_count, 3);
        for i in 0..LLR_LEN {
            assert!((third.llrs[i] - 3.0 * llrs[i]).abs() < 1e-5);
        }
    }

    #[test]
    fn hamming_distance_within_tolerance_matches() {
        let mut combiner = SoftCombiner::with_defaults();
        let key = CombinerKey::new(Mode::Ft8, 50, 1);

        // First reception: 32 sign bits all zero.
        let llrs_a = llrs_from_bits(&vec![0u8; 32]);
        combiner.combine(key, &llrs_a);

        // Second reception: flip 3 sign bits → Hamming distance 3,
        // within default tolerance of 4. Should combine.
        let mut bits_b = vec![0u8; 32];
        bits_b[0] = 1;
        bits_b[5] = 1;
        bits_b[10] = 1;
        let llrs_b = llrs_from_bits(&bits_b);
        let result = combiner.combine(key, &llrs_b);
        assert_eq!(result.repeat_count, 2, "Hamming-3 should combine");
        assert_eq!(combiner.len(), 1);
    }

    #[test]
    fn hamming_distance_above_tolerance_inserts_new_entry() {
        let mut combiner = SoftCombiner::with_defaults();
        let key = CombinerKey::new(Mode::Ft8, 50, 1);

        let llrs_a = llrs_from_bits(&vec![0u8; 32]);
        combiner.combine(key, &llrs_a);

        // Flip 5 sign bits → Hamming distance 5, above default
        // tolerance of 4. Should NOT combine, should insert.
        let mut bits_b = vec![0u8; 32];
        for i in 0..5 {
            bits_b[i] = 1;
        }
        let llrs_b = llrs_from_bits(&bits_b);
        let result = combiner.combine(key, &llrs_b);
        assert_eq!(result.repeat_count, 1, "Hamming-5 should NOT combine");
        // Bucket now holds two entries.
        assert_eq!(combiner.len(), 2);
    }

    #[test]
    fn different_coarse_keys_do_not_combine() {
        let mut combiner = SoftCombiner::with_defaults();
        let llrs = llrs_from_bits(&[0, 1, 0, 1]);
        combiner.combine(CombinerKey::new(Mode::Ft8, 100, 0), &llrs);

        // Same signature but different freq_bin: different coarse key.
        let result = combiner.combine(CombinerKey::new(Mode::Ft8, 101, 0), &llrs);
        assert_eq!(result.repeat_count, 1);
        assert_eq!(combiner.len(), 2);

        // Same signature, same freq_bin, but different time_bin.
        let result = combiner.combine(CombinerKey::new(Mode::Ft8, 100, 1), &llrs);
        assert_eq!(result.repeat_count, 1);
        assert_eq!(combiner.len(), 3);

        // Same signature, same freq_bin, same time_bin, but different
        // mode (FT4 vs FT8).
        let result = combiner.combine(CombinerKey::new(Mode::Ft4, 100, 0), &llrs);
        assert_eq!(result.repeat_count, 1);
        assert_eq!(combiner.len(), 4);
    }

    #[test]
    fn lru_eviction_at_capacity() {
        let cfg = SoftCombinerConfig {
            capacity: 3,
            ..SoftCombinerConfig::default()
        };
        let mut combiner = SoftCombiner::new(cfg);

        // Insert 3 entries with distinct coarse keys.
        let llrs = llrs_from_bits(&[0, 1]);
        for i in 0..3 {
            combiner.combine(CombinerKey::new(Mode::Ft8, 100 + i, 0), &llrs);
        }
        assert_eq!(combiner.len(), 3);

        // Inserting a 4th evicts the oldest. With our coarse-grained
        // `Instant` clock, the entries inserted in order should age in
        // insertion order, so freq_bin=100 (the oldest) gets evicted.
        // Sleep briefly so Instant differs.
        std::thread::sleep(std::time::Duration::from_millis(2));
        combiner.combine(CombinerKey::new(Mode::Ft8, 200, 0), &llrs);
        assert_eq!(combiner.len(), 3, "still at capacity");

        // freq_bin=100 should be gone now.
        // Verify by trying to combine at freq_bin=100 — should be a
        // fresh insert (repeat_count=1), not a combine (repeat_count=2).
        let result = combiner.combine(CombinerKey::new(Mode::Ft8, 100, 0), &llrs);
        assert_eq!(
            result.repeat_count, 1,
            "oldest entry should have been evicted"
        );
    }

    #[test]
    fn mark_decoded_evicts_bucket() {
        let mut combiner = SoftCombiner::with_defaults();
        let llrs = llrs_from_bits(&[0, 1, 0, 1]);
        let key = CombinerKey::new(Mode::Ft8, 100, 0);
        combiner.combine(key, &llrs);
        combiner.combine(key, &llrs);
        assert_eq!(combiner.len(), 1);
        combiner.mark_decoded(key);
        assert_eq!(combiner.len(), 0);
    }

    #[test]
    fn mark_decoded_on_unknown_key_is_noop() {
        let mut combiner = SoftCombiner::with_defaults();
        let key = CombinerKey::new(Mode::Ft8, 999, 0);
        combiner.mark_decoded(key); // should not panic
        assert_eq!(combiner.len(), 0);
    }

    #[test]
    fn ttl_eviction_drops_stale_entries() {
        let cfg = SoftCombinerConfig {
            ttl: Duration::from_millis(20),
            ..SoftCombinerConfig::default()
        };
        let mut combiner = SoftCombiner::new(cfg);
        let llrs = llrs_from_bits(&[0, 1]);
        let key_a = CombinerKey::new(Mode::Ft8, 100, 0);
        let key_b = CombinerKey::new(Mode::Ft8, 200, 0);
        combiner.combine(key_a, &llrs);
        std::thread::sleep(Duration::from_millis(40));
        // Inserting at a different key triggers inline cleanup; key_a's
        // entry is now older than TTL and should be dropped.
        combiner.combine(key_b, &llrs);
        assert_eq!(combiner.len(), 1, "key_a should have been TTL-evicted");

        // key_a is now empty — combining there is a fresh insert.
        let result = combiner.combine(key_a, &llrs);
        assert_eq!(result.repeat_count, 1);
    }

    #[test]
    fn signature_is_deterministic_across_calls() {
        let llrs = llrs_from_bits(&[0, 1, 0, 0, 1, 1, 0, 1]);
        let a = SoftCombiner::compute_signature(&llrs);
        let b = SoftCombiner::compute_signature(&llrs);
        assert_eq!(a, b);
    }

    #[test]
    fn combine_result_llr_count_is_174() {
        let mut combiner = SoftCombiner::with_defaults();
        let llrs = [0.0_f32; LLR_LEN];
        let key = CombinerKey::new(Mode::Ft8, 100, 0);
        let result = combiner.combine(key, &llrs);
        assert_eq!(result.llrs.len(), LLR_LEN);
    }

    #[test]
    fn moving_signature_tracks_accumulated_buffer() {
        // After several combines, the signature should track the SIGN
        // pattern of the accumulated LLRs (not just the first
        // reception). This is the property that lets a noisy first
        // reception "settle in" once additional receptions confirm the
        // true sign at each position.
        let mut combiner = SoftCombiner::with_defaults();
        let key = CombinerKey::new(Mode::Ft8, 100, 0);

        // Reception 1: bit 0 wrongly negative (noise), bit 1 truly negative.
        let mut bits = vec![0u8; 32];
        bits[0] = 1; // false negative
        bits[1] = 1; // true negative
        let llrs_noisy = llrs_from_bits(&bits);
        combiner.combine(key, &llrs_noisy);

        // Receptions 2 & 3: bit 0 truly positive (= no negative sign),
        // bit 1 still negative. After accumulation, the sign at bit 0
        // should flip from negative to positive.
        let mut bits_clean = vec![0u8; 32];
        bits_clean[1] = 1;
        // Use a larger LLR magnitude so the sum dominates the noisy
        // first reception at bit 0.
        let mut llrs_clean = [0.5_f32; LLR_LEN];
        for (i, &b) in bits_clean.iter().enumerate().take(LLR_LEN) {
            llrs_clean[i] = if b == 1 { -3.0 } else { 3.0 };
        }
        combiner.combine(key, &llrs_clean);
        let result = combiner.combine(key, &llrs_clean);

        // After two clean receptions of (-3, +3, +3 at bit 0), the
        // accumulated bit-0 LLR is -1 + 3 + 3 = +5 → sign-positive.
        // So the moving signature at bit 0 should now read 0.
        assert!(
            result.llrs[0] > 0.0,
            "bit 0 should be sign-positive after clean receptions dominate"
        );
        assert_eq!(result.repeat_count, 3);
    }
}
