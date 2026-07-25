//! Private Mermaid rendering worker.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use std::io::{Read, Write};

const REQUEST_MAGIC: &[u8; 4] = b"BCMW";
const RESPONSE_MAGIC: &[u8; 4] = b"BCMR";
const PROTOCOL_VERSION: u16 = 1;
const MAX_SOURCE_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

fn main() {
    if let Err(message) = run() {
        let _ = write_response(false, message.as_bytes());
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut header = [0_u8; 22];
    std::io::stdin()
        .read_exact(&mut header)
        .map_err(|error| format!("invalid worker request: {error}"))?;
    if &header[..4] != REQUEST_MAGIC {
        return Err("invalid worker request magic".to_owned());
    }
    let version = u16::from_be_bytes([header[4], header[5]]);
    if version != PROTOCOL_VERSION {
        return Err(format!("unsupported worker protocol version {version}"));
    }
    let width = u32::from_be_bytes(header[6..10].try_into().expect("fixed width field"));
    let height = u32::from_be_bytes(header[10..14].try_into().expect("fixed height field"));
    let max_output =
        u32::from_be_bytes(header[14..18].try_into().expect("fixed output field")) as usize;
    let source_len =
        u32::from_be_bytes(header[18..22].try_into().expect("fixed source field")) as usize;
    if source_len > MAX_SOURCE_BYTES {
        return Err("Mermaid worker source exceeds fixed limit".to_owned());
    }
    if max_output == 0 || max_output > MAX_RESPONSE_BYTES {
        return Err("Mermaid worker output limit is invalid".to_owned());
    }
    let mut source = vec![0_u8; source_len];
    std::io::stdin()
        .read_exact(&mut source)
        .map_err(|error| format!("incomplete worker source: {error}"))?;
    let source = String::from_utf8(source).map_err(|_| "worker source is not UTF-8".to_owned())?;
    let mut request = bcode_mermaid_render::MermaidRenderRequest::svg(source, width, height);
    request.limits.max_output_bytes = max_output;
    let rendered = bcode_mermaid_render::render_mermaid(
        &request,
        &bcode_mermaid_render::MermaidCancellationToken::default(),
    )
    .map_err(|error| error.to_string())?;
    let bcode_mermaid_render::MermaidRenderedOutput::Svg(svg) = rendered.output;
    if svg.len() > max_output {
        return Err("Mermaid worker output exceeds caller limit".to_owned());
    }
    write_response(true, &svg)
}

fn write_response(success: bool, payload: &[u8]) -> Result<(), String> {
    if payload.len() > MAX_RESPONSE_BYTES {
        return Err("Mermaid worker response exceeds fixed limit".to_owned());
    }
    let payload_len = u32::try_from(payload.len())
        .map_err(|_| "Mermaid worker response length overflow".to_owned())?;
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(RESPONSE_MAGIC)
        .and_then(|()| stdout.write_all(&PROTOCOL_VERSION.to_be_bytes()))
        .and_then(|()| stdout.write_all(&[u8::from(success)]))
        .and_then(|()| stdout.write_all(&payload_len.to_be_bytes()))
        .and_then(|()| stdout.write_all(payload))
        .map_err(|error| format!("failed to write worker response: {error}"))
}
