#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Portable filesystem contracts. This crate has no plugin entrypoint or runtime.
//! Requests never carry workspace grants; the provider authorizes each invocation.

pub mod path_search;

#[cfg(test)]
mod tests {
    use super::path_search::{
        SearchPathEntry, SearchPathsRequest, SearchPathsResponse, SearchPathsState,
    };

    #[test]
    fn caller_cannot_supply_workspace_authority() {
        for payload in [
            serde_json::json!({"root": "/tmp"}),
            serde_json::json!({"grant": {"root": "/tmp"}}),
            serde_json::json!({"invocation_id": "invented"}),
        ] {
            assert!(serde_json::from_value::<SearchPathsRequest>(payload).is_err());
        }
    }

    #[test]
    fn incomplete_and_cancelled_are_not_empty_success() {
        for state in [
            SearchPathsState::Incomplete,
            SearchPathsState::Cancelled,
            SearchPathsState::Unavailable,
        ] {
            let response = SearchPathsResponse::terminal(state, "bounded");
            let encoded = serde_json::to_vec(&response).expect("serialize response");
            let decoded: SearchPathsResponse =
                serde_json::from_slice(&encoded).expect("deserialize response");
            assert_eq!(decoded.state, state);
            assert_eq!(decoded.entries, [] as [SearchPathEntry; 0]);
            assert_eq!(decoded.reason.as_deref(), Some("bounded"));
        }
    }

    #[test]
    fn request_defaults_and_unicode_survive_wire_roundtrip() {
        let request = SearchPathsRequest {
            query: "源/👩‍💻 e\u{301}".into(),
            ..SearchPathsRequest::default()
        };
        let encoded = serde_json::to_vec(&request).expect("serialize request");
        let decoded: SearchPathsRequest =
            serde_json::from_slice(&encoded).expect("deserialize request");
        assert_eq!(decoded.query, request.query);
        assert_eq!(decoded.max_entries, request.max_entries);
        assert_eq!(decoded.scope, request.scope);
        assert!(!decoded.refresh);
    }
}
