/// Simple linear resampler for WAV playback mode.
///
/// This is a basic interpolation resampler. For real-time use, the DSP pipeline's
/// high-quality SINC resampler is preferred.
pub(crate) fn resample_linear(input: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate {
        return input.to_vec();
    }

    let ratio = from_rate as f64 / to_rate as f64;
    let output_len = (input.len() as f64 / ratio) as usize;
    let mut output = Vec::with_capacity(output_len);

    for i in 0..output_len {
        let src_idx = i as f64 * ratio;
        let idx0 = src_idx as usize;
        let frac = src_idx - idx0 as f64;

        let sample = if idx0 + 1 < input.len() {
            input[idx0] * (1.0 - frac as f32) + input[idx0 + 1] * frac as f32
        } else if idx0 < input.len() {
            input[idx0]
        } else {
            0.0
        };

        output.push(sample);
    }

    output
}
