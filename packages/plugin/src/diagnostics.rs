//! Diagnostic capture origin carried across executor dispatch.
//! Plugin data never chooses authorization, session, or originating correlation.

use bcode_xray::CaptureContext;

/// Origin captured before executor dispatch, without secret-bearing Debug output.
#[derive(Clone, Default)]
pub struct DiagnosticOrigin(pub Option<CaptureContext>);
impl std::fmt::Debug for DiagnosticOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiagnosticOrigin")
            .field("available", &self.0.is_some())
            .finish()
    }
}
impl DiagnosticOrigin {
    pub fn current() -> Self {
        Self(CaptureContext::current())
    }
}
