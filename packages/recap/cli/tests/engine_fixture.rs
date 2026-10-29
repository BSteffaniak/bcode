#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Offline engine-backed fixture for the independent subprocess/PTY acceptance.
//! Set `RECAP_TUI_FIXTURE_DIR` to retain the private store for `check-recap-tui.py`.

use std::{collections::BTreeMap, path::Path, process::Command, time::Duration};

use bcode_recap::{
    composition::CompositionLimits,
    contracts::{Block, BlockContent, GenerationIntent, RecapDocument, Section},
    engine::{Engine, EngineOptions, GenerateRequest, Stage},
    export::ExportBudget,
    media::{MediaBudget, MediaTools},
    model::{ModelExecutor, ModelFuture, ModelRequest, ModelResponse},
    process::Cancellation,
    source::{AcquisitionOptions, SourceInput},
    storage::DocumentStore,
};

struct FixtureExecutor {
    cancel_during_composition: bool,
}

impl ModelExecutor for FixtureExecutor {
    fn execute<'a>(
        &'a self,
        request: ModelRequest,
        cancellation: &'a Cancellation,
    ) -> ModelFuture<'a> {
        Box::pin(async move {
            assert!(!request.images.is_empty(), "real extracted frames required");
            assert!(
                request
                    .images
                    .iter()
                    .all(|image| image.bytes.starts_with(&[0xff, 0xd8]))
            );
            let input: serde_json::Value = serde_json::from_str(&request.input_text).unwrap();
            let grounded: RecapDocument =
                serde_json::from_value(input["untrusted_source"].clone()).unwrap();
            let asset = &grounded.assets[0];
            let evidence = grounded
                .evidence
                .iter()
                .find(|item| item.range == asset.range)
                .unwrap();
            if self.cancel_during_composition {
                cancellation.cancel();
            }
            Ok(ModelResponse {
                title: "Unicode field notes — 日本語 🌱".into(),
                sections: vec![Section {
                    id: "observations".into(),
                    title: "Motion and evidence".into(),
                    blocks: vec![
                        Block {
                            id: "explanation".into(),
                            evidence_ids: vec![evidence.id.clone()],
                            content: BlockContent::Text {
                                text: "The real test pattern changes. 日本語 🌱 café e\u{301}."
                                    .repeat(24),
                            },
                        },
                        Block {
                            id: "frame".into(),
                            evidence_ids: vec![evidence.id.clone()],
                            content: BlockContent::Image {
                                asset_id: asset.id.clone(),
                                caption: "Extracted frame — 観察".into(),
                            },
                        },
                    ],
                }],
                image_choices: BTreeMap::from([(asset.id.clone(), "Actual source frame".into())]),
            })
        })
    }
}

fn options(root: &Path) -> EngineOptions {
    EngineOptions {
        transcription: None,
        acquisition: AcquisitionOptions {
            yt_dlp: "recap-acceptance-network-disabled".into(),
            workspace: root.into(),
            max_video_bytes: 5_000_000,
            max_duration_seconds: 30.0,
            caption_language: "en".into(),
            timeout: Duration::from_secs(20),
        },
        tools: MediaTools::default(),
        media: MediaBudget {
            max_frames: 3,
            max_clips: 1,
            max_width: 320,
            ..MediaBudget::default()
        },
        composition: CompositionLimits::default(),
        detailed: bcode_recap::analysis_contracts::DetailedPolicy::default(),
        inspection: bcode_recap::media::InspectionBudget::default(),
        export: ExportBudget::default(),
    }
}

fn real_video(root: &Path) -> std::path::PathBuf {
    let video = root.join("real source.mp4");
    let status = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x180:rate=10:duration=2",
            "-an",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-n",
        ])
        .arg(&video)
        .status()
        .expect("installed ffmpeg required");
    assert!(status.success());
    video
}

fn assert_diagnostics(report: &bcode_recap::diagnostics::Report) {
    use bcode_recap::diagnostics::{Event, Phase};
    assert!(report.inference_started());
    assert!(report.committed());
    for expected in [
        Phase::Probe,
        Phase::Extraction,
        Phase::Composition,
        Phase::ValidationOutput,
        Phase::Storage,
    ] {
        assert!(
            report
                .events
                .iter()
                .any(|e| matches!(e.event, Event::Phase { phase } if phase == expected))
        );
    }
}

#[tokio::test]
async fn engine_fixture_generate_refine_export_and_cancel() {
    let temporary = tempfile::tempdir().unwrap();
    let retained = std::env::var_os("RECAP_TUI_FIXTURE_DIR").map(std::path::PathBuf::from);
    let root = retained.as_deref().unwrap_or_else(|| temporary.path());
    std::fs::create_dir_all(root).unwrap();
    let video = real_video(root);
    let engine = Engine {
        executor: &FixtureExecutor {
            cancel_during_composition: false,
        },
        store: DocumentStore::new(root.join("store"), 1_000_000),
        options: options(root),
    };
    let request = GenerateRequest {
        id: "unicode-field-notes".into(),
        source_id: "real-source".into(),
        source_title: "Actual synthetic video — 日本語".into(),
        input: SourceInput::Local(video),
        transcript: Some("WEBVTT\n\n00:00.000 --> 00:02.000\nThe test pattern changes.\n".into()),
        intent: GenerationIntent::default(),
    };
    let cancelled = Cancellation::default();
    cancelled.cancel();
    let mut cancelled_stages = Vec::new();
    assert!(
        engine
            .generate(&request, &cancelled, &mut |stage| cancelled_stages
                .push(stage))
            .await
            .is_err()
    );
    assert!(!cancelled_stages.contains(&Stage::Complete));
    assert!(!root.join("store").join("unicode-field-notes.json").exists());
    assert!(!root.join("recap.html").exists());
    let mut stages = Vec::new();
    let operation = bcode_recap::diagnostics::Operation::new("fixture-operation".into());
    let observed_cancel = Cancellation::default().observed(std::sync::Arc::new(operation.clone()));
    let initial = engine
        .generate(&request, &observed_cancel, &mut |stage| {
            stages.push(stage);
        })
        .await
        .unwrap();
    assert_diagnostics(&operation.report());
    assert_eq!(stages.last(), Some(&Stage::Complete));
    assert_eq!(initial.revision, 1);
    let refined = engine
        .refine(
            &initial.id,
            GenerationIntent {
                focus: Some("Compare source motion".into()),
                ..GenerationIntent::default()
            },
            &Cancellation::default(),
            &mut |_| {},
        )
        .await
        .unwrap();
    assert_eq!(refined.revision, 2);
    assert_eq!(refined.assets, initial.assets);
    assert_eq!(refined.source, initial.source);
    let html = engine
        .export(&initial.id, &Cancellation::default(), &mut |_| {})
        .unwrap();
    assert!(html.report.degradations.is_empty());
    assert!(html.html.contains("data:image/jpeg;base64,"));
    std::fs::write(root.join("recap.html"), html.html).unwrap();
    let cancel = Cancellation::default();
    cancel.cancel();
    assert!(
        engine
            .refine(
                &initial.id,
                GenerationIntent::default(),
                &cancel,
                &mut |_| {}
            )
            .await
            .is_err()
    );
    assert_eq!(engine.store.load(&initial.id).unwrap(), refined);
    let cancelling_engine = Engine {
        executor: &FixtureExecutor {
            cancel_during_composition: true,
        },
        store: engine.store.clone(),
        options: options(root),
    };
    assert!(
        cancelling_engine
            .refine(
                &initial.id,
                GenerationIntent::default(),
                &Cancellation::default(),
                &mut |_| {}
            )
            .await
            .is_err()
    );
    assert_eq!(engine.store.load(&initial.id).unwrap(), refined);
    println!("Reader fixture store: {}", root.join("store").display());
}
