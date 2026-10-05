use super::*;

fn page() -> WorkflowSchedulingView {
    WorkflowSchedulingView {
        graph_revision: 2,
        observed_at_ms: 10,
        policy: None,
        activations: vec![WorkflowSchedulingActivation {
            node_id: "worker".into(),
            activation_id: "current".into(),
            state: WorkflowSchedulingState::Blocked,
            reasons: vec![
                WorkflowSchedulingBlocker::Policy {
                    reason: WorkflowPolicyBlocker::Retry,
                },
                WorkflowSchedulingBlocker::Resource {
                    resource: "workspace\n\u{1b}terminal".into(),
                },
            ],
        }],
        has_more: true,
        details_truncated: false,
        next_cursor: None,
    }
}

#[test]
fn exact_cursor_round_trip_is_not_display_truncated() {
    let mut page = page();
    let token = "opaque-revision-bound-token".repeat(100);
    page.next_cursor = Some(token.clone());
    let decoded: WorkflowSchedulingView =
        serde_json::from_value(serde_json::to_value(&page).unwrap()).unwrap();
    assert_eq!(decoded.next_cursor.as_deref(), Some(token.as_str()));
    let text = decoded.summary_lines().join("\n");
    assert!(text.contains("using the exact returned cursor"));
    assert!(!text.contains("no scheduling continuation cursor"));
    assert!(!text.contains(&token));
}

#[test]
fn partial_page_discloses_unavailable_continuation_without_inventing_totals() {
    let text = page().summary_lines().join("\n");
    assert!(text.contains("1 activation observations (not whole-run totals)"));
    assert!(text.contains("no scheduling continuation cursor"));
    assert!(text.contains("Refresh is not pagination"));
    assert!(text.contains("Policy: Retry"));
    assert!(text.contains("Resource conflict: workspace  terminal"));
    assert!(!text.contains('\u{1b}'));
}

#[test]
fn selection_is_exact_and_never_borrows_a_sibling_or_historical_attempt() {
    let page = page();
    let selected = page
        .selected_activation_lines("worker", Some("current"))
        .join("\n");
    assert!(selected.contains("Policy: Retry"));
    assert!(selected.contains("not selected-attempt history"));
    for (node, activation) in [("worker", "old"), ("sibling", "current")] {
        let absent = page
            .selected_activation_lines(node, Some(activation))
            .join("\n");
        assert!(absent.contains("state is unknown"));
        assert!(!absent.contains("Policy: Retry"));
    }
    let mut shortened = page;
    shortened.details_truncated = true;
    assert!(
        shortened.selected_activation_lines("worker", Some("current"))[0].contains("unavailable")
    );
}

#[test]
fn direct_deserialized_oversized_pages_still_disclose_adapter_omissions() {
    let mut page = page();
    page.has_more = false;
    page.activations = vec![page.activations[0].clone(); 101];
    page.activations[0].reasons = vec![WorkflowSchedulingBlocker::Dependency { remaining: 1 }; 9];
    let lines = page.summary_lines();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Additional activations omitted"))
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Additional blocker reasons omitted"))
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("worker / current"))
            .count(),
        100
    );
}
