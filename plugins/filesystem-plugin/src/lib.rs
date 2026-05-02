#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled filesystem service plugin for Bcode.

use bcode_plugin_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const FILESYSTEM_INTERFACE_ID: &str = "bcode.filesystem/v1";

/// Bundled filesystem plugin.
#[derive(Default)]
pub struct FilesystemPlugin;

impl RustPlugin for FilesystemPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != FILESYSTEM_INTERFACE_ID {
            return ServiceResponse::error(
                "unsupported_interface",
                "unsupported filesystem service interface",
            );
        }

        match context.request.operation.as_str() {
            "read" => read_file(&context.request),
            "write" => write_file(&context.request),
            "exists" => path_exists(&context.request),
            _ => ServiceResponse::error(
                "unsupported_operation",
                "unsupported filesystem service operation",
            ),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ReadRequest {
    path: PathBuf,
}

#[derive(Debug, Serialize)]
struct ReadResponse {
    contents: String,
}

#[derive(Debug, Deserialize)]
struct WriteRequest {
    path: PathBuf,
    contents: String,
}

#[derive(Debug, Serialize)]
struct WriteResponse {
    bytes_written: usize,
}

#[derive(Debug, Deserialize)]
struct ExistsRequest {
    path: PathBuf,
}

#[derive(Debug, Serialize)]
struct ExistsResponse {
    exists: bool,
}

fn read_file(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<ReadRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match std::fs::read_to_string(&request.path) {
        Ok(contents) => json_response(&ReadResponse { contents }),
        Err(error) => io_error(&error),
    }
}

fn write_file(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<WriteRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    if let Some(parent) = request.path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        return io_error(&error);
    }
    match std::fs::write(&request.path, request.contents.as_bytes()) {
        Ok(()) => json_response(&WriteResponse {
            bytes_written: request.contents.len(),
        }),
        Err(error) => io_error(&error),
    }
}

fn path_exists(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<ExistsRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    json_response(&ExistsResponse {
        exists: request.path.exists(),
    })
}

fn json_response<T: Serialize>(value: &T) -> ServiceResponse {
    match ServiceResponse::json(value) {
        Ok(response) => response,
        Err(error) => ServiceResponse::error("encode_failed", error.to_string()),
    }
}

fn invalid_request(error: &serde_json::Error) -> ServiceResponse {
    ServiceResponse::error("invalid_request", error.to_string())
}

fn io_error(error: &std::io::Error) -> ServiceResponse {
    ServiceResponse::error("io_error", error.to_string())
}

bcode_plugin_sdk::export_plugin!(FilesystemPlugin, include_str!("../bcode-plugin.toml"));
