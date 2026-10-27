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
#[cfg(test)]
mod tests {
    use super::*;
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
