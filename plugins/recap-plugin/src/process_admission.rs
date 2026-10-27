//! Per-spawn admission for literal media argv, separate from recap tool permission.
use bcode_plugin_sdk::{
    ServiceBridge, ServiceBridgeRequest, ServiceBridgeResponse, ToolPolicyOperation,
};
use bcode_recap::process::{ProcessAdmission, ProcessError, ProcessRequest};
use bcode_tool::{ToolInvocationServiceRequest, ToolInvocationServiceResolution};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Debug)]
pub struct HostProcessAdmission {
    pub bridge: ServiceBridge,
    pub invocation: String,
    pub root: PathBuf,
    pub sequence: AtomicU64,
}
impl ProcessAdmission for HostProcessAdmission {
    fn authorize(&self, request: &ProcessRequest) -> Result<(), ProcessError> {
        let text = |path: &Path| {
            path.to_str()
                .map(str::to_owned)
                .ok_or(ProcessError::AdmissionDenied)
        };
        let executable = text(&request.program)?;
        if !matches!(executable.as_str(), "ffprobe" | "ffmpeg" | "yt-dlp") {
            return Err(ProcessError::AdmissionDenied);
        }
        let arguments: Vec<String> = request
            .args
            .iter()
            .map(|arg| {
                arg.to_str()
                    .map(str::to_owned)
                    .ok_or(ProcessError::AdmissionDenied)
            })
            .collect::<Result<_, _>>()?;
        let mut reads = Vec::new();
        let mut urls = Vec::new();
        for argument in &arguments {
            if argument.starts_with("https://") {
                urls.push(
                    bcode_recap::source::youtube_url(argument)
                        .map_err(|_| ProcessError::AdmissionDenied)?,
                );
            } else if Path::new(argument).is_absolute() {
                let path = Path::new(argument);
                if !path.starts_with(&self.root)
                    || path
                        .components()
                        .any(|component| matches!(component, std::path::Component::ParentDir))
                {
                    return Err(ProcessError::AdmissionDenied);
                }
                if path.exists() {
                    reads.push(argument.clone());
                }
            }
        }
        let mut writes = Vec::new();
        if let Some((directory, _)) = &request.file_budget {
            let directory = directory
                .canonicalize()
                .map_err(|_| ProcessError::AdmissionDenied)?;
            if !directory.starts_with(self.root.join(".recap")) {
                return Err(ProcessError::AdmissionDenied);
            }
            writes.push(text(&directory)?);
        }
        let operation = ToolPolicyOperation::Process {
            executable,
            arguments,
            working_directory: request.cwd.as_deref().map(text).transpose()?,
            read_paths: reads,
            write_paths: writes,
            network_urls: urls,
        };
        let response = self
            .bridge
            .request(&ServiceBridgeRequest::InvokeService(
                ToolInvocationServiceRequest {
                    invocation_id: self.invocation.clone(),
                    request_id: format!(
                        "process-{}",
                        self.sequence.fetch_add(1, Ordering::Relaxed)
                    ),
                    route_id: None,
                    interface_id: bcode_tool::PROCESS_ADMISSION_INTERFACE_ID.into(),
                    operation: bcode_tool::OP_AUTHORIZE_PROCESS.into(),
                    payload: serde_json::to_value(operation)
                        .map_err(|_| ProcessError::AdmissionDenied)?,
                },
            ))
            .map_err(|_| ProcessError::AdmissionDenied)?;
        if matches!(
            response,
            ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Responded { .. })
        ) {
            Ok(())
        } else {
            Err(ProcessError::AdmissionDenied)
        }
    }
}
