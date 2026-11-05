//! Closed, secret-free host-tool diagnostic vocabulary.
use bcode_harness_models::ManagedToolOutcome;

#[derive(Clone, Copy)]
pub enum Connection {
    Connected,
    Failed,
    Pending,
    Disabled,
    NeedsAuth,
    Missing,
    Unknown,
    Unavailable,
}
impl Connection {
    pub(super) fn from_label(label: &str) -> Self {
        match label {
            "connected" => Self::Connected,
            "failed" => Self::Failed,
            "pending" => Self::Pending,
            "disabled" => Self::Disabled,
            "needs_auth" => Self::NeedsAuth,
            "missing" => Self::Missing,
            "unavailable" => Self::Unavailable,
            _ => Self::Unknown,
        }
    }
    const fn label(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Failed => "failed",
            Self::Pending => "pending",
            Self::Disabled => "disabled",
            Self::NeedsAuth => "needs_auth",
            Self::Missing => "missing",
            Self::Unknown => "unknown",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Clone, Copy)]
pub enum CatalogResponse {
    Tools(usize),
    Rejected,
    Denied,
}
impl CatalogResponse {
    pub(super) const fn classify(outcome: &ManagedToolOutcome) -> Option<Self> {
        match outcome {
            ManagedToolOutcome::Catalog { tools, .. } => Some(Self::Tools(tools.len())),
            ManagedToolOutcome::Rejected => Some(Self::Rejected),
            ManagedToolOutcome::Denied => Some(Self::Denied),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
pub enum Refresh {
    Acknowledged,
    Rejected,
    Unknown,
}
impl Refresh {
    pub(super) const fn classify(result: &Result<(), bcode_claude_code::Error>) -> Self {
        match result {
            Ok(()) => Self::Acknowledged,
            Err(bcode_claude_code::Error::Protocol) => Self::Rejected,
            Err(_) => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy)]
pub enum CallbackDisposition {
    NoScope,
    Queued,
    Rejected,
    Unhandled,
    Unavailable,
}
impl CallbackDisposition {
    pub(super) const fn classify(
        bound: bool,
        action: Option<&crate::tool_bridge::CallbackAction>,
    ) -> Self {
        match action {
            Some(crate::tool_bridge::CallbackAction::Queued) => Self::Queued,
            Some(crate::tool_bridge::CallbackAction::Reply(_)) if !bound => Self::NoScope,
            Some(crate::tool_bridge::CallbackAction::Reply(_)) => Self::Rejected,
            None => Self::Unhandled,
        }
    }
}

#[derive(Clone, Copy)]
pub enum Diagnostic {
    Callback(CallbackDisposition),
    QueueRevoked,
    Metadata {
        counts: [u64; 3],
    },
    Refresh(Refresh),
    MetadataUnavailable,
    NativeStatus(Connection),
    SystemInit(Connection),
    CatalogDelivery {
        response: CatalogResponse,
        written: bool,
    },
}
impl Diagnostic {
    pub(super) const fn code(&self) -> &'static str {
        match self {
            Self::Callback(_) => "host_tools_callback_disposition",
            Self::QueueRevoked => "host_tools_queue_revoked",
            Self::Metadata { .. } => "host_tools_startup_metadata",
            Self::Refresh(_) => "host_tools_refresh",
            Self::MetadataUnavailable => "host_tools_metadata_unavailable",
            Self::NativeStatus(_) => "host_tools_native_status",
            Self::SystemInit(_) => "host_tools_system_init",
            Self::CatalogDelivery { .. } => "host_tools_catalog_delivery",
        }
    }
    pub(super) fn message(&self) -> String {
        match self {
            Self::Callback(value) => match value {
                CallbackDisposition::NoScope => "no_scope",
                CallbackDisposition::Queued => "queued",
                CallbackDisposition::Rejected => "rejected",
                CallbackDisposition::Unhandled => "unhandled",
                CallbackDisposition::Unavailable => "queue_unavailable",
            }
            .into(),
            Self::QueueRevoked => "exact host queue scope revoked".into(),
            Self::Metadata {
                counts: [initialize, startup, current],
            } => format!(
                "initialize requests advertising tools.listChanged=true: {initialize}; startup empty catalog requests: {startup}; current catalog requests: {current}"
            ),
            Self::Refresh(status) => match status {
                Refresh::Acknowledged => "notification acknowledged; catalog completion unproved",
                Refresh::Rejected => "notification protocol rejected; catalog completion unproved",
                Refresh::Unknown => "notification outcome unknown; catalog completion unproved",
            }
            .into(),
            Self::MetadataUnavailable => {
                "host tool metadata unavailable until successful current catalog delivery".into()
            }
            Self::NativeStatus(status) | Self::SystemInit(status) => status.label().into(),
            Self::CatalogDelivery { response, written } => {
                let summary = match response {
                    CatalogResponse::Tools(count) => {
                        format!("host catalog response contains {count} tools")
                    }
                    CatalogResponse::Rejected => "host response rejected".into(),
                    CatalogResponse::Denied => "host response denied".into(),
                };
                format!("{summary}; native response write={written}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_outcomes_have_no_native_payload() {
        assert!(matches!(Refresh::classify(&Ok(())), Refresh::Acknowledged));
        assert!(matches!(
            Refresh::classify(&Err(bcode_claude_code::Error::Protocol)),
            Refresh::Rejected
        ));
        assert!(matches!(
            Refresh::classify(&Err(bcode_claude_code::Error::Uncertain)),
            Refresh::Unknown
        ));
        assert!(
            Diagnostic::Refresh(Refresh::Acknowledged)
                .message()
                .contains("completion unproved")
        );
    }

    #[test]
    fn arbitrary_native_status_cannot_escape_the_closed_vocabulary() {
        let diagnostic =
            Diagnostic::NativeStatus(Connection::from_label("SECRET arbitrary native error"));
        assert_eq!(diagnostic.code(), "host_tools_native_status");
        assert_eq!(diagnostic.message(), "unknown");
        let response = ManagedToolOutcome::Catalog {
            tools: vec![serde_json::json!({"secret":"not diagnostic content"})],
            next: None,
        };
        let diagnostic = Diagnostic::CatalogDelivery {
            response: CatalogResponse::classify(&response).unwrap(),
            written: true,
        };
        assert_eq!(
            diagnostic.message(),
            "host catalog response contains 1 tools; native response write=true"
        );
    }
}
