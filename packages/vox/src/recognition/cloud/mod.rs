//! Explicit, OpenAI-compatible cloud transcription. No discovery or fallback.
//!
//! Requests stay on the recognizer's worker. Cancellation is checked before upload,
//! between response reads, and before returning text. A blocked HTTP call cannot be
//! interrupted through reqwest's blocking API; network waits are timeout-bounded.
//! A request deadline is also checked between reads to stop slow-drip responses.
//! Cancellation never starts a detached upload or returns a cancelled transcript.
use std::{
    io::{Cursor, Read},
    time::{Duration, Instant},
};

use reqwest::{
    Url,
    blocking::{
        Client,
        multipart::{Form, Part},
    },
    header::{AUTHORIZATION, HeaderValue},
    redirect::Policy,
};
use serde::Deserialize;

use crate::{
    Error, Result,
    config::Config,
    contracts::{Cancellation, RecognitionOptions, Recognizer, Recording, Transcript},
};

const REQUEST_TIMEOUT: Duration = Duration::from_mins(1);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_WAV_BYTES: usize = 64 * 1024 * 1024;
const MAX_PROMPT_BYTES: usize = 16 * 1024;

/// Construct an explicitly selected cloud recognizer without sending a request.
///
/// `cloud_url` is an API base (for example `https://host/v1`); the adapter appends
/// `/audio/transcriptions`. HTTPS is required except for numeric loopback HTTP.
/// URL credentials, queries, fragments, redirects and implicit proxies are rejected
/// or disabled. The key is read only from the configured environment reference.
///
/// # Errors
/// Returns a redacted error for non-cloud selection, invalid cloud settings,
/// unavailable/invalid credentials, or HTTP client initialization failure.
pub fn recognizer(config: &Config) -> Result<Box<dyn Recognizer>> {
    // Validate before even reading credentials. Construction never probes the URL.
    let endpoint = endpoint(config)?;
    let name = &config.cloud_key_env;
    if name.is_empty()
        || !name.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
        })
    {
        return Err(Error::Invalid(
            "cloud_key_env must name an environment variable".into(),
        ));
    }
    let key = std::env::var(name)
        .map_err(|_| Error::Invalid("cloud API key environment variable is unavailable".into()))?;
    Ok(Box::new(CloudRecognizer::new(
        config,
        endpoint,
        &key,
        REQUEST_TIMEOUT,
    )?))
}

fn endpoint(config: &Config) -> Result<Url> {
    if config.backend != "cloud" {
        return Err(Error::Invalid(
            "cloud recognition requires explicit backend = cloud".into(),
        ));
    }
    config.validate()?;
    let mut url = config
        .cloud_url
        .as_deref()
        .and_then(|value| Url::parse(value).ok())
        .ok_or_else(|| {
            Error::Invalid("cloud_url must be an explicit absolute API base URL".into())
        })?;
    let loopback = url.host_str().is_some_and(|host| {
        // IPv6 host_str includes brackets; no DNS resolution is used for this exception.
        host.trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
    });
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Invalid("cloud_url requires HTTPS (or numeric loopback HTTP), without credentials, query or fragment".into()));
    }
    // Use path segments, not string concatenation, to preserve URL escaping.
    url.path_segments_mut()
        .map_err(|()| Error::Invalid("cloud_url cannot be an API base".into()))?
        .pop_if_empty()
        .push("audio")
        .push("transcriptions");
    Ok(url)
}

struct CloudRecognizer {
    client: Client,
    endpoint: Url,
    authorization: HeaderValue,
    model: String,
    max_seconds: u32,
    timeout: Duration,
}

impl CloudRecognizer {
    fn new(config: &Config, endpoint: Url, key: &str, timeout: Duration) -> Result<Self> {
        let model = config
            .cloud_model
            .as_deref()
            .filter(|model| {
                !model.trim().is_empty()
                    && model.len() <= 256
                    && !model.chars().any(char::is_control)
            })
            .ok_or_else(|| {
                Error::Invalid("cloud_model must be explicitly configured (1..256 bytes)".into())
            })?;
        if key.trim().is_empty() || key.len() > 8192 || key.chars().any(char::is_whitespace) {
            return Err(Error::Invalid("cloud API key is empty or invalid".into()));
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| Error::Invalid("cloud API key is invalid".into()))?;
        authorization.set_sensitive(true);
        let client = Client::builder()
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .connect_timeout(CONNECT_TIMEOUT.min(timeout))
            .timeout(timeout)
            .build()
            .map_err(|_| Error::Backend("cannot initialize cloud HTTP client".into()))?;
        Ok(Self {
            client,
            endpoint,
            authorization,
            model: model.to_owned(),
            max_seconds: config.max_recording_seconds,
            timeout,
        })
    }

    fn form(
        &self,
        recording: &Recording,
        options: &RecognitionOptions,
        cancel: &Cancellation,
    ) -> Result<Form> {
        cancel.check()?;
        let wav = encode_wav(recording, self.max_seconds, cancel)?;
        let file = Part::bytes(wav)
            .file_name("recording.wav")
            .mime_str("audio/wav")
            .map_err(|_| Error::Backend("cannot construct cloud audio upload".into()))?;
        let mut form = Form::new()
            .part("file", file)
            .text("model", self.model.clone())
            .text("response_format", "json");
        if let Some(language) = &options.language {
            if language.trim().is_empty()
                || language.len() > 64
                || language.chars().any(char::is_control)
            {
                return Err(Error::Invalid(
                    "recognition language must be 1..64 bytes without control characters".into(),
                ));
            }
            form = form.text("language", language.clone());
        }
        if !options.vocabulary.is_empty() {
            let mut prompt = String::new();
            for term in &options.vocabulary {
                cancel.check()?;
                if term.len()
                    > MAX_PROMPT_BYTES
                        .saturating_sub(prompt.len())
                        .saturating_sub(2)
                {
                    return Err(Error::Invalid(
                        "cloud vocabulary exceeds 16384 bytes".into(),
                    ));
                }
                if !prompt.is_empty() {
                    prompt.push_str(", ");
                }
                prompt.push_str(term);
            }
            form = form.text("prompt", prompt);
        }
        cancel.check()?;
        Ok(form)
    }

    fn request(
        &self,
        recording: &Recording,
        options: &RecognitionOptions,
        cancel: &Cancellation,
    ) -> Result<Transcript> {
        let form = self.form(recording, options, cancel)?;
        cancel.check()?;
        // Never propagate reqwest/JSON/IO errors: they can contain URLs, echoed
        // credentials or transcript-bearing response content.
        let started = Instant::now();
        let response = self
            .client
            .post(self.endpoint.clone())
            .header(AUTHORIZATION, self.authorization.clone())
            .multipart(form)
            .send();
        cancel.check()?;
        let mut response = response.map_err(|_| {
            Error::Backend("cloud transcription request failed or timed out".into())
        })?;
        if !response.status().is_success() {
            return Err(Error::Backend(format!(
                "cloud transcription returned HTTP {}",
                response.status().as_u16()
            )));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(Error::Backend(
                "cloud response exceeds 1048576 bytes".into(),
            ));
        }
        let mut body = Vec::new();
        let mut chunk = [0_u8; 8192];
        loop {
            cancel.check()?;
            if started.elapsed() >= self.timeout {
                return Err(Error::Backend(
                    "cloud transcription request timed out".into(),
                ));
            }
            let read = response.read(&mut chunk);
            cancel.check()?;
            let count = read
                .map_err(|_| Error::Backend("cannot read cloud transcription response".into()))?;
            if count == 0 {
                break;
            }
            if count > MAX_RESPONSE_BYTES.saturating_sub(body.len()) {
                return Err(Error::Backend(
                    "cloud response exceeds 1048576 bytes".into(),
                ));
            }
            body.extend_from_slice(&chunk[..count]);
        }
        cancel.check()?;
        if started.elapsed() >= self.timeout {
            return Err(Error::Backend(
                "cloud transcription request timed out".into(),
            ));
        }
        let text: TextResponse = serde_json::from_slice(&body).map_err(|_| {
            Error::Backend("cloud transcription response must be JSON with a text string".into())
        })?;
        cancel.check()?;
        Ok(Transcript { text: text.text })
    }
}

impl Recognizer for CloudRecognizer {
    fn recognize(
        &mut self,
        recording: &Recording,
        options: &RecognitionOptions,
        cancel: &Cancellation,
    ) -> Result<Transcript> {
        let result = self.request(recording, options, cancel);
        // Cancellation also takes precedence over validation/transport/parse errors.
        cancel.check()?;
        result
    }
}

#[derive(Deserialize)]
struct TextResponse {
    text: String,
}

fn encode_wav(recording: &Recording, max_seconds: u32, cancel: &Cancellation) -> Result<Vec<u8>> {
    if recording.sample_rate == 0
        || recording.sample_rate > 192_000
        || recording.samples.is_empty()
        || recording.samples.len() as u64
            > u64::from(recording.sample_rate) * u64::from(max_seconds)
        || recording.samples.len() > (MAX_WAV_BYTES - 44) / 2
    {
        return Err(Error::Invalid(
            "cloud audio must be nonempty, within capture bounds, and at most 64 MiB of WAV".into(),
        ));
    }
    let mut cursor = Cursor::new(Vec::with_capacity(44 + recording.samples.len() * 2));
    {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: recording.sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::new(&mut cursor, spec)
            .map_err(|_| Error::Backend("cannot encode cloud WAV audio".into()))?;
        for (index, sample) in recording.samples.iter().copied().enumerate() {
            if index % 4096 == 0 {
                cancel.check()?;
            }
            if !sample.is_finite() || !(-1.0..=1.0).contains(&sample) {
                return Err(Error::Invalid(
                    "cloud audio samples must be finite and within [-1, 1]".into(),
                ));
            }
            // The validated sample range fits i16; quantization intentionally truncates.
            #[allow(clippy::cast_possible_truncation)]
            let pcm = (sample * f32::from(i16::MAX)) as i16;
            writer
                .write_sample(pcm)
                .map_err(|_| Error::Backend("cannot encode cloud WAV audio".into()))?;
        }
        writer
            .finalize()
            .map_err(|_| Error::Backend("cannot finalize cloud WAV audio".into()))?;
    }
    cancel.check()?;
    Ok(cursor.into_inner())
}

#[cfg(test)]
mod tests;
