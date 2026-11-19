//! Questions asked from a thread stay with it: a card asked from a pull request thread follows
//! the thread wherever the review places it after a push (moved with its code, near it, on its
//! file, on its layer), directly after it in the document, the threads panel and thread
//! motions; a follow-up from the thread continues the same session; a card whose thread left
//! the review falls back to where it was asked; and cards saved before they recorded their
//! thread recover it from their session's prompt.

use super::app::{create, fixture, settled};
use super::interaction::{self, Setup, added_line, find, index_of, move_to};
use super::publish::{layer, resume};
use super::stable;
use crate::app::ReviewApp;
use crate::config::conversation::{ConversationConfig, SignalsConfig};
use crate::notes::NoteScope;
use crate::testing::backend::TestBackend;
use crate::testing::fake_provider::{FAKE_REPOSITORY, FakeProvider};
use crate::testing::fixture::{GitFixture, HEAD_BRANCH, STACK_BRANCHES};
use crate::testing::harness::Scenario;
use bcode_code_review::document::{AnchorPlacement, ItemKey, OutdatedReason, ReviewItem};
use bcode_code_review_models::{
    ConversationCapabilities, DeleteDraftRequest, DiffSide, ListDraftsRequest, ListDraftsResponse,
    ListReviewAiExchangesRequest, ListReviewAiExchangesResponse, Participant, PullActivity,
    PullState, RemoteAnchor, RemoteComment, RemoteThread, ReviewAiExchange, ReviewLineSide,
    ReviewWorkspace, SaveReviewAiExchangeRequest, VerdictSupport, ViewerAbilities,
};
use bcode_session_view::follow::SessionViewFollowUpdate;
use bcode_session_view_models::{
    ChatMessageView, SessionConnectionViewStatus, SessionViewSnapshot, TranscriptViewItem,
    TranscriptViewItemId, TranscriptViewItemKind,
};
use std::sync::Arc;

const PULL: u64 = 102;

/// Commit `edit` of `path` on `branch` (moving the branch), as a push does.
fn push_edit(fixture: &GitFixture, branch: &str, path: &str, edit: impl FnOnce(&str) -> String) {
    let worktree = tempfile::tempdir().expect("worktree dir");
    let checkout = worktree.path().join("checkout");
    let now = format!(
        "@{} +0000",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_secs()
    );
    let git = |dir: &std::path::Path, args: &[&str]| -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_AUTHOR_DATE", &now)
            .env("GIT_COMMITTER_DATE", &now)
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };
    let path_arg = checkout.to_str().expect("path");
    git(
        fixture.root(),
        &["worktree", "add", "-q", "--detach", path_arg, branch],
    );
    let file = checkout.join(path);
    let before = std::fs::read_to_string(&file).expect("read");
    std::fs::write(&file, edit(&before)).expect("write");
    git(&checkout, &["add", path]);
    git(&checkout, &["commit", "-q", "-m", "pushed again"]);
    let head = git(&checkout, &["rev-parse", "HEAD"]);
    git(
        fixture.root(),
        &["update-ref", &format!("refs/heads/{branch}"), &head],
    );
    git(fixture.root(), &["worktree", "remove", "--force", path_arg]);
}

fn layer_path() -> String {
    GitFixture::stack_layer_path(2)
}

fn activity(threads: Vec<RemoteThread>) -> PullActivity {
    PullActivity {
        number: PULL,
        title: format!("Layer {PULL}"),
        head_oid: String::new(),
        state: PullState::Open,
        author: None,
        description: String::new(),
        url: None,
        decision: None,
        reviews: Vec::new(),
        requested: Vec::new(),
        threads,
        comments: Vec::new(),
        checks: bcode_code_review_models::Checks::default(),
        viewer: ViewerAbilities {
            is_author: false,
            can_review: true,
            can_judge: true,
            can_comment: true,
        },
        updated_at_ms: None,
    }
}

/// raj's thread on line 3 of the layer's file, written on `commit`.
fn raj_thread(path: &str, commit: &str) -> RemoteThread {
    RemoteThread {
        id: "thread-1".to_owned(),
        anchor: RemoteAnchor {
            path: path.to_owned(),
            side: DiffSide::New,
            line: None,
            original_line: Some(3),
            original_commit_oid: Some(commit.to_owned()),
            ..RemoteAnchor::default()
        },
        resolved: false,
        outdated: false,
        can_reply: true,
        can_resolve: true,
        comments: vec![RemoteComment {
            id: "c1".to_owned(),
            author: Participant {
                login: "raj".to_owned(),
                ..Participant::default()
            },
            body: "Why this constant?".to_owned(),
            created_at_ms: Some(1_700_000_000_000),
            updated_at_ms: None,
            url: Some(format!(
                "https://example.com/{FAKE_REPOSITORY}/pull/{PULL}#c1"
            )),
            review_id: None,
        }],
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

struct Pull {
    fixture: GitFixture,
    backend: Arc<TestBackend>,
    workspace: ReviewWorkspace,
    /// The layer's head the thread was written on.
    written_on: String,
}

impl Pull {
    /// The review opened on #102's layer (a push can change which layer a stack review opens
    /// on), with the cursor on its first thread (its file loads there).
    fn open(&self) -> Scenario<ReviewApp> {
        let mut scenario = resume(&self.fixture, &self.backend, &self.workspace.id);
        settled(&mut scenario);
        for _ in 0..8 {
            let shown = document(&scenario)
                .groups()
                .any(|group| group.source.source_id == "source-1" && !group.hidden);
            if shown {
                break;
            }
            scenario.key(".");
            settled(&mut scenario);
        }
        scenario.key("g");
        scenario.key(")");
        settled(&mut scenario);
        scenario
    }

    fn set_threads(&self, threads: Vec<RemoteThread>) {
        self.backend
            .provider()
            .expect("provider")
            .set_activity(activity(threads));
    }

    fn exchanges(&self) -> Vec<ReviewAiExchange> {
        let scope = NoteScope::for_workspace(&self.workspace);
        let listed: ListReviewAiExchangesResponse =
            self.backend.call_now(ListReviewAiExchangesRequest {
                repo_path: scope.repo_path,
                target: scope.target,
                scope: scope.scope,
            });
        listed.exchanges
    }
}

/// #102 reviewed on its own, with raj's thread on line 3 of its file.
fn pull_with_thread() -> Pull {
    let fixture = fixture();
    let written_on = fixture
        .git(&["rev-parse", STACK_BRANCHES[1]])
        .trim()
        .to_owned();
    let provider = FakeProvider::new(fixture.root()).with_conversation(everything());
    provider.set_activity(activity(vec![raj_thread(&layer_path(), &written_on)]));
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
    let workspace = create(&backend, &fixture, "Threads", vec![layer(PULL)]);
    Pull {
        fixture,
        backend,
        workspace,
        written_on,
    }
}

fn document(scenario: &Scenario<ReviewApp>) -> &bcode_code_review::document::ReviewDocument {
    scenario
        .surface()
        .review()
        .and_then(|review| review.document())
        .expect("a document")
}

fn app_cursor(scenario: &Scenario<ReviewApp>) -> Option<ItemKey> {
    scenario.surface().review()?.cursor().cloned()
}

/// Put the cursor on the first thread and ask `question` from it.
fn ask_from_thread(scenario: &mut Scenario<ReviewApp>, question: &str) {
    scenario.key("g");
    scenario.key(")");
    settled(scenario);
    assert!(
        matches!(app_cursor(scenario), Some(ItemKey::Thread { .. })),
        "{:?}",
        app_cursor(scenario)
    );
    scenario.key("a");
    scenario.paste(question);
    scenario.key("enter");
    settled(scenario);
}

/// The thread item and the item right after it.
fn thread_and_next(doc: &bcode_code_review::document::ReviewDocument) -> (ReviewItem, ReviewItem) {
    let index = (0..doc.len())
        .find(|index| matches!(doc.item(*index), Some(ReviewItem::Thread(_))))
        .unwrap_or_else(|| {
            panic!(
                "a thread among {:?}",
                (0..doc.len().min(60))
                    .filter_map(|index| doc.item(index).map(ReviewItem::key))
                    .collect::<Vec<_>>()
            )
        });
    (
        doc.item(index).cloned().expect("thread"),
        doc.item(index + 1)
            .cloned()
            .expect("an item after the thread"),
    )
}

fn placement(item: &ReviewItem) -> (AnchorPlacement, Option<(ReviewLineSide, u32, u32)>) {
    match item {
        ReviewItem::Thread(thread) => (thread.placement, thread.lines),
        ReviewItem::AgentThread(agent) => (agent.placement, agent.lines),
        other => panic!("not an annotation: {other:?}"),
    }
}

/// The card after the thread is `exchange_id`'s, placed as the thread is.
fn assert_card_follows_thread(
    doc: &bcode_code_review::document::ReviewDocument,
    exchange_id: &str,
) -> (AnchorPlacement, Option<(ReviewLineSide, u32, u32)>) {
    let (thread, next) = thread_and_next(doc);
    let ReviewItem::AgentThread(agent) = &next else {
        panic!("the card follows its thread: {next:?}");
    };
    assert_eq!(agent.exchange_id.as_deref(), Some(exchange_id));
    assert_eq!(placement(&next), placement(&thread));
    placement(&thread)
}

#[test]
fn a_card_asked_from_a_pull_request_thread_follows_it_across_a_push() {
    let pull = pull_with_thread();
    let mut scenario = pull.open();
    ask_from_thread(&mut scenario, "Why 3?");
    let asked = pull.exchanges();
    assert_eq!(asked.len(), 1, "{asked:?}");
    let owner = asked[0].thread_id.clone().expect("the thread is recorded");
    assert!(
        owner.starts_with("remote:") && owner.ends_with("\u{1}thread-1"),
        "{owner:?}"
    );
    let exchange_id = asked[0].exchange_id.clone();
    let session = asked[0].session_id.clone().expect("linked");
    assert_eq!(
        assert_card_follows_thread(document(&scenario), &exchange_id),
        (AnchorPlacement::Line, Some((ReviewLineSide::New, 3, 3)))
    );

    // Two lines land above the thread's line: it moves to line 5, and the card with it.
    push_edit(&pull.fixture, STACK_BRANCHES[1], &layer_path(), |text| {
        format!("// one\n// two\n{text}")
    });
    let mut scenario = pull.open();
    assert_eq!(
        assert_card_follows_thread(document(&scenario), &exchange_id),
        (AnchorPlacement::Line, Some((ReviewLineSide::New, 5, 5)))
    );
    // A thread motion from the thread reaches its card next.
    scenario.key("g");
    scenario.key(")");
    scenario.key(")");
    settled(&mut scenario);
    assert_eq!(
        app_cursor(&scenario),
        Some(ItemKey::AgentThread {
            id: exchange_id.clone()
        })
    );

    // Asking from the thread again continues its session.
    ask_from_thread(&mut scenario, "And now?");
    let messages = pull.backend.messages();
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert_eq!(messages[1].0, session, "a follow-up in the same session");
    assert_eq!(pull.backend.sessions().len(), 1);
    let exchanges = pull.exchanges();
    assert_eq!(exchanges.len(), 2);
    assert_eq!(exchanges[1].thread_id.as_deref(), Some(owner.as_str()));
    // Both cards sit right after the thread, oldest first.
    let doc = document(&scenario);
    let thread = (0..doc.len())
        .find(|index| matches!(doc.item(*index), Some(ReviewItem::Thread(_))))
        .expect("thread");
    let cards: Vec<Option<String>> = (thread + 1..thread + 3)
        .map(|index| match doc.item(index) {
            Some(ReviewItem::AgentThread(agent)) => agent.exchange_id.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(
        cards,
        [Some(exchange_id), Some(exchanges[1].exchange_id.clone())]
    );
}

#[test]
fn a_card_goes_with_its_thread_near_its_code_on_its_file_and_on_its_layer() {
    let pull = pull_with_thread();
    let mut scenario = pull.open();
    ask_from_thread(&mut scenario, "Why 3?");
    let exchange_id = pull.exchanges()[0].exchange_id.clone();

    // The push rewrites the thread's lines: the thread is carried near them, the card too.
    push_edit(&pull.fixture, STACK_BRANCHES[1], &layer_path(), |text| {
        text.lines()
            .enumerate()
            .filter(|(index, _)| !(1..=3).contains(index))
            .map(|(_, line)| format!("{line}\n"))
            .collect()
    });
    let scenario = pull.open();
    let (near, lines) = assert_card_follows_thread(document(&scenario), &exchange_id);
    assert_ne!(
        lines,
        Some((ReviewLineSide::New, 3, 3)),
        "{near:?} {lines:?}"
    );

    // The commit it was written on is not here: thread and card on its file together.
    pull.set_threads(vec![raj_thread(&layer_path(), "0ldc0mm1t")]);
    let scenario = pull.open();
    assert_eq!(
        assert_card_follows_thread(document(&scenario), &exchange_id).0,
        AnchorPlacement::File
    );

    // The layer no longer changes its file: thread and card on the layer together.
    pull.set_threads(vec![raj_thread("no/longer/changed.rs", &pull.written_on)]);
    let scenario = pull.open();
    assert_eq!(
        assert_card_follows_thread(document(&scenario), &exchange_id).0,
        AnchorPlacement::Review
    );

    // The thread is gone from the pull request: the card falls back to where it was asked,
    // a revision of the layer since replaced.
    pull.set_threads(Vec::new());
    let scenario = pull.open();
    let doc = document(&scenario);
    let card = (0..doc.len())
        .find_map(|index| match doc.item(index) {
            Some(ReviewItem::AgentThread(agent)) => Some(agent.clone()),
            _ => None,
        })
        .expect("the card is still shown");
    assert_eq!(card.exchange_id.as_deref(), Some(exchange_id.as_str()));
    assert_eq!(
        card.placement,
        AnchorPlacement::Outdated(OutdatedReason::RevisionChanged)
    );
}

// ------------------------------------------------------------------------------------------
// Local threads
// ------------------------------------------------------------------------------------------

fn local_exchanges(setup: &Setup) -> Vec<ReviewAiExchange> {
    let scope = NoteScope::for_workspace(&setup.workspace);
    let listed: ListReviewAiExchangesResponse =
        setup.backend.call_now(ListReviewAiExchangesRequest {
            repo_path: scope.repo_path,
            target: scope.target,
            scope: scope.scope,
        });
    listed.exchanges
}

fn first_thread(scenario: &Scenario<crate::surface::ReviewSurface>) -> ItemKey {
    let doc = scenario.surface().document().expect("document");
    (0..doc.len())
        .filter_map(|index| doc.item(index))
        .find(|item| matches!(item, ReviewItem::Thread(_)))
        .map(ReviewItem::key)
        .expect("a thread")
}

/// A note on the first added line, and a question asked from it.
fn asked_from_local_thread(setup: &mut Setup) -> (String, ReviewAiExchange) {
    let line = find(&setup.scenario, 0, added_line);
    move_to(&mut setup.scenario, line);
    setup.scenario.key("c");
    setup.scenario.paste("open note");
    setup.scenario.key("ctrl+s");
    stable(&mut setup.scenario);
    let ItemKey::Thread { thread_id } = first_thread(&setup.scenario) else {
        unreachable!()
    };
    let at = index_of(&setup.scenario, &first_thread(&setup.scenario));
    move_to(&mut setup.scenario, at);
    setup.scenario.key("a");
    setup.scenario.paste("Is this note right?");
    setup.scenario.key("enter");
    stable(&mut setup.scenario);
    let exchanges = local_exchanges(setup);
    assert_eq!(exchanges.len(), 1, "{exchanges:?}");
    (thread_id, exchanges[0].clone())
}

/// The path of the thread `thread_id`.
fn thread_path(setup: &Setup, thread_id: &str) -> String {
    setup.scenario.surface().notes().thread(thread_id)[0]
        .anchor
        .file_path
        .clone()
}

#[test]
fn a_card_asked_from_a_local_thread_stays_with_it_until_the_thread_is_deleted() {
    let fixture = interaction::fixture();
    let mut setup = interaction::setup(&fixture, 170, 50);
    let (thread_id, asked) = asked_from_local_thread(&mut setup);
    assert_eq!(asked.thread_id.as_deref(), Some(thread_id.as_str()));
    {
        let doc = setup.scenario.surface().document().expect("document");
        let (placed, _) = assert_card_follows_doc(doc, &asked.exchange_id);
        assert_eq!(placed, AnchorPlacement::Line);
    }

    // The threads panel lists the question right after its thread.
    setup.scenario.key("shift+t");
    let screen = stable(&mut setup.scenario);
    let pane = setup
        .scenario
        .surface()
        .body_area(crate::surface::THREADS_PANE_ID)
        .expect("threads panel");
    let rows: Vec<String> = (pane.y + 1..pane.bottom() - 1)
        .map(|y| screen.text_in(pane, y))
        .filter(|row| row.contains("open note") || row.contains("Is this note right?"))
        .collect();
    assert_eq!(rows.len(), 2, "{}", screen.text());
    assert!(rows[0].contains("open note"), "{rows:?}");
    assert!(rows[1].contains("Is this note right?"), "{rows:?}");
    setup.scenario.key("shift+t");
    stable(&mut setup.scenario);

    // The file changes: the thread is outdated on its file, and the card stays with it.
    let path = thread_path(&setup, &thread_id);
    push_edit(&fixture, HEAD_BRANCH, &path, |text| {
        format!("{text}// edited\n")
    });
    let setup = interaction::setup_with_backend(&fixture, &setup, 170, 50);
    {
        let doc = setup.scenario.surface().document().expect("document");
        assert_eq!(
            assert_card_follows_doc(doc, &asked.exchange_id).0,
            AnchorPlacement::Outdated(OutdatedReason::ContentChanged)
        );
    }

    // The thread is deleted: the card stays, at the anchor it was asked about.
    let scope = NoteScope::for_workspace(&setup.workspace);
    let drafts: ListDraftsResponse = setup.backend.call_now(ListDraftsRequest {
        repo_path: scope.repo_path.clone(),
        target: scope.target.clone(),
        scope: scope.scope.clone(),
    });
    for draft in drafts.drafts {
        let _: bcode_code_review_models::DeleteDraftResponse =
            setup.backend.call_now(DeleteDraftRequest {
                repo_path: scope.repo_path.clone(),
                target: scope.target.clone(),
                scope: scope.scope.clone(),
                comment_id: draft.comment_id,
            });
    }
    let setup = interaction::setup_with_backend(&fixture, &setup, 170, 50);
    let doc = setup.scenario.surface().document().expect("document");
    assert!(
        !(0..doc.len()).any(|index| matches!(doc.item(index), Some(ReviewItem::Thread(_)))),
        "the thread is gone"
    );
    let card = (0..doc.len())
        .find_map(|index| match doc.item(index) {
            Some(ReviewItem::AgentThread(agent)) => Some(agent.clone()),
            _ => None,
        })
        .expect("the card is still shown");
    assert_eq!(
        card.placement,
        AnchorPlacement::Outdated(OutdatedReason::ContentChanged)
    );
}

/// [`assert_card_follows_thread`] on a surface's document.
fn assert_card_follows_doc(
    doc: &bcode_code_review::document::ReviewDocument,
    exchange_id: &str,
) -> (AnchorPlacement, Option<(ReviewLineSide, u32, u32)>) {
    assert_card_follows_thread(doc, exchange_id)
}

#[test]
fn hiding_resolved_threads_hides_the_cards_asked_from_them() {
    let fixture = interaction::fixture();
    let mut setup = interaction::setup(&fixture, 170, 50);
    let (_, asked) = asked_from_local_thread(&mut setup);
    let card = ItemKey::AgentThread {
        id: asked.exchange_id.clone(),
    };
    let at = index_of(&setup.scenario, &first_thread(&setup.scenario));
    move_to(&mut setup.scenario, at);
    // Resolve the thread, then hide resolved threads.
    setup.scenario.key("shift+r");
    stable(&mut setup.scenario);
    assert!(
        setup
            .scenario
            .surface()
            .notes()
            .thread_resolved(match &first_thread(&setup.scenario) {
                ItemKey::Thread { thread_id } => thread_id,
                _ => unreachable!(),
            }),
        "resolved"
    );
    setup.scenario.key("shift+h");
    stable(&mut setup.scenario);
    let doc = setup.scenario.surface().document().expect("document");
    assert!(
        doc.index_of(&card).is_none(),
        "the card hides with its thread"
    );
    setup.scenario.key("shift+h");
    stable(&mut setup.scenario);
    let doc = setup.scenario.surface().document().expect("document");
    assert!(doc.index_of(&card).is_some(), "and shows with it");
}

// ------------------------------------------------------------------------------------------
// Cards saved before they recorded their thread
// ------------------------------------------------------------------------------------------

fn snapshot_with_prompt(prompt: &str) -> SessionViewSnapshot {
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot.connection_status = SessionConnectionViewStatus::Attached;
    snapshot.transcript.items = vec![
        TranscriptViewItem {
            id: TranscriptViewItemId::new("user"),
            revision: 1,
            sequence: Some(1),
            timestamp_ms: Some(1),
            output_location: None,
            streaming: false,
            kind: TranscriptViewItemKind::UserMessage {
                message: ChatMessageView::markdown(prompt),
            },
        },
        TranscriptViewItem {
            id: TranscriptViewItemId::new("assistant"),
            revision: 1,
            sequence: Some(2),
            timestamp_ms: Some(2),
            output_location: None,
            streaming: false,
            kind: TranscriptViewItemKind::AssistantMessage {
                message: ChatMessageView::markdown("It is."),
            },
        },
    ];
    snapshot
}

fn save_exchange(setup: &Setup, exchange: ReviewAiExchange) {
    let scope = NoteScope::for_workspace(&setup.workspace);
    let _: bcode_code_review_models::SaveReviewAiExchangeResponse =
        setup.backend.call_now(SaveReviewAiExchangeRequest {
            repo_path: scope.repo_path,
            target: scope.target,
            scope: scope.scope,
            exchange,
        });
}

#[test]
fn a_card_saved_without_its_thread_recovers_it_from_its_session() {
    let fixture = interaction::fixture();
    let mut setup = interaction::setup(&fixture, 170, 50);
    let (thread_id, asked) = asked_from_local_thread(&mut setup);
    let session = asked.session_id.clone().expect("linked");
    let prompt = setup.backend.messages()[0].1.clone();
    // As an older version saved them: the question, and a follow-up asked from its card in the
    // same session, neither naming the thread.
    save_exchange(
        &setup,
        ReviewAiExchange {
            thread_id: None,
            ..asked.clone()
        },
    );
    save_exchange(
        &setup,
        ReviewAiExchange {
            exchange_id: format!("{}-follow-up", asked.exchange_id),
            thread_id: None,
            question: "Anything else?".to_owned(),
            created_at_ms: asked.created_at_ms + 1,
            ..asked.clone()
        },
    );
    assert!(
        local_exchanges(&setup)
            .iter()
            .all(|exchange| exchange.thread_id.is_none())
    );
    let mut setup = interaction::setup_with_backend(&fixture, &setup, 170, 50);
    // The card's session shows the question's turn: its prompt names the thread.
    setup
        .scenario
        .host()
        .session_view(&session)
        .expect("followed")
        .try_send(SessionViewFollowUpdate::Snapshot(Box::new(
            snapshot_with_prompt(&prompt),
        )))
        .expect("delivered");
    stable(&mut setup.scenario);
    let recovered = local_exchanges(&setup);
    assert_eq!(recovered.len(), 2);
    for exchange in &recovered {
        assert_eq!(
            exchange.thread_id.as_deref(),
            Some(thread_id.as_str()),
            "{exchange:?}"
        );
    }
    assert_eq!(
        recovered[0].updated_at_ms, asked.updated_at_ms,
        "recording the thread does not make the card new"
    );
    let doc = setup.scenario.surface().document().expect("document");
    let (thread, next) = thread_and_next(doc);
    assert!(matches!(thread, ReviewItem::Thread(_)));
    assert!(
        matches!(&next, ReviewItem::AgentThread(agent) if agent.exchange_id.as_deref() == Some(asked.exchange_id.as_str())),
        "{next:?}"
    );
}

/// Move the app's cursor to item `index` with counted `j`/`k`.
fn app_move_to(scenario: &mut Scenario<ReviewApp>, index: usize) {
    let doc = document(scenario);
    let current = doc
        .index_of(&app_cursor(scenario).expect("cursor"))
        .expect("cursor index");
    let count = super::stops_between(doc, current, index);
    let key = if index >= current { "j" } else { "k" };
    if count > 0 {
        for digit in count.to_string().chars() {
            scenario.key(&digit.to_string());
        }
        scenario.key(key);
    }
    settled(scenario);
}

#[test]
fn saving_a_transient_review_keeps_its_cards_with_their_threads() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let entry = crate::entry::Entry::Sources {
        title: None,
        sources: vec![bcode_code_review_models::ReviewSourceKind::LastCommit],
    };
    let (mut scenario, _) = super::app::open(&fixture, &backend, entry, 160, 48);
    let first_line = {
        let doc = document(&scenario);
        (0..doc.len())
            .find(|index| doc.item(*index).is_some_and(added_line))
            .expect("an added line")
    };
    app_move_to(&mut scenario, first_line);
    scenario.key("c");
    scenario.paste("open note");
    scenario.key("ctrl+s");
    settled(&mut scenario);
    let thread = {
        let doc = document(&scenario);
        (0..doc.len())
            .find(|index| matches!(doc.item(*index), Some(ReviewItem::Thread(_))))
            .expect("the thread")
    };
    app_move_to(&mut scenario, thread);
    scenario.key("a");
    scenario.paste("Is this note right?");
    scenario.key("enter");
    settled(&mut scenario);
    scenario.key("ctrl+s");
    settled(&mut scenario);
    let review = scenario.surface().review().expect("review");
    assert!(!review.is_transient());
    let scope = NoteScope::for_workspace(review.workspace().expect("workspace"));
    let drafts: ListDraftsResponse = backend.call_now(ListDraftsRequest {
        repo_path: scope.repo_path.clone(),
        target: scope.target.clone(),
        scope: scope.scope.clone(),
    });
    let exchanges: ListReviewAiExchangesResponse = backend.call_now(ListReviewAiExchangesRequest {
        repo_path: scope.repo_path,
        target: scope.target,
        scope: scope.scope,
    });
    assert_eq!(drafts.drafts.len(), 1);
    assert_eq!(exchanges.exchanges.len(), 1);
    assert_eq!(
        exchanges.exchanges[0].thread_id.as_deref(),
        Some(drafts.drafts[0].thread_id.as_str()),
        "the card names its thread by its id in the saved review"
    );
    let (thread, next) = thread_and_next(document(&scenario));
    assert!(matches!(thread, ReviewItem::Thread(_)));
    assert!(matches!(next, ReviewItem::AgentThread(_)), "{next:?}");
}
