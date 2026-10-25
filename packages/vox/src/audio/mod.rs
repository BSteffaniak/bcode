//! Bounded microphone capture and WAV decoding into mono 16 kHz recordings.
//!
//! No audio is logged. Microphone streams exist only for the lifetime of a capture.
//! File decoding and resampling do not require a microphone or `native-audio`.
//! Rubato (with `fft_resampler`) and audioadapter-buffers are required even in
//! no-microphone builds so that WAV conversion remains available.
use crate::config::Config;
use crate::contracts::{AudioSource, Error, Recording, Result};
use std::path::Path;

#[cfg(feature = "native-audio")]
mod native;
#[cfg(test)]
mod tests;

const TARGET_RATE: u32 = 16_000;
const MAX_SECONDS: u32 = 600;
const MAX_RATE: u32 = 192_000;
const MAX_CHANNELS: u16 = 32;
// At most 128 MiB of decoded mono input, independent of the input's channel count.
const MAX_FRAMES: usize = 32 * 1024 * 1024;

/// Construct a microphone factory without opening the microphone.
///
/// # Errors
/// Returns `Invalid` for invalid audio configuration and `Unsupported` when built
/// without `native-audio`. Device discovery errors occur on `AudioSource::start`.
pub fn source(config: &Config) -> Result<Box<dyn AudioSource>> {
    if !(1..=MAX_SECONDS).contains(&config.max_recording_seconds) {
        return Err(Error::Invalid(
            "recording duration must be 1..600 seconds".into(),
        ));
    }
    if config
        .audio_device
        .as_ref()
        .is_some_and(|name| name.trim().is_empty())
    {
        return Err(Error::Invalid(
            "audio device selection must not be empty".into(),
        ));
    }
    #[cfg(feature = "native-audio")]
    {
        Ok(Box::new(native::Microphone::new(
            config.audio_device.clone(),
            config.max_recording_seconds,
        )))
    }
    #[cfg(not(feature = "native-audio"))]
    {
        Err(Error::Unsupported(
            "microphone capture requires native-audio".into(),
        ))
    }
}

/// Read a PCM (8/16/24/32 bit integer or 32 bit float) WAV as mono 16 kHz.
///
/// Channels are averaged, not summed; stereo is supported without assuming a
/// particular source rate. Files are bounded to 600 seconds, 192 kHz, 32 channels
/// and 128 MiB decoded mono storage. Non-finite samples and incomplete frames fail.
///
/// # Errors
/// * Returns `Io` when opening the file fails.
/// * Returns `Invalid` for corrupt, empty, non-finite or oversized audio.
/// * Returns `Unsupported` for unsupported WAV sample formats.
/// * Returns `Backend` if resampling or allocation fails.
///
/// Decoder diagnostics are normalized and never include file contents or paths.
pub fn read_wav(path: &Path) -> Result<Recording> {
    let file = std::fs::File::open(path)?;
    let mut reader = hound::WavReader::new(std::io::BufReader::new(file))
        .map_err(|_| Error::Invalid("invalid WAV header".into()))?;
    let spec = reader.spec();
    let frames = usize::try_from(reader.duration())
        .map_err(|_| Error::Invalid("WAV is too large".into()))?;
    let capacity = frame_limit(spec.sample_rate, MAX_SECONDS)?;
    validate_channels(spec.channels)?;
    if frames == 0 || frames > capacity {
        return Err(Error::Invalid(
            "WAV is empty or exceeds audio limits".into(),
        ));
    }
    let channels = usize::from(spec.channels);
    if !u64::from(reader.len()).is_multiple_of(u64::from(spec.channels)) {
        return Err(Error::Invalid("WAV contains an incomplete frame".into()));
    }
    let mut mono = Vec::new();
    mono.try_reserve_exact(frames)
        .map_err(|_| Error::Backend("cannot allocate WAV buffer".into()))?;
    match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Int, bits @ (8 | 16 | 24 | 32)) => {
            let scale = 2_f64.powi(i32::from(bits) - 1);
            decode_frames(
                reader
                    .samples::<i32>()
                    .map(|sample| sample.map(|value| f64::from(value) / scale)),
                channels,
                frames,
                &mut mono,
            )?;
        }
        (hound::SampleFormat::Float, 32) => {
            decode_frames(
                reader.samples::<f32>().map(|sample| sample.map(f64::from)),
                channels,
                frames,
                &mut mono,
            )?;
        }
        _ => return Err(Error::Unsupported("unsupported WAV sample format".into())),
    }
    normalize(mono, spec.sample_rate)
}

fn validate_channels(channels: u16) -> Result<()> {
    if channels == 0 || channels > MAX_CHANNELS {
        return Err(Error::Invalid("audio channel count must be 1..32".into()));
    }
    Ok(())
}

fn frame_limit(rate: u32, seconds: u32) -> Result<usize> {
    if !(1..=MAX_RATE).contains(&rate) || !(1..=MAX_SECONDS).contains(&seconds) {
        return Err(Error::Invalid(
            "audio rate or duration exceeds supported bounds".into(),
        ));
    }
    // High-rate long recordings are capped by memory as well as wall-clock time.
    let frames = u64::from(rate) * u64::from(seconds);
    Ok(usize::try_from(frames)
        .unwrap_or(usize::MAX)
        .min(MAX_FRAMES))
}

fn decode_frames(
    samples: impl Iterator<Item = std::result::Result<f64, hound::Error>>,
    channels: usize,
    expected_frames: usize,
    mono: &mut Vec<f32>,
) -> Result<()> {
    let mut channel = 0;
    let mut sum = 0.0;
    for sample in samples {
        let value = sample.map_err(|_| Error::Invalid("invalid WAV sample data".into()))?;
        if !value.is_finite() {
            return Err(Error::Invalid("audio contains non-finite samples".into()));
        }
        sum += value.clamp(-1.0, 1.0);
        channel += 1;
        if channel == channels {
            if mono.len() == expected_frames {
                return Err(Error::Invalid("WAV exceeds declared length".into()));
            }
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            mono.push((sum / channels as f64) as f32);
            channel = 0;
            sum = 0.0;
        }
    }
    if channel != 0 || mono.len() != expected_frames {
        return Err(Error::Invalid(
            "WAV is truncated or has incomplete frames".into(),
        ));
    }
    Ok(())
}

fn normalize(mut samples: Vec<f32>, rate: u32) -> Result<Recording> {
    use audioadapter_buffers::direct::InterleavedSlice;
    use rubato::{Fft, FixedSync, Resampler};

    if samples.is_empty() || samples.len() > frame_limit(rate, MAX_SECONDS)? {
        return Err(Error::Invalid(
            "recording is empty or exceeds audio limits".into(),
        ));
    }
    for sample in &mut samples {
        if !sample.is_finite() {
            return Err(Error::Invalid("audio contains non-finite samples".into()));
        }
        *sample = sample.clamp(-1.0, 1.0);
    }
    if rate == TARGET_RATE {
        return Ok(Recording {
            samples,
            sample_rate: TARGET_RATE,
        });
    }
    let mut resampler = Fft::<f32>::new(
        usize::try_from(rate).map_err(|_| Error::Invalid("unsupported audio rate".into()))?,
        16_000,
        1024,
        1,
        FixedSync::Both,
    )
    .map_err(|_| Error::Backend("cannot initialize audio resampler".into()))?;
    let expected = (u64::try_from(samples.len())
        .map_err(|_| Error::Invalid("recording is too large".into()))?
        * u64::from(TARGET_RATE))
    .div_ceil(u64::from(rate));
    let expected = usize::try_from(expected)
        .map_err(|_| Error::Invalid("resampled recording is too large".into()))?;
    let delay = resampler.output_delay();
    let end = delay
        .checked_add(expected)
        .ok_or_else(|| Error::Invalid("resampled recording is too large".into()))?;
    let input_frames = resampler.input_frames_next();
    let output_frames = resampler.output_frames_next();
    let mut input_chunk = vec![0.0; input_frames];
    let mut output_chunk = vec![0.0; output_frames];
    let mut output = Vec::new();
    output
        .try_reserve_exact(expected)
        .map_err(|_| Error::Backend("cannot allocate resampled audio".into()))?;
    let mut consumed = 0;
    let mut produced = 0;
    // Feed full, zero-padded chunks until the delayed useful output is complete.
    // Trim delay ourselves: this also covers recordings shorter than one chunk.
    while produced < end {
        input_chunk.fill(0.0);
        let copied = samples.len().saturating_sub(consumed).min(input_frames);
        input_chunk[..copied].copy_from_slice(&samples[consumed..consumed + copied]);
        consumed += copied;
        let input = InterleavedSlice::new(&input_chunk, 1, input_frames)
            .map_err(|_| Error::Backend("invalid resampler input buffer".into()))?;
        let mut destination = InterleavedSlice::new_mut(&mut output_chunk, 1, output_frames)
            .map_err(|_| Error::Backend("invalid resampler output buffer".into()))?;
        let (_, written) = resampler
            .process_into_buffer(&input, &mut destination, None)
            .map_err(|_| Error::Backend("audio resampling failed".into()))?;
        if written == 0 || written > output_frames {
            return Err(Error::Backend("resampler made no valid progress".into()));
        }
        let start = delay.saturating_sub(produced).min(written);
        let stop = end.saturating_sub(produced).min(written);
        output.extend_from_slice(&output_chunk[start..stop]);
        produced += written;
    }
    for sample in &mut output {
        if !sample.is_finite() {
            return Err(Error::Backend("resampler produced non-finite audio".into()));
        }
        *sample = sample.clamp(-1.0, 1.0);
    }
    Ok(Recording {
        samples: output,
        sample_rate: TARGET_RATE,
    })
}
