//! Completed invalid responses are corrected, not silently accepted or replayed.
use super::*;
use crate::detailed::Operation;

#[derive(Clone, Copy)]
enum Failure {
    Empty,
    Stale,
    Foreign,
    Unchanged,
    Provider,
}

struct Correcting {
    inner: Scripted,
    failure: Failure,
    always_invalid: bool,
    fail_correction: bool,
    requests: Mutex<Vec<AnalysisRequest>>,
}
impl Correcting {
    fn new(failure: Failure) -> Self {
        Self {
            inner: Scripted {
                stages: Mutex::new(vec![]),
                cycle: false,
            },
            failure,
            always_invalid: false,
            fail_correction: false,
            requests: Mutex::new(vec![]),
        }
    }
}
impl ModelExecutor for Correcting {
    fn execute<'a>(&'a self, _: ModelRequest, _: &'a Cancellation) -> ModelFuture<'a> {
        Box::pin(async { Err(ModelError::Unavailable) })
    }
    fn execute_analysis<'a>(
        &'a self,
        request: AnalysisRequest,
        cancel: &'a Cancellation,
    ) -> AnalysisFuture<'a> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request.clone());
            if request.stage != AnalysisStage::PassagePatch {
                return self.inner.execute_analysis(request, cancel).await;
            }
            let corrected = request.input.get("patch_correction").is_some();
            if corrected && self.fail_correction {
                return Err(ModelError::Cancelled);
            }
            let mut original = request.clone();
            original.instructions =
                format!("{}\n{}", super::super::PASSAGE_POLICY, super::super::PATCH);
            let AnalysisResponse::PassagePatch(mut patch) =
                self.inner.execute_analysis(original, cancel).await?
            else {
                unreachable!()
            };
            if !corrected || self.always_invalid {
                match self.failure {
                    Failure::Empty => patch.operations.clear(),
                    Failure::Stale => patch.base_revision += 1,
                    Failure::Foreign => patch.operations[0].target.block_id = "not-a-block".into(),
                    Failure::Unchanged => {
                        let passage: Passage =
                            serde_json::from_value(request.input["passage"].clone()).unwrap();
                        patch.operations[0].blocks = passage.blocks;
                    }
                    Failure::Provider => return Err(ModelError::ExecutionFailed),
                }
            }
            Ok(AnalysisResponse::PassagePatch(patch))
        })
    }
}

#[tokio::test]
async fn invalid_completed_patch_gets_distinct_bounded_feedback_and_full_rereview() {
    for failure in [
        Failure::Empty,
        Failure::Stale,
        Failure::Foreign,
        Failure::Unchanged,
    ] {
        let (document, _, _) = fixture();
        let executor = Correcting::new(failure);
        let policy = DetailedPolicy {
            inspect_visuals: false,
            ..DetailedPolicy::default()
        };
        let result = compose_passages(
            &executor,
            &document,
            vec![],
            document.intent.clone(),
            None,
            &policy,
            &Cancellation::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            result.sections[0].blocks[0].content,
            BlockContent::Text {
                text: "Hello, with example".into()
            }
        );
        let requests = executor.requests.lock().unwrap();
        let patches: Vec<_> = requests
            .iter()
            .filter(|r| r.stage == AnalysisStage::PassagePatch)
            .collect();
        assert_eq!(patches.len(), 2);
        for field in ["source_unit", "passage", "findings"] {
            assert_eq!(patches[0].input[field], patches[1].input[field]);
        }
        assert!(patches[0].input.get("patch_correction").is_none());
        assert_eq!(patches[1].input["patch_correction"]["attempt"], 1);
        assert!(
            !patches[1].input["patch_correction"]["rejection"]
                .as_str()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            requests
                .iter()
                .rev()
                .take(2)
                .map(|r| r.stage)
                .collect::<Vec<_>>(),
            vec![AnalysisStage::PassageReview; 2]
        );
    }
}

#[tokio::test]
async fn repeated_invalid_patches_exhaust_without_accepting_and_provider_errors_never_retry() {
    let (document, unit, passage) = fixture();
    let findings = vec![finding(target(&passage.blocks[0].id))];
    for allowance in [0, 2] {
        let mut executor = Correcting::new(Failure::Empty);
        executor.always_invalid = true;
        let mut policy = DetailedPolicy::default();
        policy.allowances.max_repair_rounds = allowance;
        let cancel = Cancellation::default();
        let mut operation = Operation::new(&executor, &policy, &cancel);
        let error = super::super::repair(
            &mut operation,
            &document,
            &unit,
            &passage,
            &findings,
            &[],
            &policy,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            crate::composition::CompositionError::Invalid(
                "passage patch correction attempts exhausted; unresolved findings remain"
            )
        ));
        assert_eq!(executor.requests.lock().unwrap().len(), allowance + 1);
    }
    let executor = Correcting::new(Failure::Provider);
    let policy = DetailedPolicy::default();
    let cancel = Cancellation::default();
    let mut operation = Operation::new(&executor, &policy, &cancel);
    assert!(matches!(
        super::super::repair(
            &mut operation,
            &document,
            &unit,
            &passage,
            &findings,
            &[],
            &policy
        )
        .await,
        Err(crate::composition::CompositionError::Model(
            ModelError::ExecutionFailed
        ))
    ));
    assert_eq!(executor.requests.lock().unwrap().len(), 1);
}

fn journal(root: &std::path::Path) -> crate::generation::Journal {
    crate::generation::Journal::open(
        root,
        &crate::engine::GenerateRequest {
            id: "patch-recovery".into(),
            source_id: "source".into(),
            source_title: "Source".into(),
            input: crate::source::SourceInput::Youtube(
                "https://www.youtube.com/watch?v=lXUZvyajciY".into(),
            ),
            transcript: None,
            intent: crate::contracts::GenerationIntent::default(),
        },
        Some("patch-test-v1".into()),
        "policy".into(),
        false,
    )
    .unwrap()
}

#[tokio::test]
async fn resume_keeps_old_empty_checkpoint_and_reuses_every_completed_correction() {
    let directory = tempfile::tempdir().unwrap();
    let (document, unit, passage) = fixture();
    let findings = vec![finding(target(&passage.blocks[0].id))];
    let policy = DetailedPolicy::default();
    let cancel = Cancellation::default();
    // Seed the exact pre-fix request through the real checkpoint executor.
    let stored = journal(directory.path());
    let first = Correcting::new(Failure::Empty);
    let executor = crate::generation::Executor {
        inner: &first,
        journal: &stored,
    };
    let mut operation = Operation::new(&executor, &policy, &cancel);
    let response = super::super::execute(
        &mut operation,
        AnalysisStage::PassagePatch,
        serde_json::json!({"source_unit":unit,"passage":passage,"findings":findings}),
        vec![],
        super::super::PATCH,
        &policy,
    )
    .await
    .unwrap();
    assert!(
        matches!(response, AnalysisResponse::PassagePatch(ref patch) if patch.operations.is_empty())
    );
    drop(stored);

    let stored = journal(directory.path());
    let second = Correcting::new(Failure::Empty);
    let executor = crate::generation::Executor {
        inner: &second,
        journal: &stored,
    };
    let mut operation = Operation::new(&executor, &policy, &cancel);
    let candidate = super::super::repair(
        &mut operation,
        &document,
        &unit,
        &passage,
        &findings,
        &[],
        &policy,
    )
    .await
    .unwrap();
    assert_eq!(candidate.revision, passage.revision + 1);
    assert_eq!(
        second.requests.lock().unwrap().len(),
        1,
        "only the correction may invoke the model"
    );
    drop(stored);

    let stored = journal(directory.path());
    let third = Correcting::new(Failure::Provider);
    let executor = crate::generation::Executor {
        inner: &third,
        journal: &stored,
    };
    let mut operation = Operation::new(&executor, &policy, &cancel);
    let resumed = super::super::repair(
        &mut operation,
        &document,
        &unit,
        &passage,
        &findings,
        &[],
        &policy,
    )
    .await
    .unwrap();
    assert_eq!(candidate, resumed);
    assert!(third.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn completed_generation_replays_no_model_calls_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let (document, _, _) = fixture();
    let policy = DetailedPolicy {
        inspect_visuals: false,
        ..DetailedPolicy::default()
    };
    let cancel = Cancellation::default();
    let stored = journal(directory.path());
    let first = Correcting::new(Failure::Empty);
    let executor = crate::generation::Executor {
        inner: &first,
        journal: &stored,
    };
    let accepted = compose_passages(
        &executor,
        &document,
        vec![],
        document.intent.clone(),
        None,
        &policy,
        &cancel,
    )
    .await
    .unwrap();
    // The identical final clean review also reuses its completed local review.
    assert_eq!(first.requests.lock().unwrap().len(), 5);
    drop(stored);
    let stored = journal(directory.path());
    let second = Correcting::new(Failure::Provider);
    let executor = crate::generation::Executor {
        inner: &second,
        journal: &stored,
    };
    let resumed = compose_passages(
        &executor,
        &document,
        vec![],
        document.intent.clone(),
        None,
        &policy,
        &cancel,
    )
    .await
    .unwrap();
    assert_eq!(accepted, resumed);
    assert!(second.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn interrupted_correction_requires_explicit_uncertain_call_recovery() {
    let directory = tempfile::tempdir().unwrap();
    let (document, unit, passage) = fixture();
    let findings = vec![finding(target(&passage.blocks[0].id))];
    let policy = DetailedPolicy::default();
    let cancel = Cancellation::default();
    let stored = journal(directory.path());
    let mut first = Correcting::new(Failure::Empty);
    first.fail_correction = true;
    let executor = crate::generation::Executor {
        inner: &first,
        journal: &stored,
    };
    let mut operation = Operation::new(&executor, &policy, &cancel);
    assert!(matches!(
        super::super::repair(
            &mut operation,
            &document,
            &unit,
            &passage,
            &findings,
            &[],
            &policy
        )
        .await,
        Err(crate::composition::CompositionError::Model(
            ModelError::Cancelled
        ))
    ));
    drop(stored);
    let stored = journal(directory.path());
    let second = Correcting::new(Failure::Empty);
    let executor = crate::generation::Executor {
        inner: &second,
        journal: &stored,
    };
    let mut operation = Operation::new(&executor, &policy, &cancel);
    assert!(matches!(
        super::super::repair(
            &mut operation,
            &document,
            &unit,
            &passage,
            &findings,
            &[],
            &policy
        )
        .await,
        Err(crate::composition::CompositionError::Model(
            ModelError::RecoveryRequired
        ))
    ));
    assert!(second.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cancellation_and_explicit_call_limits_stop_corrections() {
    let (document, unit, passage) = fixture();
    let findings = vec![finding(target(&passage.blocks[0].id))];
    let mut executor = Correcting::new(Failure::Empty);
    executor.fail_correction = true;
    let policy = DetailedPolicy::default();
    let cancel = Cancellation::default();
    let mut operation = Operation::new(&executor, &policy, &cancel);
    assert!(matches!(
        super::super::repair(
            &mut operation,
            &document,
            &unit,
            &passage,
            &findings,
            &[],
            &policy
        )
        .await,
        Err(crate::composition::CompositionError::Model(
            ModelError::Cancelled
        ))
    ));
    let executor = Correcting::new(Failure::Empty);
    let mut policy = DetailedPolicy::default();
    policy.allowances.max_model_calls = 1;
    let mut operation = Operation::new(&executor, &policy, &cancel);
    assert!(
        super::super::repair(
            &mut operation,
            &document,
            &unit,
            &passage,
            &findings,
            &[],
            &policy
        )
        .await
        .is_err()
    );
    assert_eq!(executor.requests.lock().unwrap().len(), 1);
}
