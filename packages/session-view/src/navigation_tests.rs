use super::*;
use bcode_session_models::{SessionLocationSummary, SessionTitleSource};

fn summary(parent_id: Option<SessionId>) -> SessionSummary {
    SessionSummary {
        id: SessionId::new(),
        parent_id,
        name: Some("子 👩‍💻 e\u{301}".to_owned()),
        explicit_name: None,
        derived_title: None,
        title_source: SessionTitleSource::Explicit,
        client_count: 0,
        created_at_ms: 1,
        updated_at_ms: 1,
        working_directory: std::path::PathBuf::new(),
        import: None,
        execution: None,
        location: None,
    }
}

#[test]
fn collapse_search_and_zero_budget_are_bounded() {
    let parent = summary(None);
    let child = summary(Some(parent.id));
    let grandchild = summary(Some(child.id));
    let summaries = vec![parent, child, grandchild];
    let collapsed = project_session_tree(&summaries, &BTreeSet::new(), None, 10);
    assert_eq!(collapsed.rows.len(), 1);
    let searched =
        project_session_tree(&summaries, &BTreeSet::new(), Some(&BTreeSet::from([2])), 10);
    assert_eq!(
        searched
            .rows
            .iter()
            .map(|row| row.depth)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert!(searched.rows[0].ancestor_context);
    assert!(!searched.rows[2].ancestor_context);
    let empty = project_session_tree(&summaries, &BTreeSet::new(), None, 0);
    assert!(empty.rows.is_empty());
    assert!(empty.truncated);
}

#[test]
fn deep_loaded_chain_is_iterative_not_architecturally_capped() {
    let mut summaries = vec![summary(None)];
    for _ in 0..2048 {
        summaries.push(summary(Some(summaries.last().unwrap().id)));
    }
    let expanded = summaries
        .iter()
        .map(SessionNavigationKey::from_summary)
        .collect();
    let page = project_session_tree(&summaries, &expanded, None, 2049);
    assert_eq!(page.rows.last().unwrap().depth, 2048);
    let bounded = project_session_tree(&summaries, &expanded, None, 3);
    assert_eq!(bounded.rows.len(), 3);
    assert!(bounded.truncated);
}

#[test]
fn missing_foreign_and_cyclic_parents_are_surfaced() {
    let mut parent = summary(None);
    parent.location = Some(SessionLocationSummary {
        location_id: "a".into(),
        ..Default::default()
    });
    let mut child = summary(Some(parent.id));
    child.location = Some(SessionLocationSummary {
        location_id: "b".into(),
        ..Default::default()
    });
    let page = project_session_tree(&[parent, child], &BTreeSet::new(), None, 10);
    assert_eq!(page.rows.len(), 2);
    assert!(page.rows[1].detached);
    let mut first = summary(None);
    let second = summary(Some(first.id));
    first.parent_id = Some(second.id);
    let expanded = [&first, &second]
        .into_iter()
        .map(SessionNavigationKey::from_summary)
        .collect();
    let page = project_session_tree(&[first, second], &expanded, None, 10);
    assert_eq!(page.rows.len(), 2);
    assert!(page.rows.iter().all(|row| row.detached));
}
