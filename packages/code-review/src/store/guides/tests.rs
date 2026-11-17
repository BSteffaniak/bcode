//! Guide storage against a temporary repository and state directory.

use super::super::db::run_migrations;
use super::super::{CodeReviewStateLocation, MIGRATIONS_TABLE, ReviewStore};
use super::*;
use bcode_code_review_models::{
    GuideOrigin, GuideScope, MAX_GUIDE_SECTIONS, ReviewScope, ReviewTarget, Section, SectionKind,
    SectionState,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

struct Fixture {
    _temp: tempfile::TempDir,
    repo: PathBuf,
    store: ReviewStore,
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("repo");
    let status = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&repo)
        .status()
        .expect("git");
    assert!(status.success());
    let store = ReviewStore::new(CodeReviewConfig {
        state_location: CodeReviewStateLocation::User,
        state_dir: Some(temp.path().join("state")),
    });
    Fixture {
        _temp: temp,
        repo,
        store,
    }
}

fn section(id: &str) -> Section {
    Section {
        id: id.to_owned(),
        kind: SectionKind::Step,
        title: format!("Step {id}"),
        body: String::new(),
        findings: Vec::new(),
        citations: Vec::new(),
        children: Vec::new(),
        state: SectionState::Written,
        refreshed_from: None,
        checks: Vec::new(),
    }
}

fn guide(id: &str) -> ReviewGuide {
    ReviewGuide {
        guide_id: id.to_owned(),
        review_key: String::new(),
        scope: GuideScope::Review,
        parent_guide_id: None,
        title: "Retry budget".to_owned(),
        current_version: 0,
        created_at_ms: 0,
        updated_at_ms: 0,
        archived_at_ms: None,
    }
}

fn version(guide_id: &str) -> GuideVersion {
    GuideVersion {
        guide_id: guide_id.to_owned(),
        version: 0,
        schema_version: REVIEW_GUIDE_SCHEMA_VERSION,
        heads: BTreeMap::new(),
        based_on: None,
        origin: GuideOrigin::default(),
        status: GuideStatus::Generating,
        generation: None,
        outline: vec![section("s1")],
        citations: BTreeMap::new(),
        created_at_ms: 0,
        updated_at_ms: 0,
    }
}

fn workspace_scope(id: &str) -> ReviewScope {
    ReviewScope::Workspace {
        workspace_id: id.to_owned(),
        target: ReviewTarget::Repository,
    }
}

impl Fixture {
    fn create(&self, id: &str, scope: Option<ReviewScope>) -> CreateReviewGuideResponse {
        self.store
            .create_guide(CreateReviewGuideRequest {
                repo_path: self.repo.clone(),
                target: ReviewTarget::Repository,
                scope,
                guide: guide(id),
                version: version(id),
            })
            .expect("create")
    }

    fn version(&self, id: &str, number: Option<u32>) -> Option<GuideVersionRecord> {
        self.store
            .guide_version(&GetGuideVersionRequest {
                repo_path: self.repo.clone(),
                guide_id: id.to_owned(),
                version: number,
            })
            .expect("version")
            .version
    }

    fn list(&self, scope: Option<ReviewScope>) -> Vec<String> {
        self.store
            .list_guides(&ListReviewGuidesRequest {
                repo_path: self.repo.clone(),
                target: ReviewTarget::Repository,
                scope,
                include_archived: false,
            })
            .expect("list")
            .guides
            .into_iter()
            .map(|record| record.guide.guide_id)
            .collect()
    }
}

#[test]
fn a_guide_is_created_with_its_first_version_and_listed_under_its_review() {
    let fixture = fixture();
    let created = fixture.create("g1", Some(workspace_scope("rv-1")));
    assert_eq!(created.guide.guide.current_version, 1);
    assert_eq!(created.guide.guide.review_key, "review-workspace-rv-1");
    assert_eq!(created.version.version.version, 1);
    assert_eq!((created.guide.revision, created.version.revision), (0, 0));
    assert_eq!(fixture.list(Some(workspace_scope("rv-1"))), ["g1"]);
    assert_eq!(
        fixture.list(Some(workspace_scope("rv-2"))),
        [] as [String; 0]
    );
    let current = fixture.version("g1", None).expect("current version");
    assert_eq!(current.version.outline, vec![section("s1")]);
    // Creating it again is a conflict, not a replacement.
    let again = fixture.store.create_guide(CreateReviewGuideRequest {
        repo_path: fixture.repo.clone(),
        target: ReviewTarget::Repository,
        scope: None,
        guide: guide("g1"),
        version: version("g1"),
    });
    assert!(matches!(again, Err(StoreError::RevisionConflict { .. })));
}

#[test]
fn version_writes_replace_only_the_revision_they_read() {
    let fixture = fixture();
    fixture.create("g1", None);
    let mut changed = version("g1");
    changed.version = 1;
    changed.outline.push(section("s2"));
    let save = |version: &GuideVersion, expected| {
        fixture.store.save_guide_version(SaveGuideVersionRequest {
            repo_path: fixture.repo.clone(),
            version: version.clone(),
            expected_revision: expected,
        })
    };
    assert_eq!(save(&changed, 0).expect("first write").revision, 1);
    // A writer that read revision 0 lost the race.
    let mut late = version("g1");
    late.version = 1;
    let stale = save(&late, 0);
    assert!(
        matches!(
            stale,
            Err(StoreError::RevisionConflict {
                current: Some(1),
                ..
            })
        ),
        "{stale:?}"
    );
    let stored = fixture.version("g1", Some(1)).expect("version");
    assert_eq!((stored.revision, stored.version.outline.len()), (1, 2));

    // A change applied against a named revision fails when the version moved on.
    let named = fixture.store.update_guide_version(
        &fixture.repo,
        "g1".to_owned(),
        None,
        Some(0),
        |version| {
            version.outline.push(section("s3"));
            Ok::<(), String>(())
        },
    );
    assert!(matches!(
        named,
        Err(StoreError::RevisionConflict {
            current: Some(1),
            ..
        })
    ));
    // A refused change writes nothing.
    let refused = fixture
        .store
        .update_guide_version(&fixture.repo, "g1".to_owned(), None, None, |_| {
            Err::<(), _>("outside_grant")
        })
        .expect("store call");
    assert_eq!(refused, Err("outside_grant"));
    assert_eq!(fixture.version("g1", None).expect("version").revision, 1);
}

#[test]
fn concurrent_writers_without_a_revision_lose_nothing() {
    let fixture = fixture();
    fixture.create("g1", None);
    std::thread::scope(|scope| {
        for writer in 0..4 {
            let (store, repo) = (fixture.store.clone(), fixture.repo.clone());
            scope.spawn(move || {
                for index in 0..5 {
                    store
                        .update_guide_version(&repo, "g1".to_owned(), None, None, move |version| {
                            version.outline.push(section(&format!("w{writer}-{index}")));
                            Ok::<(), String>(())
                        })
                        .expect("store call")
                        .expect("change");
                }
            });
        }
    });
    let stored = fixture.version("g1", None).expect("version");
    assert_eq!(stored.version.outline.len(), 21);
    assert_eq!(stored.revision, 20);
}

#[test]
fn forks_take_the_next_number_and_become_current() {
    let fixture = fixture();
    fixture.create("g1", None);
    let fork = |based_on, make_current| {
        let mut draft = version("g1");
        draft.based_on = Some(based_on);
        fixture
            .store
            .fork_guide_version(ForkGuideVersionRequest {
                repo_path: fixture.repo.clone(),
                version: draft,
                make_current,
            })
            .expect("fork")
    };
    let second = fork(1, true);
    assert_eq!(second.version.version.version, 2);
    assert_eq!(second.guide.guide.current_version, 2);
    let third = fork(2, false);
    assert_eq!(third.version.version.version, 3);
    assert_eq!(third.guide.guide.current_version, 2);
    let versions = fixture
        .store
        .list_guide_versions(&ListGuideVersionsRequest {
            repo_path: fixture.repo.clone(),
            guide_id: "g1".to_owned(),
        })
        .expect("versions")
        .versions;
    let listed: Vec<(u32, Option<u32>)> = versions
        .iter()
        .map(|summary| (summary.version, summary.based_on))
        .collect();
    assert_eq!(listed, [(1, None), (2, Some(1)), (3, Some(2))]);
    // The old version stays readable.
    assert!(fixture.version("g1", Some(1)).is_some());
}

#[test]
fn areas_are_created_once_and_replaced_by_revision() {
    let fixture = fixture();
    fixture.create("g1", None);
    let mut area = GuideArea {
        guide_id: "g1".to_owned(),
        version: 1,
        area_id: "crates/net".to_owned(),
        session_id: Some("child".to_owned()),
        status: GuideAreaStatus::Running,
        message: None,
        outline: Vec::new(),
        citations: BTreeMap::new(),
        created_at_ms: 0,
        updated_at_ms: 0,
    };
    let save = |area: &GuideArea, expected| {
        fixture.store.save_guide_area(SaveGuideAreaRequest {
            repo_path: fixture.repo.clone(),
            area: area.clone(),
            expected_revision: expected,
        })
    };
    assert_eq!(save(&area, None).expect("create").revision, 0);
    assert!(matches!(
        save(&area, None),
        Err(StoreError::RevisionConflict {
            current: Some(0),
            ..
        })
    ));
    area.status = GuideAreaStatus::Complete;
    area.outline.push(section("a1"));
    assert_eq!(save(&area, Some(0)).expect("replace").revision, 1);
    assert!(matches!(
        save(&area, Some(0)),
        Err(StoreError::RevisionConflict { .. })
    ));
    let areas = fixture
        .store
        .list_guide_areas(&ListGuideAreasRequest {
            repo_path: fixture.repo.clone(),
            guide_id: "g1".to_owned(),
            version: 1,
        })
        .expect("areas")
        .areas;
    assert_eq!(areas.len(), 1);
    assert_eq!(areas[0].area.status, GuideAreaStatus::Complete);
    assert_eq!(areas[0].area.outline, vec![section("a1")]);
}

#[test]
fn threads_and_marks_round_trip_and_go_with_their_guide() {
    let fixture = fixture();
    fixture.create("g1", None);
    let thread = GuideThread {
        thread_id: "t1".to_owned(),
        guide_id: "g1".to_owned(),
        version: 1,
        section_id: "s1".to_owned(),
        session_id: None,
        status: GuideThreadStatus::Open,
        question: "Why a token bucket?".to_owned(),
        created_at_ms: 0,
        updated_at_ms: 0,
    };
    let saved = fixture
        .store
        .save_guide_thread(SaveGuideThreadRequest {
            repo_path: fixture.repo.clone(),
            thread,
        })
        .expect("thread")
        .thread;
    assert!(saved.created_at_ms > 0);
    let threads = |version| {
        fixture
            .store
            .list_guide_threads(&ListGuideThreadsRequest {
                repo_path: fixture.repo.clone(),
                guide_id: "g1".to_owned(),
                version,
            })
            .expect("threads")
            .threads
    };
    assert_eq!(threads(Some(1)), [saved]);
    assert_eq!(threads(Some(2)), []);
    let mark = |fingerprint: &str| GuideSectionMark {
        guide_id: "g1".to_owned(),
        section_id: "s1".to_owned(),
        reviewed_at_ms: 5,
        fingerprint: fingerprint.to_owned(),
    };
    for fingerprint in ["f1", "f2"] {
        fixture
            .store
            .set_guide_section_mark(SetGuideSectionMarkRequest {
                repo_path: fixture.repo.clone(),
                mark: mark(fingerprint),
            })
            .expect("mark");
    }
    let marks = || {
        fixture
            .store
            .list_guide_section_marks(&ListGuideSectionMarksRequest {
                repo_path: fixture.repo.clone(),
                guide_id: "g1".to_owned(),
            })
            .expect("marks")
            .marks
    };
    assert_eq!(marks(), [mark("f2")]);
    let clear = || {
        fixture
            .store
            .clear_guide_section_mark(&ClearGuideSectionMarkRequest {
                repo_path: fixture.repo.clone(),
                guide_id: "g1".to_owned(),
                section_id: "s1".to_owned(),
            })
            .expect("clear")
            .cleared
    };
    assert!(clear());
    assert!(!clear());
    fixture
        .store
        .set_guide_section_mark(SetGuideSectionMarkRequest {
            repo_path: fixture.repo.clone(),
            mark: mark("f3"),
        })
        .expect("mark");
    let delete = || {
        fixture
            .store
            .delete_guide(&DeleteReviewGuideRequest {
                repo_path: fixture.repo.clone(),
                guide_id: "g1".to_owned(),
            })
            .expect("delete")
            .deleted
    };
    assert!(delete());
    assert!(!delete());
    assert_eq!(threads(None), []);
    assert_eq!(marks(), []);
    assert!(fixture.version("g1", Some(1)).is_none());
}

#[test]
fn guides_of_an_unsaved_review_move_when_it_is_saved() {
    let fixture = fixture();
    let transient = ReviewScope::Target {
        target: ReviewTarget::Repository,
    };
    fixture.create("g1", Some(transient.clone()));
    let moved = fixture
        .store
        .rekey_guides(&RekeyReviewGuidesRequest {
            repo_path: fixture.repo.clone(),
            from: transient.clone(),
            to: workspace_scope("rv-1"),
        })
        .expect("rekey")
        .moved;
    assert_eq!(moved, 1);
    assert_eq!(fixture.list(Some(transient)), [] as [String; 0]);
    assert_eq!(fixture.list(Some(workspace_scope("rv-1"))), ["g1"]);
}

#[test]
fn a_version_of_an_unknown_schema_is_rejected_on_write_and_read() {
    let fixture = fixture();
    fixture.create("g1", None);
    let mut future = version("g1");
    future.version = 1;
    future.schema_version = REVIEW_GUIDE_SCHEMA_VERSION + 1;
    let write = fixture.store.save_guide_version(SaveGuideVersionRequest {
        repo_path: fixture.repo.clone(),
        version: future,
        expected_revision: 0,
    });
    assert!(matches!(
        write,
        Err(StoreError::Guide(
            GuideDocumentError::UnsupportedSchema { .. }
        ))
    ));
    // A row written by a later build is reported, not read as a known form.
    let root = resolve_repo_root(&fixture.repo).expect("root");
    with_database(&root, fixture.store.config(), |database| {
        Box::pin(async move {
            database
                .update(VERSIONS)
                .value("schema_version", i64::from(REVIEW_GUIDE_SCHEMA_VERSION) + 1)
                .filter(Box::new(where_eq("guide_id", "g1")))
                .execute(database)
                .await?;
            Ok(())
        })
    })
    .expect("downgrade");
    let read = fixture.store.guide_version(&GetGuideVersionRequest {
        repo_path: fixture.repo.clone(),
        guide_id: "g1".to_owned(),
        version: None,
    });
    assert!(matches!(
        read,
        Err(StoreError::Guide(GuideDocumentError::UnsupportedSchema { found }))
            if found == REVIEW_GUIDE_SCHEMA_VERSION + 1
    ));
    let listed = fixture
        .store
        .list_guide_versions(&ListGuideVersionsRequest {
            repo_path: fixture.repo.clone(),
            guide_id: "g1".to_owned(),
        })
        .expect("versions")
        .versions;
    assert_eq!(listed[0].schema_version, REVIEW_GUIDE_SCHEMA_VERSION + 1);
}

#[test]
fn documents_over_their_bounds_are_not_stored() {
    let fixture = fixture();
    let mut draft = version("g1");
    draft.outline = (0..=MAX_GUIDE_SECTIONS)
        .map(|index| section(&format!("s{index}")))
        .collect();
    let created = fixture.store.create_guide(CreateReviewGuideRequest {
        repo_path: fixture.repo.clone(),
        target: ReviewTarget::Repository,
        scope: None,
        guide: guide("g1"),
        version: draft,
    });
    assert!(matches!(
        created,
        Err(StoreError::Guide(
            GuideDocumentError::TooManySections { .. }
        ))
    ));
    assert_eq!(fixture.list(None), [] as [String; 0]);
}

#[test]
fn guide_tables_are_added_to_databases_that_predate_them() {
    let fixture = fixture();
    fixture.create("g1", None);
    let root = resolve_repo_root(&fixture.repo).expect("root");
    // As on a database migrated before the guide tables existed: their migrations run again
    // over the tables (created only when missing) and keep what is there.
    let ids = [
        "031_review_guides_table",
        "032_review_guide_versions_table",
        "033_review_guide_areas_table",
        "034_review_guide_threads_table",
        "035_review_guide_marks_table",
    ];
    let recorded = with_database(&root, fixture.store.config(), move |database| {
        Box::pin(async move {
            for id in ids {
                database
                    .delete(MIGRATIONS_TABLE)
                    .filter(Box::new(where_eq("id", id)))
                    .execute(database)
                    .await?;
            }
            run_migrations(database).await?;
            let mut recorded = 0;
            for id in ids {
                recorded += database
                    .select(MIGRATIONS_TABLE)
                    .filter(Box::new(where_eq("id", id)))
                    .execute(database)
                    .await?
                    .len();
            }
            Ok(recorded)
        })
    })
    .expect("migrate");
    assert_eq!(recorded, ids.len());
    assert_eq!(fixture.list(None), ["g1"]);
    assert!(fixture.version("g1", None).is_some());
}
