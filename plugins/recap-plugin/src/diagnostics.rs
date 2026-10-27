//! Secret-safe actionable error categories. Raw provider/process messages never cross the plugin boundary.
use bcode_recap::{
    composition::CompositionError, engine::EngineError, media::MediaError, process::ProcessError,
    source::SourceError,
};

pub fn engine(error: EngineError) -> String {
    match error {
        EngineError::Source(SourceError::Process(error)) | EngineError::Media(MediaError::Process(error)) => process(&error),
        EngineError::Source(SourceError::Invalid(reason)) => format!("recap_source_invalid: {reason}; check video URL and transcript format"),
        EngineError::Source(_) => "recap_source_unavailable: check local video/transcript access or downloader configuration".into(),
        EngineError::Media(MediaError::Invalid(reason)) => format!("recap_media_invalid: {reason}; check video duration, tracks and extraction limits"),
        EngineError::Media(_) => "recap_media_unavailable: check media workspace access and metadata".into(),
        EngineError::Composition(CompositionError::Model(error)) => format!("recap_model_failed: {error}; check admitted profile and structured/image capabilities"),
        EngineError::Composition(CompositionError::Invalid(reason)) => format!("recap_grounding_invalid: {reason}; model output must reference actual evidence/assets"),
        EngineError::Composition(CompositionError::Budget) => "recap_context_budget: source evidence exceeds configured composition limits".into(),
        EngineError::Composition(_) => "recap_output_invalid: model editorial content failed document validation".into(),
        EngineError::Contract(_) => "recap_storage_contract: check existing document revision, locks and asset confinement".into(),
        EngineError::Export(_) => "recap_export_invalid: document/media cannot be exported within offline limits".into(),
        EngineError::Io(_) => "recap_storage_io: check owned store access; inspect before retrying writes".into(),
        EngineError::Cancelled => "recap_cancelled: operation cancelled; inspect retained artifacts before retrying".into(),
        EngineError::Invalid(reason) => format!("recap_operation_invalid: {reason}"),
    }
}
fn process(error: &ProcessError) -> String {
    match error {
        ProcessError::AdmissionDenied => "recap_process_denied: explicitly enable the required process executable and allow its read/write/network resources".into(),
        ProcessError::Io(error) if error.kind() == std::io::ErrorKind::NotFound => "recap_executable_missing: install/configure ffprobe and ffmpeg (yt-dlp for URLs) in the daemon execution environment".into(),
        ProcessError::Io(_) => "recap_process_io: executable could not start or its streams could not be read".into(),
        ProcessError::Cancelled => "recap_process_cancelled: media work cancelled".into(),
        ProcessError::Timeout => "recap_process_timeout: media operation exceeded its deadline".into(),
        ProcessError::OutputLimit => "recap_process_budget: media output exceeded byte/file limits".into(),
        ProcessError::Failed(code) => format!("recap_process_failed: media executable exited with code {code:?}; validate source format and executable availability"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn diagnostics_omit_untrusted_io_and_json_details() {
        let error = bcode_recap::engine::EngineError::Io(std::io::Error::other(
            "PRIVATE_TOKEN /secret/path",
        ));
        let diagnostic = super::engine(error);
        assert!(diagnostic.starts_with("recap_storage_io:"));
        assert!(!diagnostic.contains("PRIVATE_TOKEN"));
        assert!(!diagnostic.contains("/secret"));
        let missing = bcode_recap::engine::EngineError::Media(
            bcode_recap::media::MediaError::Process(bcode_recap::process::ProcessError::Io(
                std::io::Error::from(std::io::ErrorKind::NotFound),
            )),
        );
        assert!(super::engine(missing).starts_with("recap_executable_missing:"));
    }
}
