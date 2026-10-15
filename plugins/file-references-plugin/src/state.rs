//! Plugin-owned text-only path candidate construction.

use bcode_plugin_sdk::composer::{
    ComposerCandidate, ComposerContinuation, ComposerEdit, ComposerSearchRequest,
    ComposerValidationError,
};

/// Build a candidate from a service-provided path, without accessing that path.
///
/// # Errors
/// Rejects invalid requests or paths, or edits exceeding composer bounds.
pub fn path_candidate(
    request: &ComposerSearchRequest,
    id: String,
    path: &str,
    directory: bool,
) -> Result<ComposerCandidate, ComposerValidationError> {
    request.validate()?;
    let replacement = crate::grammar::encode_path(path, directory)
        .map_err(|_| ComposerValidationError::InvalidPayload)?;
    let mut cursor = request.activation.range.start + replacement.len();
    if directory && replacement.ends_with('"') {
        cursor -= 1;
    }
    let edit = ComposerEdit {
        document_id: request.document.document_id.clone(),
        revision: request.document.revision,
        range: request.activation.range,
        expected_text: request.document.text
            [request.activation.range.start..request.activation.range.end]
            .to_owned(),
        replacement,
        cursor,
    };
    edit.apply(&request.document)?;
    Ok(ComposerCandidate {
        id,
        label: path.to_owned(),
        matched_ranges: Vec::new(),
        next_interaction_state: None,
        detail: directory.then(|| "Directory".to_owned()),
        edit,
        continuation: if directory {
            ComposerContinuation::Reactivate
        } else {
            ComposerContinuation::Finish
        },
    })
}
