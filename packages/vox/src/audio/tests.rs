use super::*;

fn integer_wav(rate: u32, channels: u16, bits: u16, values: &[i32]) -> tempfile::NamedTempFile {
    let file = tempfile::NamedTempFile::new().unwrap();
    let spec = hound::WavSpec {
        channels,
        sample_rate: rate,
        bits_per_sample: bits,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(file.path(), spec).unwrap();
    for &value in values {
        writer.write_sample(value).unwrap();
    }
    writer.finalize().unwrap();
    file
}

#[test]
fn wav_pcm_depths_are_normalized_without_a_microphone() {
    for bits in [8_u16, 16, 24, 32] {
        let minimum = if bits == 32 {
            i32::MIN
        } else {
            -(1_i32 << (bits - 1))
        };
        let maximum = if bits == 32 {
            i32::MAX
        } else {
            (1_i32 << (bits - 1)) - 1
        };
        let file = integer_wav(TARGET_RATE, 1, bits, &[minimum, 0, maximum]);
        let recording = read_wav(file.path()).unwrap();
        assert_eq!(recording.sample_rate, TARGET_RATE);
        assert!((recording.samples[0] + 1.0).abs() < f32::EPSILON);
        assert!(recording.samples[1].abs() < f32::EPSILON);
        assert!(recording.samples[2] > 0.99);
        assert!(
            recording
                .samples
                .iter()
                .all(|value| (-1.0..=1.0).contains(value))
        );
    }
}

#[test]
fn stereo_channels_are_averaged() {
    let file = integer_wav(TARGET_RATE, 2, 16, &[16_384, -16_384, 16_384, 16_384]);
    let recording = read_wav(file.path()).unwrap();
    assert_eq!(recording.samples, vec![0.0, 0.5]);
}

#[test]
fn float_wav_is_clipped_and_nonfinite_is_rejected() {
    for (values, valid) in [
        (vec![-2.0_f32, 0.0, 2.0], true),
        (vec![f32::NAN], false),
        (vec![f32::INFINITY], false),
    ] {
        let file = tempfile::NamedTempFile::new().unwrap();
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: TARGET_RATE,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(file.path(), spec).unwrap();
        for value in values {
            writer.write_sample(value).unwrap();
        }
        writer.finalize().unwrap();
        let result = read_wav(file.path());
        if valid {
            assert_eq!(result.unwrap().samples, vec![-1.0, 0.0, 1.0]);
        } else {
            assert!(matches!(result, Err(Error::Invalid(_))));
        }
    }
}

#[test]
fn malformed_empty_and_unsupported_rate_files_fail() {
    let corrupt = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(corrupt.path(), b"not a wave file").unwrap();
    assert!(matches!(read_wav(corrupt.path()), Err(Error::Invalid(_))));
    let empty = integer_wav(TARGET_RATE, 1, 16, &[]);
    assert!(matches!(read_wav(empty.path()), Err(Error::Invalid(_))));
    let excessive_rate = integer_wav(MAX_RATE + 1, 1, 16, &[0]);
    assert!(matches!(
        read_wav(excessive_rate.path()),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn truncated_wav_does_not_return_partial_success() {
    let file = integer_wav(TARGET_RATE, 1, 16, &[0, 1, 2, 3]);
    let length = file.as_file().metadata().unwrap().len();
    file.as_file().set_len(length - 1).unwrap();
    assert!(read_wav(file.path()).is_err());
}

#[test]
fn incomplete_frames_and_incorrect_lengths_are_rejected() {
    let mut mono = Vec::with_capacity(2);
    assert!(decode_frames([Ok(0.0)].into_iter(), 2, 1, &mut mono).is_err());
    assert!(decode_frames([Ok(0.0), Ok(0.0)].into_iter(), 1, 1, &mut mono).is_err());
}

#[test]
fn resource_limits_are_checked_without_large_allocations() {
    assert!(frame_limit(0, 1).is_err());
    assert!(frame_limit(MAX_RATE + 1, 1).is_err());
    assert!(frame_limit(TARGET_RATE, 0).is_err());
    assert!(frame_limit(TARGET_RATE, MAX_SECONDS + 1).is_err());
    assert_eq!(frame_limit(MAX_RATE, MAX_SECONDS).unwrap(), MAX_FRAMES);
    assert!(validate_channels(0).is_err());
    assert!(validate_channels(MAX_CHANNELS + 1).is_err());
    assert!(normalize(Vec::new(), TARGET_RATE).is_err());
    assert!(normalize(vec![f32::NAN], TARGET_RATE).is_err());
}

#[test]
fn arbitrary_common_rates_have_exact_duration_including_short_and_tail_audio() {
    for rate in [8_000_u32, 11_025, 22_050, 44_100, 48_000, 96_000] {
        for length in [1_usize, 37, 4_097] {
            let mut input = vec![0.0; length];
            input[length - 1] = 0.5;
            let output = normalize(input, rate).unwrap();
            let expected =
                (u64::try_from(length).unwrap() * u64::from(TARGET_RATE)).div_ceil(u64::from(rate));
            assert_eq!(u64::try_from(output.samples.len()).unwrap(), expected);
            assert_eq!(output.sample_rate, TARGET_RATE);
            assert!(
                output
                    .samples
                    .iter()
                    .all(|value| value.is_finite() && value.abs() <= 1.0)
            );
            // A final impulse must survive delay removal and tail flushing.
            assert!(output.samples.iter().any(|value| value.abs() > 0.001));
        }
    }
}

#[test]
fn wav_resampling_produces_sixteen_khz() {
    let file = integer_wav(48_000, 2, 16, &vec![8192; 9_600]);
    let output = read_wav(file.path()).unwrap();
    assert_eq!(output.sample_rate, TARGET_RATE);
    assert_eq!(output.samples.len(), 1600);
    assert!((output.samples[800] - 0.25).abs() < 0.01);
}

#[test]
fn downsampling_rejects_out_of_band_energy() {
    fn tone(frequency: f32) -> Vec<f32> {
        (0_u16..48_000)
            .map(|frame| {
                (std::f32::consts::TAU * frequency * f32::from(frame) / 48_000.0).sin() * 0.5
            })
            .collect()
    }
    fn power(samples: &[f32]) -> f64 {
        samples
            .iter()
            .map(|sample| f64::from(*sample).powi(2))
            .sum::<f64>()
    }
    let speech_band = normalize(tone(1_000.0), 48_000).unwrap();
    let above_nyquist = normalize(tone(12_000.0), 48_000).unwrap();
    assert!(
        power(&above_nyquist.samples[1_000..15_000])
            < power(&speech_band.samples[1_000..15_000]) * 0.001
    );
}

#[test]
fn factory_rejects_invalid_audio_configuration_without_device_access() {
    let mut config = Config {
        max_recording_seconds: 0,
        ..Config::default()
    };
    assert!(matches!(source(&config), Err(Error::Invalid(_))));
    config.max_recording_seconds = 1;
    config.audio_device = Some("  ".into());
    assert!(matches!(source(&config), Err(Error::Invalid(_))));
}

#[cfg(not(feature = "native-audio"))]
#[test]
fn feature_disabled_factory_is_explicitly_unsupported() {
    assert!(matches!(
        source(&Config::default()),
        Err(Error::Unsupported(_))
    ));
}
