#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Explicit live acceptance harness. It runs only when its dedicated environment gate is set.
//! Source-bearing artifacts are written only to the caller's explicit private destination.
#[path = "../../src/bcode_adapter.rs"]
mod bcode_adapter;
#[path = "../../src/connections.rs"]
mod connections;

use bcode_recap::{
    analysis_contracts::{AnalysisRequest, AnalysisResponse, DetailedPolicy},
    contracts::{
        Block, BlockContent, Depth, Evidence, GenerationIntent, RecapDocument, Section, Source,
        TimeRange, TranscriptSegment,
    },
    model::{AnalysisFuture, ModelExecutor, ModelFuture, ModelRequest},
    process::Cancellation,
};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct RecordingExecutor {
    inner: bcode_adapter::BcodeExecutor,
    root: PathBuf,
    calls: AtomicUsize,
}
impl ModelExecutor for RecordingExecutor {
    fn execute<'a>(&'a self, request: ModelRequest, cancel: &'a Cancellation) -> ModelFuture<'a> {
        self.inner.execute(request, cancel)
    }
    fn execute_analysis<'a>(
        &'a self,
        request: AnalysisRequest,
        cancel: &'a Cancellation,
    ) -> AnalysisFuture<'a> {
        Box::pin(async move {
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            if index >= 12 {
                return Err(bcode_recap::model::ModelError::InvalidResponse);
            }
            // Deliberately omit trusted prompts, credentials, provider payloads and image bytes.
            let input = serde_json::json!({"stage":request.stage,"input":request.input});
            std::fs::write(
                self.root.join(format!("call-{index}-source.json")),
                serde_json::to_vec_pretty(&input).unwrap(),
            )
            .unwrap();
            let response = self.inner.execute_analysis(request, cancel).await;
            let result = match &response {
                Ok(AnalysisResponse::PassageComposition(value)) => {
                    serde_json::to_value(value).unwrap()
                }
                Ok(AnalysisResponse::PassageReview(value)) => serde_json::to_value(value).unwrap(),
                Ok(AnalysisResponse::PassagePatch(value)) => serde_json::to_value(value).unwrap(),
                Ok(_) => serde_json::json!({"unexpected_stage":true}),
                Err(_) => serde_json::json!({"model_failure":true}),
            };
            std::fs::write(
                self.root.join(format!("call-{index}-result.json")),
                serde_json::to_vec_pretty(&result).unwrap(),
            )
            .unwrap();
            response
        })
    }
}

#[tokio::test]
async fn explicit_live_passage_excerpt() {
    let Some(root) = std::env::var_os("RECAP_PASSAGE_LIVE_OUTPUT") else {
        return;
    };
    let config = std::env::var_os("RECAP_PASSAGE_LIVE_CONFIG")
        .expect("explicit model configuration required for live acceptance");
    let profile = std::env::var("RECAP_PASSAGE_LIVE_PROFILE")
        .expect("explicit model profile required for live acceptance");
    let root = PathBuf::from(root);
    assert!(root.starts_with("/tmp/recap-passage-acceptance"));
    std::fs::create_dir(&root).expect("new explicit artifact directory");
    let grounded = grounded_excerpt();
    grounded.validate().unwrap();
    let operation = bcode_recap::diagnostics::Operation::new("live-passage-excerpt".into());
    let executor = RecordingExecutor {
        inner: bcode_adapter::BcodeExecutor::configured(bcode_adapter::BcodeOptions {
            config_path: PathBuf::from(config),
            profile: Some(profile),
            context: None,
            model: None,
            provider: None,
            endpoint: None,
            auth_profile: Some(
                std::env::var("BCODE_AUTH_PROFILE")
                    .expect("explicit authentication profile required"),
            ),
            max_output_tokens: 8192,
            timeout: Duration::from_secs(120),
        })
        .await
        .unwrap()
        .with_observer(Arc::new(operation.clone())),
        root: root.clone(),
        calls: AtomicUsize::new(0),
    };
    let mut policy = DetailedPolicy::default();
    policy.allowances.max_model_calls = 12;
    policy.source_passages = true;
    let cancel = Cancellation::default().observed(Arc::new(operation.clone()));
    executor.inner.observe_selection(&cancel);
    let deadline = cancel.clone();
    let timer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(180)).await;
        deadline.cancel();
    });
    let result = bcode_recap::passage::compose_passages(
        &executor,
        &grounded,
        vec![],
        grounded.intent.clone(),
        None,
        &policy,
        &cancel,
    )
    .await;
    timer.abort();
    drop(timer);
    drop(executor);
    std::fs::write(
        root.join("report.json"),
        serde_json::to_vec_pretty(&operation.report()).unwrap(),
    )
    .unwrap();
    match result {
        Ok(document) => {
            let html = bcode_recap::export::export_html(
                &document,
                &root,
                bcode_recap::export::ExportBudget::default(),
            )
            .unwrap();
            std::fs::write(
                root.join("document.json"),
                serde_json::to_vec_pretty(&document).unwrap(),
            )
            .unwrap();
            std::fs::write(root.join("recap.html"), html.html).unwrap();
        }
        Err(error) => {
            std::fs::write(root.join("failure.txt"), error.to_string()).unwrap();
            panic!("live passage acceptance failed: {error}");
        }
    }
}

fn grounded_excerpt() -> RecapDocument {
    let range = TimeRange {
        start_ms: 0,
        end_ms: 60_000,
    };
    let text = "For this procedure, use 250 millilitres of water at 38 degrees Celsius, not boiling water. If the room is below 18 degrees, warm the bowl first. Stir for two minutes before adding the powder; reversing that order creates lumps. In our small trial, four of five samples dissolved completely. That is an observation, not proof the method always works. Mira recommends this method for soft materials. Leon disagrees for brittle materials because repeated heating can crack them. Neither recommends applying it to glass. After mixing, wait ten minutes before checking; do not treat a cloudy mixture as ready. The example uses a wooden bowl, but the speaker says the bowl material was not controlled in the trial.";
    RecapDocument {
        schema_version: 1,
        id: "live-passage".into(),
        revision: 1,
        title: "Procedure and limitations".into(),
        source: Source {
            id: "excerpt".into(),
            title: "Procedure and limitations".into(),
            url: None,
            duration_ms: 60_000,
        },
        intent: GenerationIntent {
            depth: Depth::Detailed,
            ..GenerationIntent::default()
        },
        transcript: vec![TranscriptSegment {
            id: "cue-1".into(),
            range,
            text: text.into(),
        }],
        evidence: vec![Evidence {
            id: "evidence-1".into(),
            range,
            transcript_segment_ids: vec!["cue-1".into()],
        }],
        assets: vec![],
        sections: vec![Section {
            id: "grounded".into(),
            title: "Source".into(),
            blocks: vec![Block {
                id: "source".into(),
                evidence_ids: vec!["evidence-1".into()],
                content: BlockContent::Text { text: text.into() },
            }],
        }],
    }
}
