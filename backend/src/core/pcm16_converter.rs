/// PCM16 audio converter: resamples and converts float32 audio to 16kHz mono PCM16.
///
/// Microphone input arrives at 44100Hz or 48000Hz float32. AssemblyAI requires
/// 16000Hz signed 16-bit little-endian mono PCM.
#[allow(dead_code)]
pub const TARGET_SAMPLE_RATE: u32 = 16000;

/// Converts float32 audio samples at an arbitrary rate to PCM16 mono at 16kHz.
#[allow(dead_code)]
pub fn convert_float32_to_pcm16_mono(
    input_samples: &[f32],
    input_sample_rate: u32,
    input_channels: u16,
) -> Vec<u8> {
    let mono_samples = if input_channels > 1 {
        downmix_to_mono(input_samples, input_channels)
    } else {
        input_samples.to_vec()
    };

    let resampled = if input_sample_rate != TARGET_SAMPLE_RATE {
        resample_linear(&mono_samples, input_sample_rate, TARGET_SAMPLE_RATE)
    } else {
        mono_samples
    };

    let mut pcm16_bytes = Vec::with_capacity(resampled.len() * 2);
    for sample in &resampled {
        let clamped = sample.clamp(-1.0, 1.0);
        let scaled = (clamped * i16::MAX as f32) as i16;
        pcm16_bytes.extend_from_slice(&scaled.to_le_bytes());
    }

    pcm16_bytes
}

fn downmix_to_mono(interleaved: &[f32], channel_count: u16) -> Vec<f32> {
    let channels = channel_count as usize;
    let frames = interleaved.len() / channels;
    let mut mono = Vec::with_capacity(frames);
    for frame in 0..frames {
        let sum: f32 = (0..channels).map(|ch| interleaved[frame * channels + ch]).sum();
        mono.push(sum / channels as f32);
    }
    mono
}

fn resample_linear(input: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if input.is_empty() {
        return Vec::new();
    }
    let ratio = from_rate as f64 / to_rate as f64;
    let out_len = (input.len() as f64 / ratio).ceil() as usize;
    let mut output = Vec::with_capacity(out_len);

    for i in 0..out_len {
        let pos = i as f64 * ratio;
        let idx = pos as usize;
        let frac = pos - idx as f64;
        let sample = if idx + 1 < input.len() {
            let a = input[idx] as f64;
            let b = input[idx + 1] as f64;
            (a + frac * (b - a)) as f32
        } else if idx < input.len() {
            input[idx]
        } else {
            0.0
        };
        output.push(sample);
    }

    output
}
