//! CPAL boundary. The factory is Send; its stream stays on its owning worker.
use super::{frame_limit, normalize, validate_channels};
use crate::contracts::{AudioSource, Capture, Error, Recording, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const HEALTHY: u8 = 0;
const OVERFLOW: u8 = 1;
const DEVICE_FAILED: u8 = 2;
const DURATION_EXCEEDED: u8 = 3;
const INVALID_SAMPLES: u8 = 4;
const MAX_CALLBACK_FRAMES: usize = 8192;

pub(super) struct Microphone {
    selection: Option<String>,
    max_seconds: u32,
    active: Arc<AtomicBool>,
}

impl Microphone {
    pub(super) fn new(selection: Option<String>, max_seconds: u32) -> Self {
        Self {
            selection,
            max_seconds,
            active: Arc::new(AtomicBool::new(false)),
        }
    }
}

struct Permit(Arc<AtomicBool>);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct Buffer {
    mono: Vec<f32>,
    limit: usize,
}

impl Buffer {
    fn new(limit: usize) -> Result<Self> {
        let mut mono = Vec::new();
        mono.try_reserve_exact(limit)
            .map_err(|_| Error::Backend("cannot allocate capture buffer".into()))?;
        Ok(Self { mono, limit })
    }

    // No allocation, waits, logging, I/O or inference. At most 8192 frames and
    // 32 channels are visited. A rejected callback invalidates the whole capture.
    fn append<T>(&mut self, data: &[T], channels: usize) -> std::result::Result<(), u8>
    where
        T: Sample + Copy,
        f32: FromSample<T>,
    {
        if channels == 0 || channels > 32 || !data.len().is_multiple_of(channels) {
            return Err(INVALID_SAMPLES);
        }
        let frames = data.len() / channels;
        if frames > MAX_CALLBACK_FRAMES || frames > self.limit.saturating_sub(self.mono.len()) {
            return Err(OVERFLOW);
        }
        for frame in data.chunks_exact(channels) {
            let mut sum = 0.0_f64;
            for sample in frame {
                let value = f32::from_sample(*sample);
                if !value.is_finite() {
                    return Err(INVALID_SAMPLES);
                }
                sum += f64::from(value.clamp(-1.0, 1.0));
            }
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            self.mono.push((sum / channels as f64) as f32);
        }
        Ok(())
    }
}

fn select_device(selection: Option<&str>) -> Result<cpal::Device> {
    let host = cpal::default_host();
    let Some(selection) = selection else {
        return host
            .default_input_device()
            .ok_or_else(|| Error::Unsupported("no default audio input device".into()));
    };
    // Prefer an exact stable identifier; never silently choose a different
    // microphone when a configured identifier/name is absent or ambiguous.
    let devices = host
        .input_devices()
        .map_err(|_| Error::Backend("cannot enumerate audio input devices".into()))?;
    let mut by_name = None;
    let mut ambiguous = false;
    for device in devices {
        if device.id().is_ok_and(|id| id.to_string() == selection) {
            return Ok(device);
        }
        if device
            .description()
            .is_ok_and(|description| description.name() == selection)
        {
            ambiguous |= by_name.is_some();
            by_name = Some(device);
        }
    }
    if ambiguous {
        return Err(Error::Invalid(
            "audio device name is ambiguous; use its stable identifier".into(),
        ));
    }
    by_name.ok_or_else(|| Error::Unsupported("configured audio input device not found".into()))
}

impl AudioSource for Microphone {
    fn start(&mut self, max_seconds: u32) -> Result<Box<dyn Capture>> {
        if max_seconds == 0 || max_seconds > self.max_seconds {
            return Err(Error::Invalid(
                "capture duration exceeds configured bound".into(),
            ));
        }
        self.active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Busy)?;
        let permit = Permit(Arc::clone(&self.active));
        let device = select_device(self.selection.as_deref())?;
        let supported = device
            .default_input_config()
            .map_err(|_| Error::Backend("cannot query audio input configuration".into()))?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        validate_channels(config.channels)?;
        let rate = config.sample_rate;
        let limit = frame_limit(rate, max_seconds)?;
        let buffer = Arc::new(Mutex::new(Buffer::new(limit)?));
        let failure = Arc::new(AtomicU8::new(HEALTHY));
        let duration = Duration::from_secs(u64::from(max_seconds));
        let started = Instant::now();
        let deadline = started + duration;
        macro_rules! build {
            ($sample:ty) => {
                build_stream::<$sample>(&device, config, &buffer, &failure, deadline)
            };
        }
        let stream = match format {
            SampleFormat::I8 => build!(i8),
            SampleFormat::I16 => build!(i16),
            SampleFormat::I24 => build!(cpal::I24),
            SampleFormat::I32 => build!(i32),
            SampleFormat::I64 => build!(i64),
            SampleFormat::U8 => build!(u8),
            SampleFormat::U16 => build!(u16),
            SampleFormat::U24 => build!(cpal::U24),
            SampleFormat::U32 => build!(u32),
            SampleFormat::U64 => build!(u64),
            SampleFormat::F32 => build!(f32),
            SampleFormat::F64 => build!(f64),
            _ => {
                return Err(Error::Unsupported(
                    "unsupported microphone sample format".into(),
                ));
            }
        }?;
        stream
            .play()
            .map_err(|_| Error::Backend("cannot start microphone stream".into()))?;
        Ok(Box::new(NativeCapture {
            stream: Some(stream),
            buffer,
            failure,
            rate,
            deadline,
            _permit: permit,
        }))
    }
}

fn build_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    buffer: &Arc<Mutex<Buffer>>,
    failure: &Arc<AtomicU8>,
    deadline: Instant,
) -> Result<cpal::Stream>
where
    T: SizedSample + Copy,
    f32: FromSample<T>,
{
    let channels = usize::from(config.channels);
    let buffer = Arc::clone(buffer);
    let callback_failure = Arc::clone(failure);
    let device_failure = Arc::clone(failure);
    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                if callback_failure.load(Ordering::Acquire) != HEALTHY {
                    return;
                }
                let result = if Instant::now() >= deadline {
                    Err(DURATION_EXCEEDED)
                } else if let Ok(mut buffer) = buffer.try_lock() {
                    buffer.append(data, channels)
                } else {
                    Err(OVERFLOW)
                };
                if let Err(code) = result {
                    let _ = callback_failure.compare_exchange(
                        HEALTHY,
                        code,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                }
            },
            move |_| {
                let _ = device_failure.compare_exchange(
                    HEALTHY,
                    DEVICE_FAILED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            },
            Some(Duration::from_secs(2)),
        )
        .map_err(|_| Error::Backend("cannot build microphone input stream".into()))
}

struct NativeCapture {
    // Drop the stream before its buffers and permit, including when cancelled by
    // dropping the handle. No Send bound is imposed on this worker-owned stream.
    stream: Option<cpal::Stream>,
    buffer: Arc<Mutex<Buffer>>,
    failure: Arc<AtomicU8>,
    rate: u32,
    deadline: Instant,
    _permit: Permit,
}

impl Capture for NativeCapture {
    fn finish(mut self: Box<Self>) -> Result<Recording> {
        let expired = Instant::now() >= self.deadline;
        // Dropping, rather than relying on pause support, closes the microphone
        // before potentially expensive final conversion and resampling.
        drop(self.stream.take());
        match self.failure.load(Ordering::Acquire) {
            OVERFLOW => {
                return Err(Error::Backend(
                    "audio capture overflow; recording discarded".into(),
                ));
            }
            DEVICE_FAILED => {
                return Err(Error::Backend(
                    "microphone stream failed; recording discarded".into(),
                ));
            }
            INVALID_SAMPLES => return Err(Error::Invalid("invalid microphone sample data".into())),
            HEALTHY if !expired => {}
            _ => return Err(Error::Invalid("recording duration exceeded".into())),
        }
        let samples = {
            let mut buffer = self
                .buffer
                .lock()
                .map_err(|_| Error::Backend("capture buffer unavailable".into()))?;
            std::mem::take(&mut buffer.mono)
        };
        normalize(samples, self.rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_unsigned_and_float_samples_are_normalized() {
        let mut buffer = Buffer::new(8).unwrap();
        buffer.append(&[i16::MIN, 0, i16::MAX], 1).unwrap();
        buffer.append(&[0_u16, 32768, u16::MAX], 1).unwrap();
        buffer.append(&[-2.0_f64, 2.0], 1).unwrap();
        assert!((buffer.mono[0] + 1.0).abs() < f32::EPSILON);
        assert!(buffer.mono[1].abs() < f32::EPSILON);
        assert!(buffer.mono[2] > 0.99);
        assert!((buffer.mono[3] + 1.0).abs() < f32::EPSILON);
        assert!(buffer.mono[4].abs() < f32::EPSILON);
        assert!(buffer.mono[5] > 0.99);
        assert_eq!(&buffer.mono[6..], &[-1.0, 1.0]);
    }

    #[test]
    fn downmix_is_average_and_overflow_does_not_grow_storage() {
        let mut buffer = Buffer::new(2).unwrap();
        let capacity = buffer.mono.capacity();
        buffer.append(&[1.0_f32, -1.0, 0.5, 0.5], 2).unwrap();
        assert_eq!(buffer.mono, vec![0.0, 0.5]);
        assert_eq!(buffer.append(&[0.0_f32, 0.0], 2), Err(OVERFLOW));
        assert_eq!(buffer.mono.capacity(), capacity);
        assert_eq!(buffer.mono.len(), 2);
    }

    #[test]
    fn callbacks_reject_nonfinite_partial_and_oversized_input() {
        let mut buffer = Buffer::new(MAX_CALLBACK_FRAMES + 1).unwrap();
        assert_eq!(buffer.append(&[f32::NAN], 1), Err(INVALID_SAMPLES));
        assert_eq!(buffer.append(&[0.0_f32], 2), Err(INVALID_SAMPLES));
        assert_eq!(buffer.append(&[0.0_f32], 0), Err(INVALID_SAMPLES));
        let large = vec![0.0_f32; MAX_CALLBACK_FRAMES + 1];
        assert_eq!(buffer.append(&large, 1), Err(OVERFLOW));
        assert!(buffer.mono.is_empty());
    }

    #[test]
    fn duration_validation_does_not_open_a_device() {
        let mut source = Microphone::new(None, 5);
        assert!(matches!(source.start(0), Err(Error::Invalid(_))));
        assert!(matches!(source.start(6), Err(Error::Invalid(_))));
    }

    #[test]
    fn busy_factory_does_not_open_a_device_and_permit_releases() {
        let mut source = Microphone::new(None, 5);
        source.active.store(true, Ordering::Release);
        assert!(matches!(source.start(1), Err(Error::Busy)));
        let permit = Permit(Arc::clone(&source.active));
        drop(permit);
        assert!(!source.active.load(Ordering::Acquire));
    }
}
