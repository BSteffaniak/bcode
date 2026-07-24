#![cfg(feature = "testing")]

use bcode::workflow::{Step, WorkflowBuilder, WorkflowCancellation, agent, field, parallel_named};

use bcode::{
    ProviderError, ProviderErrorCategory, ProviderTurnEvent, StopReason,
    testing::ScriptedProviderTurn,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
struct Task {
    request: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct ChangeSet {
    revision: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct Review {
    approved: bool,
    findings: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
struct ReviewPair {
    correctness: Review,
    security: Review,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct Verdict {
    needs_fixes: bool,
    revision: String,
}

fn scripted<T: Serialize>(value: &T) -> bcode::testing::ScriptedProvider {
    bcode::testing::ScriptedProvider::new([ScriptedProviderTurn::new().events([
        ProviderTurnEvent::TextDelta {
            text: serde_json::to_string(value).unwrap(),
        },
        ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::EndTurn,
        },
    ])])
}

fn provider_error(code: &str) -> ProviderError {
    ProviderError {
        code: code.to_string(),
        category: ProviderErrorCategory::ProviderInternal,
        message: code.to_string(),
        retryable: false,
        provider_message: None,
        failure: None,
        request_id: None,
        diagnostic_context: Box::default(),
        sources: Box::default(),
        retry: None,
    }
}

#[tokio::test]
async fn parallel_reviewer_failure_stops_before_adjudication() {
    let adjudications = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&adjudications);
    let successful = agent::<ChangeSet, Review, _, _>("successful-review", || {
        scripted(&Review {
            approved: true,
            findings: Vec::new(),
        })
    })
    .read_only()
    .build();
    let failing = agent::<ChangeSet, Review, _, _>("failing-review", || {
        bcode::testing::ScriptedProvider::new([ScriptedProviderTurn::provider_error(
            provider_error("review_failed"),
        )])
    })
    .read_only()
    .build();
    let adjudicate = Step::map("adjudicate", move |_reviews: (Review, Review)| {
        observed.fetch_add(1, Ordering::SeqCst);
        Ok(Verdict {
            needs_fixes: false,
            revision: "unexpected".to_string(),
        })
    });
    let workflow = WorkflowBuilder::new(
        "review-failure",
        parallel_named("reviews", successful, failing).then(adjudicate),
    )
    .build()
    .expect("workflow builds");

    let error = workflow
        .run(ChangeSet {
            revision: "revision-1".to_string(),
        })
        .await
        .expect_err("review fails");
    assert!(error.to_string().contains("review_failed"));
    assert_eq!(adjudications.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn disagreement_is_delivered_to_the_adjudicator_without_collapsing_reviews() {
    let approved = agent::<ChangeSet, Review, _, _>("approved-review", || {
        scripted(&Review {
            approved: true,
            findings: Vec::new(),
        })
    })
    .read_only()
    .build();
    let rejected = agent::<ChangeSet, Review, _, _>("rejected-review", || {
        scripted(&Review {
            approved: false,
            findings: vec!["blocking issue".to_string()],
        })
    })
    .read_only()
    .build();
    let adjudicate = Step::map(
        "adjudicate-disagreement",
        |(left, right): (Review, Review)| {
            assert!(left.approved);
            assert!(!right.approved);
            assert_eq!(right.findings, ["blocking issue"]);
            Ok(Verdict {
                needs_fixes: true,
                revision: "revision-1".to_string(),
            })
        },
    );
    let workflow = WorkflowBuilder::new(
        "review-disagreement",
        parallel_named("disagreeing-reviews", approved, rejected).then(adjudicate),
    )
    .build()
    .expect("workflow builds");

    let verdict = workflow
        .run(ChangeSet {
            revision: "revision-1".to_string(),
        })
        .await
        .expect("adjudicator resolves disagreement");
    assert!(verdict.needs_fixes);
}

#[tokio::test]
async fn cancellation_before_reference_work_prevents_implementation() {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let implement = Step::map("implement", move |_task: Task| {
        observed.fetch_add(1, Ordering::SeqCst);
        Ok(ChangeSet {
            revision: "unexpected".to_string(),
        })
    });
    let workflow = WorkflowBuilder::new("cancel-reference", implement)
        .build()
        .expect("workflow builds");
    let cancellation = WorkflowCancellation::new();
    cancellation.cancel();

    workflow
        .run_with_cancellation(
            Task {
                request: "cancel".to_string(),
            },
            cancellation,
        )
        .await
        .expect_err("cancelled");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn reference_cycle_reports_exhaustion() {
    let cycle = Step::map("fix", |mut verdict: Verdict| {
        verdict.revision.push_str("-fix");
        Ok(verdict)
    })
    .repeat_while("fix-cycle", field::<Verdict>("needs_fixes").eq(true), 2);
    let workflow = WorkflowBuilder::new("exhausted-cycle", cycle)
        .build()
        .expect("workflow builds");

    let error = workflow
        .run(Verdict {
            needs_fixes: true,
            revision: "revision-1".to_string(),
        })
        .await
        .expect_err("cycle exhausts");
    assert!(error.to_string().contains("2 iterations"));
}

#[tokio::test]
async fn bun_style_implement_parallel_review_adjudicate_and_fix_flow() {
    let implement = agent::<Task, ChangeSet, _, _>("implement", || {
        scripted(&ChangeSet {
            revision: "revision-1".to_string(),
        })
    })
    .agent_id("build")
    .build();
    let review = |name: &'static str| {
        agent::<ChangeSet, Review, _, _>(name, || {
            scripted(&Review {
                approved: false,
                findings: vec!["address edge case".to_string()],
            })
        })
        .agent_id("plan")
        .read_only()
        .build()
    };
    let pair = Step::map("pair-reviews", |(correctness, security)| {
        Ok(ReviewPair {
            correctness,
            security,
        })
    });
    let adjudicate = agent::<ReviewPair, Verdict, _, _>("adjudicate", || {
        scripted(&Verdict {
            needs_fixes: true,
            revision: "revision-1".to_string(),
        })
    })
    .agent_id("plan")
    .read_only()
    .build();
    let fix_attempts = Arc::new(AtomicUsize::new(0));
    let attempts = Arc::clone(&fix_attempts);
    let fix = Step::map("fix", move |mut verdict: Verdict| {
        let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
        verdict.revision = format!("revision-{}", attempt + 1);
        verdict.needs_fixes = false;
        Ok(verdict)
    })
    .repeat_while(
        "fix-review-cycle",
        field::<Verdict>("needs_fixes").eq(true),
        3,
    );
    let finish = Step::map("finish-clean", |verdict: Verdict| Ok(verdict));

    let workflow = WorkflowBuilder::new(
        "bun-style-review",
        implement
            .then(parallel_named(
                "parallel-reviews",
                review("correctness-review"),
                review("security-review"),
            ))
            .then(pair)
            .then(adjudicate)
            .branch(
                "needs-fixes?",
                field::<Verdict>("needs_fixes").eq(true),
                fix,
                finish,
            ),
    )
    .build()
    .expect("workflow builds");

    let verdict = workflow
        .run(Task {
            request: "Implement the change".to_string(),
        })
        .await
        .expect("workflow runs");
    assert!(!verdict.needs_fixes);
    assert_eq!(verdict.revision, "revision-2");
    assert_eq!(fix_attempts.load(Ordering::SeqCst), 1);
    assert!(workflow.definition().node("parallel-reviews").is_some());
    assert!(workflow.definition().node("fix-review-cycle").is_some());
}
