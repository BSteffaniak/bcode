//! Canonical-workspace confined direct recap library operations.
use bcode_plugin_sdk::NativeServiceContext;
use bcode_recap::{
    contracts::GenerationIntent,
    engine::{Engine, EngineOptions, GenerateRequest},
    process::Cancellation,
    source::{AcquisitionOptions, SourceInput},
    storage::DocumentStore,
};
use std::{
    collections::BTreeMap,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    time::Duration,
};

pub fn preparation_root(request: &bcode_tool::ToolPreparationRequest) -> Result<PathBuf, String> {
    let mut contexts = request
        .host_context
        .iter()
        .filter(|entry| entry.schema == bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA);
    let context = contexts.next().ok_or("canonical workspace required")?;
    if contexts.next().is_some()
        || context.schema_version != bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA_VERSION
    {
        return Err("unsupported workspace context".into());
    }
    let root = context
        .payload
        .get("working_directory")
        .and_then(serde_json::Value::as_str)
        .ok_or("canonical workspace required")?;
    canonical_root(Path::new(root))
}
fn canonical_root(root: &Path) -> Result<PathBuf, String> {
    if !root.is_absolute() {
        return Err("absolute canonical workspace required".into());
    }
    root.canonicalize()
        .map_err(|_| "workspace unavailable".into())
}
fn source(args: &BTreeMap<String, String>, root: &Path) -> Result<SourceInput, String> {
    let value = args.get("source").ok_or("source is required")?;
    if value.starts_with("https://") {
        return bcode_recap::source::youtube_url(value)
            .map(SourceInput::Youtube)
            .map_err(|_| "unsupported video URL".into());
    }
    let input = root
        .join(value)
        .canonicalize()
        .map_err(|_| "source unavailable")?;
    if !input.starts_with(root) || !input.is_file() {
        return Err("source must be a file in the canonical workspace".into());
    }
    Ok(SourceInput::Local(input))
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}
pub fn validate(
    operation: &str,
    args: &BTreeMap<String, String>,
    root: &Path,
) -> Result<(), String> {
    if !super::OPERATIONS
        .iter()
        .any(|name| operation == format!("recap.{name}"))
    {
        return Err("unsupported recap operation".into());
    }
    if !valid_id(args.get("id").ok_or("id is required")?) {
        return Err("invalid recap identity".into());
    }
    if args
        .keys()
        .any(|key| !matches!(key.as_str(), "id" | "source" | "focus" | "transcript"))
    {
        return Err("unknown recap argument".into());
    }
    if operation == "recap.create" {
        let _ = source(args, root)?;
    }
    if operation == "recap.refine"
        && args
            .get("focus")
            .is_none_or(|focus| focus.trim().is_empty())
    {
        return Err("refinement focus is required".into());
    }
    if args.get("focus").is_some_and(|focus| focus.len() > 8192) {
        return Err("focus exceeds budget".into());
    }
    Ok(())
}
/// Canonical all-of resource facts; inference admission remains a separate host check.
pub fn policy_operations(
    operation: &str,
    args: &BTreeMap<String, String>,
    root: &Path,
) -> Result<Vec<bcode_plugin_sdk::ToolPolicyOperation>, String> {
    use bcode_plugin_sdk::ToolPolicyOperation as Operation;
    if operation == "recap.create" {
        let mut facts = vec![
            Operation::Write {
                paths: vec![root.join(".recap").display().to_string()],
                category: "write".into(),
            },
            Operation::Mutating,
        ];
        match source(args, root)? {
            SourceInput::Local(path) => facts.push(Operation::Read {
                paths: vec![path.display().to_string()],
            }),
            SourceInput::Youtube(url) => facts.push(Operation::Web { url: Some(url) }),
        }
        if let Some(path) = args.get("transcript") {
            let path = root
                .join(path)
                .canonicalize()
                .map_err(|_| "transcript unavailable")?;
            if !path.starts_with(root) {
                return Err("transcript must stay in workspace".into());
            }
            facts.push(Operation::Read {
                paths: vec![path.display().to_string()],
            });
        }
        return Ok(facts);
    }
    let id = args.get("id").ok_or("id is required")?;
    let store = root.join(".recap");
    let mut facts = vec![Operation::Read {
        paths: vec![
            store.join(format!("{id}.json")).display().to_string(),
            store.join(format!("{id}.assets")).display().to_string(),
        ],
    }];
    match operation {
        "recap.refine" => {
            facts.push(Operation::Write {
                paths: vec![
                    store.join(format!("{id}.json")).display().to_string(),
                    store.join(format!("{id}.lock")).display().to_string(),
                ],
                category: "write".into(),
            });
            facts.push(Operation::Mutating); // Explicit inference request; host separately admits account/profile.
        }
        "recap.export" => facts.push(Operation::Write {
            paths: vec![store.join(format!("{id}.html")).display().to_string()],
            category: "write".into(),
        }),
        "recap.inspect" => {}
        _ => return Err("unsupported recap operation".into()),
    }
    Ok(facts)
}

pub fn run(
    context: &NativeServiceContext,
    operation: &str,
    invocation: &str,
    args: &BTreeMap<String, String>,
    root: &Path,
) -> Result<String, String> {
    let root = canonical_root(root)?;
    validate(operation, args, &root)?;
    let id = args.get("id").ok_or("id is required")?;
    let store_root = root.join(".recap");
    validate_store_root(&store_root)?;
    let store = DocumentStore::new(&store_root, 4 * 1024 * 1024);
    if operation == "recap.inspect" {
        return serde_json::to_string(&store.load(id).map_err(|_| "recap inspection failed")?)
            .map_err(|_| "recap encoding failed".into());
    }
    if context.cancellation.is_cancelled() {
        return Err("recap cancelled".into());
    }
    let executor = super::model::HostExecutor {
        bridge: context.bridge.clone(),
        invocation_id: invocation.into(),
        cancellation: context.cancellation.clone(),
    };
    if matches!(operation, "recap.create" | "recap.refine") {
        executor.check_admission().map_err(
            |_| "recap model-host admission required; no media or store writes performed",
        )?;
    }
    fs::create_dir_all(&store_root).map_err(|_| "recap store unavailable")?;
    let engine = Engine {
        executor: &executor,
        store,
        options: engine_options(&store_root),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "recap runtime unavailable")?;
    let cancel = operation_cancellation(context, invocation, &root);
    runtime.block_on(async {
        let operation = async {
            let mut progress = |_| {
                if context.cancellation.is_cancelled() {
                    cancel.cancel();
                }
            };
            let intent = GenerationIntent {
                focus: args.get("focus").cloned(),
                ..Default::default()
            };
            match operation {
                "recap.create" => {
                    let request = GenerateRequest {
                        id: id.clone(),
                        source_id: id.clone(),
                        source_title: id.clone(),
                        input: source(args, &root)?,
                        transcript: transcript(args, &root)?,
                        intent,
                    };
                    let document = engine
                        .generate(&request, &cancel, &mut progress)
                        .await
                        .map_err(super::diagnostics::engine)?;
                    serde_json::to_string(&document).map_err(|_| "recap encoding failed".into())
                }
                "recap.refine" => {
                    let document = engine
                        .refine(id, intent, &cancel, &mut progress)
                        .await
                        .map_err(super::diagnostics::engine)?;
                    serde_json::to_string(&document).map_err(|_| "recap encoding failed".into())
                }
                "recap.export" => {
                    let export = engine
                        .export(id, &cancel, &mut progress)
                        .map_err(super::diagnostics::engine)?;
                    publish_export(&store_root, id, &export.html)
                }
                _ => Err("unsupported recap operation".into()),
            }
        };
        tokio::pin!(operation);
        loop {
            tokio::select! {
                result = &mut operation => break result,
                () = tokio::time::sleep(Duration::from_millis(25)) => {
                    if context.cancellation.is_cancelled() { cancel.cancel(); }
                }
            }
        }
    })
}

fn validate_store_root(store_root: &Path) -> Result<(), String> {
    if store_root.exists()
        && (fs::symlink_metadata(store_root)
            .map_err(|_| "store unavailable")?
            .file_type()
            .is_symlink()
            || !store_root.is_dir())
    {
        return Err("unsafe recap store".into());
    }
    Ok(())
}

fn operation_cancellation(
    context: &NativeServiceContext,
    invocation: &str,
    root: &Path,
) -> Cancellation {
    let cancel = Cancellation::default();
    if context.request.interface_id == bcode_tool::TOOL_SERVICE_INTERFACE_ID {
        cancel.set_process_admission(std::sync::Arc::new(
            super::process_admission::HostProcessAdmission {
                bridge: context.bridge.clone(),
                invocation: invocation.into(),
                root: root.to_path_buf(),
                sequence: std::sync::atomic::AtomicU64::new(0),
            },
        ));
    }
    cancel
}

fn transcript(args: &BTreeMap<String, String>, root: &Path) -> Result<Option<String>, String> {
    use std::io::Read as _;
    let Some(value) = args.get("transcript") else {
        return Ok(None);
    };
    let path = root
        .join(value)
        .canonicalize()
        .map_err(|_| "transcript unavailable")?;
    if !path.starts_with(root) || !path.is_file() {
        return Err("transcript must be a workspace file".into());
    }
    let mut text = String::new();
    fs::File::open(path)
        .map_err(|_| "transcript unavailable")?
        .take(1024 * 1024 + 1)
        .read_to_string(&mut text)
        .map_err(|_| "invalid transcript")?;
    if text.len() > 1024 * 1024 {
        return Err("transcript exceeds byte budget".into());
    }
    Ok(Some(text))
}
fn publish_export(root: &Path, id: &str, html: &str) -> Result<String, String> {
    let destination = root.join(format!("{id}.html"));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)
        .map_err(|_| "export destination exists or is unavailable")?;
    file.write_all(html.as_bytes())
        .map_err(|_| "recap export write failed")?;
    file.sync_all()
        .map_err(|_| "recap export durability uncertain")?;
    Ok(format!("Offline recap exported: {}", destination.display()))
}

fn engine_options(store_root: &Path) -> EngineOptions {
    EngineOptions {
        acquisition: AcquisitionOptions {
            yt_dlp: PathBuf::from("yt-dlp"),
            workspace: store_root.to_path_buf(),
            max_video_bytes: 512 * 1024 * 1024,
            max_duration_seconds: 7200.0,
            caption_language: "en".into(),
            timeout: Duration::from_secs(120),
        },
        transcription: None,
        tools: bcode_recap::media::MediaTools::default(),
        media: bcode_recap::media::MediaBudget::default(),
        composition: bcode_recap::composition::CompositionLimits::default(),
        export: bcode_recap::export::ExportBudget::default(),
    }
}

#[cfg(test)]
mod tests {
    use bcode_plugin_sdk::{NativeServiceContext, ServiceRequest};
    #[test]
    fn denied_inference_does_not_create_store_or_extract_media() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("video.mp4"), b"not-probed").unwrap();
        let context = NativeServiceContext {
            plugin_id: crate::PLUGIN_ID.into(),
            request: ServiceRequest {
                interface_id: bcode_command::COMMAND_INTERFACE_ID.into(),
                operation: "invoke".into(),
                payload: Vec::new(),
            },
            config: bcode_plugin_sdk::PluginConfigContext::default(),
            events: bcode_plugin_sdk::ServiceEventEmitter::default(),
            cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
            bridge: bcode_plugin_sdk::ServiceBridge::default(),
            transient_progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
        };
        let args = std::collections::BTreeMap::from([
            ("id".into(), "denied".into()),
            ("source".into(), "video.mp4".into()),
        ]);
        assert!(super::run(&context, "recap.create", "receipt", &args, workspace.path()).is_err());
        assert!(!workspace.path().join(".recap").exists());
        assert_eq!(std::fs::read_dir(workspace.path()).unwrap().count(), 1);
    }
    #[test]
    fn create_preparation_declares_reads_writes_without_spawning() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        std::fs::write(root.join("input.mp4"), b"source").unwrap();
        std::fs::write(root.join("captions.vtt"), b"WEBVTT\n").unwrap();
        let args = std::collections::BTreeMap::from([
            ("id".into(), "create-tool".into()),
            ("source".into(), "input.mp4".into()),
            ("transcript".into(), "captions.vtt".into()),
        ]);
        let facts = super::policy_operations("recap.create", &args, &root).unwrap();
        assert!(facts.iter().any(|fact| matches!(fact, bcode_plugin_sdk::ToolPolicyOperation::Write { paths, .. } if paths == &[root.join(".recap").display().to_string()])));
        assert_eq!(
            facts
                .iter()
                .filter(|fact| matches!(fact, bcode_plugin_sdk::ToolPolicyOperation::Read { .. }))
                .count(),
            2
        );
        assert!(!root.join(".recap").exists());
    }

    #[test]
    fn transcript_is_bounded_and_workspace_confined() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("captions.vtt"), "WEBVTT\n").unwrap();
        let mut args =
            std::collections::BTreeMap::from([("transcript".into(), "captions.vtt".into())]);
        let root = workspace.path().canonicalize().unwrap();
        assert_eq!(
            super::transcript(&args, &root).unwrap().as_deref(),
            Some("WEBVTT\n")
        );
        args.insert("transcript".into(), "../outside.vtt".into());
        assert!(super::transcript(&args, &root).is_err());
    }
    #[test]
    fn rejects_bad_identity_and_missing_refinement_before_side_effects() {
        let root = std::env::temp_dir();
        assert!(
            super::validate(
                "recap.inspect",
                &std::collections::BTreeMap::from([("id".into(), "../escape".into())]),
                &root
            )
            .is_err()
        );
        assert!(
            super::validate(
                "recap.refine",
                &std::collections::BTreeMap::from([("id".into(), "safe".into())]),
                &root
            )
            .is_err()
        );
    }
}
