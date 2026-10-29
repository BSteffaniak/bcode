//! Real ffprobe metadata and budgeted JPEG/silent MP4 extraction.
use crate::{
    contracts::{Asset, AssetPath, TimeRange},
    process::{self, Cancellation, ProcessRequest},
    source,
};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// Local media executables, selected explicitly by the application.
#[derive(Clone, Debug)]
pub struct MediaTools {
    /// ffmpeg executable.
    pub ffmpeg: PathBuf,
    /// ffprobe executable.
    pub ffprobe: PathBuf,
}
impl Default for MediaTools {
    fn default() -> Self {
        Self {
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
        }
    }
}
/// Validated source metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoMetadata {
    /// Duration in milliseconds.
    pub duration_ms: u64,
    /// Encoded video width.
    pub width: u32,
    /// Encoded video height.
    pub height: u32,
    /// Whether an audio stream is present.
    pub has_audio: bool,
}
/// Practical extraction limits, independent of source length.
#[derive(Clone, Debug)]
pub struct MediaBudget {
    /// Maximum requested still frames.
    pub max_frames: usize,
    /// Maximum requested clips.
    pub max_clips: usize,
    /// Maximum total selected clip duration.
    pub max_clip_duration_ms: u64,
    /// Maximum output width (height preserves aspect ratio).
    pub max_width: u32,
    /// Maximum bytes for all assets.
    pub max_asset_bytes: u64,
    /// Deadline for each ffmpeg invocation.
    pub operation_timeout: Duration,
}
impl Default for MediaBudget {
    fn default() -> Self {
        Self {
            max_frames: 12,
            max_clips: 3,
            max_clip_duration_ms: 24_000,
            max_width: 960,
            max_asset_bytes: 24 * 1024 * 1024,
            operation_timeout: Duration::from_secs(60),
        }
    }
}
/// Extracted portable asset records and the owned files backing them.
#[derive(Debug)]
pub struct VisualAssets {
    /// Portable source-grounded assets.
    pub assets: Vec<Asset>,
    /// Unique owned directory; retain while consuming files.
    pub directory: tempfile::TempDir,
}
/// Metadata/extraction failure.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// Invalid metadata, range or budget.
    #[error("invalid media: {0}")]
    Invalid(&'static str),
    /// External tool failed.
    #[error(transparent)]
    Process(#[from] process::ProcessError),
    /// Unsafe or missing input/output file.
    #[error(transparent)]
    Source(#[from] source::SourceError),
    /// Invalid ffprobe response.
    #[error("invalid ffprobe response")]
    Json(#[from] serde_json::Error),
    /// Filesystem failure.
    #[error("media I/O failed")]
    Io(#[from] std::io::Error),
    /// Contract rejected generated provenance.
    #[error(transparent)]
    Contract(#[from] crate::contracts::Error),
}
fn seconds_ms(seconds: f64) -> Option<u64> {
    if !seconds.is_finite() || !(0.001..=604_800.0).contains(&seconds) {
        return None;
    }
    // A checked, bounded Duration conversion avoids saturating float casts.
    u64::try_from(Duration::try_from_secs_f64(seconds).ok()?.as_millis()).ok()
}
fn decode_probe(bytes: &[u8]) -> Result<VideoMetadata, MediaError> {
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    let streams = value
        .get("streams")
        .and_then(serde_json::Value::as_array)
        .ok_or(MediaError::Invalid("missing streams"))?;
    let video = streams
        .iter()
        .find(|stream| {
            stream.get("codec_type").and_then(serde_json::Value::as_str) == Some("video")
                && stream
                    .pointer("/disposition/attached_pic")
                    .and_then(serde_json::Value::as_u64)
                    != Some(1)
        })
        .ok_or(MediaError::Invalid("no moving video stream"))?;
    let dimension = |name: &str| {
        video
            .get(name)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| (1..=16_384).contains(n))
            .ok_or(MediaError::Invalid("invalid video dimensions"))
    };
    let duration_ms = value
        .pointer("/format/duration")
        .and_then(serde_json::Value::as_str)
        .and_then(|s| s.parse().ok())
        .or_else(|| {
            video
                .get("duration")
                .and_then(serde_json::Value::as_str)
                .and_then(|s| s.parse().ok())
        })
        .and_then(seconds_ms)
        .ok_or(MediaError::Invalid("unknown or excessive duration"))?;
    Ok(VideoMetadata {
        duration_ms,
        width: dimension("width")?,
        height: dimension("height")?,
        has_audio: streams.iter().any(|stream| {
            stream.get("codec_type").and_then(serde_json::Value::as_str) == Some("audio")
        }),
    })
}
/// Probe a regular local file; protocols/playlists are not accepted as input URLs.
/// # Errors
/// Rejects audio-only media, unbounded/invalid metadata, tool failure or cancellation.
pub async fn probe(
    video: &Path,
    tools: &MediaTools,
    cancel: &Cancellation,
) -> Result<VideoMetadata, MediaError> {
    let video = source::local_video(video, u64::MAX)?;
    let request = ProcessRequest { program: tools.ffprobe.clone(), args: ["-v", "error", "-protocol_whitelist", "file", "-format_whitelist", "mov,matroska,webm,avi,mpegts,mpeg,ogg,flv", "-show_entries", "format=duration:stream=codec_type,width,height,duration:stream_disposition=attached_pic", "-of", "json"].iter().map(Into::into).chain(std::iter::once(video.into_os_string())).collect(), cwd: None, timeout: Duration::from_secs(20), max_output_bytes: 128 * 1024, file_budget: None };
    decode_probe(&Box::pin(process::run(&request, cancel)).await?.stdout)
}
fn extraction_args(
    video: &Path,
    range: TimeRange,
    is_clip: bool,
    scale: &str,
) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = [
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-n",
        "-threads",
        "1",
        "-protocol_whitelist",
        "file",
        "-format_whitelist",
        "mov,matroska,webm,avi,mpegts,mpeg,ogg,flv",
        "-ss",
        &format!("{}.{:03}", range.start_ms / 1000, range.start_ms % 1000),
        "-i",
    ]
    .iter()
    .map(Into::into)
    .collect();
    args.push(video.as_os_str().to_owned());
    args.extend(
        [
            "-map",
            "0:V:0",
            "-an",
            "-sn",
            "-dn",
            "-map_metadata",
            "-1",
            "-vf",
            scale,
            "-threads",
            "1",
        ]
        .iter()
        .map(std::ffi::OsString::from),
    );
    if is_clip {
        let length = range.end_ms - range.start_ms;
        args.extend(
            [
                "-t",
                &format!("{}.{:03}", length / 1000, length % 1000),
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-crf",
                "28",
                "-pix_fmt",
                "yuv420p",
                "-r",
                "24",
                "-movflags",
                "+faststart",
            ]
            .iter()
            .map(std::ffi::OsString::from),
        );
    } else {
        args.extend(
            ["-frames:v", "1", "-c:v", "mjpeg", "-q:v", "4"]
                .iter()
                .map(std::ffi::OsString::from),
        );
    }
    args
}
/// Source-aligned visual selections.
#[derive(Clone, Copy, Debug)]
pub struct VisualSelection<'a> {
    /// Source still positions in milliseconds.
    pub frame_times_ms: &'a [u64],
    /// Source intervals selected as silent clips.
    pub clips: &'a [TimeRange],
}
/// Extract requested source stills and selected silent clips into a unique owned directory.
/// No transcript-only fallback is returned when visual extraction fails.
/// # Errors
/// Rejects out-of-source ranges, exhausted budgets, unsafe output or tool failure.
pub async fn extract_visuals(
    video: &Path,
    metadata: &VideoMetadata,
    selection: &VisualSelection<'_>,
    workspace: &Path,
    tools: &MediaTools,
    budget: &MediaBudget,
    cancel: &Cancellation,
) -> Result<VisualAssets, MediaError> {
    let VisualSelection {
        frame_times_ms,
        clips,
    } = *selection;
    if frame_times_ms.is_empty()
        || frame_times_ms.len() > budget.max_frames
        || clips.len() > budget.max_clips
        || budget.max_width < 2
        || budget.max_width > 3840
        || budget.max_asset_bytes == 0
    {
        return Err(MediaError::Invalid("visual budget"));
    }
    let mut total_duration = 0_u64;
    for range in clips {
        range.validate(metadata.duration_ms)?;
        total_duration = total_duration
            .checked_add(range.end_ms - range.start_ms)
            .ok_or(MediaError::Invalid("clip duration overflow"))?;
    }
    if total_duration > budget.max_clip_duration_ms
        || frame_times_ms
            .iter()
            .any(|time| *time >= metadata.duration_ms)
    {
        return Err(MediaError::Invalid("source range or clip duration budget"));
    }
    let video = source::local_video(video, u64::MAX)?;
    let directory = tempfile::Builder::new()
        .prefix("recap-assets-")
        .tempdir_in(workspace.canonicalize()?)?;
    let mut assets = Vec::new();
    let scale = format!("scale=w='min({},iw)':h=-2", budget.max_width);
    let mut used_bytes = 0_u64;
    for (index, range) in frame_times_ms
        .iter()
        .map(|time| {
            (
                false,
                TimeRange {
                    start_ms: *time,
                    end_ms: time + 1,
                },
            )
        })
        .chain(clips.iter().copied().map(|range| (true, range)))
        .enumerate()
    {
        let (is_clip, range) = range;
        let name = format!("visual-{index:04}.{}", if is_clip { "mp4" } else { "jpg" });
        let path = directory.path().join(&name);
        let mut args = extraction_args(&video, range, is_clip, &scale);
        let remaining = budget
            .max_asset_bytes
            .checked_sub(used_bytes)
            .filter(|bytes| *bytes > 0)
            .ok_or(MediaError::Invalid("asset byte budget"))?;
        args.extend([
            "-fs".into(),
            remaining.to_string().into(),
            path.as_os_str().to_owned(),
        ]);
        let request = ProcessRequest {
            program: tools.ffmpeg.clone(),
            args,
            cwd: None,
            timeout: budget.operation_timeout,
            max_output_bytes: 128 * 1024,
            file_budget: Some((directory.path().to_owned(), budget.max_asset_bytes)),
        };
        Box::pin(process::run(&request, cancel)).await?;
        let checked = source::owned_file(directory.path(), &path, remaining)?;
        used_bytes += checked.metadata()?.len();
        if is_clip {
            let actual = Box::pin(probe(&checked, tools, cancel)).await?;
            if actual.has_audio || actual.duration_ms.abs_diff(range.end_ms - range.start_ms) > 150
            {
                return Err(MediaError::Invalid("clip truncated or not silent"));
            }
        }
        assets.push(Asset {
            id: format!("visual-{index:04}"),
            path: AssetPath::new(name)?,
            mime_type: if is_clip { "video/mp4" } else { "image/jpeg" }.into(),
            range,
        });
    }
    Ok(VisualAssets { assets, directory })
}
/// Uniformly sample source positions without reading or decoding the whole source.
#[must_use]
pub fn sample_frame_times(duration_ms: u64, count: usize) -> Vec<u64> {
    if duration_ms == 0 || count == 0 {
        return vec![];
    }
    let count = count
        .min(100)
        .min(usize::try_from(duration_ms).unwrap_or(usize::MAX));
    (0..count)
        .map(|index| {
            duration_ms / u64::try_from(count + 1).unwrap_or(101)
                * u64::try_from(index + 1).unwrap_or(100)
        })
        .collect()
}
/// Why a frame was requested for inspection; this is not publication evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateReason {
    /// Explicit source position selected by the caller for closer inspection.
    Targeted,
    /// A bounded ffmpeg scene-change observation.
    SceneChange,
    /// A source transcript cue boundary.
    TranscriptCue,
    /// Temporal anchor, including the source tail.
    TemporalAnchor,
}
/// An inspection request, separate from the document's published asset registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualCandidate {
    /// Source position in milliseconds.
    pub time_ms: u64,
    /// Selection provenance; not a claim about image contents.
    pub reason: CandidateReason,
}
/// Explicit Detailed inspection budgets; Standard media defaults are unchanged.
#[derive(Clone, Debug)]
pub struct InspectionBudget {
    /// Maximum merged candidates, no more than 128.
    pub max_candidates: usize,
    /// Maximum scene observations, no more than 128.
    pub max_scene_candidates: usize,
    /// Number of evenly distributed transcript cue positions.
    pub max_transcript_candidates: usize,
    /// Number of temporal anchors (at least two for nonempty video).
    pub temporal_anchors: usize,
    /// Minimum distance between merged candidates. Targeted positions take priority.
    pub min_spacing_ms: u64,
    /// Deadline for the scene detector, independent of extraction deadlines.
    pub scene_timeout: Duration,
}
impl Default for InspectionBudget {
    fn default() -> Self {
        Self {
            max_candidates: 96,
            max_scene_candidates: 48,
            max_transcript_candidates: 24,
            temporal_anchors: 12,
            min_spacing_ms: 500,
            scene_timeout: Duration::from_secs(120),
        }
    }
}
/// Bounded observations only. No set of these samples proves complete visual coverage.
#[derive(Debug)]
pub struct VisualCandidates {
    /// Sorted unique source positions to inspect before choosing published assets.
    pub candidates: Vec<VisualCandidate>,
    /// Detector hit its observation cap; later scenes may not have been scanned.
    pub scene_limit_reached: bool,
    /// Merge/spacing budgets omitted some candidate positions.
    pub selection_limited: bool,
}
fn validate_inspection_budget(budget: &InspectionBudget) -> Result<(), MediaError> {
    if !(2..=128).contains(&budget.max_candidates)
        || !(1..=128).contains(&budget.max_scene_candidates)
        || budget.max_transcript_candidates > 128
        || !(2..=128).contains(&budget.temporal_anchors)
        || budget.scene_timeout.is_zero()
    {
        return Err(MediaError::Invalid("inspection budget"));
    }
    Ok(())
}
fn scene_times(bytes: &[u8], duration_ms: u64, cap: usize) -> Result<Vec<u64>, MediaError> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| MediaError::Invalid("scene output encoding"))?;
    let mut times = Vec::new();
    for line in text.lines().filter(|line| line.contains("showinfo")) {
        if let Some(value) = line
            .split_once("pts_time:")
            .and_then(|(_, tail)| tail.split_whitespace().next())
        {
            let time = value
                .parse::<f64>()
                .ok()
                .filter(|seconds| seconds.is_finite() && *seconds >= 0.0 && *seconds <= 604_800.0)
                .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
                .and_then(|duration| u64::try_from(duration.as_millis()).ok())
                .filter(|time| *time < duration_ms)
                .ok_or(MediaError::Invalid("scene timestamp"))?;
            if times.len() >= cap {
                return Err(MediaError::Invalid("scene output budget"));
            }
            times.push(time);
        }
    }
    Ok(times)
}
/// Visit endpoints, then progressively bisect the remaining observations. This
/// keeps a tight selection budget from taking only the beginning of a source.
fn distributed_times(mut times: Vec<u64>) -> Vec<u64> {
    times.sort_unstable();
    times.dedup();
    if times.len() < 3 {
        return times;
    }
    let mut ordered = vec![times[0], times[times.len() - 1]];
    let mut pending = vec![(1, times.len() - 1)];
    let mut cursor = 0;
    while let Some(&(start, end)) = pending.get(cursor) {
        cursor += 1;
        if start < end {
            let middle = start + (end - start) / 2;
            ordered.push(times[middle]);
            pending.push((start, middle));
            pending.push((middle + 1, end));
        }
    }
    ordered
}

/// Deterministically combine bounded scene observations, source cues, caller targets,
/// and temporal anchors.
///
/// This pure planner is also usable by headless hosts with their
/// own scene detector. Samples never imply complete visual coverage.
/// # Errors
/// Rejects invalid budgets, out-of-source positions and excessive candidate/cue counts.
pub fn plan_visual_candidates(
    duration_ms: u64,
    scenes: &[u64],
    transcript: &[crate::contracts::TranscriptSegment],
    targeted: &[u64],
    budget: &InspectionBudget,
) -> Result<VisualCandidates, MediaError> {
    validate_inspection_budget(budget)?;
    if duration_ms == 0
        || duration_ms > 604_800_000
        || targeted.len() > budget.max_candidates
        || transcript.len() > 50_000
        || scenes.len() > budget.max_scene_candidates
        || targeted
            .iter()
            .chain(scenes)
            .any(|time| *time >= duration_ms)
        || transcript
            .iter()
            .any(|segment| segment.range.validate(duration_ms).is_err())
    {
        return Err(MediaError::Invalid("inspection source positions or budget"));
    }
    let mut candidates: Vec<VisualCandidate> = Vec::new();
    let mut limited = false;
    let mut insert = |time_ms, reason| {
        if candidates
            .iter()
            .any(|candidate| candidate.time_ms == time_ms)
        {
            return;
        }
        if candidates.len() >= budget.max_candidates
            || (reason != CandidateReason::Targeted
                && candidates
                    .iter()
                    .any(|candidate| candidate.time_ms.abs_diff(time_ms) < budget.min_spacing_ms))
        {
            limited = true;
        } else {
            candidates.push(VisualCandidate { time_ms, reason });
        }
    };
    for time in targeted {
        insert(*time, CandidateReason::Targeted);
    }
    // Reserve temporal distribution before scene/cue density can consume the budget.
    for index in 0..budget.temporal_anchors {
        let denominator = u64::try_from(budget.temporal_anchors - 1).unwrap_or(127);
        let numerator = u64::try_from(index).unwrap_or(127);
        // Stay inside the final half-second rather than requesting a position
        // after the last displayed frame (container duration includes its hold).
        // These remain samples, not proof of complete tail inspection.
        let last = duration_ms.saturating_sub(500).max((duration_ms - 1) / 2);
        let time =
            (last / denominator) * numerator + (last % denominator) * numerator / denominator;
        insert(time, CandidateReason::TemporalAnchor);
    }
    let scene_order = distributed_times(scenes.to_vec());
    let count = transcript.len().min(budget.max_transcript_candidates);
    let cues = distributed_times(
        (0..count)
            .map(|index| {
                let position = if count == 1 {
                    0
                } else {
                    index * (transcript.len() - 1) / (count - 1)
                };
                transcript[position].range.start_ms
            })
            .collect(),
    );
    // Alternate source classes rather than letting dense scene observations
    // exhaust all capacity before a transcript cue is considered. Within each
    // class, inspect both ends before progressively filling interior positions.
    for index in 0..scene_order.len().max(cues.len()) {
        if let Some(time) = cues.get(index) {
            insert(*time, CandidateReason::TranscriptCue);
        }
        if let Some(time) = scene_order.get(index) {
            insert(*time, CandidateReason::SceneChange);
        }
    }
    candidates.sort_by_key(|candidate| candidate.time_ms);
    Ok(VisualCandidates {
        candidates,
        scene_limit_reached: scenes.len() == budget.max_scene_candidates,
        selection_limited: limited || transcript.len() > count,
    })
}
/// Detect scene changes at bounded decode resolution/rate, then merge transcript and
/// targeted positions with temporal anchors.
///
/// This samples, never establishes complete
/// visual coverage. Detector exhaustion/failure is explicit, not a transcript-only fallback.
/// No frames are published or added to a document by this function.
/// # Errors
/// Rejects invalid budgets/ranges, excessive transcript count, tool failure or cancellation.
pub async fn discover_visual_candidates(
    video: &Path,
    metadata: &VideoMetadata,
    transcript: &[crate::contracts::TranscriptSegment],
    targeted: &[u64],
    tools: &MediaTools,
    budget: &InspectionBudget,
    cancel: &Cancellation,
) -> Result<VisualCandidates, MediaError> {
    // Validate all caller input before launching a subprocess.
    plan_visual_candidates(metadata.duration_ms, &[], transcript, targeted, budget)?;
    let video = source::local_video(video, u64::MAX)?;
    let request = ProcessRequest {
        program: tools.ffmpeg.clone(),
        args: [
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "info",
            "-protocol_whitelist",
            "file",
            "-format_whitelist",
            "mov,matroska,webm,avi,mpegts,mpeg,ogg,flv",
            "-i",
        ]
        .iter()
        .map(Into::into)
        .chain(std::iter::once(video.into_os_string()))
        .chain([
            "-map".into(),
            "0:V:0".into(),
            "-threads".into(),
            "1".into(),
            "-an".into(),
            "-sn".into(),
            "-dn".into(),
            "-vf".into(),
            "fps=2,scale=160:-2,select=gt(scene\\,0.3),showinfo".into(),
            "-frames:v".into(),
            budget.max_scene_candidates.to_string().into(),
            "-fps_mode".into(),
            "vfr".into(),
            "-f".into(),
            "null".into(),
            "-".into(),
        ])
        .collect(),
        cwd: None,
        timeout: budget.scene_timeout,
        max_output_bytes: 256 * 1024,
        file_budget: None,
    };
    let output = Box::pin(process::run(&request, cancel)).await?;
    let scenes = scene_times(
        &output.stderr,
        metadata.duration_ms,
        budget.max_scene_candidates,
    )?;
    plan_visual_candidates(metadata.duration_ms, &scenes, transcript, targeted, budget)
}
/// Temporary inspection images, deliberately separate from a published asset selection.
///
/// The directory remains owned for the lifetime of this value. Callers may retain
/// these images in the host evidence registry for refinement; doing so is not a
/// publication choice. Renderers/exporters should read only assets referenced by blocks.
#[derive(Debug)]
pub struct InspectionFrames {
    /// Private analysis images; do not automatically register these in a document.
    pub visuals: VisualAssets,
}
/// Extract bounded candidate JPEGs using a caller-selected inspection media budget.
/// Published media budgets are not changed or consumed implicitly.
/// # Errors
/// Rejects more than 128 candidates and any ordinary media extraction failure.
pub async fn extract_inspection_frames(
    video: &Path,
    metadata: &VideoMetadata,
    candidates: &VisualCandidates,
    workspace: &Path,
    tools: &MediaTools,
    budget: &MediaBudget,
    cancel: &Cancellation,
) -> Result<InspectionFrames, MediaError> {
    if candidates.candidates.len() > 128 {
        return Err(MediaError::Invalid("inspection frame budget"));
    }
    let times: Vec<_> = candidates
        .candidates
        .iter()
        .map(|candidate| candidate.time_ms)
        .collect();
    let visuals = extract_visuals(
        video,
        metadata,
        &VisualSelection {
            frame_times_ms: &times,
            clips: &[],
        },
        workspace,
        tools,
        budget,
        cancel,
    )
    .await?;
    Ok(InspectionFrames { visuals })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_candidates_are_bounded_deterministic_and_retain_tail_anchor() {
        let budget = InspectionBudget {
            max_candidates: 6,
            temporal_anchors: 3,
            min_spacing_ms: 0,
            ..InspectionBudget::default()
        };
        let transcript = vec![crate::contracts::TranscriptSegment {
            id: "cue-original".into(),
            range: TimeRange {
                start_ms: 7000,
                end_ms: 8000,
            },
            text: "untrusted cue".into(),
        }];
        let result =
            plan_visual_candidates(10_000, &[2000, 3000], &transcript, &[6500], &budget).unwrap();
        assert_eq!(result.candidates.len(), 6);
        assert!(result.selection_limited);
        assert!(result.candidates.iter().any(|c| c.time_ms == 9500));
        assert!(
            result
                .candidates
                .iter()
                .any(|c| c.time_ms == 6500 && c.reason == CandidateReason::Targeted)
        );
        assert!(
            result
                .candidates
                .iter()
                .any(|c| c.reason == CandidateReason::SceneChange)
        );
        let again =
            plan_visual_candidates(10_000, &[2000, 3000], &transcript, &[6500], &budget).unwrap();
        assert_eq!(result.candidates, again.candidates);
    }
    #[test]
    fn dense_opening_scenes_do_not_starve_late_transcript_cues() {
        let budget = InspectionBudget {
            max_candidates: 5,
            temporal_anchors: 2,
            min_spacing_ms: 100,
            ..InspectionBudget::default()
        };
        let transcript = vec![crate::contracts::TranscriptSegment {
            id: "late-caveat".into(),
            range: TimeRange {
                start_ms: 8_000,
                end_ms: 9_000,
            },
            text: "Important final qualification".into(),
        }];
        let result = plan_visual_candidates(
            10_000,
            &[1_000, 1_500, 2_000, 2_500, 3_000],
            &transcript,
            &[],
            &budget,
        )
        .unwrap();
        assert_eq!(result.candidates.len(), 5);
        assert!(result.selection_limited);
        assert!(result.candidates.iter().any(|candidate| {
            candidate.time_ms == 8_000 && candidate.reason == CandidateReason::TranscriptCue
        }));
        assert!(
            result
                .candidates
                .iter()
                .any(|candidate| candidate.time_ms == 0)
        );
        assert!(
            result
                .candidates
                .iter()
                .any(|candidate| candidate.time_ms == 9_500)
        );
        assert!(
            result
                .candidates
                .windows(2)
                .all(|pair| pair[0].time_ms < pair[1].time_ms)
        );
    }

    #[test]
    fn scene_order_and_duplicates_do_not_bias_tight_selection() {
        let budget = InspectionBudget {
            max_candidates: 4,
            temporal_anchors: 2,
            min_spacing_ms: 0,
            ..InspectionBudget::default()
        };
        let first =
            plan_visual_candidates(10_000, &[1_000, 2_000, 3_000, 8_000], &[], &[], &budget)
                .unwrap();
        let shuffled = plan_visual_candidates(
            10_000,
            &[3_000, 8_000, 1_000, 2_000, 1_000],
            &[],
            &[],
            &budget,
        )
        .unwrap();
        assert_eq!(first.candidates, shuffled.candidates);
        assert!(
            first
                .candidates
                .iter()
                .any(|candidate| candidate.time_ms == 8_000)
        );
        assert!(first.selection_limited);
        // Explicit targets win both capacity and nearby-source spacing conflicts.
        let targeted = plan_visual_candidates(
            10_000,
            &[1_000, 2_000, 3_000, 8_000],
            &[],
            &[1_001, 7_999, 5_000, 6_000],
            &InspectionBudget {
                min_spacing_ms: 500,
                ..budget
            },
        )
        .unwrap();
        assert_eq!(targeted.candidates.len(), 4);
        assert!(
            targeted
                .candidates
                .iter()
                .all(|candidate| candidate.reason == CandidateReason::Targeted)
        );
        assert!(targeted.selection_limited);
    }

    #[test]
    fn transcript_positions_and_scene_exhaustion_are_explicit() {
        let budget = InspectionBudget {
            max_scene_candidates: 1,
            temporal_anchors: 2,
            min_spacing_ms: 0,
            ..InspectionBudget::default()
        };
        let transcript = vec![crate::contracts::TranscriptSegment {
            id: "cue".into(),
            range: TimeRange {
                start_ms: 7000,
                end_ms: 8000,
            },
            text: "new subject".into(),
        }];
        let result = plan_visual_candidates(10_000, &[2000], &transcript, &[], &budget).unwrap();
        assert!(result.scene_limit_reached);
        assert!(
            result
                .candidates
                .iter()
                .any(|c| c.time_ms == 7000 && c.reason == CandidateReason::TranscriptCue)
        );
        assert!(plan_visual_candidates(10_000, &[], &transcript, &[10_000], &budget).is_err());
        let tiny = plan_visual_candidates(1, &[], &[], &[], &budget).unwrap();
        assert_eq!(tiny.candidates.len(), 1);
    }
    #[test]
    fn parses_only_showinfo_timestamps_and_rejects_bad_or_excessive_output() {
        let bytes = b"unrelated pts_time:9\n[Parsed_showinfo_3] n: 0 pts: 1 pts_time:1.5 pos: 3\n";
        assert_eq!(scene_times(bytes, 10_000, 1).unwrap(), vec![1500]);
        assert!(scene_times(bytes, 1000, 1).is_err());
        assert!(scene_times(bytes, 10_000, 0).is_err());
        assert!(scene_times(b"[showinfo] pts_time:nan", 10_000, 2).is_err());
    }
    #[test]
    fn validates_metadata_and_video_requirement() {
        let valid = br#"{"format":{"duration":"12.5"},"streams":[{"codec_type":"video","width":1920,"height":1080},{"codec_type":"audio"}]}"#;
        assert_eq!(
            decode_probe(valid).unwrap(),
            VideoMetadata {
                duration_ms: 12500,
                width: 1920,
                height: 1080,
                has_audio: true
            }
        );
        assert!(
            decode_probe(br#"{"format":{"duration":"NaN"},"streams":[{"codec_type":"audio"}]}"#)
                .is_err()
        );
    }
    async fn generate_test_video(
        video: &Path,
        workspace: &Path,
        tools: &MediaTools,
        cancel: &Cancellation,
    ) {
        let mut args: Vec<std::ffi::OsString> = [
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-n",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=160x120:rate=24",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440",
            "-t",
            "2",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
        ]
        .iter()
        .map(Into::into)
        .collect();
        args.push(video.as_os_str().to_owned());
        process::run(
            &ProcessRequest {
                program: tools.ffmpeg.clone(),
                args,
                cwd: None,
                timeout: Duration::from_secs(20),
                max_output_bytes: 128 * 1024,
                file_budget: Some((workspace.to_owned(), 4 * 1024 * 1024)),
            },
            cancel,
        )
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn real_tools_extract_source_grounded_visuals() {
        let workspace = tempfile::tempdir().unwrap();
        let video = workspace.path().join("source.mp4");
        let tools = MediaTools::default();
        let cancel = Cancellation::default();
        generate_test_video(&video, workspace.path(), &tools, &cancel).await;
        let metadata = probe(&video, &tools, &cancel).await.unwrap();
        assert!(metadata.has_audio);
        let selected = TimeRange {
            start_ms: 500,
            end_ms: 1500,
        };
        let visuals = extract_visuals(
            &video,
            &metadata,
            &VisualSelection {
                frame_times_ms: &[500],
                clips: &[selected],
            },
            workspace.path(),
            &tools,
            &MediaBudget::default(),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(visuals.assets.len(), 2);
        let jpeg = std::fs::read(
            visuals
                .directory
                .path()
                .join(visuals.assets[0].path.as_str()),
        )
        .unwrap();
        assert!(jpeg.starts_with(&[0xff, 0xd8]));
        assert_eq!(visuals.assets[1].range, selected);
        let clip = probe(
            &visuals
                .directory
                .path()
                .join(visuals.assets[1].path.as_str()),
            &tools,
            &cancel,
        )
        .await
        .unwrap();
        assert!(!clip.has_audio);
        assert!(clip.duration_ms.abs_diff(1000) <= 150);
        assert!(
            extract_visuals(
                &video,
                &metadata,
                &VisualSelection {
                    frame_times_ms: &[metadata.duration_ms],
                    clips: &[]
                },
                workspace.path(),
                &tools,
                &MediaBudget::default(),
                &cancel
            )
            .await
            .is_err()
        );
    }
    #[test]
    fn sampling_is_bounded_and_inside_source() {
        assert_eq!(sample_frame_times(1000, 3), vec![250, 500, 750]);
        assert_eq!(sample_frame_times(1, 20), vec![0]);
        assert!(sample_frame_times(0, 3).is_empty());
        assert_eq!(sample_frame_times(1000, usize::MAX).len(), 100);
    }
}
