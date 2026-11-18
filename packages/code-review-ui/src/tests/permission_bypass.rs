//! Permission bypass for the review's agents: review-wide (existing and new sessions, guide
//! agents included or not), one card at a time, and the confirmation once per launch.

use super::interaction::{Setup, added_line, find, fixture, move_to, setup};
use super::stable;
use bcode_session_models::TurnPermissionMode;

/// Run the palette command whose description starts with `description`.
fn palette(setup: &mut Setup, description: &str) {
    setup.scenario.key(":");
    stable(&mut setup.scenario);
    setup.scenario.paste(description);
    stable(&mut setup.scenario);
    setup.scenario.key("enter");
    stable(&mut setup.scenario);
}

/// Ask a question at the `nth` added line; returns the session it created.
fn ask(setup: &mut Setup, nth: usize, question: &str) -> String {
    let mut index = find(&setup.scenario, 0, added_line);
    for _ in 0..nth {
        index = find(&setup.scenario, index + 1, added_line);
    }
    move_to(&mut setup.scenario, index);
    setup.scenario.key("a");
    setup.scenario.paste(question);
    setup.scenario.key("enter");
    stable(&mut setup.scenario);
    setup
        .backend
        .sessions()
        .last()
        .expect("a session")
        .0
        .clone()
}

const REVIEW: &str = "Permission bypass: step this review's agents in or out";

#[test]
fn review_wide_bypass_steps_existing_and_new_agents_in_after_one_confirmation() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 120, 40);
    let first = ask(&mut setup, 0, "Why this change?");
    assert_eq!(
        setup.backend.permission_mode(&first),
        TurnPermissionMode::Enforce
    );

    // The first step in of the launch only says what it does.
    palette(&mut setup, REVIEW);
    assert_eq!(
        setup.backend.permission_mode(&first),
        TurnPermissionMode::Enforce
    );
    assert!(!setup.scenario.surface().review_bypass());
    let screen = setup.scenario.render();
    assert!(screen.contains("not on yet"), "{}", screen.text());

    // Again: the review's existing agents step in, and the header says so.
    palette(&mut setup, REVIEW);
    assert_eq!(
        setup.backend.permission_mode(&first),
        TurnPermissionMode::Bypass
    );
    assert!(setup.scenario.surface().review_bypass());
    let screen = setup.scenario.render();
    assert!(
        screen.contains("agents bypass permissions"),
        "{}",
        screen.text()
    );

    // A question asked now starts in bypass.
    let second = ask(&mut setup, 1, "And this one?");
    assert_ne!(first, second);
    assert_eq!(
        setup.backend.permission_mode(&second),
        TurnPermissionMode::Bypass
    );

    // Leaving guide agents out keeps questions in bypass.
    palette(&mut setup, "Permission bypass: include guide agents");
    assert_eq!(
        setup.backend.permission_mode(&first),
        TurnPermissionMode::Bypass
    );

    // Stepping out needs no confirmation and reaches every agent.
    palette(&mut setup, REVIEW);
    assert!(!setup.scenario.surface().review_bypass());
    for session in [&first, &second] {
        assert_eq!(
            setup.backend.permission_mode(session),
            TurnPermissionMode::Enforce
        );
    }
}

#[test]
fn one_card_steps_in_and_out_on_its_own() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 120, 40);
    let first = ask(&mut setup, 0, "Why this change?");
    let second = ask(&mut setup, 1, "And this one?");
    // The cursor is on the second card.
    palette(&mut setup, "Permission bypass: step the AI card");
    palette(&mut setup, "Permission bypass: step the AI card");
    assert_eq!(
        setup.backend.permission_mode(&second),
        TurnPermissionMode::Bypass
    );
    assert_eq!(
        setup.backend.permission_mode(&first),
        TurnPermissionMode::Enforce
    );
    assert!(!setup.scenario.surface().review_bypass());

    // The card says so once its session reports bypass (from here or any other client).
    let mut snapshot = bcode_session_view_models::SessionViewSnapshot::empty();
    snapshot.runtime.permission_mode = Some(TurnPermissionMode::Bypass);
    setup
        .scenario
        .host()
        .session_view(&second)
        .expect("subscribed")
        .try_send(bcode_session_view::follow::SessionViewFollowUpdate::Snapshot(Box::new(snapshot)))
        .unwrap();
    let screen = stable(&mut setup.scenario);
    assert!(
        screen.contains("bypass: tools run without asking"),
        "{}",
        screen.text()
    );

    // The same action on a card in bypass steps it out, without asking.
    palette(&mut setup, "Permission bypass: step the AI card");
    assert_eq!(
        setup.backend.permission_mode(&second),
        TurnPermissionMode::Enforce
    );
}

#[test]
fn a_bypass_launch_steps_the_reviews_agents_in_when_it_opens_and_new_ones_without_asking() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 120, 40);
    let first = ask(&mut setup, 0, "Why this change?");
    // As `bcode review --yolo` starts the app: review-wide bypass on, confirmed by the flag.
    let _ = setup
        .backend
        .services()
        .with_launch(crate::ReviewLaunch::with_bypass(true));
    assert!(setup.scenario.surface().review_bypass());
    let second = ask(&mut setup, 1, "And this one?");
    assert_eq!(
        setup.backend.permission_mode(&second),
        TurnPermissionMode::Bypass
    );
    // A review opened in this launch steps its existing agents in.
    let mut reopened = super::interaction::setup_with_backend(&fixture, &setup, 120, 40);
    stable(&mut reopened.scenario);
    assert_eq!(
        setup.backend.permission_mode(&first),
        TurnPermissionMode::Bypass
    );
    let screen = reopened.scenario.render();
    assert!(
        screen.contains("agents bypass permissions"),
        "{}",
        screen.text()
    );
}

/// Click the first place `needle` is painted.
fn click(setup: &mut Setup, needle: &str) {
    let screen = setup.scenario.render();
    let at = screen
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} is not painted:\n{}", screen.text()));
    setup.scenario.click(at.x, at.y);
    stable(&mut setup.scenario);
}

/// Report `session` in bypass, as its followed session view would.
fn report_bypass(setup: &mut Setup, session: &str) {
    let mut snapshot = bcode_session_view_models::SessionViewSnapshot::empty();
    snapshot.runtime.permission_mode = Some(TurnPermissionMode::Bypass);
    setup
        .scenario
        .host()
        .session_view(session)
        .expect("subscribed")
        .try_send(bcode_session_view::follow::SessionViewFollowUpdate::Snapshot(Box::new(snapshot)))
        .unwrap();
    stable(&mut setup.scenario);
}

#[test]
fn a_new_questions_agent_starts_in_bypass_from_the_composer_chip() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 140, 40);
    let index = find(&setup.scenario, 0, added_line);
    move_to(&mut setup.scenario, index);
    setup.scenario.key("a");
    stable(&mut setup.scenario);
    // Asked first, then on for this question.
    click(&mut setup, "◇ asks first");
    click(&mut setup, "◇ asks first");
    let screen = setup.scenario.render();
    assert!(screen.contains("⚠ bypass"), "{}", screen.text());
    setup
        .scenario
        .paste("Go fix this on the right pull request and restack");
    setup.scenario.key("enter");
    stable(&mut setup.scenario);
    let session = setup
        .backend
        .sessions()
        .last()
        .expect("a session")
        .0
        .clone();
    assert_eq!(
        setup.backend.permission_mode(&session),
        TurnPermissionMode::Bypass
    );
    assert!(
        !setup.scenario.surface().review_bypass(),
        "only this question"
    );
}

#[test]
fn a_follow_up_flips_its_cards_session_from_the_composer_chip() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 140, 40);
    let session = ask(&mut setup, 0, "Why this change?");
    // `Follow up` on the card opens the composer for the same session.
    click(&mut setup, "Follow up");
    click(&mut setup, "◇ asks first");
    click(&mut setup, "◇ asks first");
    assert_eq!(
        setup.backend.permission_mode(&session),
        TurnPermissionMode::Bypass
    );
    // The chip shows the session's own mode once it reports it.
    report_bypass(&mut setup, &session);
    let screen = setup.scenario.render();
    assert!(screen.contains("⚠ bypass"), "{}", screen.text());
    click(&mut setup, "⚠ bypass");
    assert_eq!(
        setup.backend.permission_mode(&session),
        TurnPermissionMode::Enforce
    );
}

#[test]
fn the_cards_button_steps_its_session_in_and_out() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 160, 40);
    let session = ask(&mut setup, 0, "Why this change?");
    click(&mut setup, "Bypass ·");
    assert_eq!(
        setup.backend.permission_mode(&session),
        TurnPermissionMode::Enforce,
        "asked first"
    );
    click(&mut setup, "Bypass ·");
    assert_eq!(
        setup.backend.permission_mode(&session),
        TurnPermissionMode::Bypass
    );
    let screen = setup.scenario.render();
    assert!(!screen.contains("not on yet"), "settled: {}", screen.text());
    report_bypass(&mut setup, &session);
    click(&mut setup, "⚠ Stop bypass");
    assert_eq!(
        setup.backend.permission_mode(&session),
        TurnPermissionMode::Enforce
    );
}
