//! Independent acquisition → media → composition → storage → offline export pipeline.
//!
//! New media is staged privately. Refinement never rewrites source assets, and the
//! previous document remains authoritative until a validated next revision commits.
use std::{fs, io::Read as _, path::PathBuf, time::Duration};

use crate::{
    composition::{Composer, CompositionError, CompositionLimits},
    contracts::{DOCUMENT_SCHEMA_VERSION, Evidence, GenerationIntent, RecapDocument, Source},
    export::{self, ExportBudget, HtmlExport},
    media::{self, MediaBudget, MediaTools, VisualAssets},
    model::{ImageAsset, ModelExecutor},
    process::Cancellation,
    source::{self, AcquisitionOptions, SourceInput},
    storage::DocumentStore,
};

/// Coarse semantic progress, independent of any UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Acquiring real source media and available captions.
    Acquisition,
    /// Probing and extracting actual source visuals.
    Media,
    /// Awaiting structured model composition.
    Composition,
    /// Committing the validated revision.
    Storage,
    /// Rendering an offline HTML artifact.
    Export,
    /// The requested operation succeeded, not merely its model call.
    Complete,
}

/// Inputs for noninteractive default generation or optional technical focus.
#[derive(Clone, Debug)]
pub struct GenerateRequest {
    /// Stable caller-selected document identity.
    pub id: String,
    /// Source identity (independent of document revision).
    pub source_id: String,
    /// Source title, kept as provenance; model creates the editorial title.
    pub source_title: String,
    /// Local video or validated `YouTube` location.
    pub input: SourceInput,
    /// Optional real timestamped VTT/SRT transcript supplied by the caller.
    pub transcript: Option<String>,
    /// Defaults to a general, non-conversational visual recap.
    pub intent: GenerationIntent,
}

/// Explicit tools and budgets; no automatic installation, credentials, or provider selection.
#[derive(Clone, Debug)]
pub struct EngineOptions {
    /// Acquisition workspace, downloader and budgets.
    pub acquisition: AcquisitionOptions,
    /// Optional caller-approved speech transcription when source captions are absent.
    pub transcription: Option<source::TranscriptionCommand>,
    /// Media executable selection.
    pub tools: MediaTools,
    /// Extraction budgets; `max_frames` is the default sampling count.
    pub media: MediaBudget,
    /// Model context and response allowances.
    pub composition: CompositionLimits,
    /// Portable export limits.
    pub export: ExportBudget,
}

/// Generated content and real extracted assets, kept alive before persistence.
#[derive(Debug)]
pub struct PreparedRecap {
    /// Fully validated AI-composed document.
    pub document: RecapDocument,
    /// Actual extracted files backing its asset registry.
    pub visuals: VisualAssets,
}

/// Domain-local errors; a failed model never masquerades as a generated recap.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// Acquisition failed.
    #[error(transparent)]
    Source(#[from] source::SourceError),
    /// Extraction failed.
    #[error(transparent)]
    Media(#[from] media::MediaError),
    /// Model composition or validation failed.
    #[error(transparent)]
    Composition(#[from] CompositionError),
    /// Storage or document contract failed.
    #[error(transparent)]
    Contract(#[from] crate::contracts::Error),
    /// Offline rendering failed.
    #[error(transparent)]
    Export(#[from] export::ExportError),
    /// Filesystem operation failed.
    #[error("recap engine I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// Cancellation before the commit point.
    #[error("recap operation cancelled")]
    Cancelled,
    /// Source/budget configuration or unsafe staging was rejected.
    #[error("invalid recap operation: {0}")]
    Invalid(&'static str),
}

/// Application service over injected model execution and recap-owned adapters.
pub struct Engine<'a> {
    /// Actual provider-neutral model implementation, never a fallback summarizer.
    pub executor: &'a dyn ModelExecutor,
    /// Owned persistence location.
    pub store: DocumentStore,
    /// Explicit operation options.
    pub options: EngineOptions,
}

impl Engine<'_> {
    /// Acquire, extract and compose without committing a document.
    ///
    /// # Errors
    /// Returns acquisition, media, cancellation, composition and contract failures.
    pub async fn prepare(
        &self,
        request: &GenerateRequest,
        cancel: &Cancellation,
        progress: &mut (dyn FnMut(Stage) + Send),
    ) -> Result<PreparedRecap, EngineError> {
        crate::contracts::validate_id(&request.id)?;
        crate::contracts::validate_id(&request.source_id)?;
        if request.source_title.trim().is_empty() {
            return Err(EngineError::Invalid("empty source title"));
        }
        stage(cancel, progress, Stage::Acquisition)?;
        let acquired = source::acquire(&request.input, &self.options.acquisition, cancel).await?;
        stage(cancel, progress, Stage::Media)?;
        let metadata = media::probe(&acquired.video, &self.options.tools, cancel).await?;
        let maximum = Duration::try_from_secs_f64(self.options.acquisition.max_duration_seconds)
            .map_err(|_| EngineError::Invalid("duration limit"))?;
        if u128::from(metadata.duration_ms) > maximum.as_millis() {
            return Err(EngineError::Invalid("source duration exceeds budget"));
        }
        let captions = if let Some(supplied) = &request.transcript {
            source::parse_subtitles(supplied)?
        } else if acquired.captions.is_empty() {
            if let Some(command) = &self.options.transcription {
                source::transcribe_with_command(&acquired.video, command, cancel).await?
            } else {
                vec![]
            }
        } else {
            acquired.captions.clone()
        };
        let transcript = source::transcript_segments(&captions, metadata.duration_ms)?;
        let (times, clips) = select_moments(
            metadata.duration_ms,
            &transcript,
            &request.intent,
            &self.options.media,
        );
        let visuals = media::extract_visuals(
            &acquired.video,
            &metadata,
            &media::VisualSelection {
                frame_times_ms: &times,
                clips: &clips,
            },
            &self.options.acquisition.workspace,
            &self.options.tools,
            &self.options.media,
            cancel,
        )
        .await?;
        let evidence = source_evidence(&transcript, &visuals.assets);
        let grounded = RecapDocument {
            schema_version: DOCUMENT_SCHEMA_VERSION,
            id: request.id.clone(),
            revision: 1,
            title: request.source_title.clone(),
            source: Source {
                id: request.source_id.clone(),
                title: request.source_title.clone(),
                url: match &request.input {
                    SourceInput::Youtube(url) => Some(source::youtube_url(url)?),
                    SourceInput::Local(_) => None,
                },
                duration_ms: metadata.duration_ms,
            },
            intent: request.intent.clone(),
            transcript,
            evidence,
            assets: visuals.assets.clone(),
            sections: vec![],
        };
        let images = read_images(
            &grounded,
            |asset, limit| {
                let path = source::owned_file(
                    visuals.directory.path(),
                    &visuals.directory.path().join(asset.path.as_str()),
                    limit,
                )?;
                read_bounded(path, limit)
            },
            self.options.composition.image_bytes,
        )?;
        stage(cancel, progress, Stage::Composition)?;
        let document = Composer {
            executor: self.executor,
            limits: self.options.composition,
        }
        .compose(&grounded, images, request.intent.clone(), None, cancel)
        .await?;
        Ok(PreparedRecap { document, visuals })
    }

    /// Generate and persist a new document; no existing document/assets are replaced.
    ///
    /// The atomic document save is the commit point. Once committed, cancellation
    /// does not retroactively report the operation as cancelled. HTML rendering is
    /// validated before commit; `export()` is a separate read-only operation whose
    /// later failure cannot roll back an already committed generation.
    ///
    /// # Errors
    /// Returns preparation, storage and cancellation errors. On a storage failure,
    /// an owned asset directory may remain for explicit recovery; it is not a recap.
    pub async fn generate(
        &self,
        request: &GenerateRequest,
        cancel: &Cancellation,
        progress: &mut (dyn FnMut(Stage) + Send),
    ) -> Result<RecapDocument, EngineError> {
        let prepared = self.prepare(request, cancel, progress).await?;
        stage(cancel, progress, Stage::Export)?;
        let _export = export::export_html(
            &prepared.document,
            prepared.visuals.directory.path(),
            self.options.export,
        )?;
        stage(cancel, progress, Stage::Storage)?;
        let destination = self.store.asset_directory(&prepared.document.id)?;
        let parent = destination
            .parent()
            .ok_or(EngineError::Invalid("asset root"))?;
        fs::create_dir_all(parent)?;
        if fs::symlink_metadata(parent)?.file_type().is_symlink() {
            return Err(EngineError::Invalid("symlink store root"));
        }
        // create_dir, not create_dir_all: never reuse or overwrite another operation's assets.
        fs::create_dir(&destination)?;
        for asset in &prepared.document.assets {
            check_cancelled(cancel)?;
            let from = source::owned_file(
                prepared.visuals.directory.path(),
                &prepared.visuals.directory.path().join(asset.path.as_str()),
                self.options.media.max_asset_bytes,
            )?;
            let target = destination.join(asset.path.as_str());
            let mut input = fs::File::open(from)?;
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(target)?;
            std::io::copy(&mut input, &mut output)?;
            output.sync_all()?;
        }
        fs::File::open(&destination)?.sync_all()?;
        check_cancelled(cancel)?;
        self.store.save(&prepared.document, None)?;
        progress(Stage::Complete);
        Ok(prepared.document)
    }

    /// Refine an existing recap with a focused intent and unchanged source evidence.
    ///
    /// The prior revision and assets remain untouched until a validated successor
    /// commits under the store's optimistic revision precondition. Candidate HTML
    /// is rendered before commit, so export failure preserves the prior revision.
    ///
    /// # Errors
    /// Returns read, cancellation, model, validation, or optimistic save failures.
    pub async fn refine(
        &self,
        id: &str,
        intent: GenerationIntent,
        cancel: &Cancellation,
        progress: &mut (dyn FnMut(Stage) + Send),
    ) -> Result<RecapDocument, EngineError> {
        check_cancelled(cancel)?;
        let previous = self.store.load(id)?;
        let images = read_images(
            &previous,
            |asset, limit| Ok(self.store.read_asset(id, asset, limit)?),
            self.options.composition.image_bytes,
        )?;
        stage(cancel, progress, Stage::Composition)?;
        let candidate = Composer {
            executor: self.executor,
            limits: self.options.composition,
        }
        .compose(&previous, images, intent, Some(&previous), cancel)
        .await?;
        stage(cancel, progress, Stage::Export)?;
        let _export = export::export_html(
            &candidate,
            &self.store.asset_directory(id)?,
            self.options.export,
        )?;
        stage(cancel, progress, Stage::Storage)?;
        self.store.save(&candidate, Some(previous.revision))?;
        progress(Stage::Complete);
        Ok(candidate)
    }

    /// Render one self-contained offline HTML file in memory with explicit media
    /// degradation reporting; callers choose where to publish the resulting HTML.
    ///
    /// # Errors
    /// Returns cancellation, document read or export validation failures.
    pub fn export(
        &self,
        id: &str,
        cancel: &Cancellation,
        progress: &mut (dyn FnMut(Stage) + Send),
    ) -> Result<HtmlExport, EngineError> {
        stage(cancel, progress, Stage::Export)?;
        let document = self.store.load(id)?;
        let result = export::export_html(
            &document,
            &self.store.asset_directory(id)?,
            self.options.export,
        )?;
        check_cancelled(cancel)?;
        progress(Stage::Complete);
        Ok(result)
    }
}

fn source_evidence(
    transcript: &[crate::contracts::TranscriptSegment],
    assets: &[crate::contracts::Asset],
) -> Vec<Evidence> {
    transcript
        .iter()
        .enumerate()
        .map(|(index, segment)| Evidence {
            id: format!("speech-{index}"),
            range: segment.range,
            transcript_segment_ids: vec![segment.id.clone()],
        })
        .chain(assets.iter().map(|asset| Evidence {
            id: format!("evidence-{}", asset.id),
            range: asset.range,
            transcript_segment_ids: vec![],
        }))
        .collect()
}

// Candidate acquisition is deterministic, not an AI result. Captions/focus guide
// source moments; the model still decides which inspected frames explain the recap.
// With no captions, sparse coverage is only a candidate set, never scene semantics.
fn select_moments(
    duration_ms: u64,
    transcript: &[crate::contracts::TranscriptSegment],
    intent: &GenerationIntent,
    budget: &MediaBudget,
) -> (Vec<u64>, Vec<crate::contracts::TimeRange>) {
    use std::collections::BTreeSet;
    let keywords: BTreeSet<_> = intent
        .focus
        .as_deref()
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_lowercase)
        .collect();
    let mut ranked: Vec<_> = transcript
        .iter()
        .map(|segment| {
            let text = segment.text.to_lowercase();
            let relevance = keywords
                .iter()
                .filter(|word| text.contains(word.as_str()))
                .count();
            // Prefer substantial source explanations, not single-word caption fragments.
            (
                relevance,
                segment.text.split_whitespace().take(100).count(),
                segment,
            )
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(b.1.cmp(&a.1))
            .then(a.2.range.start_ms.cmp(&b.2.range.start_ms))
    });
    let spacing = duration_ms / u64::try_from(budget.max_frames.max(1)).unwrap_or(u64::MAX) / 2;
    let mut moments = BTreeSet::new();
    for (_, _, segment) in ranked {
        if moments.len() >= budget.max_frames {
            break;
        }
        let midpoint = segment.range.start_ms + (segment.range.end_ms - segment.range.start_ms) / 2;
        if moments
            .iter()
            .all(|time: &u64| time.abs_diff(midpoint) >= spacing.max(1))
        {
            moments.insert(midpoint);
        }
    }
    for time in media::sample_frame_times(duration_ms, budget.max_frames) {
        if moments.len() >= budget.max_frames {
            break;
        }
        if moments
            .iter()
            .all(|existing| existing.abs_diff(time) >= spacing.max(1))
        {
            moments.insert(time);
        }
    }
    let ordered: Vec<_> = moments.into_iter().collect();
    let clips = motion_candidates(duration_ms, &ordered, budget);
    (ordered, clips)
}

fn motion_candidates(
    duration_ms: u64,
    moments: &[u64],
    budget: &MediaBudget,
) -> Vec<crate::contracts::TimeRange> {
    let mut clips: Vec<crate::contracts::TimeRange> = Vec::new();
    let mut remaining = budget.max_clip_duration_ms;
    // Motion is real source media around selected explanatory moments. Spread the
    // default candidates across the source instead of spending the entire clip
    // budget on introduction/setup merely because moments are chronological.
    let clip_count = budget.max_clips.min(moments.len());
    for slot in 0..clip_count {
        let time = moments[slot * moments.len() / clip_count + moments.len() / clip_count / 2];
        if remaining < 1000 {
            break;
        }
        let length = 6000.min(remaining).min(duration_ms);
        if length < 1000 {
            break;
        }
        let start_ms = time.saturating_sub(length / 2).min(duration_ms - length);
        let range = crate::contracts::TimeRange {
            start_ms,
            end_ms: start_ms + length,
        };
        if clips
            .iter()
            .any(|prior| prior.start_ms < range.end_ms && range.start_ms < prior.end_ms)
        {
            continue;
        }
        clips.push(range);
        remaining -= length;
    }
    clips
}

fn read_bounded(path: PathBuf, limit: u64) -> Result<Vec<u8>, EngineError> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err(EngineError::Invalid("image byte budget"));
    }
    Ok(bytes)
}

fn read_images(
    document: &RecapDocument,
    mut read: impl FnMut(&crate::contracts::Asset, u64) -> Result<Vec<u8>, EngineError>,
    budget: usize,
) -> Result<Vec<ImageAsset>, EngineError> {
    let mut remaining = u64::try_from(budget).unwrap_or(u64::MAX);
    let mut images = Vec::new();
    for asset in document
        .assets
        .iter()
        .filter(|asset| asset.mime_type.starts_with("image/"))
    {
        let bytes = read(asset, remaining)?;
        remaining = remaining
            .checked_sub(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
            .ok_or(EngineError::Invalid("image byte budget"))?;
        images.push(ImageAsset {
            asset_id: asset.id.clone(),
            mime_type: asset.mime_type.clone(),
            bytes,
        });
    }
    Ok(images)
}

fn stage(
    cancel: &Cancellation,
    progress: &mut (dyn FnMut(Stage) + Send),
    next: Stage,
) -> Result<(), EngineError> {
    check_cancelled(cancel)?;
    progress(next);
    check_cancelled(cancel)
}

fn check_cancelled(cancel: &Cancellation) -> Result<(), EngineError> {
    if cancel.is_cancelled() {
        return Err(EngineError::Cancelled);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{Depth, TimeRange, TranscriptSegment};

    #[test]
    fn focused_source_moments_prioritize_explanation_and_extract_real_motion() {
        let transcript = vec![
            TranscriptSegment {
                id: "intro".into(),
                range: TimeRange {
                    start_ms: 1000,
                    end_ms: 2000,
                },
                text: "Introduction to the talk and overview of the presentation".into(),
            },
            TranscriptSegment {
                id: "detail".into(),
                range: TimeRange {
                    start_ms: 40_000,
                    end_ms: 44_000,
                },
                text: "The compiler optimization eliminates repeated allocation".into(),
            },
        ];
        let intent = GenerationIntent {
            focus: Some("compiler allocation".into()),
            depth: Depth::Detailed,
        };
        let budget = MediaBudget {
            max_frames: 1,
            max_clips: 1,
            ..MediaBudget::default()
        };
        let (frames, clips) = select_moments(60_000, &transcript, &intent, &budget);
        assert_eq!(frames, vec![42_000]);
        assert_eq!(clips.len(), 1);
        assert!(clips[0].start_ms <= frames[0] && frames[0] < clips[0].end_ms);
        assert!(clips[0].validate(60_000).is_ok());
    }

    #[test]
    fn default_motion_candidates_cover_later_source_not_only_introduction() {
        let budget = MediaBudget::default();
        let (_, clips) = select_moments(800_000, &[], &GenerationIntent::default(), &budget);
        assert_eq!(clips.len(), 3);
        assert!(clips[0].start_ms < 300_000);
        assert!(clips[1].start_ms > 300_000);
        assert!(clips[2].start_ms > 550_000);
    }

    #[test]
    fn captionless_candidates_and_motion_stay_within_explicit_budgets() {
        let budget = MediaBudget {
            max_frames: 8,
            max_clips: 2,
            max_clip_duration_ms: 9000,
            ..MediaBudget::default()
        };
        let (frames, clips) = select_moments(90_000, &[], &GenerationIntent::default(), &budget);
        assert!(!frames.is_empty());
        assert!(frames.len() <= budget.max_frames);
        assert!(!clips.is_empty());
        assert!(clips.len() <= budget.max_clips);
        assert!(
            clips
                .iter()
                .map(|range| range.end_ms - range.start_ms)
                .sum::<u64>()
                <= budget.max_clip_duration_ms
        );
        for range in clips {
            range.validate(90_000).unwrap();
        }
    }
}
