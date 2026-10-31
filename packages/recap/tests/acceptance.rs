#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Offline acceptance through the actual source/media/composition/storage/export engine.

use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use bcode_recap::{
    composition::CompositionLimits,
    contracts::{Block, BlockContent, Depth, Error, GenerationIntent, RecapDocument, Section},
    engine::{Engine, EngineError, EngineOptions, GenerateRequest, Stage},
    export::{DegradationReason, ExportBudget},
    media::{MediaBudget, MediaTools},
    model::{ModelExecutor, ModelFuture, ModelRequest, ModelResponse},
    process::{self, Cancellation, ProcessRequest},
    source::{AcquisitionOptions, SourceInput},
    storage::DocumentStore,
};

const HOSTILE: &str = "<script>globalThis.recapInjected=true</script><img src=https://invalid.example/beacon onerror=alert(1)>";

#[derive(Default)]
struct DeterministicModel {
    calls: AtomicUsize,
    cancel: bool,
    fabricate: bool,
    concurrent_store: Option<DocumentStore>,
}

impl ModelExecutor for DeterministicModel {
    fn recovery_identity(&self) -> Option<String> {
        Some("offline-deterministic-v1".into())
    }

    fn resume_configuration(
        &self,
    ) -> Option<bcode_recap::model_configuration::ResumeModelConfiguration> {
        use bcode_recap::model_configuration::{ModelConfiguration, ResumeModelConfiguration};
        Some(ResumeModelConfiguration {
            version: 1,
            identity: self.recovery_identity().unwrap(),
            options: ModelConfiguration {
                config_path: std::path::PathBuf::new(),
                profile: Some("offline-original".into()),
                context: None,
                model: Some("fixture".into()),
                provider: Some("fixture".into()),
                endpoint: None,
                auth_profile: None,
                max_output_tokens: 10000,
                timeout: std::time::Duration::from_secs(300),
            },
            selection_fingerprint: "offline-selection".into(),
            config_fingerprint: Some("offline-config".into()),
        })
    }

    fn execute<'a>(
        &'a self,
        request: ModelRequest,
        cancellation: &'a Cancellation,
    ) -> ModelFuture<'a> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            validate_received_images(&request);
            let input: serde_json::Value = serde_json::from_str(&request.input_text).unwrap();
            let grounded: RecapDocument =
                serde_json::from_value(input["untrusted_source"].clone()).unwrap();
            let intent: GenerationIntent = serde_json::from_value(input["intent"].clone()).unwrap();
            validate_received_grounding(&request, &grounded);
            let clip = grounded
                .assets
                .iter()
                .find(|asset| asset.mime_type == "video/mp4")
                .expect("engine must extract a real clip");
            let clip_evidence = grounded
                .evidence
                .iter()
                .find(|item| item.range == clip.range)
                .unwrap();
            let asset = &grounded.assets[0];
            let evidence = grounded
                .evidence
                .iter()
                .find(|item| item.range == asset.range)
                .unwrap();
            if let Some(store) = &self.concurrent_store {
                let mut winner = store.load(&grounded.id).unwrap();
                winner.revision += 1;
                winner.title = "Concurrent winner".into();
                store.save(&winner, Some(grounded.revision)).unwrap();
            }
            if self.cancel {
                cancellation.cancel();
            }
            let asset_id = if self.fabricate {
                "fabricated-frame".into()
            } else {
                asset.id.clone()
            };
            Ok(ModelResponse {
                title: if intent.focus.is_some() {
                    "Focused visual recap".into()
                } else {
                    "Visual recap".into()
                },
                sections: vec![Section {
                    id: "overview".into(),
                    title: "What changes".into(),
                    blocks: vec![
                        Block {
                            id: "explanation".into(),
                            evidence_ids: vec![evidence.id.clone()],
                            content: BlockContent::Text {
                                text: format!(
                                    "The synthetic test pattern changes over time. {HOSTILE}"
                                ),
                            },
                        },
                        Block {
                            id: "motion".into(),
                            evidence_ids: vec![clip_evidence.id.clone()],
                            content: BlockContent::Clip {
                                asset_id: clip.id.clone(),
                                caption: "Actual extracted motion with supplied captions".into(),
                            },
                        },
                        Block {
                            id: "visual".into(),
                            evidence_ids: vec![evidence.id.clone()],
                            content: BlockContent::Image {
                                asset_id: asset_id.clone(),
                                caption: "Actual extracted test-pattern frame".into(),
                            },
                        },
                    ],
                }],
                image_choices: BTreeMap::from([
                    (
                        clip.id.clone(),
                        "Real source motion demonstrates the pattern change".into(),
                    ),
                    (asset_id, "An actual frame shows the source pattern".into()),
                ]),
            })
        })
    }
}

fn validate_received_grounding(request: &ModelRequest, grounded: &RecapDocument) {
    assert_eq!(
        grounded.transcript.len(),
        1,
        "caller-supplied captions must survive acquisition"
    );
    assert_eq!(grounded.transcript[0].text, "The test pattern changes.");
    assert_eq!(
        request.images.len(),
        grounded
            .assets
            .iter()
            .filter(|asset| asset.mime_type.starts_with("image/"))
            .count()
    );
}

fn validate_received_images(request: &ModelRequest) {
    assert!(!request.instructions.contains(HOSTILE));
    assert!(
        !request.images.is_empty(),
        "engine must supply actual extracted visuals"
    );
    for image in &request.images {
        assert_eq!(image.mime_type, "image/jpeg");
        assert!(image.bytes.starts_with(&[0xff, 0xd8, 0xff]));
        assert!(image.bytes.len() > 100);
    }
}

fn options(root: &Path) -> EngineOptions {
    EngineOptions {
        transcription: None,
        acquisition: AcquisitionOptions {
            yt_dlp: "deliberately-unavailable-no-network".into(),
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

async fn video(root: &Path) -> std::path::PathBuf {
    let path = root.join("input with spaces.mp4");
    Box::pin(process::run(
        &ProcessRequest {
            program: "ffmpeg".into(),
            args: [
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
                "-y",
            ]
            .into_iter()
            .map(Into::into)
            .chain(std::iter::once(path.clone().into_os_string()))
            .collect(),
            cwd: None,
            timeout: Duration::from_secs(20),
            max_output_bytes: 64 * 1024,
            file_budget: None,
        },
        &Cancellation::default(),
    ))
    .await
    .expect("acceptance requires installed ffmpeg with libx264");
    path
}

fn request(path: std::path::PathBuf) -> GenerateRequest {
    GenerateRequest {
        id: "acceptance".into(),
        source_id: "actual-video".into(),
        source_title: format!("Synthetic source {HOSTILE}"),
        input: SourceInput::Local(path),
        transcript: Some("WEBVTT\n\n00:00.000 --> 00:02.000\nThe test pattern changes.\n".into()),
        intent: GenerationIntent::default(),
    }
}

#[derive(Default)]
struct DetailedInspectionProbe {
    inspected: AtomicUsize,
}

impl ModelExecutor for DetailedInspectionProbe {
    fn execute<'a>(&'a self, _request: ModelRequest, _cancel: &'a Cancellation) -> ModelFuture<'a> {
        panic!("Detailed must use the staged analysis interface")
    }

    fn execute_analysis<'a>(
        &'a self,
        request: bcode_recap::model::AnalysisRequest,
        _cancel: &'a Cancellation,
    ) -> bcode_recap::model::AnalysisFuture<'a> {
        use bcode_recap::analysis_contracts::{AnalysisResponse, AnalysisStage};
        Box::pin(async move {
            match request.stage {
                AnalysisStage::ExtractionInventory => {
                    let window = &request.input["window"];
                    let speech = request.input["untrusted_source"]["evidence"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|evidence| {
                            !evidence["transcript_segment_ids"]
                                .as_array()
                                .unwrap()
                                .is_empty()
                        })
                        .unwrap();
                    let response = serde_json::json!({
                        "window_id": window["id"],
                        "items": [{"id":"pattern", "window_id":window["id"], "range":speech["range"],
                            "kind":"fact", "text":"The test pattern changes.", "evidence_ids":[speech["id"]]}],
                        "limitations":[]
                    });
                    AnalysisResponse::from_json_slice(
                        request.stage,
                        &serde_json::to_vec(&response).unwrap(),
                        request.max_response_bytes,
                    )
                }
                AnalysisStage::VisualInspection => {
                    assert!(!request.images.is_empty());
                    assert!(
                        request.images.len() <= 4,
                        "composition image cap must constrain inspection extraction"
                    );
                    let assets = request.input["untrusted_source"]["assets"]
                        .as_array()
                        .unwrap();
                    for image in &request.images {
                        assert!(image.bytes.starts_with(&[0xff, 0xd8, 0xff]));
                        let asset = assets
                            .iter()
                            .find(|asset| asset["id"].as_str() == Some(image.asset_id.as_str()))
                            .unwrap();
                        let evidence_id = format!("evidence-{}", image.asset_id);
                        assert!(
                            request.input["untrusted_source"]["evidence"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|evidence| evidence["id"].as_str()
                                    == Some(evidence_id.as_str())
                                    && evidence["range"] == asset["range"])
                        );
                    }
                    self.inspected.store(request.images.len(), Ordering::SeqCst);
                    // Stop before editorial stages: this fixture proves actual adaptive
                    // extraction/registration/admission, not model quality or success.
                    Err(bcode_recap::model::ModelError::Unavailable)
                }
                AnalysisStage::PassageComposition => {
                    assert!(!request.images.is_empty());
                    assert!(request.images.len() <= 4);
                    let unit: bcode_recap::passage_contracts::SourceUnit =
                        serde_json::from_value(request.input["source_unit"].clone()).unwrap();
                    for image in &request.images {
                        assert!(image.bytes.starts_with(&[0xff, 0xd8, 0xff]));
                        assert!(
                            unit.primary
                                .assets
                                .iter()
                                .any(|asset| asset.id == image.asset_id)
                        );
                    }
                    self.inspected.store(request.images.len(), Ordering::SeqCst);
                    Err(bcode_recap::model::ModelError::Unavailable)
                }
                _ => panic!("inspection probe must stop before editorial stages"),
            }
        })
    }
}

#[tokio::test]
async fn detailed_real_video_uses_adaptive_inspection_with_effective_image_bound() {
    let root = tempfile::tempdir().unwrap();
    let model = DetailedInspectionProbe::default();
    let mut configured = options(root.path());
    configured.composition.images = 4;
    configured.inspection.min_spacing_ms = 0;
    assert_eq!(configured.inspection.max_candidates, 96);
    let mut input = request(video(root.path()).await);
    input.intent.depth = Depth::Detailed;
    let engine = Engine {
        executor: &model,
        store: DocumentStore::new(root.path().join("store"), 1_000_000),
        options: configured,
    };
    let error = engine
        .prepare(&input, &Cancellation::default(), &mut |_| {})
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        EngineError::Composition(bcode_recap::composition::CompositionError::Model(
            bcode_recap::model::ModelError::Unavailable
        ))
    ));
    assert_eq!(model.inspected.load(Ordering::SeqCst), 4);
    assert!(
        engine.store.load(&input.id).is_err(),
        "failed analysis must not commit"
    );
}

#[tokio::test]
async fn detailed_rejects_unusable_effective_inspection_bound_before_inference() {
    let root = tempfile::tempdir().unwrap();
    let model = DetailedInspectionProbe::default();
    let mut configured = options(root.path());
    configured.composition.images = 1;
    let mut input = request(video(root.path()).await);
    input.intent.depth = Depth::Detailed;
    let engine = Engine {
        executor: &model,
        store: DocumentStore::new(root.path().join("store"), 1_000_000),
        options: configured,
    };
    assert!(matches!(
        engine
            .prepare(&input, &Cancellation::default(), &mut |_| {})
            .await,
        Err(EngineError::Invalid(_))
    ));
    assert_eq!(model.inspected.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unreferenced_inspection_evidence_is_retained_but_not_read_by_export() {
    let root = tempfile::tempdir().unwrap();
    let model = DeterministicModel::default();
    let engine = Engine {
        executor: &model,
        store: DocumentStore::new(root.path().join("store"), 1_000_000),
        options: options(root.path()),
    };
    let document = engine
        .generate(
            &request(video(root.path()).await),
            &Cancellation::default(),
            &mut |_| {},
        )
        .await
        .unwrap();
    let used: std::collections::BTreeSet<_> = document
        .sections
        .iter()
        .flat_map(|section| &section.blocks)
        .flat_map(|block| match &block.content {
            BlockContent::Text { .. }
            | BlockContent::Heading { .. }
            | BlockContent::List { .. }
            | BlockContent::Diagram { .. } => vec![],
            BlockContent::Image { asset_id, .. } | BlockContent::Clip { asset_id, .. } => {
                vec![asset_id.as_str()]
            }
            BlockContent::Comparison { items } => {
                items.iter().map(|item| item.asset_id.as_str()).collect()
            }
        })
        .collect();
    let unused = document
        .assets
        .iter()
        .find(|asset| !used.contains(asset.id.as_str()))
        .expect("fixture provides more evidence than publication uses");
    fs::remove_file(
        engine
            .store
            .asset_directory(&document.id)
            .unwrap()
            .join(unused.path.as_str()),
    )
    .unwrap();
    let html = engine
        .export(&document.id, &Cancellation::default(), &mut |_| {})
        .unwrap();
    assert!(
        html.report.degradations.is_empty(),
        "unreferenced evidence must not be embedded or read"
    );
    assert_eq!(
        engine.store.load(&document.id).unwrap().assets,
        document.assets
    );
}

#[tokio::test]
async fn real_video_generate_export_refine_and_missing_offline_asset() {
    let root = tempfile::tempdir().unwrap();
    let model = DeterministicModel::default();
    let engine = Engine {
        executor: &model,
        store: DocumentStore::new(root.path().join("store"), 1_000_000),
        options: options(root.path()),
    };
    let mut stages = Vec::new();
    let initial = engine
        .generate(
            &request(video(root.path()).await),
            &Cancellation::default(),
            &mut |stage| stages.push(stage),
        )
        .await
        .unwrap();
    assert_eq!(
        stages,
        [
            Stage::Acquisition,
            Stage::Media,
            Stage::Composition,
            Stage::Export,
            Stage::Storage,
            Stage::Complete
        ]
    );
    assert_eq!(initial.revision, 1);
    assert_eq!(initial.intent, GenerationIntent::default());
    let asset_bytes: Vec<_> = initial
        .assets
        .iter()
        .map(|asset| {
            engine
                .store
                .read_asset(&initial.id, asset, 1_000_000)
                .unwrap()
        })
        .collect();
    let html = engine
        .export(&initial.id, &Cancellation::default(), &mut |_| {})
        .unwrap();
    assert!(html.report.degradations.is_empty());
    assert!(html.html.contains("data:image/jpeg;base64,"));
    assert!(html.html.contains("data:video/mp4;base64,"));
    assert!(html.html.contains("data:text/vtt;base64,"));
    assert!(html.html.contains("&lt;script&gt;"));
    assert!(!html.html.contains(HOSTILE));
    assert!(html.report.embedded_media_bytes > 100);
    let focus = GenerationIntent {
        focus: Some("Explain the changing pattern".into()),
        // This fixture exercises the selective single-pass adapter. Detailed's
        // staged analysis uses the separate real-media inspection test below.
        depth: Depth::Standard,
    };
    let refined = engine
        .refine(
            &initial.id,
            focus.clone(),
            &Cancellation::default(),
            &mut |_| {},
        )
        .await
        .unwrap();
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    assert_eq!(refined.revision, 2);
    assert_eq!(refined.intent, focus);
    assert_eq!(refined.sections[0].id, initial.sections[0].id);
    assert_eq!(refined.source, initial.source);
    assert_eq!(refined.evidence, initial.evidence);
    assert_eq!(refined.assets, initial.assets);
    for (asset, bytes) in refined.assets.iter().zip(asset_bytes) {
        assert_eq!(
            engine
                .store
                .read_asset(&refined.id, asset, 1_000_000)
                .unwrap(),
            bytes
        );
    }
    let refined_html = engine
        .export(&refined.id, &Cancellation::default(), &mut |_| {})
        .unwrap();
    if let Some(output) = std::env::var_os("RECAP_ACCEPTANCE_OUTPUT") {
        let output = std::path::PathBuf::from(output);
        fs::create_dir_all(&output).unwrap();
        fs::write(output.join("recap.html"), refined_html.html).unwrap();
    }
    assert_missing_asset_degrades(&engine, &refined);
}

fn assert_missing_asset_degrades(engine: &Engine<'_>, refined: &RecapDocument) {
    let selected = &refined.assets[0];
    fs::remove_file(
        engine
            .store
            .asset_directory(&refined.id)
            .unwrap()
            .join(selected.path.as_str()),
    )
    .unwrap();
    let degraded = engine
        .export(&refined.id, &Cancellation::default(), &mut |_| {})
        .unwrap();
    assert!(
        degraded
            .report
            .degradations
            .iter()
            .any(|item| item.reason == DegradationReason::UnreadableAsset)
    );
    assert!(
        degraded
            .html
            .contains("Actual extracted test-pattern frame")
    );
    assert_eq!(&engine.store.load(&refined.id).unwrap(), refined);
}

#[tokio::test]
async fn hostile_model_references_and_cancellation_never_commit() {
    let root = tempfile::tempdir().unwrap();
    let input = video(root.path()).await;
    for (name, model) in [
        (
            "fabricated",
            DeterministicModel {
                fabricate: true,
                ..Default::default()
            },
        ),
        (
            "cancelled",
            DeterministicModel {
                cancel: true,
                ..Default::default()
            },
        ),
    ] {
        let engine = Engine {
            executor: &model,
            store: DocumentStore::new(root.path().join(name), 1_000_000),
            options: options(root.path()),
        };
        let mut stages = Vec::new();
        assert!(
            engine
                .generate(
                    &request(input.clone()),
                    &Cancellation::default(),
                    &mut |stage| stages.push(stage)
                )
                .await
                .is_err()
        );
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        assert!(!stages.contains(&Stage::Complete));
        assert!(!root.path().join(name).join("acceptance.json").exists());
        assert!(!engine.store.asset_directory("acceptance").unwrap().exists());
    }
    let model = DeterministicModel::default();
    let engine = Engine {
        executor: &model,
        store: DocumentStore::new(root.path().join("pre-cancel"), 1_000_000),
        options: options(root.path()),
    };
    let cancel = Cancellation::default();
    cancel.cancel();
    assert!(
        engine
            .generate(&request(input.clone()), &cancel, &mut |_| panic!(
                "cancelled operation reported progress"
            ))
            .await
            .is_err()
    );
    let mut hostile = request(input);
    hostile.id = "../escape".into();
    assert!(
        engine
            .generate(&hostile, &Cancellation::default(), &mut |_| {})
            .await
            .is_err()
    );
    hostile.id = "acceptance".into();
    hostile.input =
        SourceInput::Youtube("https://youtube.com.evil.invalid/watch?v=abcdefghijk".into());
    assert!(
        engine
            .generate(&hostile, &Cancellation::default(), &mut |_| {})
            .await
            .is_err()
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancelled_and_stale_refinement_preserve_authoritative_revision() {
    let root = tempfile::tempdir().unwrap();
    let store = DocumentStore::new(root.path().join("store"), 1_000_000);
    let model = DeterministicModel::default();
    let engine = Engine {
        executor: &model,
        store: store.clone(),
        options: options(root.path()),
    };
    let initial = engine
        .generate(
            &request(video(root.path()).await),
            &Cancellation::default(),
            &mut |_| {},
        )
        .await
        .unwrap();
    let cancelled_model = DeterministicModel {
        cancel: true,
        ..Default::default()
    };
    let cancelled_engine = Engine {
        executor: &cancelled_model,
        store: store.clone(),
        options: options(root.path()),
    };
    assert!(
        cancelled_engine
            .refine(
                &initial.id,
                GenerationIntent::default(),
                &Cancellation::default(),
                &mut |_| {}
            )
            .await
            .is_err()
    );
    assert_eq!(store.load(&initial.id).unwrap(), initial);
    let racing_model = DeterministicModel {
        concurrent_store: Some(store.clone()),
        ..Default::default()
    };
    let racing_engine = Engine {
        executor: &racing_model,
        store: store.clone(),
        options: options(root.path()),
    };
    let mut stages = Vec::new();
    let result = racing_engine
        .resume_generation(
            "acceptance-r2",
            &Cancellation::default(),
            &mut |stage| {
                stages.push(stage);
                if stage == Stage::Storage {
                    let mut winner = initial.clone();
                    winner.revision += 1;
                    winner.title = "Concurrent winner".into();
                    store.save(&winner, Some(initial.revision)).unwrap();
                }
            },
            true,
        )
        .await;
    assert!(
        matches!(
            result,
            Err(EngineError::Contract(Error::RevisionConflict {
                expected: Some(1),
                actual: Some(2)
            }))
        ),
        "{result:?}"
    );
    assert!(!stages.contains(&Stage::Complete));
    let winner = store.load(&initial.id).unwrap();
    assert_eq!(winner.revision, 2);
    assert_eq!(winner.title, "Concurrent winner");
    assert_eq!(winner.assets, initial.assets);
}

#[tokio::test]
async fn generation_survives_process_reopen_without_source_or_repeated_completed_calls() {
    let root = tempfile::tempdir().unwrap();
    let path = video(root.path()).await;
    let request = request(path.clone());
    let model = DeterministicModel::default();
    let store = DocumentStore::new(root.path().join("store"), 8 * 1024 * 1024);
    let engine = Engine {
        executor: &model,
        store: store.clone(),
        options: options(root.path()),
    };
    let cancel = Cancellation::default();
    let first = engine
        .generate(&request, &cancel, &mut |stage| {
            if stage == Stage::Storage {
                cancel.cancel();
            }
        })
        .await;
    assert!(matches!(first, Err(EngineError::Cancelled)));
    assert!(store.load(&request.id).is_err());
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    drop(engine);
    fs::remove_file(&path).unwrap();
    let mut configuration = options(root.path());
    // Same immutable policy, recreated in a fresh engine; media executables would
    // fail if invoked with the now-missing source. Grounded assets are restored.
    configuration.acquisition.workspace = root.path().into();
    let engine = Engine {
        executor: &model,
        store: store.clone(),
        options: configuration,
    };
    let result = engine
        .generate(&request, &Cancellation::default(), &mut |_| {})
        .await
        .unwrap();
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert_eq!(store.load(&request.id).unwrap(), result);
    let again = engine
        .generate(&request, &Cancellation::default(), &mut |_| {})
        .await
        .unwrap();
    assert_eq!(again, result);
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert!(
        !engine
            .export(&request.id, &Cancellation::default(), &mut |_| {})
            .unwrap()
            .html
            .is_empty()
    );
}

// The subprocess is deliberately stopped without running destructors. The parent
// owns the temporary directory and always reaps the child before asserting.
#[tokio::test]
async fn crash_restart_reuses_completed_work_without_original_source() {
    const CHILD_ROOT: &str = "RECAP_CRASH_ACCEPTANCE_ROOT";
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let root = std::path::PathBuf::from(root);
        let request = request(video(&root).await);
        fs::write(
            root.join("request.json"),
            serde_json::to_vec(&request).unwrap(),
        )
        .unwrap();
        let model = DeterministicModel::default();
        let engine = Engine {
            executor: &model,
            store: DocumentStore::new(root.join("store"), 8 * 1024 * 1024),
            options: options(&root),
        };
        engine
            .generate(&request, &Cancellation::default(), &mut |stage| {
                if stage == Stage::Storage {
                    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
                    fs::write(root.join("ready"), b"completed model and media").unwrap();
                    loop {
                        std::thread::park();
                    }
                }
            })
            .await
            .unwrap();
        panic!("child must be killed before publication");
    }
    let root = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "crash_restart_reuses_completed_work_without_original_source",
            "--nocapture",
        ])
        .env(CHILD_ROOT, root.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !root.path().join("ready").exists() && std::time::Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let ready = root.path().join("ready").exists();
    let _ = child.kill();
    child.wait().unwrap();
    assert!(
        ready,
        "child did not reach durable pre-publication boundary"
    );
    let request: GenerateRequest =
        serde_json::from_slice(&fs::read(root.path().join("request.json")).unwrap()).unwrap();
    let SourceInput::Local(source) = &request.input else {
        panic!("expected local fixture")
    };
    fs::remove_file(source).unwrap();
    let model = DeterministicModel::default();
    let store = DocumentStore::new(root.path().join("store"), 8 * 1024 * 1024);
    assert!(store.load(&request.id).is_err());
    let saved_model =
        bcode_recap::generation::Journal::model_configuration(store.generation_root(), &request.id)
            .unwrap()
            .unwrap();
    assert_eq!(Some(saved_model.clone()), model.resume_configuration());
    let engine = Engine {
        executor: &model,
        store: store.clone(),
        options: options(root.path()),
    };
    let result = engine
        .resume_generation(&request.id, &Cancellation::default(), &mut |_| {}, false)
        .await
        .unwrap();
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
    assert_eq!(store.load(&request.id).unwrap(), result);
    assert!(
        !engine
            .export(&request.id, &Cancellation::default(), &mut |_| {})
            .unwrap()
            .html
            .is_empty()
    );
}
