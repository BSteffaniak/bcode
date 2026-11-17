//! The app-wide review assistant: the panel on home and in reviews, staying open with its
//! conversation while the user moves between them; the Ask AI button and its hit region; the
//! panel's placement; the history picker (switch, search, rename, delete, needs-you rows) and
//! the earlier per-review conversations it lists; the permission prompt answered inline; and the
//! review following what the conversation's review tools change.

use super::app::settled;
use crate::app::{ReviewApp, RouteName};
use crate::assistant::button::BUTTON_ID;
use crate::assistant::history::Tag;
use crate::config::ReviewViewConfig;
use crate::entry::Entry;
use crate::host::AppSession;
use crate::testing::backend::TestBackend;
use crate::testing::fixture::{FixtureSpec, GitFixture};
use crate::testing::fixture_branch_compare;
use crate::testing::harness::{Scenario, Screen};
use bcode_code_review::assistant::{ReviewRef, ReviewTools};
use bcode_code_review::document::ItemKey;
use bcode_code_review_models::{
    BindReviewAssistantRequest, GetReviewAssistantRequest, REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION,
    ReviewAssistantBinding, ReviewAssistantLookup, ReviewAssistantTag, ReviewLineSide,
    ReviewTarget, ReviewWorkspace, UpdateReviewWorkspaceRequest,
};
use bcode_session_view::follow::SessionViewFollowUpdate;
use bcode_session_view_models::{
    ChatMessageView, PermissionView, SessionConnectionViewStatus, SessionViewSnapshot,
    ToolInvocationView, ToolInvocationViewStatus, ToolTimingView, TranscriptViewItem,
    TranscriptViewItemId, TranscriptViewItemKind,
};
use bmux_tui::geometry::Rect;
use std::sync::Arc;
use std::time::Duration;

fn item(id: &str, kind: TranscriptViewItemKind) -> TranscriptViewItem {
    TranscriptViewItem {
        id: TranscriptViewItemId::new(id),
        revision: 1,
        sequence: Some(1),
        timestamp_ms: Some(1),
        output_location: None,
        streaming: false,
        kind,
    }
}

pub(super) fn user(text: &str) -> TranscriptViewItem {
    item(
        "user",
        TranscriptViewItemKind::UserMessage {
            message: ChatMessageView::markdown(text),
        },
    )
}

fn answer(text: &str) -> TranscriptViewItem {
    item(
        "answer",
        TranscriptViewItemKind::AssistantMessage {
            message: ChatMessageView::markdown(text),
        },
    )
}

/// A finished `name` call with `arguments`.
fn tool(id: &str, name: &str, arguments: &str) -> TranscriptViewItem {
    item(
        id,
        TranscriptViewItemKind::ToolInvocation {
            tool: Box::new(ToolInvocationView {
                tool_call_id: id.to_owned(),
                producer_plugin_id: Some("bcode.code_review".to_owned()),
                tool_name: Some(name.to_owned()),
                arguments_json: Some(arguments.to_owned()),
                working_directory: None,
                request_draft: None,
                status: ToolInvocationViewStatus::Finished,
                result_text: Some("done".to_owned()),
                is_error: Some(false),
                result: None,
                presentation: None,
                timing: ToolTimingView::default(),
            }),
        },
    )
}

pub(super) fn snapshot(items: Vec<TranscriptViewItem>) -> SessionViewSnapshot {
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot.connection_status = SessionConnectionViewStatus::Attached;
    snapshot.transcript.items = items;
    snapshot
}

/// The app over a fixture repository with two saved reviews.
pub(super) struct Setup {
    pub(super) scenario: Scenario<ReviewApp>,
    pub(super) backend: Arc<TestBackend>,
    /// The branch comparison review.
    pub(super) review: ReviewWorkspace,
    /// The last-commit review.
    pub(super) other: ReviewWorkspace,
    _fixture: GitFixture,
}

fn saved(backend: &TestBackend, workspace: ReviewWorkspace) -> ReviewWorkspace {
    let _ = backend.call_now(UpdateReviewWorkspaceRequest {
        repo_path: workspace.repo_root.clone(),
        workspace: workspace.clone(),
        base: None,
    });
    workspace
}

pub(super) fn setup(width: u16, height: u16, start_in_review: bool) -> Setup {
    setup_with(TestBackend::default(), width, height, start_in_review)
}

/// [`setup`] over `backend`.
pub(super) fn setup_with(
    backend: TestBackend,
    width: u16,
    height: u16,
    start_in_review: bool,
) -> Setup {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let backend = Arc::new(backend);
    let root = fixture.root().to_path_buf();
    let review = saved(
        &backend,
        ReviewWorkspace {
            id: "assistant-review".to_owned(),
            title: "Retry backoff".to_owned(),
            ..ReviewWorkspace::from_target(root.clone(), fixture_branch_compare())
        },
    );
    let other = saved(
        &backend,
        ReviewWorkspace {
            id: "assistant-other".to_owned(),
            title: "Last commit".to_owned(),
            ..ReviewWorkspace::from_target(root.clone(), ReviewTarget::LastCommit)
        },
    );
    let entry = if start_in_review {
        Entry::Workspace {
            workspace: Box::new(review.clone()),
        }
    } else {
        Entry::Home
    };
    let app = ReviewApp::new(backend.services(), root, ReviewViewConfig::default(), entry)
        .with_scroll_motion(Duration::ZERO);
    let mut scenario = Scenario::with_shared_host(app, backend.host(), width, height);
    settled(&mut scenario);
    Setup {
        scenario,
        backend,
        review,
        other,
        _fixture: fixture,
    }
}

impl Setup {
    pub(super) fn app(&self) -> &ReviewApp {
        self.scenario.surface()
    }

    pub(super) fn session(&self) -> Option<String> {
        self.app().assistant().session().map(str::to_owned)
    }

    pub(super) fn settle(&mut self) -> Screen {
        settled(&mut self.scenario)
    }

    /// Type `text` into the focused panel and send it; returns the session it went to.
    pub(super) fn send(&mut self, text: &str) -> String {
        self.scenario.paste(text);
        self.scenario.key("enter");
        self.settle();
        let (session, _) = self
            .backend
            .host()
            .turns()
            .last()
            .cloned()
            .expect("submitted");
        session
    }

    pub(super) fn binding(&self, session: &str) -> Option<ReviewAssistantBinding> {
        self.backend
            .call_now(GetReviewAssistantRequest {
                repo_path: self.review.repo_root.clone(),
                lookup: ReviewAssistantLookup::Session {
                    session_id: session.to_owned(),
                },
            })
            .binding
    }

    /// Deliver `snapshot` to the followed `session`.
    pub(super) fn deliver(&mut self, session: &str, snapshot: SessionViewSnapshot) -> Screen {
        self.backend
            .host()
            .session_view(session)
            .expect("the panel follows its session")
            .try_send(SessionViewFollowUpdate::Snapshot(Box::new(snapshot)))
            .expect("delivered");
        self.settle()
    }

    /// Open `title` from home.
    pub(super) fn open_from_home(&mut self, title: &str) {
        assert_eq!(self.app().route_name(), RouteName::Home);
        let screen = self.settle();
        let at = screen.find(title).expect("the review is listed");
        self.scenario.click(at.x, at.y);
        self.settle();
        self.scenario.key("enter");
        self.settle();
        assert_eq!(self.app().route_name(), RouteName::Review);
    }

    fn painted_button(&self, screen: &Screen) -> Option<Rect> {
        let region = screen
            .hits()
            .iter()
            .find(|region| region.id.as_str() == BUTTON_ID)
            .map(|region| region.area);
        assert_eq!(
            region,
            self.app().assistant_button(),
            "the hit region is where the button was painted"
        );
        region
    }
}

/// The last row showing `text` (history rows sit under the panel's title row).
fn row_of(screen: &Screen, text: &str) -> u16 {
    (0..screen.height())
        .rev()
        .find(|y| screen.row_text(*y).contains(text))
        .unwrap_or_else(|| panic!("no row shows {text:?}:\n{}", screen.text()))
}

#[test]
fn the_panel_opens_on_home_and_keeps_its_conversation_into_a_review_and_back() {
    let mut setup = setup(160, 40, false);
    setup.scenario.key("ctrl+k");
    let screen = setup.settle();
    assert!(setup.app().assistant().is_focused());
    let title = screen.find("Assistant").expect("panel title");
    assert!(title.x >= 60, "docked on the right: {title:?}");
    assert!(screen.contains("⌖ home"), "{}", screen.text());

    let session = setup.send("Create a review for my stack");
    let origin = setup
        .backend
        .host()
        .session_origins()
        .last()
        .cloned()
        .expect("origin");
    assert_eq!(
        origin.context.get("purpose").map(String::as_str),
        Some("assistant")
    );
    assert!(
        !origin.context.contains_key("workspace"),
        "started on home: {origin:?}"
    );
    assert_eq!(setup.app().assistant().tag(), Some(&Tag::Home));
    let (_, request) = setup.backend.host().turns().last().cloned().expect("turn");
    assert_eq!(
        request.text, "Create a review for my stack",
        "sent as typed"
    );
    assert!(setup.binding(&session).is_none(), "home attaches no review");

    // The screen takes the keyboard back; the panel stays open into a review.
    setup.scenario.key("esc");
    setup.open_from_home("Retry backoff");
    assert!(setup.app().assistant().is_open());
    assert_eq!(setup.session().as_deref(), Some(session.as_str()));
    let screen = setup.settle();
    assert!(
        screen.contains("Create a review for my"),
        "{}",
        screen.text()
    );
    let binding = setup
        .binding(&session)
        .expect("attached to the review on screen");
    assert_eq!(binding.workspace.id, setup.review.id);
    assert!(!binding.is_detached());
    assert_eq!(
        binding.started,
        Some(ReviewAssistantTag::none()),
        "the tag stays home"
    );

    // Back home: the conversation stays and the session is detached.
    setup.scenario.key("q");
    setup.settle();
    assert_eq!(setup.app().route_name(), RouteName::Home);
    assert_eq!(setup.session().as_deref(), Some(session.as_str()));
    assert!(setup.binding(&session).expect("binding").is_detached());

    // Into the other review: attached there.
    setup.open_from_home("Last commit");
    let binding = setup.binding(&session).expect("binding");
    assert_eq!(binding.workspace.id, setup.other.id);
    assert!(!binding.is_detached());
}

#[test]
fn a_conversation_started_in_a_review_is_tagged_with_it_and_follows_its_tools() {
    let mut setup = setup(160, 50, true);
    setup.scenario.key("ctrl+k");
    let session = setup.send("Review this file.");
    let origin = setup
        .backend
        .host()
        .session_origins()
        .last()
        .cloned()
        .expect("origin");
    assert_eq!(origin.context.get("workspace"), Some(&setup.review.id));
    assert_eq!(origin.validate(), Ok(()));
    let binding = setup.binding(&session).expect("bound to the review");
    assert_eq!(binding.workspace.id, setup.review.id);
    assert_ne!(
        binding.resolved.len(),
        0,
        "the resolved sources are recorded"
    );

    // The tools write through the store the review reads, as the plugin does.
    let review = setup.app().review().expect("review");
    let doc = review.document().expect("document");
    let (path, line) = (0..doc.len())
        .filter_map(|index| doc.item(index))
        .find_map(|item| match item.key() {
            ItemKey::Line {
                file,
                line,
                side: ReviewLineSide::New,
            } => Some((file.path.clone(), line)),
            ItemKey::LinePair {
                file,
                new: Some(new),
                ..
            } => Some((file.path.clone(), new)),
            _ => None,
        })
        .expect("a new-side line");
    let services = setup.backend.services();
    let data = bcode_code_review::ReviewData::default();
    let mut tools = ReviewTools::new(&data, services.store());
    let opened = tools
        .open(
            &setup.review.repo_root,
            &ReviewRef::Session(session.clone()),
        )
        .expect("bound review");
    tools
        .add_comment(
            &opened,
            &bcode_code_review::assistant::Location {
                path: Some(path.clone()),
                line: Some(line),
                end_line: None,
                side: bcode_code_review::assistant::Side::New,
                source: None,
            },
            "TOOL_DRAFTED_COMMENT",
            None,
        )
        .expect("drafted");
    let navigate = format!(r#"{{"path":"{path}","line":{line}}}"#);
    let screen = setup.deliver(
        &session,
        snapshot(vec![
            user("Review this file."),
            tool("write-1", "code_review.add_comment", "{}"),
            tool("nav-1", "code_review.navigate", &navigate),
            answer("Drafted it: ASSISTANT_ANSWER"),
        ]),
    );
    assert!(screen.contains("ASSISTANT_ANSWER"), "{}", screen.text());
    // The review reloaded its comments: the drafted one shows at its line.
    let drafted = |setup: &Setup| {
        setup.app().review().is_some_and(|review| {
            review
                .notes()
                .drafts()
                .iter()
                .any(|draft| draft.body == "TOOL_DRAFTED_COMMENT")
        })
    };
    for _ in 0..8 {
        if drafted(&setup) {
            break;
        }
        setup.settle();
    }
    assert!(drafted(&setup), "the review shows the drafted comment");
    let review = setup.app().review().expect("review");
    match review.cursor() {
        Some(ItemKey::Line { file, line: at, .. }) => {
            assert_eq!((file.path.as_str(), *at), (path.as_str(), line));
        }
        Some(ItemKey::LinePair { file, new, .. }) => {
            assert_eq!((file.path.as_str(), *new), (path.as_str(), Some(line)));
        }
        other => panic!("cursor moved elsewhere: {other:?}"),
    }
}

#[test]
fn the_permission_prompt_is_answered_in_the_panel_and_never_remembers_publishing() {
    let mut setup = setup(160, 40, true);
    setup.scenario.key("ctrl+k");
    let session = setup.send("Draft a comment here.");
    let permission = |id: &str, tool: &str| PermissionView {
        permission_id: id.to_owned(),
        session_id: None,
        tool_call_id: format!("call-{id}"),
        tool_name: tool.to_owned(),
        producer_plugin_id: Some("bcode.code_review".to_owned()),
        arguments_json: r#"{"path":"a.rs","line":3,"body":"Rename this"}"#.to_owned(),
        batch: None,
        agent_id: "build".to_owned(),
        title: None,
        policy_source: None,
        detail: None,
        resolved: false,
        approved: None,
        can_remember: true,
    };
    let mut asking = snapshot(vec![user("Draft a comment here.")]);
    asking.runtime.active_turn_id = Some("turn-1".to_owned());
    asking
        .permissions
        .push(permission("p-1", "code_review.add_comment"));
    let screen = setup.deliver(&session, asking.clone());
    assert!(screen.contains("y allow"), "{}", screen.text());
    assert!(screen.contains("a always"), "{}", screen.text());
    setup.scenario.key("y");
    setup.settle();
    assert_eq!(
        setup.backend.host().permission_answers(),
        vec![("p-1".to_owned(), true, false)]
    );

    // Publishing is asked every time: Always is not offered, and `a` does not remember.
    asking.permissions = vec![permission("p-2", "code_review.publish")];
    let screen = setup.deliver(&session, asking);
    assert!(screen.contains("y allow"), "{}", screen.text());
    assert!(!screen.contains("a always"), "{}", screen.text());
    setup.scenario.key("a");
    setup.settle();
    assert_eq!(setup.backend.host().permission_answers().len(), 1);

    // While the panel is closed the button shows that the conversation needs the user.
    setup.scenario.key("ctrl+k");
    let screen = setup.settle();
    let button = setup.painted_button(&screen).expect("button");
    assert!(
        screen.text_in(button, button.y).contains('?'),
        "{}",
        screen.row_text(button.y)
    );
}

#[test]
fn the_ask_ai_button_and_ctrl_k_open_the_panel_on_every_screen() {
    let mut setup = setup(160, 40, false);
    // Home: a click on the button opens and focuses the panel; a second click closes it.
    let screen = setup.settle();
    let button = setup.painted_button(&screen).expect("button on home");
    assert!(screen.text_in(button, button.y).contains("✦ Ask AI"));
    assert_eq!(button.right(), 160, "at the end of the header row");
    setup.scenario.click(button.x, button.y);
    setup.settle();
    assert!(setup.app().assistant().is_focused());
    let screen = setup.settle();
    let button = setup.painted_button(&screen).expect("button");
    setup.scenario.click(button.x, button.y);
    setup.settle();
    assert!(!setup.app().assistant().is_open());

    // The palette entry on home.
    setup.scenario.key(":");
    setup.scenario.paste("ask ai");
    setup.scenario.key("enter");
    setup.settle();
    assert!(
        setup.app().assistant().is_focused(),
        "the palette opens the panel"
    );
    setup.scenario.key("ctrl+k");
    setup.settle();
    assert!(!setup.app().assistant().is_open());

    // The wizard.
    setup.scenario.key("n");
    setup.settle();
    assert_eq!(setup.app().route_name(), RouteName::NewReview);
    let screen = setup.settle();
    assert!(setup.painted_button(&screen).is_some(), "{}", screen.text());
    setup.scenario.key("ctrl+k");
    let screen = setup.settle();
    assert!(setup.app().assistant().is_focused());
    assert!(screen.contains("⌖ new review"), "{}", screen.text());

    // A review: the button sits after the publish button.
    let mut setup = self::setup(160, 40, true);
    let screen = setup.settle();
    let button = setup.painted_button(&screen).expect("button in a review");
    let publish = screen.find("Publish").expect("publish button");
    assert!(publish.x < button.x && publish.y == button.y);
    setup.scenario.click(button.x, button.y);
    setup.settle();
    assert!(setup.app().assistant().is_focused());
    // esc gives the review the keyboard; ctrl+k focuses the panel again, then closes it.
    setup.scenario.key("esc");
    setup.settle();
    assert!(setup.app().assistant().is_open() && !setup.app().assistant().is_focused());
    setup.scenario.keys(&["ctrl+k", "ctrl+k"]);
    setup.settle();
    assert!(!setup.app().assistant().is_open());
}

#[test]
fn the_buttons_hit_region_is_its_painted_cells_at_every_width() {
    for start_in_review in [false, true] {
        let mut setup = setup(200, 30, start_in_review);
        for width in [200, 80, 30, 12, 8, 1] {
            setup.scenario.resize(width, 30);
            let screen = setup.settle();
            match setup.painted_button(&screen) {
                Some(area) => {
                    assert!(
                        area.right() <= width && area.width < width,
                        "{width}: {area:?}"
                    );
                    let label = screen.text_in(area, area.y);
                    let expected = if width >= crate::assistant::button::FULL_HEADER {
                        "✦ Ask AI"
                    } else if width >= crate::assistant::button::SHORT_HEADER {
                        "✦ AI"
                    } else {
                        "✦"
                    };
                    assert!(label.contains(expected), "{width}: {label:?}");
                }
                None => assert!(
                    width < crate::assistant::button::MIN_HEADER,
                    "{width}: no button"
                ),
            }
        }
    }
}

#[test]
fn the_button_keeps_its_cells_beside_wide_titles() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let backend = Arc::new(TestBackend::default());
    let workspace = saved(
        &backend,
        ReviewWorkspace {
            id: "wide-title".to_owned(),
            title: "レビュー 🚀 再試行のバックオフ e\u{301} 漢字".repeat(3),
            ..ReviewWorkspace::from_target(fixture.root().to_path_buf(), fixture_branch_compare())
        },
    );
    let app = ReviewApp::new(
        backend.services(),
        fixture.root().to_path_buf(),
        ReviewViewConfig::default(),
        Entry::Workspace {
            workspace: Box::new(workspace),
        },
    )
    .with_scroll_motion(Duration::ZERO);
    let mut scenario = Scenario::with_shared_host(app, backend.host(), 120, 30);
    for width in [120, 61, 47, 33, 23, 15] {
        scenario.resize(width, 30);
        let screen = settled(&mut scenario);
        let region = screen
            .hits()
            .iter()
            .find(|region| region.id.as_str() == BUTTON_ID)
            .map(|region| region.area);
        assert_eq!(region, scenario.surface().assistant_button(), "{width}");
        let area = region.expect("button");
        let label = screen.text_in(area, area.y);
        assert!(label.trim_start().starts_with('✦'), "{width}: {label:?}");
        // The title never runs into the button's cells.
        let before = screen.text_in(Rect::new(0, 0, area.x, 1), 0);
        assert!(!before.contains('✦'), "{width}: {before:?}");
    }
}

#[test]
fn the_panel_docks_right_below_or_over_the_screen_by_size() {
    let mut setup = setup(100, 40, false);
    setup.scenario.key("ctrl+k");
    let screen = setup.settle();
    let title = screen.find("Assistant").expect("panel title");
    assert!(title.x >= 60, "right dock at 100 columns: {title:?}");

    setup.scenario.resize(99, 40);
    let screen = setup.settle();
    let title = screen.find("Assistant").expect("panel title");
    assert!(title.y >= 8 && title.x < 10, "bottom dock: {title:?}");
    assert!(screen.contains("Code reviews"), "home keeps its rows above");

    setup.scenario.resize(99, 15);
    let screen = setup.settle();
    assert!(
        !screen.contains("Code reviews"),
        "zoomed over home: {}",
        screen.text()
    );
    assert!(screen.contains("Assistant"));
    // Unfocused, the zoomed panel hides but stays open.
    setup.scenario.key("esc");
    let screen = setup.settle();
    assert!(screen.contains("Code reviews"), "{}", screen.text());
    assert!(setup.app().assistant().is_open());

    // Tiny areas: the title row only, and nothing panics at any size.
    setup.scenario.key("ctrl+k");
    for (width, height) in [(11, 15), (23, 5), (1, 1), (0, 0), (40, 2)] {
        setup.scenario.resize(width, height);
        setup.settle();
    }
}

#[test]
fn the_history_lists_every_conversation_and_switches_to_the_one_searched() {
    let mut setup = setup(160, 40, true);
    let (first, _) = two_conversations(&mut setup);
    // An earlier per-review conversation, created before origins carried the review: known
    // from its binding only.
    setup.backend.host().add_catalog_session(AppSession {
        id: "legacy-session".to_owned(),
        title: Some("Review assistant: Last commit".to_owned()),
        created_at_ms: 10,
        updated_at_ms: 10,
        origin_app: None,
        origin_context: std::collections::BTreeMap::new(),
    });
    let legacy = ReviewAssistantBinding {
        schema_version: REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION,
        session_id: "legacy-session".to_owned(),
        workspace: setup.other.clone(),
        resolved: Vec::new(),
        stacks: bcode_code_review_models::ReviewAssistantStacks::default(),
        created_at_ms: 0,
        updated_at_ms: 0,
        started: None,
        detached_at_ms: None,
    };
    let _ = setup.backend.call_now(BindReviewAssistantRequest {
        repo_path: setup.other.repo_root.clone(),
        binding: legacy.clone(),
    });
    // A binding whose session Bcode no longer has is left out.
    let _ = setup.backend.call_now(BindReviewAssistantRequest {
        repo_path: setup.other.repo_root.clone(),
        binding: ReviewAssistantBinding {
            session_id: "vanished-session".to_owned(),
            ..legacy
        },
    });
    setup
        .backend
        .host()
        .set_pending_permissions(vec![crate::host::PendingPermission {
            session_id: first.clone(),
            permission_id: "p-1".to_owned(),
            tool: "code_review.add_comment".to_owned(),
        }]);

    setup.scenario.key("alt+h");
    let screen = setup.settle();
    let text = screen.text();
    assert!(screen.contains("Why did the cap move?"), "{text}");
    assert!(screen.contains("Explain the retry loop"), "{text}");
    assert!(screen.contains("Review assistant: Last commit"), "{text}");
    assert!(!screen.contains("vanished"), "{text}");
    let first_row = row_of(&screen, "Why did the cap move?");
    assert!(
        screen.row_text(first_row).contains("needs you"),
        "{}",
        screen.row_text(first_row)
    );
    let second_row = row_of(&screen, "Explain the retry loop");
    assert!(second_row < first_row, "newest first");
    assert!(
        screen.row_text(second_row).contains('●'),
        "the active conversation is marked"
    );
    let rows = setup.app().assistant().picker().expect("picker").rows();
    let legacy_row = rows
        .iter()
        .find(|row| row.session_id == "legacy-session")
        .expect("legacy row");
    assert_eq!(
        legacy_row.tag.label(),
        "Last commit",
        "tagged with its review"
    );

    // Search, then switch.
    setup.scenario.paste("cap");
    setup.settle();
    let matching: Vec<String> = setup
        .app()
        .assistant()
        .picker()
        .expect("picker")
        .matching()
        .iter()
        .map(|row| row.title.clone())
        .collect();
    assert_eq!(matching, vec!["Why did the cap move?".to_owned()]);
    setup.scenario.key("enter");
    let screen = setup.settle();
    assert_eq!(setup.session().as_deref(), Some(first.as_str()));
    assert!(
        screen.contains("Why did the cap move?"),
        "the header names it"
    );
}

/// Two conversations started in the review; the second is active.
fn two_conversations(setup: &mut Setup) -> (String, String) {
    setup.scenario.key("ctrl+k");
    let first = setup.send("Why did the cap move?");
    setup.scenario.key("ctrl+n");
    let second = setup.send("Explain the retry loop");
    assert_ne!(first, second);
    (first, second)
}

#[test]
fn conversations_are_renamed_and_deleted_with_bcode() {
    let mut setup = setup(160, 40, true);
    let (first, second) = two_conversations(&mut setup);
    // Switch to the first.
    setup.scenario.key("alt+h");
    setup.settle();
    setup.scenario.paste("cap");
    setup.scenario.key("enter");
    setup.settle();
    assert_eq!(setup.session().as_deref(), Some(first.as_str()));

    // Rename the active conversation from the panel.
    setup.scenario.key("alt+r");
    for _ in 0.."Why did the cap move?".len() {
        setup.scenario.key("backspace");
    }
    setup.scenario.paste("Cap history");
    setup.scenario.key("enter");
    let screen = setup.settle();
    assert_eq!(
        setup
            .backend
            .host()
            .catalog_session(&first)
            .and_then(|session| session.title),
        Some("Cap history".to_owned())
    );
    assert!(screen.contains("Cap history"), "{}", screen.text());

    // Delete from the picker: the first press marks, the second deletes.
    setup.scenario.key("alt+h");
    setup.settle();
    setup.scenario.paste("retry loop");
    setup.settle();
    setup.scenario.key("alt+d");
    let screen = setup.settle();
    assert!(screen.contains("again to delete"), "{}", screen.text());
    assert_eq!(
        setup.backend.host().deleted_sessions(),
        Vec::<String>::new()
    );
    setup.scenario.key("alt+d");
    let screen = setup.settle();
    assert_eq!(
        setup.backend.host().deleted_sessions(),
        vec![second.clone()]
    );
    assert!(setup.binding(&second).is_none(), "its binding is removed");
    assert!(
        setup
            .app()
            .assistant()
            .picker()
            .expect("picker")
            .rows()
            .iter()
            .all(|row| row.session_id != second),
        "{}",
        screen.text()
    );

    // Delete the active conversation after a confirmation: `n` keeps it.
    setup.scenario.key("esc");
    setup.scenario.key("alt+d");
    let screen = setup.settle();
    assert!(
        screen.contains("Delete this conversation"),
        "{}",
        screen.text()
    );
    setup.scenario.key("n");
    setup.settle();
    assert_eq!(setup.backend.host().deleted_sessions().len(), 1);
    setup.scenario.keys(&["alt+d", "y"]);
    setup.settle();
    assert_eq!(setup.backend.host().deleted_sessions(), vec![second, first]);
    assert_eq!(setup.session(), None, "an empty conversation is left");
}

#[test]
fn opening_a_conversation_in_bcode_hands_over_the_terminal() {
    let mut setup = setup(160, 40, true);
    setup.scenario.key("ctrl+k");
    let session = setup.send("Hello");
    let result = setup.scenario.key("ctrl+o");
    assert_eq!(
        result.action,
        crate::action::Action::OpenSession {
            session_id: session.parse().expect("id")
        }
    );
    // The picker's ctrl+o opens the selected row.
    setup.scenario.key("alt+h");
    setup.settle();
    let result = setup.scenario.key("ctrl+o");
    assert!(matches!(
        result.action,
        crate::action::Action::OpenSession { .. }
    ));
}

#[test]
fn viewed_marks_a_tool_wrote_survive_the_open_reviews_own_saves() {
    let mut setup = setup(160, 50, true);
    let paths: Vec<String> = {
        let review = setup.app().review().expect("review");
        review
            .document()
            .expect("document")
            .file_keys()
            .map(|file| file.path.clone())
            .collect()
    };
    assert!(paths.len() >= 2, "{paths:?}");
    // The tool marks the first file viewed while the review is open, as the plugin does.
    let services = setup.backend.services();
    let data = bcode_code_review::ReviewData::default();
    let mut tools = ReviewTools::new(&data, services.store());
    let opened = tools
        .open(
            &setup.review.repo_root,
            &ReviewRef::Workspace(setup.review.id.clone()),
        )
        .expect("saved review");
    tools
        .mark_viewed(&opened, std::slice::from_ref(&paths[0]), true)
        .expect("marked");
    // Before the review hears of it, the reviewer changes something the review saves, and
    // marks the second file viewed.
    setup.scenario.key("shift+h");
    setup.settle();
    let stored = |setup: &Setup| {
        setup
            .backend
            .call_now(bcode_code_review_models::GetReviewWorkspaceRequest {
                repo_path: setup.review.repo_root.clone(),
                workspace_id: setup.review.id.clone(),
            })
            .workspace
            .expect("stored")
            .viewed_files
    };
    assert!(
        stored(&setup).contains(&paths[0]),
        "the review's save kept the tool's mark: {:?}",
        stored(&setup)
    );
    // The review shows the tool's mark once it has saved (it merged the stored state in).
    let shown = |setup: &Setup| {
        setup
            .app()
            .review()
            .and_then(ReviewSurfaceExt::viewed_paths)
            .unwrap_or_default()
    };
    for _ in 0..4 {
        if shown(&setup).contains(&paths[0]) {
            break;
        }
        setup.settle();
    }
    assert!(shown(&setup).contains(&paths[0]), "{:?}", shown(&setup));
}

/// A mark the tool asks for after the reviewer changed the same file since the tool opened the
/// review is written and reported as what the store now holds.
#[test]
fn a_tool_mark_after_the_reviewer_unmarked_the_file_is_written_and_reported() {
    let setup = setup(160, 50, true);
    let path = setup
        .app()
        .review()
        .and_then(|review| review.document())
        .and_then(|doc| doc.file_keys().next().map(|file| file.path.clone()))
        .expect("a file");
    let services = setup.backend.services();
    let data = bcode_code_review::ReviewData::default();
    let reference = ReviewRef::Workspace(setup.review.id.clone());
    let mut reviewer = ReviewTools::new(&data, services.store());
    let opened = reviewer
        .open(&setup.review.repo_root, &reference)
        .expect("saved review");
    reviewer
        .mark_viewed(&opened, std::slice::from_ref(&path), true)
        .expect("marked");
    // The tool opens the review with the file viewed.
    let mut tool = ReviewTools::new(&data, services.store());
    let stale = tool
        .open(&setup.review.repo_root, &reference)
        .expect("saved review");
    let viewed_now = || {
        stored_review(&setup)
            .expect("saved")
            .viewed_files
            .contains(&path)
    };
    // The reviewer unmarks it before the tool marks it again.
    let unmark = |reviewer: &mut ReviewTools<'_>| {
        let opened = reviewer
            .open(&setup.review.repo_root, &reference)
            .expect("saved review");
        reviewer
            .mark_viewed(&opened, std::slice::from_ref(&path), false)
            .expect("unmarked");
        assert!(!viewed_now());
    };
    unmark(&mut reviewer);
    let selection = bcode_code_review::assistant::manage::FileSelection {
        paths: vec![path.clone()],
        ..Default::default()
    };
    let report = tool
        .mark_viewed_selection(
            &stale,
            &selection,
            true,
            bcode_code_review::viewed::ViewedReset::Diff,
        )
        .expect("marked");
    assert_eq!(report.changed, std::slice::from_ref(&path), "{report:?}");
    assert!(
        viewed_now(),
        "the tool's mark is stored: {}",
        report.message
    );
    // The same through the single-file write.
    unmark(&mut reviewer);
    let written = tool
        .mark_viewed(&stale, std::slice::from_ref(&path), true)
        .expect("marked");
    assert!(
        viewed_now(),
        "the tool's mark is stored: {}",
        written.message
    );
    assert_eq!(written.message, format!("marked {path} viewed"));
}

/// A reload that read the store before the review's own save landed does not undo that save
/// on screen: it is read again.
#[test]
fn a_reload_read_before_a_save_landed_does_not_undo_it_on_screen() {
    let mut setup = setup(160, 50, true);
    setup.settle();
    let shown = |setup: &Setup| {
        setup
            .app()
            .review()
            .and_then(ReviewSurfaceExt::viewed_paths)
            .unwrap_or_default()
    };
    assert!(shown(&setup).is_empty());
    // Returning to the app reads the stored record; that read answers late.
    setup.backend.set_delay(
        <bcode_code_review_models::GetReviewWorkspaceRequest as crate::services::Operation>::NAME,
        8,
    );
    setup.scenario.send(&bmux_tui::event::Event::Focus(
        bmux_tui::event::FocusEvent::Gained,
    ));
    // Meanwhile the reviewer marks a file viewed, and the review saves it.
    setup.scenario.key("v");
    setup.scenario.poll_only();
    let marked = shown(&setup);
    assert_eq!(marked.len(), 1, "{marked:?}");
    for _ in 0..4 {
        setup.settle();
    }
    setup.backend.set_delay(
        <bcode_code_review_models::GetReviewWorkspaceRequest as crate::services::Operation>::NAME,
        0,
    );
    assert_eq!(
        shown(&setup),
        marked,
        "the late read kept the save on screen"
    );
    assert_eq!(
        stored_review(&setup).expect("saved").viewed_files,
        marked,
        "and in the store"
    );
}

/// The saved review as the store holds it now, if it still exists.
fn stored_review(setup: &Setup) -> Option<ReviewWorkspace> {
    setup
        .backend
        .call_now(bcode_code_review_models::GetReviewWorkspaceRequest {
            repo_path: setup.review.repo_root.clone(),
            workspace_id: setup.review.id.clone(),
        })
        .workspace
}

#[test]
fn a_rename_and_archive_made_elsewhere_survive_the_open_reviews_saves_and_show() {
    let mut setup = setup(160, 50, true);
    let services = setup.backend.services();
    let data = bcode_code_review::ReviewData::default();
    let tools = ReviewTools::new(&data, services.store());
    // `code_review.update_review` from an AI card or `bcode tui`, while the review is open.
    tools
        .update_review(
            &setup.review.repo_root,
            &setup.review.id,
            &bcode_code_review::assistant::manage::ReviewChange {
                title: Some("Renamed elsewhere".to_owned()),
                archived: Some(true),
            },
        )
        .expect("renamed and archived");
    // The reviewer marks a file viewed, which the review saves at once.
    setup.scenario.key("v");
    for _ in 0..4 {
        setup.settle();
    }
    let stored = stored_review(&setup).expect("still saved");
    assert_eq!(
        stored.title, "Renamed elsewhere",
        "the save kept the rename"
    );
    assert!(stored.archived_at_ms.is_some(), "the save kept the archive");
    let review = setup.app().review().expect("review");
    assert_eq!(
        review.title(),
        "Renamed elsewhere",
        "the review shows the rename"
    );
    assert!(
        review
            .workspace()
            .is_some_and(|workspace| workspace.archived_at_ms.is_some()),
        "the review's copy is archived too, so later saves keep it"
    );
}

#[test]
fn deleting_the_open_review_elsewhere_returns_home_and_never_brings_it_back() {
    let mut setup = setup(160, 50, true);
    let services = setup.backend.services();
    let data = bcode_code_review::ReviewData::default();
    let tools = ReviewTools::new(&data, services.store());
    // `code_review.delete_review` from `bcode tui`: the app hears nothing of it.
    tools
        .delete_review(&setup.review.repo_root, &setup.review.id)
        .expect("deleted");
    // The reviewer's next save finds it gone instead of writing it again.
    setup.scenario.key("v");
    for _ in 0..4 {
        setup.settle();
    }
    assert_eq!(
        stored_review(&setup),
        None,
        "the save did not bring it back"
    );
    assert_eq!(setup.app().route_name(), RouteName::Home);
    let home = setup.app().home().expect("home");
    assert!(
        home.toasts()
            .iter()
            .any(|toast| toast.contains("Retry backoff") && toast.contains("deleted")),
        "{:?}",
        home.toasts()
    );
}

#[test]
fn returning_to_the_app_notices_the_open_review_was_deleted() {
    let mut setup = setup(160, 50, true);
    let services = setup.backend.services();
    let data = bcode_code_review::ReviewData::default();
    let tools = ReviewTools::new(&data, services.store());
    tools
        .delete_review(&setup.review.repo_root, &setup.review.id)
        .expect("deleted");
    setup.scenario.send(&bmux_tui::event::Event::Focus(
        bmux_tui::event::FocusEvent::Gained,
    ));
    for _ in 0..4 {
        setup.settle();
    }
    assert_eq!(setup.app().route_name(), RouteName::Home);
    assert_eq!(stored_review(&setup), None);
}

/// What the review shows as viewed.
trait ReviewSurfaceExt {
    fn viewed_paths(&self) -> Option<std::collections::BTreeSet<String>>;
}

impl ReviewSurfaceExt for crate::ReviewSurface {
    fn viewed_paths(&self) -> Option<std::collections::BTreeSet<String>> {
        self.workspace()
            .map(|workspace| workspace.viewed_files.clone())
    }
}

#[test]
fn home_lists_a_review_the_assistant_created_and_leaves_one_it_deleted() {
    let mut setup = setup(160, 50, false);
    setup.scenario.key("ctrl+k");
    let session = setup.send("Create a review of my last commit.");
    let services = setup.backend.services();
    let data = bcode_code_review::ReviewData::default();
    let tools = ReviewTools::new(&data, services.store());
    let created = tools
        .create_review(
            &setup.review.repo_root,
            Some("Made by the assistant".to_owned()),
            vec![(bcode_code_review_models::ReviewSourceKind::LastCommit, None)],
        )
        .expect("created");
    let screen = setup.deliver(
        &session,
        snapshot(vec![
            user("Create a review of my last commit."),
            tool("create-1", "code_review.create_review", "{}"),
            answer("Created it."),
        ]),
    );
    let listed = |setup: &Setup| {
        setup.app().home().is_some_and(|home| {
            home.row_labels()
                .iter()
                .any(|label| label.contains("Made by the assistant"))
        })
    };
    for _ in 0..6 {
        if listed(&setup) {
            break;
        }
        setup.settle();
    }
    assert!(listed(&setup), "home reloaded:\n{}", screen.text());

    // In the review, deleting it returns home.
    setup.scenario.key("esc");
    setup.settle();
    setup.open_from_home("Retry backoff");
    let review_id = setup.review.id.clone();
    tools
        .delete_review(&setup.review.repo_root, &review_id)
        .expect("deleted");
    setup.deliver(
        &session,
        snapshot(vec![
            user("Create a review of my last commit."),
            tool("create-1", "code_review.create_review", "{}"),
            answer("Created it."),
            user("Delete this review."),
            tool(
                "delete-1",
                "code_review.delete_review",
                &format!(r#"{{"review":"{review_id}"}}"#),
            ),
        ]),
    );
    assert_eq!(setup.app().route_name(), RouteName::Home);
    let _ = created;
}
