//! Behavioral tests use only numeric loopback HTTP and inject a test-only key.
//! No process environment mutation, cloud endpoint, desktop, or microphone access.
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use super::*;

const TEST_KEY: &str = "vox-private-test-key";

struct Mock {
    base: String,
    thread: JoinHandle<Vec<u8>>,
}

impl Mock {
    fn spawn(handler: impl FnOnce(&mut TcpStream) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback mock");
        let base = format!("http://{}/v1", listener.local_addr().expect("mock address"));
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let thread = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "mock did not receive a request");
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("mock accept failed: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("read timeout");
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .expect("write timeout");
            let request = read_request(&mut stream);
            handler(&mut stream);
            request
        });
        Self { base, thread }
    }

    fn response(status: &str, body: &[u8]) -> Self {
        let response = response(status, body);
        Self::spawn(move |stream| stream.write_all(&response).expect("mock response"))
    }

    fn finish(self) -> Vec<u8> {
        self.thread.join().expect("mock server completed")
    }
}

fn response(status: &str, body: &[u8]) -> Vec<u8> {
    let mut result = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
    result.extend_from_slice(body);
    result
}

fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let (header_end, content_length) = loop {
        let count = stream.read(&mut buffer).expect("read request headers");
        assert!(count > 0, "request ended before headers");
        request.extend_from_slice(&buffer[..count]);
        assert!(
            request.len() <= 128 * 1024,
            "mock request exceeded test bound"
        );
        if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&request[..end]).expect("ASCII headers");
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("content length"))
                })
                .expect("multipart request has a fixed length");
            break (end + 4, length);
        }
    };
    assert!(
        content_length <= 128 * 1024,
        "mock body exceeded test bound"
    );
    while request.len() < header_end + content_length {
        let count = stream.read(&mut buffer).expect("read request body");
        assert!(count > 0, "request ended before body");
        request.extend_from_slice(&buffer[..count]);
    }
    request
}

fn config(base: &str) -> Config {
    Config {
        backend: "cloud".into(),
        cloud_url: Some(base.into()),
        cloud_model: Some("test-model".into()),
        ..Config::default()
    }
}

fn client(base: &str) -> CloudRecognizer {
    let config = config(base);
    CloudRecognizer::new(
        &config,
        endpoint(&config).expect("mock URL"),
        TEST_KEY,
        Duration::from_secs(2),
    )
    .expect("mock client")
}

fn audio() -> Recording {
    Recording {
        samples: vec![-1.0, -0.5, 0.0, 0.5, 1.0],
        sample_rate: 16_000,
    }
}

fn contains(bytes: &[u8], needle: &[u8]) -> bool {
    bytes.windows(needle.len()).any(|window| window == needle)
}

#[test]
fn uploads_in_memory_mono_wav_with_explicit_model_language_and_vocabulary() {
    let mock = Mock::response("200 OK", br#"{"text":"recognized final","ignored":true}"#);
    let mut recognizer = client(&mock.base);
    let options = RecognitionOptions {
        language: Some("en".into()),
        vocabulary: vec!["Vox".into(), "Rust".into()],
    };
    let result = recognizer
        .recognize(&audio(), &options, &Cancellation::new())
        .expect("recognition");
    assert_eq!(result.text, "recognized final");
    let request = mock.finish();
    assert!(request.starts_with(b"POST /v1/audio/transcriptions HTTP/1.1\r\n"));
    assert!(contains(&request, format!("Bearer {TEST_KEY}").as_bytes()));
    assert!(contains(&request, b"multipart/form-data; boundary="));
    for field in [
        "name=\"model\"\r\n\r\ntest-model",
        "name=\"language\"\r\n\r\nen",
        "name=\"prompt\"\r\n\r\nVox, Rust",
        "name=\"response_format\"\r\n\r\njson",
        "filename=\"recording.wav\"",
        "Content-Type: audio/wav",
    ] {
        assert!(
            contains(&request, field.as_bytes()),
            "missing multipart field"
        );
    }
    let start = request
        .windows(4)
        .position(|window| window == b"RIFF")
        .expect("WAV upload");
    let length = u32::from_le_bytes(request[start + 4..start + 8].try_into().expect("RIFF size"))
        as usize
        + 8;
    let mut wav =
        hound::WavReader::new(Cursor::new(&request[start..start + length])).expect("valid WAV");
    assert_eq!(wav.spec().channels, 1);
    assert_eq!(wav.spec().sample_rate, 16_000);
    assert_eq!(wav.spec().bits_per_sample, 16);
    let samples: Vec<i16> = wav
        .samples()
        .collect::<std::result::Result<_, _>>()
        .expect("PCM samples");
    assert_eq!(samples, [-32767, -16383, 0, 16383, 32767]);
}

#[test]
fn absent_optional_hints_are_not_sent_and_text_is_not_trimmed() {
    let mock = Mock::response("200 OK", "{\"text\":\"  final 世界\\n\"}".as_bytes());
    let result = client(&mock.base)
        .recognize(
            &audio(),
            &RecognitionOptions::default(),
            &Cancellation::new(),
        )
        .expect("recognition");
    assert_eq!(result.text, "  final 世界\n");
    let request = mock.finish();
    assert!(!contains(&request, b"name=\"language\""));
    assert!(!contains(&request, b"name=\"prompt\""));
}

#[test]
fn rejects_implicit_cloud_or_unsafe_url_without_exposing_input() {
    let mut value = config("https://example.invalid/v1");
    value.backend = "local".into();
    assert!(matches!(recognizer(&value), Err(Error::Invalid(_))));
    value.backend = "cloud".into();
    for base in [
        None,
        Some("not a URL private-secret"),
        Some("https://private-secret:password@example.invalid/v1"),
        Some("https://example.invalid/v1?api_key=private-secret"),
        Some("https://example.invalid/v1#private-secret"),
        Some("http://example.invalid/v1"),
        Some("file:///private-secret"),
    ] {
        value.cloud_url = base.map(str::to_owned);
        let Err(error) = endpoint(&value) else {
            panic!("unsafe URL accepted");
        };
        assert!(!format!("{error:?} {error}").contains("private-secret"));
    }
    value.cloud_url = Some("https://example.invalid/v1/".into());
    assert_eq!(
        endpoint(&value).expect("explicit HTTPS base").as_str(),
        "https://example.invalid/v1/audio/transcriptions"
    );
    value.cloud_url = Some("http://[::1]:1234/v1".into());
    assert!(endpoint(&value).is_ok());
}

#[test]
fn requires_explicit_model_valid_environment_reference_and_valid_key() {
    let mut value = config("http://127.0.0.1:1/v1");
    value.cloud_key_env = "NOT-A-VALID-NAME".into();
    assert!(matches!(recognizer(&value), Err(Error::Invalid(_))));
    for key in ["", " ", "private-secret\r\nInjected: yes"] {
        let Err(error) = CloudRecognizer::new(
            &value,
            endpoint(&value).expect("endpoint"),
            key,
            REQUEST_TIMEOUT,
        ) else {
            panic!("invalid key accepted");
        };
        assert!(!format!("{error:?} {error}").contains("private-secret"));
    }
    value.cloud_model = None;
    assert!(
        CloudRecognizer::new(
            &value,
            endpoint(&value).expect("endpoint"),
            TEST_KEY,
            REQUEST_TIMEOUT
        )
        .is_err()
    );
}

#[test]
fn redirect_is_not_followed_even_to_same_origin() {
    let location = "http://127.0.0.1:1/private-secret";
    let wire = format!(
        "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let mock = Mock::spawn(move |stream| {
        stream
            .write_all(wire.as_bytes())
            .expect("redirect response");
    });
    let error = client(&mock.base)
        .recognize(
            &audio(),
            &RecognitionOptions::default(),
            &Cancellation::new(),
        )
        .expect_err("redirect rejected");
    assert!(error.to_string().contains("HTTP 307"));
    assert!(!format!("{error:?} {error}").contains("private-secret"));
    mock.finish();
}

#[test]
fn response_failures_are_redacted() {
    for (status, body) in [
        (
            "401 Unauthorized",
            br#"{"error":"vox-private-test-key private-transcript"}"#.as_slice(),
        ),
        (
            "200 OK",
            br#"{"text":123,"secret":"vox-private-test-key private-transcript"}"#.as_slice(),
        ),
        (
            "200 OK",
            b"vox-private-test-key private-transcript not JSON".as_slice(),
        ),
        (
            "200 OK",
            br#"{"other":"vox-private-test-key private-transcript"}"#.as_slice(),
        ),
    ] {
        let mock = Mock::response(status, body);
        let error = client(&mock.base)
            .recognize(
                &audio(),
                &RecognitionOptions::default(),
                &Cancellation::new(),
            )
            .expect_err("invalid response");
        let diagnostic = format!("{error:?} {error}");
        assert!(!diagnostic.contains(TEST_KEY));
        assert!(!diagnostic.contains("private-transcript"));
        assert!(!diagnostic.contains("127.0.0.1"));
        mock.finish();
    }
}

#[test]
fn response_limit_applies_to_declared_and_chunked_bodies() {
    for chunked in [false, true] {
        let mock = Mock::spawn(move |stream| {
            if chunked {
                let headers =
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
                stream.write_all(headers).expect("chunked headers");
                let body = vec![b' '; MAX_RESPONSE_BYTES + 1];
                // The adapter may close as soon as its cap is reached.
                let _ = write!(stream, "{:x}\r\n", body.len());
                let _ = stream.write_all(&body);
                let _ = stream.write_all(b"\r\n0\r\n\r\n");
            } else {
                let wire = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    MAX_RESPONSE_BYTES + 1
                );
                stream
                    .write_all(wire.as_bytes())
                    .expect("oversized headers");
            }
        });
        let error = client(&mock.base)
            .recognize(
                &audio(),
                &RecognitionOptions::default(),
                &Cancellation::new(),
            )
            .expect_err("oversize rejected");
        assert!(error.to_string().contains("exceeds 1048576 bytes"));
        mock.finish();
    }
}

#[test]
fn pre_cancelled_recognition_does_not_connect() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind sentinel");
    listener
        .set_nonblocking(true)
        .expect("nonblocking sentinel");
    let base = format!("http://{}/v1", listener.local_addr().expect("address"));
    let cancel = Cancellation::new();
    cancel.cancel();
    assert!(matches!(
        client(&base).recognize(&audio(), &RecognitionOptions::default(), &cancel),
        Err(Error::Cancelled)
    ));
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
}

#[test]
fn cancellation_during_inflight_request_discards_even_successful_result() {
    let (received, ready) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let mock = Mock::spawn(move |stream| {
        received.send(()).expect("signal upload received");
        wait.recv_timeout(Duration::from_secs(2))
            .expect("release response");
        stream
            .write_all(&response("200 OK", br#"{"text":"must not deliver"}"#))
            .expect("response after cancellation");
    });
    let cancel = Cancellation::new();
    let worker_cancel = cancel.clone();
    let base = mock.base.clone();
    let worker = thread::spawn(move || {
        client(&base).recognize(&audio(), &RecognitionOptions::default(), &worker_cancel)
    });
    ready
        .recv_timeout(Duration::from_secs(2))
        .expect("request in flight");
    cancel.cancel();
    release.send(()).expect("release server");
    assert!(matches!(
        worker.join().expect("recognition worker"),
        Err(Error::Cancelled)
    ));
    mock.finish();
}

#[test]
fn cancellation_during_response_body_never_returns_transcript() {
    let (received, ready) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let mock = Mock::spawn(move |stream| {
        let body = br#"{"text":"must not deliver"}"#;
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(headers.as_bytes())
            .expect("response headers");
        stream.write_all(&body[..1]).expect("partial body");
        received.send(()).expect("signal partial response");
        wait.recv_timeout(Duration::from_secs(2))
            .expect("release remaining body");
        // Closing early is valid cancellation behavior as well.
        let _ = stream.write_all(&body[1..]);
    });
    let cancel = Cancellation::new();
    let worker_cancel = cancel.clone();
    let base = mock.base.clone();
    let worker = thread::spawn(move || {
        client(&base).recognize(&audio(), &RecognitionOptions::default(), &worker_cancel)
    });
    ready
        .recv_timeout(Duration::from_secs(2))
        .expect("partial response available");
    cancel.cancel();
    release.send(()).expect("release server");
    assert!(matches!(
        worker.join().expect("recognition worker"),
        Err(Error::Cancelled)
    ));
    mock.finish();
}

#[test]
fn stalled_response_times_out_with_safe_error() {
    let mock = Mock::spawn(|_stream| thread::sleep(Duration::from_millis(250)));
    let config = config(&mock.base);
    let mut recognizer = CloudRecognizer::new(
        &config,
        endpoint(&config).expect("endpoint"),
        TEST_KEY,
        Duration::from_millis(50),
    )
    .expect("client");
    let error = recognizer
        .recognize(
            &audio(),
            &RecognitionOptions::default(),
            &Cancellation::new(),
        )
        .expect_err("timeout");
    assert!(matches!(error, Error::Backend(_)));
    assert!(!error.to_string().contains(TEST_KEY));
    assert!(!error.to_string().contains("127.0.0.1"));
    mock.finish();
}

#[test]
fn malformed_audio_and_oversized_hints_never_connect() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind sentinel");
    listener
        .set_nonblocking(true)
        .expect("nonblocking sentinel");
    let base = format!("http://{}/v1", listener.local_addr().expect("address"));
    let mut recognizer = client(&base);
    for recording in [
        Recording {
            samples: vec![],
            sample_rate: 16_000,
        },
        Recording {
            samples: vec![0.0],
            sample_rate: 0,
        },
        Recording {
            samples: vec![f32::NAN],
            sample_rate: 16_000,
        },
        Recording {
            samples: vec![f32::INFINITY],
            sample_rate: 16_000,
        },
        Recording {
            samples: vec![1.1],
            sample_rate: 16_000,
        },
        Recording {
            samples: vec![0.0; 121],
            sample_rate: 1,
        },
    ] {
        assert!(matches!(
            recognizer.recognize(
                &recording,
                &RecognitionOptions::default(),
                &Cancellation::new()
            ),
            Err(Error::Invalid(_))
        ));
    }
    let options = RecognitionOptions {
        language: Some("\n".into()),
        vocabulary: vec![],
    };
    assert!(matches!(
        recognizer.recognize(&audio(), &options, &Cancellation::new()),
        Err(Error::Invalid(_))
    ));
    let options = RecognitionOptions {
        language: None,
        vocabulary: vec!["x".repeat(MAX_PROMPT_BYTES + 1)],
    };
    assert!(matches!(
        recognizer.recognize(&audio(), &options, &Cancellation::new()),
        Err(Error::Invalid(_))
    ));
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
}
