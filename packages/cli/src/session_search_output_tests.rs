//! Behavioral coverage for human search coverage reporting.
use super::*;
use bcode_session_search::*;

fn response() -> FederatedSessionSearchResponse {
    FederatedSessionSearchResponse {
        hits: Vec::new(),
        query_complete: true,
        coverage_complete: true,
        providers: vec![FederatedProviderReport {
            provider_id: "messages".into(),
            outcome: ProviderSearchOutcome::Complete,
            elapsed_ms: 7,
            query_complete: true,
            coverage_complete: true,
            next_cursor: None,
            searched_content: vec![SearchContentKind::UserMessage],
            excluded_content: vec![SearchContentKind::ToolOutput],
        }],
        failures: Vec::new(),
    }
}

fn render(filters: &SessionSearchFilters, response: &FederatedSessionSearchResponse) -> String {
    let mut output = Vec::new();
    write_session_search_coverage(&mut output, filters, response).unwrap();
    String::from_utf8(output).unwrap()
}

#[test]
fn no_results_names_provider_content_and_scope_without_catalog_claim() {
    let session_id = SessionId::new();
    let filters = SessionSearchFilters {
        session_ids: BTreeSet::from([session_id]),
        working_directory: Some(PathBuf::from("/project")),
        ..Default::default()
    };
    let text = render(&filters, &response());
    for expected in [
        session_id.to_string().as_str(),
        "/project",
        "provider messages: Complete",
        "searched=[UserMessage]",
        "excluded=[ToolOutput]",
        "Content selection: provider defaults",
        "No hits returned within the reported provider/content coverage",
        "does not establish absence in all catalog sessions",
    ] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }
}

#[test]
fn explicit_content_is_distinct_from_actual_provider_coverage() {
    let text = render(
        &SessionSearchFilters {
            content_kinds: BTreeSet::from([SearchContentKind::ToolOutput]),
            ..Default::default()
        },
        &response(),
    );
    assert!(text.contains("Content selection: explicit {ToolOutput}"));
    assert!(text.contains("searched=[UserMessage], excluded=[ToolOutput]"));
    assert!(!text.contains("Content selection: provider defaults"));
}

#[test]
fn partial_and_truncated_provider_results_are_not_conclusive() {
    let mut response = response();
    response.query_complete = false;
    response.coverage_complete = false;
    response.providers[0].outcome = ProviderSearchOutcome::Partial;
    response.providers[0].query_complete = false;
    response.providers[0].coverage_complete = false;
    response.providers[0].next_cursor = Some(SearchCursor {
        provider_id: "messages".into(),
        query_fingerprint: "query".into(),
        value: "next-page".into(),
    });
    let text = render(&SessionSearchFilters::default(), &response);
    assert!(text.contains("provider messages: Partial"));
    assert!(text.contains("next-page"));
    assert!(text.contains("query_complete=false, coverage_complete=false"));
    assert!(text.contains("Coverage is partial or failed; no-results is not conclusive"));
}

#[test]
fn failure_without_provider_report_does_not_claim_search_coverage() {
    let mut response = response();
    response.providers.clear();
    response.failures.push(SessionSearchProviderFailure {
        plugin_id: "unavailable".into(),
        error: SessionSearchServiceError {
            code: SearchErrorCode::ProviderUnavailable,
            message: "offline".into(),
            retryable: true,
        },
        stage: SessionSearchProviderStage::Execution,
        elapsed_ms: 2,
        content: vec![SearchContentKind::ToolOutput],
    });
    let text = render(&SessionSearchFilters::default(), &response);
    assert!(text.contains("provider unavailable Execution (ProviderUnavailable"));
    assert!(text.contains("content=[ToolOutput]): offline"));
    assert!(text.contains("content coverage is unverified"));
    assert!(text.contains("provider_failures=1"));
    assert!(text.contains("Coverage is partial or failed"));
}

#[test]
fn truncated_preview_is_labeled_and_json_keeps_the_existing_flag() {
    let mut response = response();
    response.hits.push(SessionSearchHit {
        locator: SessionSearchLocator {
            session_id: SessionId::new(),
            sequence: 42,
            record_id: None,
        },
        content_kind: SearchContentKind::UserMessage,
        matched_field: SearchField::Text,
        provider_id: "messages".into(),
        provider_rank: 1,
        provider_score: None,
        preview: Some("bounded preview".into()),
        preview_truncated: true,
    });
    let mut output = Vec::new();
    write_session_search_hit(&mut output, &response.hits[0]).unwrap();
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("#42 UserMessage [messages rank 1]: bounded preview [truncated]"));
    let json = session_search_json(SessionSearchExecutionClass::Ordinary, &response, &[]);
    assert_eq!(json["hits"][0]["preview_truncated"], true);
    assert_eq!(json["hits"][0]["preview"], "bounded preview");
}

#[test]
fn coverage_write_errors_propagate() {
    let error = write_session_search_coverage(
        &mut std::io::Cursor::new(&mut [0_u8; 0][..]),
        &SessionSearchFilters::default(),
        &response(),
    )
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::WriteZero);
}
