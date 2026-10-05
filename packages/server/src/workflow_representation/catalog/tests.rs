use super::*;

#[tokio::test]
async fn ordinary_catalog_returns_explicit_basic_page_when_extension_installed() {
    let root = tempfile::tempdir().unwrap();
    let store =
        bcode_workflow_store::WorkflowStore::create_coexistence_baseline(root.path()).unwrap();
    let db = open(store.path(), true).unwrap();
    super::super::representation::install(&db).await.unwrap();
    let mut state = crate::tests::test_server_state(crate::SessionManager::default());
    state.workflow_store = std::sync::Arc::new(std::sync::Mutex::new(store));
    let request = bcode_workflow_view_models::WorkflowCatalogRequest {
        limit: 31,
        cursor: None,
        filter: bcode_workflow_view_models::WorkflowCatalogFilter::default(),
        sort: bcode_workflow_view_models::WorkflowCatalogSort::default(),
        group: bcode_workflow_view_models::WorkflowCatalogGroup::default(),
        search: None,
    };
    let result = basic_view(&state, &request).await;
    drop(state);
    let page = result.unwrap().unwrap();
    assert!(page.runs.is_empty());
    assert!(!page.has_more);
    let availability = page.basic_discovery.unwrap();
    assert!(!availability.requested_query_applied);
    assert!(!availability.progress_available);
    assert_eq!(availability.version2, Health::Available);
}

#[test]
fn mixed_205_pages_preserve_every_identity_and_conflict() {
    let mut observed = Vec::new();
    for start in (0..205).step_by(31) {
        let end = (start + 31).min(205);
        let input = store::CatalogPage {
            entries: (start..end)
                .map(|index| store::CatalogIdentity {
                    run_id: format!("run-{index:03}"),
                    representation: if index == 100 {
                        store::CatalogRepresentation::Conflict
                    } else if index % 2 == 0 {
                        store::CatalogRepresentation::Legacy
                    } else {
                        store::CatalogRepresentation::Version2
                    },
                })
                .collect(),
            next_after: (end < 205).then(|| format!("run-{:03}", end - 1)),
            legacy: store::FamilyState::Available,
            version2: store::FamilyState::Available,
        };
        let page = project(input);
        assert_eq!(page.entries.len(), end - start);
        assert_eq!(page.next_after.is_some(), end < 205);
        observed.extend(page.entries);
    }
    assert_eq!(observed.len(), 205);
    assert_eq!(observed[100].membership, Membership::Conflict);
    assert!(
        observed
            .windows(2)
            .all(|pair| pair[0].run_id < pair[1].run_id)
    );
}

#[test]
fn unsupported_and_degraded_family_remain_explicit_on_exhausted_page() {
    for state in [
        store::FamilyState::Unsupported,
        store::FamilyState::Degraded,
    ] {
        let page = project(store::CatalogPage {
            entries: vec![store::CatalogIdentity {
                run_id: "legacy".into(),
                representation: store::CatalogRepresentation::Legacy,
            }],
            next_after: None,
            legacy: store::FamilyState::Available,
            version2: state,
        });
        assert_eq!(page.version2, health(state));
        assert_eq!(page.entries[0].membership, Membership::Legacy);
        let json = serde_json::to_value(&page).unwrap();
        assert_eq!(
            serde_json::from_value::<WorkflowIdentityCatalogPage>(json).unwrap(),
            page
        );
    }
}
