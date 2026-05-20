//! BMUX-native TUI backend skeleton.

use bcode_session_models::SessionId;

use super::TuiError;

/// Run the BMUX-native TUI backend.
pub fn run(session_id: Option<SessionId>) -> Result<(), TuiError> {
    let _ = session_id;
    Err(TuiError::BackendUnavailable("bmux runtime skeleton"))
}
