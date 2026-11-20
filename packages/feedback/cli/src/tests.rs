use bcode_feedback_models::FeedbackId;
use clap::Parser as _;

use crate::{FeedbackCli, FeedbackCommand};

#[test]
fn submit_parses_facets_text_and_modes() {
    let cli = FeedbackCli::try_parse_from([
        "feedback",
        "submit",
        "--app",
        "bmux",
        "--facet",
        "bmux.pane=3",
        "--sensitive-facet",
        "token=abc=def",
        "--wait",
        "split",
        "flickers",
    ])
    .unwrap();
    let FeedbackCommand::Submit(args) = cli.command else {
        panic!("submit");
    };
    assert_eq!(args.app.as_deref(), Some("bmux"));
    assert_eq!(args.facets, vec![("bmux.pane".into(), "3".into())]);
    assert_eq!(
        args.sensitive_facets,
        vec![("token".into(), "abc=def".into())]
    );
    assert_eq!(args.wait, Some(600));
    assert_eq!(args.text.join(" "), "split flickers");

    assert!(
        FeedbackCli::try_parse_from(["feedback", "submit", "--no-triage", "--wait", "--app", "x"])
            .is_err(),
        "recording without triage cannot wait for a reply"
    );
    assert!(
        FeedbackCli::try_parse_from(["feedback", "submit", "--request-json", "--app", "x"])
            .is_err(),
        "a JSON request carries its own app"
    );
    assert!(FeedbackCli::try_parse_from(["feedback", "submit", "--request-json"]).is_ok());
}

#[test]
fn ids_parse_in_either_form() {
    let cli = FeedbackCli::try_parse_from(["feedback", "verify", "12", "--note", "works"]).unwrap();
    let FeedbackCommand::Verify(args) = cli.command else {
        panic!("verify");
    };
    assert_eq!(args.id, FeedbackId(12));
    assert!(FeedbackCli::try_parse_from(["feedback", "show", "fb-x"]).is_err());
}
