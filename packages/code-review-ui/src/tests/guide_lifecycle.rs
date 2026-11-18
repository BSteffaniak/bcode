//! A guide after new pushes on the review screen: stale banners and rows, refreshing a section
//! forks the next version (the older one stays readable read-only), and the versions picker.

use super::guide::{entry_of, guided, quote_citation};
use super::stable;
use crate::assistant::LinkRequest;
use crate::surface::Modal;
use bcode_code_review_models::{GrantMode, GuideGrant};

/// Edit the cited changed line in the fixture and commit it, then refresh the review.
fn push_edit_of_cited_line(
    setup: &mut super::interaction::Setup,
    cited: &super::guide::Cited,
    fixture: &crate::testing::fixture::GitFixture,
) {
    let (file, line) = &cited.changed;
    let path = fixture.root().join(&file.path);
    let text = std::fs::read_to_string(&path).expect("file");
    let edited: Vec<String> = text
        .lines()
        .enumerate()
        .map(|(index, row)| {
            if index + 1 == usize::try_from(*line).expect("line") {
                format!("edited after the guide {row}")
            } else {
                row.to_owned()
            }
        })
        .collect();
    std::fs::write(&path, edited.join("\n") + "\n").expect("write");
    fixture.run(&["commit", "-qam", "edit the cited line"]);
    setup.scenario.key("r");
    stable(&mut setup.scenario);
}

#[test]
fn refreshing_a_stale_section_forks_the_next_version_and_keeps_the_old_one_readable() {
    let (mut setup, cited, fixture) = guided(160, 40);
    for id in ["c1", "c2", "c3"] {
        quote_citation(&mut setup, &fixture, id);
    }
    push_edit_of_cited_line(&mut setup, &cited, &fixture);
    let guide = setup.scenario.surface().guide().expect("guide");
    assert!(guide.stale.contains("s2.1"), "{:?}", guide.stale);
    assert!(
        !guide.stale.contains("s2.2"),
        "a section whose lines did not change stays fresh: {:?}",
        guide.stale
    );

    // The panel says what is stale and how to refresh it; the stale row says so.
    setup.scenario.key("alt+g");
    let screen = stable(&mut setup.scenario);
    assert!(screen.contains("stale since"), "{}", screen.text());
    assert!(screen.contains("r refresh"), "{}", screen.text());
    assert!(screen.contains("⚠ stale"), "{}", screen.text());

    // Refresh the stale section: a new version is forked and the assistant asked to rewrite it.
    let target = entry_of(&setup.scenario, "s2.1");
    while setup.scenario.surface().guide().expect("guide").selected != target {
        setup.scenario.key("j");
    }
    setup.scenario.key("r");
    stable(&mut setup.scenario);
    let guide = setup.scenario.surface().guide().expect("guide");
    assert_eq!(guide.guide.current_version, 2);
    assert_eq!(guide.version.version, 2, "the new version shows");
    assert_eq!(guide.version.based_on, Some(1));
    assert!(!guide.is_old_version());
    assert_eq!(
        setup.scenario.surface().guide_grant(),
        Some(&GuideGrant {
            guide_id: "guide-1".to_owned(),
            version: Some(2),
            mode: GrantMode::Refresh {
                section_ids: vec!["s2.1".to_owned()],
            },
        })
    );
    let copied = guide.version.citations["c2"].revision.clone();
    let asked = setup.scenario.surface_mut().take_assistant_requests();
    assert!(
        asked.iter().any(|request| matches!(
            request,
            LinkRequest::Message { text, send: true }
                if text.contains("Refresh §2.1") && text.contains("version 2")
        )),
        "{asked:?}"
    );
    // The fresh section was copied with its citation remapped to the new head.
    let head = setup
        .scenario
        .surface()
        .document()
        .and_then(|doc| doc.groups().next().map(|group| group.source.head.clone()))
        .expect("head");
    assert_eq!(copied, head);

    // A second refresh while v2 is pinned to the code now edits v2 in place.
    setup.scenario.key("alt+r");
    stable(&mut setup.scenario);
    let guide = setup.scenario.surface().guide().expect("guide");
    assert_eq!(guide.guide.current_version, 2, "no third version");

    // v1 stays readable from the versions picker, read-only and pinned to its code.
    setup.scenario.key("shift+v");
    stable(&mut setup.scenario);
    assert!(matches!(
        setup.scenario.surface().modal(),
        Some(Modal::GuideVersions(_))
    ));
    setup.scenario.key("down");
    setup.scenario.key("enter");
    let screen = stable(&mut setup.scenario);
    let guide = setup.scenario.surface().guide().expect("guide");
    assert_eq!(guide.version.version, 1);
    assert!(guide.is_old_version());
    assert!(screen.contains("read-only"), "{}", screen.text());
    setup.scenario.key("r");
    let screen = stable(&mut setup.scenario);
    assert!(screen.contains("older version"), "{}", screen.text());
    assert_eq!(
        setup
            .scenario
            .surface()
            .guide()
            .expect("guide")
            .guide
            .current_version,
        2,
        "an older version is never refreshed"
    );
}
