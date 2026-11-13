//! Pull request conversations on the review screen, against the fake provider (a provider like
//! any other: the review knows it only by its manifest).
//!
//! * The pull request bar shows the decision, approvals, checks, and open threads, with buttons
//!   for what the provider and the viewer may do; opened, it shows reviews, checks, and the
//!   conversation.
//! * Remote threads sit on their lines with Reply, Resolve, and Open; resolving goes to the
//!   provider and the thread folds as resolved.
//! * A reply held for the review goes with an approval started from the bar.
//! * A provider that only reads shows the conversation with no actions.

use super::app::{fixture, open, settled};
use crate::config::conversation::{ConversationConfig, SignalsConfig};
use crate::entry::Entry;
use crate::testing::backend::TestBackend;
use crate::testing::fake_provider::{FAKE_REPOSITORY, FakeProvider};
use crate::testing::fixture::GitFixture;
use crate::testing::harness::{Scenario, Screen};
use bcode_code_review_models::{
    Check, CheckState, Checks, ConversationCapabilities, DiffSide, Participant, PullActivity,
    PullReview, PullState, RemoteAnchor, RemoteComment, RemoteThread, ReviewDecision, ReviewState,
    ReviewVerdict, VerdictSupport, ViewerAbilities,
};
use std::sync::Arc;

const PULL: u64 = 102;

fn person(login: &str) -> Participant {
    Participant {
        login: login.to_owned(),
        is_viewer: login == "me",
        ..Participant::default()
    }
}

fn comment(id: &str, login: &str, body: &str) -> RemoteComment {
    RemoteComment {
        id: id.to_owned(),
        author: person(login),
        body: body.to_owned(),
        created_at_ms: Some(1_700_000_000_000),
        updated_at_ms: None,
        url: Some(format!(
            "https://example.com/{FAKE_REPOSITORY}/pull/{PULL}#{id}"
        )),
        review_id: None,
    }
}

/// #102's conversation: approved by ana, a failing check, a thread on the layer's new file, and
/// a conversation comment.
fn activity() -> PullActivity {
    PullActivity {
        number: PULL,
        title: format!("Layer {PULL}"),
        head_oid: String::new(),
        state: PullState::Open,
        author: Some(person("octocat")),
        description: "Adds the second layer.".to_owned(),
        url: Some(format!("https://example.com/{FAKE_REPOSITORY}/pull/{PULL}")),
        decision: Some(ReviewDecision::Approved),
        reviews: vec![PullReview {
            id: "r1".to_owned(),
            author: person("ana"),
            state: ReviewState::Approved,
            body: "Looks right.".to_owned(),
            at_ms: Some(1_700_000_000_000),
            commit_oid: None,
            url: None,
        }],
        requested: vec![person("raj")],
        threads: vec![RemoteThread {
            id: "thread-1".to_owned(),
            anchor: RemoteAnchor {
                path: GitFixture::stack_layer_path(2),
                side: DiffSide::New,
                line: Some(3),
                original_line: Some(3),
                ..RemoteAnchor::default()
            },
            resolved: false,
            outdated: false,
            can_reply: true,
            can_resolve: true,
            comments: vec![comment("c1", "raj", "Why this constant?")],
        }],
        comments: vec![comment("c2", "li", "Ship it after the lint fix.")],
        checks: Checks {
            state: Some(CheckState::Failing),
            checks: vec![
                Check {
                    name: "lint".to_owned(),
                    state: CheckState::Failing,
                    url: Some("https://example.com/checks/lint".to_owned()),
                },
                Check {
                    name: "test".to_owned(),
                    state: CheckState::Passing,
                    url: None,
                },
            ],
        },
        viewer: ViewerAbilities {
            is_author: false,
            can_review: true,
            can_judge: true,
            can_comment: true,
        },
        updated_at_ms: None,
    }
}

const fn everything() -> ConversationCapabilities {
    ConversationCapabilities {
        activity: true,
        watch: None,
        thread_reply: true,
        thread_resolve: true,
        comment_post: true,
        review_preview: true,
        verdicts: VerdictSupport {
            comment: true,
            approve: true,
            request_changes: true,
        },
        replies_in_review: true,
    }
}

fn backend(fixture: &GitFixture, capabilities: ConversationCapabilities) -> Arc<TestBackend> {
    backend_with(fixture, capabilities, &ConversationConfig::default())
}

fn backend_with(
    fixture: &GitFixture,
    capabilities: ConversationCapabilities,
    config: &ConversationConfig,
) -> Arc<TestBackend> {
    let provider = FakeProvider::new(fixture.root()).with_conversation(capabilities);
    provider.set_activity(activity());
    Arc::new(
        TestBackend::default()
            .with_provider(provider)
            .with_conversation_config(ConversationConfig {
                signals: SignalsConfig {
                    min_gap_ms: 0,
                    ..SignalsConfig::default()
                },
                ..config.clone()
            }),
    )
}

fn open_pull(
    backend: &Arc<TestBackend>,
    fixture: &GitFixture,
) -> (Scenario<crate::app::ReviewApp>, Screen) {
    let entry = Entry::PullRequest {
        reference: format!("https://example.com/{FAKE_REPOSITORY}/pull/{PULL}"),
    };
    open(fixture, backend, entry, 160, 48)
}

/// Click the first place `needle` is painted.
fn click(scenario: &mut Scenario<crate::app::ReviewApp>, needle: &str) -> Screen {
    let screen = scenario.frame();
    let at = screen
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} is not painted:\n{}", screen.text()));
    scenario.click(at.x, at.y);
    settled(scenario)
}

#[test]
fn the_bar_and_threads_show_the_conversation_with_buttons_for_what_is_allowed() {
    let fixture = fixture();
    let backend = backend(&fixture, everything());
    let (mut scenario, screen) = open_pull(&backend, &fixture);
    let bar = screen.find("Approved").map_or_else(
        || panic!("no bar:\n{}", screen.text()),
        |at| screen.row_text(at.y),
    );
    for part in ["#102", "Approved", "by ana", "1 failing", "1 open"] {
        assert!(bar.contains(part), "{part:?} missing from {bar:?}");
    }
    for button in ["Approve", "Request changes", "Comment", "Open ↗", "↻"] {
        assert!(bar.contains(button), "{button:?} missing from {bar:?}");
    }
    // Before the files, the pull request's card: the top of its page, then a torn break.
    for part in [
        "Adds the second layer.",
        "Reviews",
        "ana approved",
        "Looks right.",
        "waiting for raj",
        "lint failing",
        "1 passing",
        "Ship it after the lint fix.",
    ] {
        assert!(screen.contains(part), "{part:?}:\n{}", screen.text());
    }
    let card = screen.find("Adds the second layer.").expect("card").y;
    let file = screen.find("stack/layer_2.rs").map_or(u16::MAX, |at| at.y);
    assert!(card < file, "{}", screen.text());
    assert!(
        (card..file).any(|y| screen.row_text(y).contains('▀')),
        "a torn break between the card and the files:\n{}",
        screen.text()
    );

    // Folded, the card is its status line, and the files come up.
    let screen = click(&mut scenario, "▾ ✓");
    assert!(
        !screen.contains("Ship it after the lint fix."),
        "{}",
        screen.text()
    );
    // The thread sits on its line with the provider's actions.
    for part in [
        "raj",
        "on Fake forge",
        "Why this constant?",
        "Reply",
        "Resolve",
    ] {
        assert!(screen.contains(part), "{part:?}:\n{}", screen.text());
    }

    // Resolving goes to the provider; the thread folds as resolved.
    let screen = click(&mut scenario, "Resolve");
    let provider = backend.provider().expect("provider");
    assert!(provider.activity(PULL).expect("activity").threads[0].resolved);
    assert!(screen.contains("resolved"), "{}", screen.text());
    assert!(!screen.contains("Why this constant?"));
    // The bar's status goes to the card, open again.
    let screen = click(&mut scenario, "Approved");
    assert!(
        screen.contains("Ship it after the lint fix."),
        "{}",
        screen.text()
    );
    assert!(
        provider
            .calls()
            .iter()
            .filter(
                |call| call.as_str() == bcode_code_review_models::OP_REVIEW_SOURCE_PULLS_ACTIVITY
            )
            .count()
            >= 2,
        "the change is read back: {:?}",
        provider.calls()
    );
}

#[test]
fn a_held_reply_goes_with_an_approval_started_from_the_bar() {
    let fixture = fixture();
    let backend = backend(&fixture, everything());
    let (mut scenario, _) = open_pull(&backend, &fixture);
    // Replies send at once by default; "Add to review" holds this one.
    let screen = click(&mut scenario, "Reply");
    assert!(screen.contains("Add to review"), "{}", screen.text());
    scenario.paste("A typo; fixing.");
    let screen = click(&mut scenario, "Add to review");
    assert!(
        screen.contains("1 reply held for your review"),
        "{}",
        screen.text()
    );
    let provider = backend.provider().expect("provider");
    assert_eq!(
        provider.activity(PULL).expect("activity").threads[0]
            .comments
            .len(),
        1
    );

    // Approve from the bar: straight to the provider's review, verdict chosen.
    click(&mut scenario, " Approve ");
    scenario.key("enter");
    settled(&mut scenario);
    let screen = scenario.frame();
    assert!(screen.contains("Approve on #102"), "{}", screen.text());
    scenario.key("enter");
    scenario.key("enter");
    settled(&mut scenario);
    let submitted = provider.submitted();
    assert_eq!(submitted.len(), 1, "{submitted:?}");
    assert_eq!(submitted[0].verdict, ReviewVerdict::Approve);
    assert_eq!(submitted[0].pull.number, PULL);
    assert_eq!(submitted[0].replies.len(), 1);
    assert_eq!(submitted[0].replies[0].body, "A typo; fixing.");
    // Sent: the draft is gone and the reply is on the thread.
    let review = scenario.surface().review().expect("review");
    assert_eq!(review.notes().held_replies().len(), 0);
    assert_eq!(
        provider.activity(PULL).expect("activity").threads[0]
            .comments
            .len(),
        2
    );
}

#[test]
fn a_reply_sent_now_reaches_the_thread() {
    let fixture = fixture();
    let backend = backend(&fixture, everything());
    let (mut scenario, _) = open_pull(&backend, &fixture);
    click(&mut scenario, "Reply");
    scenario.paste("Because the spec says so.");
    // The composer's main button sends at once.
    let screen = scenario.frame();
    let row = screen.find("Add to review").expect("composer").y;
    let x = screen.column_of(row, " Reply ").expect("reply button");
    scenario.click(x + 1, row);
    let screen = settled(&mut scenario);
    let provider = backend.provider().expect("provider");
    let thread = &provider.activity(PULL).expect("activity").threads[0];
    assert_eq!(
        thread.comments.last().map(|c| c.body.as_str()),
        Some("Because the spec says so.")
    );
    assert!(
        screen.contains("Because the spec says so."),
        "{}",
        screen.text()
    );
}

#[test]
fn a_provider_that_only_reads_shows_the_conversation_without_actions() {
    let fixture = fixture();
    let read_only = ConversationCapabilities {
        activity: true,
        ..ConversationCapabilities::default()
    };
    let backend = backend(&fixture, read_only);
    let (_, screen) = open_pull(&backend, &fixture);
    let bar = screen.find("Approved").map_or_else(
        || panic!("no bar:\n{}", screen.text()),
        |at| screen.row_text(at.y),
    );
    assert!(!bar.contains(" Approve "), "{bar}");
    assert!(!bar.contains("Request changes"), "{bar}");
    assert!(!bar.contains(" Comment "), "{bar}");
    assert!(screen.contains("Why this constant?"), "{}", screen.text());
    assert!(!screen.contains("Resolve"), "{}", screen.text());
    // Without approvals among its verdicts, only "Request changes" would be missing.
    let no_request_changes = ConversationCapabilities {
        verdicts: VerdictSupport {
            comment: true,
            approve: true,
            request_changes: false,
        },
        ..everything()
    };
    let backend = super::conversation::backend(&fixture, no_request_changes);
    let (_, screen) = open_pull(&backend, &fixture);
    let bar = screen.row_text(screen.find("Approved").expect("bar").y);
    assert!(bar.contains(" Approve "), "{bar}");
    assert!(!bar.contains("Request changes"), "{bar}");
}

/// The row of the thread card's header (the row naming its first author on the forge).
fn card_header_row(screen: &Screen) -> u16 {
    (0..screen.height())
        .find(|y| {
            let row = screen.row_text(*y);
            row.contains("raj") && row.contains("Viewed")
        })
        .unwrap_or_else(|| panic!("no thread card header:\n{}", screen.text()))
}

/// Click the thread card's viewed checkbox.
fn click_viewed(scenario: &mut Scenario<crate::app::ReviewApp>) -> Screen {
    let screen = scenario.frame();
    let row = card_header_row(&screen);
    let x = screen.column_of(row, "Viewed").expect("checkbox");
    scenario.click(x, row);
    settled(scenario)
}

/// raj replies on the thread, and the review reads the conversation again.
fn raj_replies(
    backend: &Arc<TestBackend>,
    scenario: &mut Scenario<crate::app::ReviewApp>,
) -> Screen {
    let mut activity = activity();
    activity.threads[0]
        .comments
        .push(comment("c3", "raj", "Still curious about it."));
    backend.provider().expect("provider").set_activity(activity);
    let screen = scenario.frame();
    let at = screen.find("↻").expect("refresh");
    scenario.click(at.x, at.y);
    settled(scenario)
}

#[test]
fn a_thread_is_a_card_and_viewing_it_folds_it_until_someone_replies() {
    let fixture = fixture();
    let backend = backend(&fixture, everything());
    let (mut scenario, screen) = open_pull(&backend, &fixture);
    // A framed card: the header in the top border, the buttons in the bottom one.
    let header = card_header_row(&screen);
    let top = screen.row_text(header);
    assert!(
        top.contains('╭') && top.contains('╮') && top.contains("○ Viewed"),
        "{top}"
    );
    let bottom = (header..screen.height())
        .find(|y| screen.row_text(*y).contains('╰'))
        .expect("bottom border");
    assert!(
        screen.row_text(bottom).contains("Reply"),
        "{}",
        screen.text()
    );
    assert!(screen.contains("Why this constant?"));

    // Viewed: one row, still unresolved on the forge.
    let screen = click_viewed(&mut scenario);
    assert!(!screen.contains("Why this constant?"), "{}", screen.text());
    let header = card_header_row(&screen);
    assert!(
        screen.row_text(header).contains("✓ Viewed"),
        "{}",
        screen.text()
    );
    assert!(screen.row_text(header).contains('╶'));
    let provider = backend.provider().expect("provider");
    assert!(!provider.activity(PULL).expect("activity").threads[0].resolved);

    // raj replies: the thread opens again, the reply marked new.
    let screen = raj_replies(&backend, &mut scenario);
    assert!(
        screen.contains("Still curious about it."),
        "{}",
        screen.text()
    );
    assert!(screen.contains("1 new"), "{}", screen.text());
    assert!(
        screen
            .row_text(card_header_row(&screen))
            .contains("○ Viewed")
    );
    // The bar counts it as new until the thread is viewed again.
    let bar = |screen: &Screen| {
        screen
            .find("Approved")
            .map(|at| screen.row_text(at.y))
            .unwrap_or_default()
    };
    assert!(bar(&screen).contains("1 new"), "{}", screen.text());
    let screen = click_viewed(&mut scenario);
    assert!(!bar(&screen).contains(" new"), "{}", screen.text());
}

#[test]
fn folds_can_be_kept_through_new_comments_with_a_new_chip() {
    let fixture = fixture();
    let backend = backend_with(
        &fixture,
        everything(),
        &ConversationConfig {
            threads: crate::config::conversation::ThreadsConfig {
                new_comments: crate::config::conversation::NewCommentsMode::KeepFolded,
                ..crate::config::conversation::ThreadsConfig::default()
            },
            ..ConversationConfig::default()
        },
    );
    let (mut scenario, _) = open_pull(&backend, &fixture);
    click_viewed(&mut scenario);
    let screen = raj_replies(&backend, &mut scenario);
    assert!(
        !screen.contains("Still curious about it."),
        "{}",
        screen.text()
    );
    let header = screen.row_text(card_header_row(&screen));
    assert!(
        header.contains("1 new") && header.contains('╶'),
        "{}",
        screen.text()
    );
}

#[test]
fn a_roomy_card_puts_its_header_and_buttons_on_rows_of_their_own() {
    let fixture = fixture();
    let backend = backend_with(
        &fixture,
        everything(),
        &ConversationConfig {
            threads: crate::config::conversation::ThreadsConfig {
                card: crate::config::conversation::ThreadCardMode::Roomy,
                ..crate::config::conversation::ThreadsConfig::default()
            },
            ..ConversationConfig::default()
        },
    );
    let (_, screen) = open_pull(&backend, &fixture);
    let header = card_header_row(&screen);
    assert!(
        screen.row_text(header - 1).contains('╭'),
        "{}",
        screen.text()
    );
    assert!(screen.row_text(header).contains('│'), "{}", screen.text());
    let buttons = (header..screen.height())
        .find(|y| screen.row_text(*y).contains("Reply"))
        .expect("buttons");
    assert!(screen.row_text(buttons).contains('│'));
    assert!(
        screen.row_text(buttons + 1).contains('╰'),
        "{}",
        screen.text()
    );
}

#[test]
fn asking_about_a_thread_on_the_pull_request_as_a_whole_opens_the_composer_under_it() {
    let fixture = fixture();
    let provider = FakeProvider::new(fixture.root()).with_conversation(everything());
    let mut on_layer = activity();
    // A file this layer no longer changes: the thread is on the layer as a whole.
    on_layer.threads[0].anchor.path = "no/longer/changed.rs".to_owned();
    provider.set_activity(on_layer);
    let backend = Arc::new(
        TestBackend::default()
            .with_provider(provider)
            .with_conversation_config(ConversationConfig {
                signals: SignalsConfig {
                    min_gap_ms: 0,
                    ..SignalsConfig::default()
                },
                ..ConversationConfig::default()
            }),
    );
    let (mut scenario, screen) = open_pull(&backend, &fixture);
    let header = card_header_row(&screen);
    let bottom = (header..screen.height())
        .find(|y| screen.row_text(*y).contains('╰'))
        .expect("bottom border");
    let x = screen.column_of(bottom, "Ask Bcode").expect("ask button");
    scenario.click(x, bottom);
    settled(&mut scenario);
    let doc = scenario.surface().review().unwrap().document().unwrap();
    let composer = doc
        .index_of(&bcode_code_review::document::ItemKey::Composer)
        .expect("composer shown");
    assert!(
        matches!(
            doc.item(composer - 1),
            Some(bcode_code_review::document::ReviewItem::Thread(_))
        ),
        "the composer follows its thread"
    );
    // Not at the top: the pull request's card comes before it.
    let card = (0..composer).find(|index| {
        matches!(
            doc.item(*index),
            Some(bcode_code_review::document::ReviewItem::SourceIntro(_))
        )
    });
    assert!(
        card.is_some(),
        "the composer is under the pull request's card"
    );
}

/// #102 with an outdated thread kevin wrote on a file the layer no longer changes (so the review
/// shows it on the pull request), with a reply, next to the thread on the layer's file.
fn activity_with_outdated_thread() -> PullActivity {
    let mut activity = activity();
    activity.threads.push(RemoteThread {
        id: "thread-gone".to_owned(),
        anchor: RemoteAnchor {
            path: "src/retry.rs".to_owned(),
            side: DiffSide::New,
            line: None,
            original_line: Some(14),
            original_start_line: Some(12),
            original_commit_oid: Some("0ldc0mm1t".to_owned()),
            diff_hunk: Some(
                "@@ -10,3 +10,6 @@ fn send()\n context\n+loop {\n+    send()?;\n+}".to_owned(),
            ),
            ..RemoteAnchor::default()
        },
        resolved: false,
        outdated: true,
        can_reply: true,
        can_resolve: true,
        comments: vec![
            comment("k1", "kevin", "This retry loop never backs off."),
            comment("k2", "octocat", "Backoff comes in the next layer."),
        ],
    });
    activity
}

/// The prompt and title of the only question asked so far.
fn asked(backend: &Arc<TestBackend>) -> (String, String) {
    let sessions = backend.sessions();
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    let messages = backend.messages();
    assert_eq!(messages.len(), 1, "{messages:?}");
    (messages[0].1.clone(), sessions[0].1.clone())
}

#[test]
fn asking_from_an_outdated_pull_request_thread_carries_the_thread() {
    let fixture = fixture();
    let backend = backend(&fixture, everything());
    backend
        .provider()
        .expect("provider")
        .set_activity(activity_with_outdated_thread());
    let (mut scenario, screen) = open_pull(&backend, &fixture);
    assert!(
        screen.contains("This retry loop never backs off."),
        "{}",
        screen.text()
    );
    // Ask from kevin's thread: the second "Ask Bcode" under his comment.
    let kevin = screen.find("This retry loop").expect("thread").y;
    let row = (kevin..screen.height())
        .find(|&y| screen.row_text(y).contains("Ask Bcode"))
        .expect("Ask Bcode on the thread");
    let x = screen.column_of(row, "Ask Bcode").expect("button");
    scenario.click(x + 1, row);
    settled(&mut scenario);
    scenario.paste("Is this still a problem?");
    scenario.key("enter");
    settled(&mut scenario);

    let (prompt, title) = asked(&backend);
    for part in [
        "a Fake forge thread on pull request",
        "#102",
        "outdated: its code changed since it was written",
        "Thread id: ",
        ":thread-gone (code_review.get_thread reads it in full)",
        "Written on: src/retry.rs, new lines 12-14, at commit 0ldc0mm1t",
        "on the pull request: the review no longer changes its file",
        "```diff\n@@ -10,3 +10,6 @@ fn send()\n context\n+loop {\n+    send()?;\n+}\n```",
        "--- kevin:\nThis retry loop never backs off.",
        "--- octocat:\nBackoff comes in the next layer.",
        "code_review.list_threads and code_review.get_thread",
        "Asked about: the whole review",
        "Other review threads on src/retry.rs:\nnone",
        "Question:\nIs this still a problem?",
    ] {
        assert!(prompt.contains(part), "{part:?} missing from:\n{prompt}");
    }
    assert!(
        prompt.find("--- kevin").expect("kevin") < prompt.find("--- octocat").expect("octocat"),
        "comments in order"
    );
    assert!(!prompt.contains("review.context.get"), "{prompt}");
    assert_eq!(title, "Review: src/retry.rs · kevin");

    // The review tools read the same threads from the review store.
    let surface = scenario.surface();
    let workspace = surface
        .review()
        .and_then(|review| review.workspace())
        .expect("workspace");
    let scope = crate::notes::NoteScope::for_workspace(workspace);
    let saved: bcode_code_review_models::GetReviewConversationResponse =
        backend.call_now(bcode_code_review_models::GetReviewConversationRequest {
            repo_path: scope.repo_path,
            target: scope.target,
            scope: scope.scope,
        });
    let snapshot = saved.snapshot.expect("saved for the review tools");
    let ids: Vec<&str> = snapshot
        .threads
        .iter()
        .map(|record| record.thread.id.as_str())
        .collect();
    assert_eq!(ids, ["thread-1", "thread-gone"]);
    let gone = &snapshot.threads[1];
    assert_eq!(
        gone.placement,
        bcode_code_review_models::ThreadPlacement::OnLayer
    );
    assert!(gone.thread.anchor.diff_hunk.is_some());
}

#[test]
fn a_question_on_a_file_lists_its_pull_request_threads() {
    let fixture = fixture();
    let backend = backend(&fixture, everything());
    let (mut scenario, screen) = open_pull(&backend, &fixture);
    // Ask about the layer's file from its header.
    let at = screen
        .find(" Ask Bcode ")
        .unwrap_or_else(|| panic!("no file Ask:\n{}", screen.text()));
    scenario.click(at.x + 1, at.y);
    settled(&mut scenario);
    scenario.paste("What does this file do?");
    scenario.key("enter");
    settled(&mut scenario);
    let (prompt, title) = asked(&backend);
    let others = prompt
        .split_once(&format!(
            "Other review threads on {}:\n",
            GitFixture::stack_layer_path(2)
        ))
        .map(|(_, rest)| rest)
        .expect("other threads");
    let line = others.lines().next().unwrap_or_default();
    assert!(
        line.contains(":3 by raj (Fake forge")
            && line.contains("#102")
            && line.contains("Why this constant?"),
        "{prompt}"
    );
    assert_eq!(
        title,
        format!("Review: {}", GitFixture::stack_layer_path(2))
    );
}
