//! Reviewing with the guide, on a guide seeded through the review store: reviewed sections
//! mark their files viewed and un-viewing un-reviews them, a finding becomes a draft comment
//! linked back to it, expand asks the assistant under an `Expand` grant and what it adds
//! changes only that section's subtree, provisional sections are promoted and discarded, an ask
//! thread is kept across reopening, and restructuring writes a new version and keeps the old.

use super::guide::{Cited, cited, seed, selected, setup_configured, step_to};
use super::interaction::{Setup, build, fixture};
use super::stable;
use crate::assistant::LinkRequest;
use crate::testing::harness::Scenario;
use bcode_code_review::guide::config::{GuideConfig, MarkFilesViewed};
use bcode_code_review::guide::write::{ExpansionWrite, GuideOp, Writer};
use bcode_code_review_models::{
    GetGuideVersionRequest, GrantMode, GuideVersion, ListGuideSectionMarksRequest,
    ListGuideThreadsRequest, SaveGuideVersionRequest, SectionState,
};
use std::time::Duration;

/// A review with the seeded guide, marking cited files viewed.
fn guided(width: u16, height: u16) -> (Setup, Cited, crate::testing::fixture::GitFixture) {
    let fixture = fixture();
    let mut setup = setup_configured(
        &fixture,
        width,
        height,
        GuideConfig {
            mark_files_viewed: MarkFilesViewed::Cited,
            ..GuideConfig::default()
        },
    );
    let cited = cited(&setup.scenario);
    seed(&mut setup, &cited);
    assert!(setup.scenario.surface().guide().is_some());
    (setup, cited, fixture)
}

fn stored(setup: &Setup, version: Option<u32>) -> (GuideVersion, u64) {
    let record = setup
        .backend
        .call_now(GetGuideVersionRequest {
            repo_path: setup.workspace.repo_root.clone(),
            guide_id: "guide-1".to_owned(),
            version,
        })
        .version
        .expect("version");
    (record.version, record.revision)
}

fn marked(setup: &Setup) -> Vec<String> {
    setup
        .backend
        .call_now(ListGuideSectionMarksRequest {
            repo_path: setup.workspace.repo_root.clone(),
            guide_id: "guide-1".to_owned(),
        })
        .marks
        .into_iter()
        .map(|mark| mark.section_id)
        .collect()
}

fn viewed(setup: &Setup, cited: &Cited) -> bool {
    setup
        .scenario
        .surface()
        .document()
        .and_then(|doc| doc.viewed_state(&cited.changed.0))
        .is_some_and(bcode_code_review::viewed::ViewedState::is_viewed)
}

fn reviewed(setup: &Setup, id: &str) -> bool {
    setup
        .scenario
        .surface()
        .guide()
        .is_some_and(|guide| guide.reviewed(id))
}

/// Focus the panel and select `id`.
fn select(setup: &mut Setup, id: &str) {
    setup.scenario.key("alt+g");
    stable(&mut setup.scenario);
    step_to(&mut setup.scenario, id);
}

/// Select `id` in the guide's outline with the panel's keys.
fn select_row(setup: &mut Setup, id: &str) {
    for _ in 0..16 {
        if selected(&setup.scenario) == id {
            return;
        }
        setup.scenario.key("j");
        stable(&mut setup.scenario);
    }
    panic!("never reached {id}");
}

#[test]
fn reviewing_a_section_views_its_files_and_unviewing_one_unreviews_it() {
    let (mut setup, cited, _fixture) = guided(150, 40);
    select(&mut setup, "s2.1");
    assert!(!reviewed(&setup, "s2.1"));
    setup.scenario.key("v");
    stable(&mut setup.scenario);
    stable(&mut setup.scenario);
    assert_eq!(marked(&setup), ["s2.1"], "the mark is stored");
    assert!(viewed(&setup, &cited), "the cited file is viewed");
    assert!(reviewed(&setup, "s2.1"));
    let saved = setup
        .backend
        .call_now(bcode_code_review_models::GetReviewWorkspaceRequest {
            repo_path: setup.workspace.repo_root.clone(),
            workspace_id: setup.workspace.id.clone(),
        })
        .workspace
        .expect("saved review");
    assert!(
        saved
            .viewed_files
            .iter()
            .any(|file| file.contains(&cited.changed.0.path))
            || saved
                .viewed_marks
                .iter()
                .any(|mark| mark.path == cited.changed.0.path),
        "the viewed mark is saved with the review: {saved:?}"
    );

    // Un-viewing the file from the review un-reviews the section.
    setup.scenario.key("esc");
    stable(&mut setup.scenario);
    let header = setup
        .scenario
        .surface()
        .document()
        .and_then(|doc| {
            doc.index_of(&bcode_code_review::document::ItemKey::FileHeader(
                cited.changed.0.clone(),
            ))
        })
        .expect("the file's header");
    super::interaction::move_to(&mut setup.scenario, header);
    setup.scenario.key("v");
    stable(&mut setup.scenario);
    let screen = stable(&mut setup.scenario);
    assert!(!viewed(&setup, &cited), "{}", screen.text());
    assert!(!reviewed(&setup, "s2.1"));
    assert_eq!(marked(&setup), Vec::<String>::new(), "the mark is cleared");
}

#[test]
fn n_steps_to_the_next_unreviewed_section() {
    let (mut setup, _, _fixture) = guided(150, 40);
    select(&mut setup, "s2.1");
    setup.scenario.key("n");
    stable(&mut setup.scenario);
    assert_eq!(selected(&setup.scenario), "s2.2");
}

#[test]
fn a_finding_becomes_a_draft_comment_linked_back_to_it() {
    let (mut setup, _, _fixture) = guided(150, 40);
    setup.scenario.key("alt+g");
    stable(&mut setup.scenario);
    select_row(&mut setup, "s3");
    setup.scenario.key("c");
    let screen = stable(&mut setup.scenario);
    assert!(screen.contains("the change may race"), "{}", screen.text());
    setup.scenario.key("ctrl+s");
    stable(&mut setup.scenario);
    stable(&mut setup.scenario);
    let screen = stable(&mut setup.scenario);
    let (version, _) = stored(&setup, None);
    let finding = version.section("s3").expect("s3").findings[0].clone();
    let thread = finding
        .drafted_thread_id
        .unwrap_or_else(|| panic!("the draft is linked: {}", screen.text()));
    let draft = setup
        .scenario
        .surface()
        .notes()
        .drafts()
        .iter()
        .find(|draft| draft.thread_id == thread)
        .cloned()
        .expect("the draft thread");
    assert_eq!(draft.body, "the change may race");
    assert_eq!(
        draft.thread_kind,
        bcode_code_review_models::ReviewThreadKind::Finding
    );
    assert_eq!(
        draft.severity,
        bcode_code_review_models::ReviewThreadSeverity::Warning
    );
    let screen = stable(&mut setup.scenario);
    assert!(screen.contains("drafted"), "{}", screen.text());
}

#[test]
#[allow(clippy::too_many_lines)]
fn expand_is_granted_one_section_and_changes_only_its_subtree() {
    let (mut setup, _, _fixture) = guided(150, 40);
    select(&mut setup, "s2.1");
    let _ = setup.scenario.surface_mut().take_assistant_requests();
    setup.scenario.key("e");
    stable(&mut setup.scenario);
    let requests = setup.scenario.surface_mut().take_assistant_requests();
    let Some(LinkRequest::GuideMessage {
        text,
        grant,
        context,
    }) = requests.into_iter().next()
    else {
        panic!("expand asks the assistant");
    };
    assert!(text.starts_with("Expand §2.1"), "{text}");
    let grant = grant.expect("a grant");
    assert_eq!(
        grant.mode,
        GrantMode::Expand {
            section_id: "s2.1".to_owned()
        }
    );
    assert_eq!(grant.version, Some(1));
    assert!(context[0].text.contains("s2.1"), "{context:?}");

    // What the assistant writes under that grant.
    let (before, revision) = stored(&setup, None);
    let mut after = before.clone();
    let op = GuideOp::AddExpansion(ExpansionWrite {
        under: "s2.1".to_owned(),
        id: None,
        title: "Why it retries".to_owned(),
        body: "It retries on success too.".to_owned(),
        citations: Vec::new(),
        findings: Vec::new(),
    });
    bcode_code_review::guide::write::apply(
        &mut after,
        &[op],
        &[],
        &Writer {
            mode: &grant.mode,
            session_id: Some("assistant"),
            now_ms: 2,
        },
    )
    .expect("allowed");
    let _ = setup.backend.call_now(SaveGuideVersionRequest {
        repo_path: setup.workspace.repo_root.clone(),
        version: after,
        expected_revision: revision,
    });
    let (now, _) = stored(&setup, None);
    for id in ["s1", "s3", "s2.2", "s2.3"] {
        assert_eq!(
            serde_json::to_string(now.section(id).unwrap()).unwrap(),
            serde_json::to_string(before.section(id).unwrap()).unwrap(),
            "{id} is byte-identical"
        );
    }
    let expansion = now.section("s2.1").unwrap().children[0].clone();
    assert!(matches!(expansion.state, SectionState::Provisional { .. }));

    // Promote it, then discard a second one.
    setup.scenario.surface_mut().reload_guide();
    stable(&mut setup.scenario);
    select_row(&mut setup, &expansion.id);
    let screen = stable(&mut setup.scenario);
    assert!(
        screen.contains("provisional expansion"),
        "{}",
        screen.text()
    );
    setup.scenario.key("p");
    stable(&mut setup.scenario);
    stable(&mut setup.scenario);
    let (now, revision) = stored(&setup, None);
    assert_eq!(
        now.section(&expansion.id).unwrap().state,
        SectionState::Written
    );
    let mut again = now;
    bcode_code_review::guide::write::apply(
        &mut again,
        &[GuideOp::AddExpansion(ExpansionWrite {
            under: "s2.1".to_owned(),
            id: None,
            title: "Another".to_owned(),
            body: "More.".to_owned(),
            citations: Vec::new(),
            findings: Vec::new(),
        })],
        &[],
        &Writer {
            mode: &grant.mode,
            session_id: Some("assistant"),
            now_ms: 3,
        },
    )
    .expect("allowed");
    let second = again.section("s2.1").unwrap().children[1].id.clone();
    let _ = setup.backend.call_now(SaveGuideVersionRequest {
        repo_path: setup.workspace.repo_root.clone(),
        version: again,
        expected_revision: revision,
    });
    setup.scenario.surface_mut().reload_guide();
    stable(&mut setup.scenario);
    select_row(&mut setup, &second);
    setup.scenario.key("x");
    stable(&mut setup.scenario);
    stable(&mut setup.scenario);
    let (now, _) = stored(&setup, None);
    assert!(now.section(&second).is_none(), "discarded");
    assert!(
        now.section(&expansion.id).is_some(),
        "the promoted one stays"
    );
}

#[test]
fn an_ask_thread_is_kept_under_its_section_across_reopening() {
    let (mut setup, _, _fixture) = guided(150, 40);
    select(&mut setup, "s2.1");
    setup.scenario.key("a");
    stable(&mut setup.scenario);
    setup.scenario.paste("Why a token bucket?");
    setup.scenario.key("ctrl+s");
    stable(&mut setup.scenario);
    stable(&mut setup.scenario);
    let threads = setup
        .backend
        .call_now(ListGuideThreadsRequest {
            repo_path: setup.workspace.repo_root.clone(),
            guide_id: "guide-1".to_owned(),
            version: Some(1),
        })
        .threads;
    assert_eq!(threads.len(), 1, "{threads:?}");
    assert_eq!(threads[0].section_id, "s2.1");
    assert_eq!(threads[0].question, "Why a token bucket?");
    let session = threads[0].session_id.clone().expect("its own session");
    let bound = setup
        .backend
        .call_now(bcode_code_review_models::GetReviewAssistantRequest {
            repo_path: setup.workspace.repo_root.clone(),
            lookup: bcode_code_review_models::ReviewAssistantLookup::Session {
                session_id: session.clone(),
            },
        })
        .binding
        .expect("bound");
    assert_eq!(
        bound.guide_grant.map(|grant| grant.mode),
        Some(GrantMode::Ask {
            section_id: "s2.1".to_owned()
        })
    );
    assert!(
        setup
            .backend
            .host()
            .messages()
            .iter()
            .any(|(to, text)| *to == session && text.contains("Why a token bucket?"))
    );

    // Reopened: the thread is read again and shows under its section.
    let mut scenario = Scenario::new(
        build(&setup.backend, &setup.workspace, Duration::ZERO),
        150,
        40,
    );
    stable(&mut scenario);
    stable(&mut scenario);
    scenario.key("alt+g");
    stable(&mut scenario);
    let screen = step_to(&mut scenario, "s2.1");
    let guide = scenario.surface().guide().expect("guide");
    assert_eq!(guide.threads.len(), 1);
    assert!(screen.contains("Why a token bucket?"), "{}", screen.text());
}

#[test]
fn restructuring_writes_the_next_version_and_keeps_the_old() {
    let (mut setup, _, _fixture) = guided(150, 40);
    select(&mut setup, "s2.1");
    let _ = setup.scenario.surface_mut().take_assistant_requests();
    setup.scenario.key("shift+r");
    let screen = stable(&mut setup.scenario);
    assert!(
        screen.contains("keeps v1"),
        "the composer asks for directions: {}",
        screen.text()
    );
    setup.scenario.paste("group by layer");
    setup.scenario.key("ctrl+s");
    stable(&mut setup.scenario);
    stable(&mut setup.scenario);
    let screen = stable(&mut setup.scenario);
    let (current, _) = stored(&setup, None);
    assert_eq!(current.version, 2, "{}", screen.text());
    assert_eq!(current.based_on, Some(1));
    let (old, _) = stored(&setup, Some(1));
    assert_eq!(
        old.outline, current.outline,
        "v2 starts as a copy; v1 is kept"
    );
    let requests = setup.scenario.surface_mut().take_assistant_requests();
    let Some(LinkRequest::GuideMessage { text, grant, .. }) = requests.into_iter().next() else {
        panic!("the assistant is asked to restructure");
    };
    assert!(text.contains("group by layer"), "{text}");
    let grant = grant.expect("grant");
    assert_eq!(grant.mode, GrantMode::Restructure);
    assert_eq!(grant.version, Some(2));
}
