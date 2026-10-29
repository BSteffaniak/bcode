//! Additional public-pipeline negative cases; no test-local patch implementation.
use super::{
    AnalysisStage, BlockContent, Cancellation, CompositionError, ModelError, Passage,
    PassageFinding, PassageFindingKind, Scenario, Scripted, compose, compose_passages, passage,
    policy, source,
};

#[tokio::test]
async fn source_backed_omission_and_unsupported_findings_drive_real_targeted_repairs() {
    let corpus = passage::corpus();
    let case = corpus
        .cases
        .iter()
        .find(|case| case.id == "causal_caveats")
        .unwrap();
    for (scenario, expected_kind) in [
        (Scenario::Omission, PassageFindingKind::Omission),
        (Scenario::Unsupported, PassageFindingKind::Unsupported),
    ] {
        let source = source(case, true);
        let before = source.clone();
        let model = Scripted::new(scenario, case);
        let result = compose(&model, &source, &policy(180_000)).await.unwrap();
        assert_eq!(source, before);
        let requests = model.requests.lock().unwrap().clone();
        let patch = requests
            .iter()
            .find(|request| request.stage == AnalysisStage::PassagePatch)
            .unwrap();
        let findings: Vec<PassageFinding> =
            serde_json::from_value(patch.input["findings"].clone()).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, expected_kind);
        let base: Passage = serde_json::from_value(patch.input["passage"].clone()).unwrap();
        assert_eq!(result.sections[0].blocks[0].id, base.blocks[0].id);
        assert_eq!(
            result.sections[0].blocks[0].content,
            BlockContent::Text {
                text: case.acceptable.clone()
            }
        );
        assert_eq!(result.sections[0].blocks[1..3], base.blocks[1..3]);
        assert!(requests.iter().all(|request| matches!(
            request.stage,
            AnalysisStage::PassageComposition
                | AnalysisStage::PassageReview
                | AnalysisStage::PassagePatch
        )));
    }
}

#[tokio::test]
async fn malformed_findings_fail_before_patch_execution_without_mutation() {
    let corpus = passage::corpus();
    let case = corpus
        .cases
        .iter()
        .find(|case| case.id == "causal_caveats")
        .unwrap();
    for (scenario, expected) in [
        (
            Scenario::ForeignEvidence,
            "foreign passage finding evidence",
        ),
        (Scenario::UnknownTarget, "unknown passage block target"),
        (
            Scenario::OutsideRange,
            "passage finding outside primary source",
        ),
    ] {
        let source = source(case, true);
        let before = source.clone();
        let model = Scripted::new(scenario, case);
        let error = compose(&model, &source, &policy(180_000))
            .await
            .unwrap_err();
        assert!(matches!(error, CompositionError::Invalid(message) if message == expected));
        assert_eq!(source, before);
        assert!(
            !model
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.stage == AnalysisStage::PassagePatch)
        );
    }
}

#[tokio::test]
async fn cancellation_at_each_passage_stage_blocks_followup_and_success() {
    let corpus = passage::corpus();
    let case = corpus
        .cases
        .iter()
        .find(|case| case.id == "causal_caveats")
        .unwrap();
    for stage in [
        AnalysisStage::PassageComposition,
        AnalysisStage::PassageReview,
        AnalysisStage::PassagePatch,
    ] {
        let source = source(case, true);
        let before = source.clone();
        let mut model = Scripted::new(Scenario::Repair, case);
        model.cancel_stage = Some(stage);
        let cancel = Cancellation::default();
        let result = compose_passages(
            &model,
            &source,
            vec![],
            source.intent.clone(),
            None,
            &policy(180_000),
            &cancel,
        )
        .await;
        assert!(matches!(
            result,
            Err(CompositionError::Model(ModelError::Cancelled))
        ));
        assert!(cancel.is_cancelled());
        assert_eq!(source, before);
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.last().unwrap().stage, stage);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.stage == stage)
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn precancelled_passage_operation_never_calls_executor() {
    let corpus = passage::corpus();
    let case = corpus
        .cases
        .iter()
        .find(|case| case.id == "causal_caveats")
        .unwrap();
    let source = source(case, true);
    let model = Scripted::new(Scenario::Repair, case);
    let cancel = Cancellation::default();
    cancel.cancel();
    let result = compose_passages(
        &model,
        &source,
        vec![],
        source.intent.clone(),
        None,
        &policy(180_000),
        &cancel,
    )
    .await;
    assert!(matches!(
        result,
        Err(CompositionError::Model(ModelError::Cancelled))
    ));
    assert!(model.requests.lock().unwrap().is_empty());
}
