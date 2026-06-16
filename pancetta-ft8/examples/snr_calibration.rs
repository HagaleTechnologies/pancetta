//! SNR calibration bench.
//!
//! Generates real encoder -> modulator FT8 signals at a series of KNOWN true
//! SNRs (WSJT-X 2500 Hz reference convention), decodes them through both decode
//! paths (native `Ft8Decoder` and the `ft8_lib` FFI), and records the reported
//! SNR. Prints a table of true-vs-reported and the mean offset so the
//! estimators can be calibrated to the WSJT-X convention.
//!
//! WSJT-X SNR convention:
//!   SNR = 10*log10( P_signal / P_noise_in_2500Hz )
//! where P_signal is the mean power of the FT8 waveform over the slot and
//! P_noise_in_2500Hz is the white-noise power measured in a 2500 Hz reference
//! bandwidth. For real white Gaussian noise sampled at fs with total variance
//! sigma^2, the one-sided spectrum spans 0..fs/2, so the power in a 2500 Hz
//! slice is sigma^2 * 2500/(fs/2). The decode floor is ~ -21 dB; the reported
//! range is ~ -24..+20 dB.
//!
//! Run: cargo run -p pancetta-ft8 --features transmit --example snr_calibration

use pancetta_ft8::ft8_lib_ffi::ft8lib_decode_audio;
use pancetta_ft8::{
    Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator, NUM_SYMBOLS, SAMPLE_RATE, WINDOW_SAMPLES,
};

const TEST_MESSAGE: &str = "CQ K5ARH EM12";
/// Additional offset on top of the modulator's 1500 Hz base (so the signal
/// sits at ~1500 Hz, comfortably inside the audio band).
const TX_FREQ: f64 = 0.0;

/// Deterministic Box-Muller Gaussian noise generator (xorshift PRNG).
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    fn next_f64(&mut self) -> f64 {
        // xorshift64
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        // map to (0,1)
        ((x >> 11) as f64 + 1.0) / ((1u64 << 53) as f64 + 1.0)
    }
    fn next_gaussian(&mut self) -> f64 {
        let u1 = self.next_f64();
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

/// Build a clean FT8 waveform for `text`, padded to a full slot window.
fn clean_signal(text: &str) -> Vec<f32> {
    let mut encoder = Ft8Encoder::new();
    let symbols: [u8; NUM_SYMBOLS] = encoder.encode_message(text, None).unwrap();
    let mut modulator = Ft8Modulator::new_default().unwrap();
    let mut audio = modulator.modulate_symbols(&symbols, TX_FREQ).unwrap();
    audio.resize(WINDOW_SAMPLES, 0.0);
    audio
}

/// Mean power of the signal over the samples that actually carry signal energy
/// (i.e. excluding the trailing zero-pad). FT8 transmits continuously across
/// its ~12.64 s slot, so the active region is the modulated portion.
fn signal_power(audio: &[f32]) -> f64 {
    let active_len = audio
        .iter()
        .rposition(|&x| x != 0.0)
        .map(|i| i + 1)
        .unwrap_or(audio.len());
    let sum: f64 = audio[..active_len]
        .iter()
        .map(|&x| (x as f64) * (x as f64))
        .sum();
    sum / active_len as f64
}

/// Add white Gaussian noise calibrated so the WSJT-X 2500 Hz-reference SNR
/// equals `target_snr_db`. Returns the realized noise power in 2500 Hz.
fn add_calibrated_noise(audio: &mut [f32], p_sig: f64, target_snr_db: f64, seed: u64) -> f64 {
    // P_noise_2500 = p_sig / 10^(snr/10)
    let p_noise_2500 = p_sig / 10f64.powf(target_snr_db / 10.0);
    // P_noise_2500 = sigma^2 * 2500 / (fs/2)  =>  sigma^2 = P_noise_2500 * (fs/2) / 2500
    let half_band = SAMPLE_RATE as f64 / 2.0;
    let sigma2 = p_noise_2500 * half_band / 2500.0;
    let sigma = sigma2.sqrt();
    let mut rng = Rng::new(seed);
    for s in audio.iter_mut() {
        *s += (sigma * rng.next_gaussian()) as f32;
    }
    p_noise_2500
}

fn native_snr(audio: &[f32]) -> Option<f32> {
    let config = Ft8Config::default();
    let mut decoder = Ft8Decoder::new(config).unwrap();
    let msgs = decoder.decode_window(audio).unwrap_or_default();
    msgs.iter()
        .find(|m| m.text.contains("K5ARH"))
        .or_else(|| msgs.first())
        .map(|m| m.snr_db)
}

fn ffi_snr(audio: &[f32]) -> Option<f32> {
    let msgs = ft8lib_decode_audio(audio);
    msgs.iter()
        .find(|(t, ..): &&(String, f32, f32, i32, f32)| t.contains("K5ARH"))
        .or_else(|| msgs.first())
        .map(|(_, _, _, _, snr)| *snr)
}

fn main() {
    // Optional level scale + message override to confirm the calibration is
    // independent of absolute signal level and of message content.
    let scale: f32 = std::env::var("SNR_SCALE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1.0);
    let msg = std::env::var("SNR_MSG").unwrap_or_else(|_| TEST_MESSAGE.to_string());
    let key = if msg.contains("K5ARH") { "K5ARH" } else { "CQ" };
    let mut clean = clean_signal(&msg);
    if scale != 1.0 {
        for s in clean.iter_mut() {
            *s *= scale;
        }
    }
    let _ = key;
    let p_sig = signal_power(&clean);
    println!("Test message: {TEST_MESSAGE:?}  @ {TX_FREQ} Hz");
    println!("Active signal power P_sig = {p_sig:.6}");
    println!();
    println!(
        "{:>8} {:>10} {:>16} {:>16} {:>10} {:>10}",
        "true", "p_noise", "native_rep", "ffi_rep", "n_off", "f_off"
    );

    let targets: [f64; 11] = [
        -21.0, -19.0, -17.0, -15.0, -13.0, -11.0, -9.0, -6.0, -3.0, 0.0, 6.0,
    ];
    let trials = 8u64;

    let mut native_offsets = Vec::new();
    let mut ffi_offsets = Vec::new();
    // (true, reported) pairs over the operational band (true <= -3) for a fit.
    let mut nat_fit: Vec<(f64, f64)> = Vec::new();
    let mut ffi_fit: Vec<(f64, f64)> = Vec::new();

    for &target in &targets {
        let mut nat_sum = 0.0f64;
        let mut nat_n = 0u32;
        let mut ffi_sum = 0.0f64;
        let mut ffi_n = 0u32;
        let mut p_noise_2500 = 0.0;
        for trial in 0..trials {
            let mut audio = clean.clone();
            let seed = 0xABCD_0000u64
                .wrapping_add(trial.wrapping_mul(7919))
                .wrapping_add((target as i64 as u64).wrapping_mul(31));
            p_noise_2500 = add_calibrated_noise(&mut audio, p_sig, target, seed);
            if let Some(s) = native_snr(&audio) {
                nat_sum += s as f64;
                nat_n += 1;
            }
            if let Some(s) = ffi_snr(&audio) {
                ffi_sum += s as f64;
                ffi_n += 1;
            }
        }
        let nat_rep = if nat_n > 0 {
            nat_sum / nat_n as f64
        } else {
            f64::NAN
        };
        let ffi_rep = if ffi_n > 0 {
            ffi_sum / ffi_n as f64
        } else {
            f64::NAN
        };
        let nat_off = nat_rep - target;
        let ffi_off = ffi_rep - target;
        if nat_n > 0 {
            native_offsets.push(nat_off);
            if target <= -3.0 {
                nat_fit.push((target, nat_rep));
            }
        }
        if ffi_n > 0 {
            ffi_offsets.push(ffi_off);
            if target <= -3.0 {
                ffi_fit.push((target, ffi_rep));
            }
        }
        println!(
            "{:>8.1} {:>10.6} {:>9.2}({}/{}) {:>9.2}({}/{}) {:>10.2} {:>10.2}",
            target, p_noise_2500, nat_rep, nat_n, trials, ffi_rep, ffi_n, trials, nat_off, ffi_off
        );
    }

    let mean = |v: &[f64]| {
        if v.is_empty() {
            f64::NAN
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    };
    println!();
    println!(
        "Mean native offset (reported - true): {:+.2} dB  (n={})",
        mean(&native_offsets),
        native_offsets.len()
    );
    println!(
        "Mean ffi    offset (reported - true): {:+.2} dB  (n={})",
        mean(&ffi_offsets),
        ffi_offsets.len()
    );

    // Least-squares fit reported = slope*true + intercept over the operational
    // band (true <= -3 dB), where SNR reports actually matter for QSO decisions.
    let fit = |pts: &[(f64, f64)]| -> (f64, f64) {
        let n = pts.len() as f64;
        let sx: f64 = pts.iter().map(|p| p.0).sum();
        let sy: f64 = pts.iter().map(|p| p.1).sum();
        let sxx: f64 = pts.iter().map(|p| p.0 * p.0).sum();
        let sxy: f64 = pts.iter().map(|p| p.0 * p.1).sum();
        let slope = (n * sxy - sx * sy) / (n * sxx - sx * sx);
        let intercept = (sy - slope * sx) / n;
        (slope, intercept)
    };
    let (ns, ni) = fit(&nat_fit);
    let (fs, fi) = fit(&ffi_fit);
    println!();
    println!("Operational-band fit (true <= -3 dB), reported = slope*true + b:");
    println!("  native: slope={ns:.3}  b={ni:+.2}");
    println!("  ffi:    slope={fs:.3}  b={fi:+.2}");
}
