use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier};
use std::time::Duration;

#[cfg(target_os = "linux")]
use bcode_mermaid_render::WORKER_MEMORY_BYTES;
use bcode_mermaid_render::{
    MermaidCancellationToken, MermaidRenderError, MermaidRenderRequest, MermaidRenderedOutput,
    render_mermaid_with_worker,
};

const REQUEST_MAGIC: &[u8; 4] = b"BCMW";
const RESPONSE_MAGIC: &[u8; 4] = b"BCMR";
const VERSION: u16 = 1;

fn worker() -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_bcode-mermaid-worker"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn Mermaid worker")
}

fn request(version: u16, source: &str) -> Vec<u8> {
    let mut request = Vec::new();
    request.extend_from_slice(REQUEST_MAGIC);
    request.extend_from_slice(&version.to_be_bytes());
    request.extend_from_slice(&800_u32.to_be_bytes());
    request.extend_from_slice(&600_u32.to_be_bytes());
    request.extend_from_slice(&(4_u32 * 1024 * 1024).to_be_bytes());
    request.extend_from_slice(&u32::try_from(source.len()).unwrap().to_be_bytes());
    request.extend_from_slice(source.as_bytes());
    request
}

fn response(mut child: std::process::Child, request: &[u8]) -> (bool, Vec<u8>, bool) {
    child
        .stdin
        .take()
        .expect("worker stdin")
        .write_all(request)
        .expect("write request");
    let mut response = Vec::new();
    child
        .stdout
        .take()
        .expect("worker stdout")
        .read_to_end(&mut response)
        .expect("read response");
    let status = child.wait().expect("worker status");
    assert!(
        response.len() >= 11,
        "worker {:?} exited {status} with {} response bytes",
        env!("CARGO_BIN_EXE_bcode-mermaid-worker"),
        response.len()
    );
    assert_eq!(&response[..4], RESPONSE_MAGIC);
    assert_eq!(u16::from_be_bytes([response[4], response[5]]), VERSION);
    let success = response[6] == 1;
    let length = u32::from_be_bytes(response[7..11].try_into().unwrap()) as usize;
    assert_eq!(response.len(), 11 + length);
    (success, response[11..].to_vec(), status.success())
}

fn scripted_worker(script: &str) -> tempfile::NamedTempFile {
    use std::os::unix::fs::PermissionsExt;

    let mut worker = tempfile::NamedTempFile::new().unwrap();
    worker.write_all(script.as_bytes()).unwrap();
    let mut permissions = worker.as_file().metadata().unwrap().permissions();
    permissions.set_mode(0o700);
    worker.as_file().set_permissions(permissions).unwrap();
    worker
}

#[test]
#[cfg(target_os = "linux")]
fn public_worker_adapter_enforces_address_space_limit() {
    let worker = scripted_worker(&format!(
        "#!/bin/sh\npython3 - <<'PY'\nvalue = bytearray({})\nprint(len(value))\nPY\n",
        WORKER_MEMORY_BYTES.saturating_add(64 * 1024 * 1024)
    ));
    let request = MermaidRenderRequest::svg("flowchart LR\nA --> B", 800, 600);

    assert!(matches!(
        render_mermaid_with_worker(
            worker.path(),
            &request,
            &MermaidCancellationToken::default()
        ),
        Err(MermaidRenderError::InvalidWorkerResponse { .. })
    ));
}

#[test]
#[cfg(unix)]
fn public_worker_adapter_forcefully_terminates_timed_out_worker() {
    let worker = scripted_worker("#!/bin/sh\nsleep 30\n");
    let mut request = MermaidRenderRequest::svg("flowchart LR\nA --> B", 800, 600);
    request.limits.timeout = Duration::from_millis(50);
    let started = std::time::Instant::now();

    assert_eq!(
        render_mermaid_with_worker(
            worker.path(),
            &request,
            &MermaidCancellationToken::default()
        ),
        Err(MermaidRenderError::TimedOut)
    );
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
#[cfg(unix)]
fn public_worker_adapter_forcefully_terminates_cancelled_worker() {
    let worker = scripted_worker("#!/bin/sh\nsleep 30\n");
    let request = MermaidRenderRequest::svg("flowchart LR\nA --> B", 800, 600);
    let cancellation = MermaidCancellationToken::default();
    let barrier = Arc::new(Barrier::new(2));
    let cancelling = cancellation.clone();
    let cancelling_barrier = Arc::clone(&barrier);
    let thread = std::thread::spawn(move || {
        cancelling_barrier.wait();
        std::thread::sleep(Duration::from_millis(50));
        cancelling.cancel();
    });
    barrier.wait();
    let started = std::time::Instant::now();

    assert_eq!(
        render_mermaid_with_worker(worker.path(), &request, &cancellation),
        Err(MermaidRenderError::Cancelled)
    );
    thread.join().unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
#[cfg(unix)]
fn public_worker_adapter_rejects_malformed_and_oversized_responses() {
    let malformed = scripted_worker("#!/bin/sh\nprintf 'garbage'\n");
    let request = MermaidRenderRequest::svg("flowchart LR\nA --> B", 800, 600);
    assert!(matches!(
        render_mermaid_with_worker(
            malformed.path(),
            &request,
            &MermaidCancellationToken::default()
        ),
        Err(MermaidRenderError::InvalidWorkerResponse { .. })
    ));

    let oversized = scripted_worker(
        "#!/bin/sh\nprintf 'BCMR\\000\\001\\001\\000\\000\\020\\000'; dd if=/dev/zero bs=4096 count=2 2>/dev/null\n",
    );
    let mut request = MermaidRenderRequest::svg("flowchart LR\nA --> B", 800, 600);
    request.limits.max_output_bytes = 4096;
    assert!(matches!(
        render_mermaid_with_worker(
            oversized.path(),
            &request,
            &MermaidCancellationToken::default()
        ),
        Err(MermaidRenderError::InvalidWorkerResponse { .. })
    ));
}

#[test]
#[cfg(unix)]
fn public_worker_adapter_maps_worker_crash_to_typed_protocol_failure() {
    let crashed = scripted_worker("#!/bin/sh\nexit 7\n");
    let request = MermaidRenderRequest::svg("flowchart LR\nA --> B", 800, 600);

    assert!(matches!(
        render_mermaid_with_worker(
            crashed.path(),
            &request,
            &MermaidCancellationToken::default()
        ),
        Err(MermaidRenderError::InvalidWorkerResponse { .. })
    ));
}

#[test]
fn public_worker_adapter_round_trips_and_maps_failures() {
    let worker = std::path::Path::new(env!("CARGO_BIN_EXE_bcode-mermaid-worker"));
    let request = MermaidRenderRequest::svg("flowchart LR\nA --> B", 800, 600);
    let rendered =
        render_mermaid_with_worker(worker, &request, &MermaidCancellationToken::default()).unwrap();
    let MermaidRenderedOutput::Svg(svg) = rendered.output;
    assert!(String::from_utf8(svg).unwrap().contains("<svg"));

    let directive = MermaidRenderRequest::svg("%%{init: {}}%%\nflowchart LR\nA --> B", 800, 600);
    assert!(matches!(
        render_mermaid_with_worker(worker, &directive, &MermaidCancellationToken::default()),
        Err(MermaidRenderError::DirectiveNotAllowed)
    ));

    let invalid = MermaidRenderRequest::svg("not a diagram", 800, 600);
    assert!(matches!(
        render_mermaid_with_worker(worker, &invalid, &MermaidCancellationToken::default()),
        Err(MermaidRenderError::InvalidDiagram { .. })
    ));
}

#[test]
fn public_worker_adapter_honors_pre_cancelled_request() {
    let cancellation = MermaidCancellationToken::default();
    cancellation.cancel();
    assert_eq!(
        render_mermaid_with_worker(
            std::path::Path::new(env!("CARGO_BIN_EXE_bcode-mermaid-worker")),
            &MermaidRenderRequest::svg("flowchart LR\nA --> B", 800, 600),
            &cancellation,
        ),
        Err(MermaidRenderError::Cancelled)
    );
}

#[test]
fn worker_round_trips_versioned_request_and_svg_response() {
    let (success, payload, exit_success) =
        response(worker(), &request(VERSION, "flowchart LR\nA --> B"));
    assert!(success);
    assert!(exit_success);
    let svg = String::from_utf8(payload).unwrap();
    assert!(svg.contains("<svg"));
    assert!(svg.contains('A'));
}

#[test]
fn worker_rejects_unknown_protocol_version_with_typed_envelope() {
    let (success, payload, exit_success) =
        response(worker(), &request(VERSION + 1, "flowchart LR\nA --> B"));
    assert!(!success);
    assert!(!exit_success);
    assert!(
        String::from_utf8(payload)
            .unwrap()
            .contains("unsupported worker protocol version")
    );
}
