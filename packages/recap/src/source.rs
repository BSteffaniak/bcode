//! Local/YouTube acquisition and timestamped subtitle parsing.
use crate::process::{self, Cancellation, ProcessRequest};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// Input accepted by the source adapter.
#[derive(Clone, Debug)]
pub enum SourceInput {
    /// Existing local video.
    Local(PathBuf),
    /// Validated `YouTube` watch/short URL.
    Youtube(String),
}
/// A timestamped transcript segment in seconds.
#[derive(Clone, Debug, PartialEq)]
pub struct SubtitleCue {
    /// Inclusive source start.
    pub start_seconds: f64,
    /// Exclusive source end.
    pub end_seconds: f64,
    /// Caption text, never interpreted as HTML.
    pub text: String,
}
/// Explicit acquisition budgets and downloader selection.
#[derive(Clone, Debug)]
pub struct AcquisitionOptions {
    /// Executable supplied by caller; never installed automatically.
    pub yt_dlp: PathBuf,
    /// Existing directory under caller ownership. A unique child is created.
    pub workspace: PathBuf,
    /// Maximum downloaded video bytes.
    pub max_video_bytes: u64,
    /// Maximum source duration.
    pub max_duration_seconds: f64,
    /// Preferred subtitle language (simple language tag only).
    pub caption_language: String,
    /// Per-operation deadline.
    pub timeout: Duration,
}
/// Source media and optional real timestamped captions.
#[derive(Debug)]
pub struct AcquiredSource {
    /// Canonical local media path.
    pub video: PathBuf,
    /// Captions, possibly empty; absence is not a transcript.
    pub captions: Vec<SubtitleCue>,
    /// Source-provided display title, when available. Never executable markup.
    pub title: Option<String>,
    /// Keeps downloaded media alive; local input is not owned or removed.
    pub workspace: Option<tempfile::TempDir>,
}
/// Acquisition/parsing errors without untrusted subprocess diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// Invalid source, resource budget or subtitle representation.
    #[error("invalid source: {0}")]
    Invalid(&'static str),
    /// Local filesystem failure.
    #[error("source I/O failed")]
    Io(#[from] std::io::Error),
    /// External acquisition failed.
    #[error(transparent)]
    Process(#[from] process::ProcessError),
    /// Allowlisted download failure; raw executable output is discarded.
    #[error("{stage}: {failure} (exit {exit_code:?})")]
    Acquisition {
        /// Failed acquisition phase.
        stage: AcquisitionStage,
        /// Safe diagnostic category, not raw executable text.
        failure: AcquisitionFailure,
        /// Exit status when available.
        exit_code: Option<i32>,
    },
    /// Invalid downloader metadata.
    #[error("invalid downloader metadata")]
    Metadata(#[from] serde_json::Error),
}
/// Acquisition phase attached to safe downloader failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AcquisitionStage {
    /// Resolving source metadata.
    Metadata,
    /// Downloading video and captions (yt-dlp may fail either).
    Download,
}
impl std::fmt::Display for AcquisitionStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Metadata => "YouTube metadata",
            Self::Download => "YouTube video/caption download",
        })
    }
}
/// Allowlisted facts extracted from bounded downloader diagnostics, never executable text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AcquisitionFailure {
    /// Downloader explicitly reported HTTP 403, not its cause.
    Forbidden,
    /// Downloader explicitly reported HTTP 429.
    RateLimited,
    /// Authentication or access restriction was reported.
    AccessRequired,
    /// Selected source format was unavailable.
    FormatUnavailable,
    /// A challenge/runtime dependency was reported.
    ChallengeRuntime,
    /// No supported classification was established.
    Unknown,
}
impl std::fmt::Display for AcquisitionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Forbidden => "HTTP 403 Forbidden; update yt-dlp in your package environment and verify its JavaScript runtime/challenge support; access may still be restricted",
            Self::RateLimited => "HTTP 429 Too Many Requests; wait before explicitly retrying",
            Self::AccessRequired => "source requires authentication or is restricted; Recap never loads browser cookies automatically",
            Self::FormatUnavailable => "requested video format unavailable; check downloader version and source availability",
            Self::ChallengeRuntime => "JavaScript challenge/runtime support unavailable; use an up-to-date yt-dlp with its supported runtime and challenge package",
            Self::Unknown => "downloader failed; cause unclassified; check source availability and downloader setup",
        })
    }
}
fn classify_download(stderr: &[u8]) -> AcquisitionFailure {
    // Inspect only diagnostic lines, not arbitrary URLs, title output or echoed arguments.
    for line in String::from_utf8_lossy(stderr)
        .lines()
        .filter(|line| line.starts_with("ERROR:") || line.starts_with("WARNING:"))
    {
        if line.contains("HTTP Error 403:") {
            return AcquisitionFailure::Forbidden;
        }
        if line.contains("HTTP Error 429:") {
            return AcquisitionFailure::RateLimited;
        }
        if line.contains("Requested format is not available") {
            return AcquisitionFailure::FormatUnavailable;
        }
        if line.contains("Sign in to confirm")
            || line.contains("Private video")
            || line.contains("members-only")
        {
            return AcquisitionFailure::AccessRequired;
        }
        if line.contains("challenge solving failed")
            || line.contains("No supported JavaScript runtime")
        {
            return AcquisitionFailure::ChallengeRuntime;
        }
    }
    AcquisitionFailure::Unknown
}
async fn download_process(
    request: &ProcessRequest,
    cancel: &Cancellation,
    stage: AcquisitionStage,
) -> Result<process::ProcessOutput, SourceError> {
    cancel.emit(crate::diagnostics::Event::Phase {
        phase: match stage {
            AcquisitionStage::Metadata => crate::diagnostics::Phase::Metadata,
            AcquisitionStage::Download => crate::diagnostics::Phase::Download,
        },
    });
    process::run_inspected(request, cancel, |exit_code, stderr| {
        let failure = classify_download(stderr);
        cancel.emit(crate::diagnostics::Event::AcquisitionFailed {
            phase: match stage {
                AcquisitionStage::Metadata => crate::diagnostics::Phase::Metadata,
                AcquisitionStage::Download => crate::diagnostics::Phase::Download,
            },
            exit_code,
            http_status: match failure {
                AcquisitionFailure::Forbidden => Some(403),
                AcquisitionFailure::RateLimited => Some(429),
                _ => None,
            },
        });
        SourceError::Acquisition {
            stage,
            failure,
            exit_code,
        }
    })
    .await
}

/// Validate only known `YouTube` URL forms and discard unrelated query arguments.
/// # Errors
/// Rejects credentials, non-HTTPS, foreign hosts, playlists and malformed IDs.
pub fn youtube_url(input: &str) -> Result<String, SourceError> {
    if input.len() > 4096 {
        return Err(SourceError::Invalid("URL byte budget"));
    }
    let url = url::Url::parse(input).map_err(|_| SourceError::Invalid("malformed URL"))?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err(SourceError::Invalid(
            "YouTube requires HTTPS without credentials or port",
        ));
    }
    let id = match url.host_str() {
        Some("youtu.be") => url.path().strip_prefix('/').map(str::to_owned),
        Some("youtube.com" | "www.youtube.com" | "m.youtube.com") => {
            if url.path() == "/watch" {
                let ids: Vec<_> = url
                    .query_pairs()
                    .filter(|(key, _)| key == "v")
                    .map(|(_, value)| value.into_owned())
                    .collect();
                if ids.len() == 1 {
                    ids.into_iter().next()
                } else {
                    None
                }
            } else {
                url.path().strip_prefix("/shorts/").map(str::to_owned)
            }
        }
        _ => None,
    }
    .ok_or(SourceError::Invalid("unsupported YouTube URL"))?;
    if id.len() != 11
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(SourceError::Invalid("malformed video ID"));
    }
    Ok(format!("https://www.youtube.com/watch?v={id}"))
}
/// Canonicalize a regular local file, never a URL or device.
/// # Errors
/// Rejects missing paths, symlinks to non-files and files exceeding the budget.
pub fn local_video(path: &Path, max_bytes: u64) -> Result<PathBuf, SourceError> {
    let path = path.canonicalize()?;
    let metadata = path.metadata()?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > max_bytes {
        return Err(SourceError::Invalid(
            "video is not a bounded nonempty regular file",
        ));
    }
    Ok(path)
}
/// Verify an adapter-created file is regular and confined to its owned directory.
/// # Errors
/// Rejects symlinks, escapes, missing files and oversized assets.
pub fn owned_file(root: &Path, path: &Path, max_bytes: u64) -> Result<PathBuf, SourceError> {
    if path.symlink_metadata()?.file_type().is_symlink() {
        return Err(SourceError::Invalid("symlink asset"));
    }
    let root = root.canonicalize()?;
    let path = local_video(path, max_bytes)?;
    if !path.starts_with(root) {
        return Err(SourceError::Invalid("asset escaped workspace"));
    }
    Ok(path)
}
fn timestamp(value: &str) -> Option<f64> {
    let normalized = value.replace(',', ".");
    let parts: Vec<_> = normalized.split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let seconds: f64 = parts.last()?.parse().ok()?;
    let minutes: f64 = parts[parts.len() - 2].parse().ok()?;
    let hours: f64 = if parts.len() == 3 {
        parts[0].parse().ok()?
    } else {
        0.0
    };
    if !seconds.is_finite()
        || !(0.0..60.0).contains(&seconds)
        || !(0.0..60.0).contains(&minutes)
        || !hours.is_finite()
        || hours < 0.0
    {
        return None;
    }
    Some(hours.mul_add(3600.0, minutes.mul_add(60.0, seconds)))
}
/// Parse bounded `WebVTT` or SRT cues, preserving source times.
/// # Errors
/// Rejects oversized input, malformed/reversed timing, no textual cues and excessive cues.
/// Blank cues have no transcript content and are ignored after validating their timing.
pub fn parse_subtitles(input: &str) -> Result<Vec<SubtitleCue>, SourceError> {
    if input.len() > 4 * 1024 * 1024 {
        return Err(SourceError::Invalid("subtitle byte budget"));
    }
    let normalized = input.replace("\r\n", "\n").replace('\r', "\n");
    let mut cues = Vec::new();
    let mut cue_count = 0;
    for block in normalized.split("\n\n") {
        let lines: Vec<_> = block.lines().collect();
        let Some(index) = lines.iter().position(|line| line.contains("-->")) else {
            continue;
        };
        cue_count += 1;
        if cue_count > 50_000 {
            return Err(SourceError::Invalid("caption count budget"));
        }
        let (start, end) = lines[index]
            .split_once("-->")
            .ok_or(SourceError::Invalid("caption timing"))?;
        let start = timestamp(start.trim()).ok_or(SourceError::Invalid("caption start"))?;
        let end = end
            .split_whitespace()
            .next()
            .and_then(timestamp)
            .ok_or(SourceError::Invalid("caption end"))?;
        let text = lines[index + 1..].join("\n");
        if end <= start {
            return Err(SourceError::Invalid("reversed or zero-duration caption"));
        }
        if text.trim().is_empty() {
            continue;
        }
        cues.push(SubtitleCue {
            start_seconds: start,
            end_seconds: end,
            text,
        });
    }
    if cues.is_empty() {
        return Err(SourceError::Invalid("no timestamped captions"));
    }
    Ok(cues)
}
fn format_selection(max_bytes: u64) -> String {
    // Prefer a single video-only file: Recap consumes stills/silent clips and captions.
    // Every fallback retains both height and caller-selected byte constraints.
    format!(
        "bestvideo[ext=mp4][height<=720][filesize<=?{max_bytes}]/bestvideo[ext=webm][height<=720][filesize<=?{max_bytes}]/best[ext=mp4][height<=720][filesize<=?{max_bytes}]/best[ext=webm][height<=720][filesize<=?{max_bytes}]"
    )
}
fn downloader_request(options: &AcquisitionOptions, workspace: &Path, url: &str) -> ProcessRequest {
    ProcessRequest {
        program: options.yt_dlp.clone(),
        args: [
            "--ignore-config",
            "--js-runtimes",
            "node",
            "--js-runtimes",
            "bun",
            "--js-runtimes",
            "quickjs",
            "--no-remote-components",
            "--no-playlist",
            "--no-warnings",
            "--socket-timeout",
            "15",
            "--retries",
            "1",
            "--skip-download",
            "--dump-single-json",
            "--",
            url,
        ]
        .iter()
        .map(Into::into)
        .collect(),
        cwd: Some(workspace.to_owned()),
        timeout: options.timeout,
        max_output_bytes: 4 * 1024 * 1024,
        file_budget: Some((
            workspace.to_owned(),
            options.max_video_bytes.saturating_add(8 * 1024 * 1024),
        )),
    }
}
/// Acquire local video or bounded `YouTube` media, including manual/automatic captions.
/// # Errors
/// Fails closed on invalid input, unknown duration, exhausted budgets or adapter failure.
pub async fn acquire(
    input: &SourceInput,
    options: &AcquisitionOptions,
    cancel: &Cancellation,
) -> Result<AcquiredSource, SourceError> {
    if cancel.is_cancelled() {
        return Err(process::ProcessError::Cancelled.into());
    }
    if options.max_video_bytes == 0
        || !options.max_duration_seconds.is_finite()
        || options.max_duration_seconds <= 0.0
    {
        return Err(SourceError::Invalid("acquisition budget"));
    }
    if let SourceInput::Local(path) = input {
        return Ok(AcquiredSource {
            video: local_video(path, options.max_video_bytes)?,
            captions: vec![],
            title: None,
            workspace: None,
        });
    }
    let SourceInput::Youtube(url) = input else {
        unreachable!()
    };
    let url = youtube_url(url)?;
    if options.caption_language.is_empty()
        || options.caption_language.len() > 35
        || !options
            .caption_language
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(SourceError::Invalid("caption language"));
    }
    let root = options.workspace.canonicalize()?;
    let workspace = tempfile::Builder::new()
        .prefix("recap-source-")
        .tempdir_in(root)?;
    let mut request = downloader_request(options, workspace.path(), &url);
    let metadata: serde_json::Value = serde_json::from_slice(
        &download_process(&request, cancel, AcquisitionStage::Metadata)
            .await?
            .stdout,
    )?;
    if metadata.get("id").and_then(serde_json::Value::as_str)
        != url.strip_prefix("https://www.youtube.com/watch?v=")
    {
        return Err(SourceError::Invalid("downloader source identity mismatch"));
    }
    let duration = metadata
        .get("duration")
        .and_then(serde_json::Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0 && *value <= options.max_duration_seconds)
        .ok_or(SourceError::Invalid("unknown or over-budget duration"))?;
    if metadata.get("is_live").and_then(serde_json::Value::as_bool) == Some(true) || duration <= 0.0
    {
        return Err(SourceError::Invalid("live video unsupported"));
    }
    let size = options.max_video_bytes.to_string();
    request.args = [
        "--ignore-config",
        "--js-runtimes",
        "node",
        "--js-runtimes",
        "bun",
        "--js-runtimes",
        "quickjs",
        "--no-remote-components",
        "--no-playlist",
        "--no-warnings",
        "--socket-timeout",
        "15",
        "--retries",
        "1",
        "--max-filesize",
        &size,
        "--format",
        &format_selection(options.max_video_bytes),
        "--write-subs",
        "--write-auto-subs",
        "--sub-langs",
        &options.caption_language,
        "--sub-format",
        "vtt/srt",
        "--output",
        "source.%(ext)s",
        "--",
        &url,
    ]
    .iter()
    .map(Into::into)
    .collect();
    download_process(&request, cancel, AcquisitionStage::Download).await?;
    let mut acquired = collect_download(workspace, options, cancel)?;
    acquired.title = metadata
        .get("title")
        .and_then(serde_json::Value::as_str)
        .filter(|title| !title.trim().is_empty() && title.len() <= 4096)
        .map(str::to_owned);
    Ok(acquired)
}
fn collect_download(
    workspace: tempfile::TempDir,
    options: &AcquisitionOptions,
    cancel: &Cancellation,
) -> Result<AcquiredSource, SourceError> {
    cancel.emit(crate::diagnostics::Event::Phase {
        phase: crate::diagnostics::Phase::Captions,
    });
    let mut video = None;
    let mut captions = vec![];
    for entry in std::fs::read_dir(workspace.path())? {
        let path = entry?.path();
        match path.extension().and_then(|value| value.to_str()) {
            Some("mp4" | "webm" | "mkv") => {
                if video.is_some() {
                    return Err(SourceError::Invalid("ambiguous acquired video"));
                }
                video = Some(owned_file(
                    workspace.path(),
                    &path,
                    options.max_video_bytes,
                )?);
            }
            Some("vtt" | "srt") => {
                let path = owned_file(workspace.path(), &path, 4 * 1024 * 1024)?;
                captions = parse_subtitles(&std::fs::read_to_string(path)?)?;
            }
            _ => {}
        }
    }
    Ok(AcquiredSource {
        video: video.ok_or(SourceError::Invalid(
            "downloader produced no video within budget",
        ))?,
        captions,
        title: None,
        workspace: Some(workspace),
    })
}
/// Convert parsed cues to the portable transcript contract, checking source bounds.
/// Boundary-crossing caption tails are intersected with the actual media duration.
/// # Errors
/// Rejects invalid, non-finite or wholly out-of-source timestamps; no cues are silently dropped.
pub fn transcript_segments(
    cues: &[SubtitleCue],
    duration_ms: u64,
) -> Result<Vec<crate::contracts::TranscriptSegment>, SourceError> {
    if cues.len() > 50_000 {
        return Err(SourceError::Invalid("caption count budget"));
    }
    cues.iter()
        .enumerate()
        .map(|(index, cue)| {
            let to_ms = |value: f64| {
                Duration::try_from_secs_f64(value)
                    .ok()
                    .and_then(|value| u64::try_from(value.as_millis()).ok())
                    .ok_or(SourceError::Invalid("caption timestamp"))
            };
            let range = crate::contracts::TimeRange {
                start_ms: to_ms(cue.start_seconds)?,
                end_ms: to_ms(cue.end_seconds)?.min(duration_ms),
            };
            range
                .validate(duration_ms)
                .map_err(|_| SourceError::Invalid("caption outside source"))?;
            if cue.text.trim().is_empty() {
                return Err(SourceError::Invalid("empty caption"));
            }
            Ok(crate::contracts::TranscriptSegment {
                id: format!("transcript-{index:05}"),
                range,
                text: cue.text.clone(),
            })
        })
        .collect()
}
/// Produce an analysis-only projection of rolling captions, retaining surviving source IDs.
///
/// Persist the original transcript: a fully repeated overlapping cue is omitted only here.
/// Exact whitespace-delimited suffix/prefix matches are removed only for temporally
/// overlapping/touching cues. Case and punctuation remain significant. At most 256 words
/// are compared at a boundary; uncertain longer repetition is retained, never guessed away.
/// # Errors
/// Rejects inputs beyond the existing caption count/byte budgets or invalid ranges.
pub fn deduplicate_rolling_captions(
    segments: &[crate::contracts::TranscriptSegment],
) -> Result<Vec<crate::contracts::TranscriptSegment>, SourceError> {
    if segments.len() > 50_000 {
        return Err(SourceError::Invalid("caption count budget"));
    }
    let mut total = 0_usize;
    for segment in segments {
        total = total
            .checked_add(segment.text.len())
            .ok_or(SourceError::Invalid("subtitle byte budget"))?;
        if total > 4 * 1024 * 1024 {
            return Err(SourceError::Invalid("subtitle byte budget"));
        }
        if segment.range.start_ms >= segment.range.end_ms || segment.text.trim().is_empty() {
            return Err(SourceError::Invalid("invalid caption"));
        }
    }
    let mut result = Vec::new();
    for (index, segment) in segments.iter().enumerate() {
        let mut projected = segment.clone();
        if let Some(previous) = index.checked_sub(1).map(|i| &segments[i])
            && segment.range.start_ms >= previous.range.start_ms
            && segment.range.start_ms <= previous.range.end_ms
        {
            let previous_words: Vec<_> = previous.text.split_whitespace().rev().take(256).collect();
            let current_words: Vec<_> = segment.text.split_whitespace().take(256).collect();
            let overlap = (1..=previous_words.len().min(current_words.len()))
                .rev()
                .find(|count| {
                    previous_words[..*count]
                        .iter()
                        .rev()
                        .copied()
                        .eq(current_words[..*count].iter().copied())
                })
                .unwrap_or(0);
            if overlap > 0 {
                // Slice at an original UTF-8 word boundary, not a reconstructed string.
                let mut words = segment.text.split_whitespace();
                for _ in 0..overlap {
                    words.next();
                }
                if let Some(next) = words.next() {
                    let offset = next.as_ptr() as usize - segment.text.as_ptr() as usize;
                    segment.text[offset..].clone_into(&mut projected.text);
                } else {
                    continue;
                }
            }
        }
        result.push(projected);
    }
    Ok(result)
}

/// Explicit argument-vector transcription command. The canonical input video path is
/// appended to `args`; the command must emit timestamped VTT/SRT on standard output.
#[derive(Clone, Debug)]
pub struct TranscriptionCommand {
    /// Caller-approved executable; never installed or resolved through a shell.
    pub program: PathBuf,
    /// Literal configured arguments (secrets should not be placed here).
    pub args: Vec<std::ffi::OsString>,
    /// Hard wall-clock budget.
    pub timeout: Duration,
}
/// Run an explicitly configured transcription command with bounded output.
/// # Errors
/// Rejects invalid input, missing timestamps, cancellation or adapter failure.
pub async fn transcribe_with_command(
    video: &Path,
    config: &TranscriptionCommand,
    cancel: &Cancellation,
) -> Result<Vec<SubtitleCue>, SourceError> {
    let video = local_video(video, u64::MAX)?;
    let mut args = config.args.clone();
    args.push(video.into_os_string());
    let request = ProcessRequest {
        program: config.program.clone(),
        args,
        cwd: None,
        timeout: config.timeout,
        max_output_bytes: 4 * 1024 * 1024,
        file_budget: None,
    };
    let result = Box::pin(process::run(&request, cancel)).await?;
    let text = std::str::from_utf8(&result.stdout)
        .map_err(|_| SourceError::Invalid("transcription is not UTF-8"))?;
    parse_subtitles(text)
}
/// Explicit transcription extension point. Implementations must honor cancellation and limits.
pub trait TranscriptionAdapter: Send + Sync {
    /// Transcribe a validated local media file into real timestamped cues.
    /// # Errors
    /// Returns an actionable failure when transcription cannot complete.
    fn transcribe(
        &self,
        video: &Path,
        cancel: &Cancellation,
    ) -> Result<Vec<SubtitleCue>, SourceError>;
}
/// Prefer supplied transcript, then acquired captions, then an explicitly configured adapter.
/// # Errors
/// Rejects missing transcript sources rather than fabricating text.
pub fn resolve_transcript(
    source: &AcquiredSource,
    supplied: Option<&str>,
    adapter: Option<&dyn TranscriptionAdapter>,
    cancel: &Cancellation,
) -> Result<Vec<SubtitleCue>, SourceError> {
    if cancel.is_cancelled() {
        return Err(process::ProcessError::Cancelled.into());
    }
    if let Some(text) = supplied {
        return parse_subtitles(text);
    }
    if !source.captions.is_empty() {
        return Ok(source.captions.clone());
    }
    adapter
        .ok_or(SourceError::Invalid(
            "no captions: supply VTT/SRT or configure a transcription adapter",
        ))?
        .transcribe(&source.video, cancel)
}
#[cfg(test)]
mod tests {
    use super::*;

    fn caption(id: &str, start: u64, end: u64, text: &str) -> crate::contracts::TranscriptSegment {
        crate::contracts::TranscriptSegment {
            id: id.into(),
            range: crate::contracts::TimeRange {
                start_ms: start,
                end_ms: end,
            },
            text: text.into(),
        }
    }
    #[test]
    fn rolling_projection_retains_novel_words_and_original_identity() {
        let source = vec![
            caption("a", 0, 2000, "hello world"),
            caption("b", 1000, 3000, "hello world and 語🙂"),
            caption("c", 2000, 4000, "and 語🙂! new ending"),
        ];
        let result = deduplicate_rolling_captions(&source).unwrap();
        assert_eq!(result[1].text, "and 語🙂");
        // Changed punctuation is uncertain: do not erase source words.
        assert_eq!(result[2].text, "and 語🙂! new ending");
        assert_eq!(result[1].id, "b");
        assert_eq!(result[1].range, source[1].range);
        assert_eq!(source[1].text, "hello world and 語🙂");
    }
    #[test]
    fn repeated_rolling_cues_do_not_erase_later_novel_tail_or_nonoverlapping_repetition() {
        let source = vec![
            caption("a", 0, 2000, "a b"),
            caption("b", 1000, 3000, "a b"),
            caption("c", 2000, 4000, "a b c d"),
            caption("d", 5000, 6000, "c d"),
        ];
        let result = deduplicate_rolling_captions(&source).unwrap();
        assert_eq!(
            result
                .iter()
                .map(|s| (s.id.as_str(), s.text.as_str()))
                .collect::<Vec<_>>(),
            vec![("a", "a b"), ("c", "c d"), ("d", "c d")]
        );
    }
    #[test]
    fn youtube_blank_cues_are_valid_but_do_not_become_transcript() {
        let input = "WEBVTT\n\n00:01.000 --> 00:02.000 align:start position:0%\nActual words\n\n00:02.000 --> 00:02.010 align:start position:0%\n \n \n\n00:02.010 --> 00:03.000\nMore words\n";
        let cues = super::parse_subtitles(input).unwrap();
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "Actual words");
        assert!((cues[1].start_seconds - 2.010).abs() < f64::EPSILON);
        assert_eq!(cues[1].text, "More words");
        assert!(super::parse_subtitles("WEBVTT\n\n00:02.000 --> 00:01.000\n \n").is_err());
        assert!(super::parse_subtitles("WEBVTT\n\n00:02.000 --> 00:02.000\n \n").is_err());
        assert!(super::parse_subtitles("WEBVTT\n\n00:01.000 --> 00:02.000\n \n").is_err());
        let excessive = format!(
            "WEBVTT\n\n{}",
            "00:01.000 --> 00:02.000\n \n\n".repeat(50_001)
        );
        assert!(matches!(
            super::parse_subtitles(&excessive),
            Err(super::SourceError::Invalid("caption count budget"))
        ));
    }
    #[test]
    fn downloader_classification_never_echoes_untrusted_output() {
        use super::*;
        for (text, expected) in [
            (
                "ERROR: unable to download video data: HTTP Error 403: Forbidden secret=https://signed/?token=PRIVATE\u{1b}[2J",
                AcquisitionFailure::Forbidden,
            ),
            (
                "ERROR: HTTP Error 429: Too Many Requests",
                AcquisitionFailure::RateLimited,
            ),
            (
                "ERROR: Requested format is not available",
                AcquisitionFailure::FormatUnavailable,
            ),
            (
                "WARNING: No supported JavaScript runtime could be found",
                AcquisitionFailure::ChallengeRuntime,
            ),
            (
                "ERROR: Sign in to confirm your age",
                AcquisitionFailure::AccessRequired,
            ),
            (
                "https://example.com/HTTP Error 403: SECRET",
                AcquisitionFailure::Unknown,
            ),
            (
                "PRIVATE unknown failure\u{1b}[2J",
                AcquisitionFailure::Unknown,
            ),
        ] {
            let failure = classify_download(text.as_bytes());
            assert_eq!(failure, expected);
            let error = SourceError::Acquisition {
                stage: AcquisitionStage::Download,
                failure,
                exit_code: Some(1),
            };
            for rendered in [error.to_string(), format!("{error:?}")] {
                assert!(!rendered.contains("PRIVATE"));
                assert!(!rendered.contains("signed"));
                assert!(!rendered.contains('\u{1b}'));
            }
        }
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn real_failed_downloader_reports_phase_without_raw_stderr() {
        use super::*;
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("downloader");
        std::fs::write(&script, "#!/bin/sh\nprintf '%s\\n' 'ERROR: unable to download video data: HTTP Error 403: Forbidden PRIVATE' >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let request = ProcessRequest {
            program: script,
            args: vec![],
            cwd: None,
            timeout: Duration::from_secs(2),
            max_output_bytes: 1024,
            file_budget: None,
        };
        let error = download_process(
            &request,
            &Cancellation::default(),
            AcquisitionStage::Download,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            SourceError::Acquisition {
                stage: AcquisitionStage::Download,
                failure: AcquisitionFailure::Forbidden,
                exit_code: Some(1)
            }
        ));
        assert!(!format!("{error:?}").contains("PRIVATE"));
    }
    #[test]
    fn validates_youtube_hosts_and_ids() {
        assert!(
            youtube_url(&format!(
                "https://youtu.be/fJwhaC_Fn-8?{}",
                "x".repeat(4096)
            ))
            .is_err()
        );
        assert_eq!(
            youtube_url("https://youtu.be/fJwhaC_Fn-8?t=2").unwrap(),
            "https://www.youtube.com/watch?v=fJwhaC_Fn-8"
        );
        for value in [
            "https://youtube.com.evil/watch?v=fJwhaC_Fn-8",
            "file:///tmp/video",
            "https://user@youtube.com/watch?v=fJwhaC_Fn-8",
            "https://youtube.com/watch?v=bad",
            "https://youtube.com/watch?v=fJwhaC_Fn-8&v=fJwhaC_Fn-8",
        ] {
            assert!(youtube_url(value).is_err());
        }
    }
    #[test]
    fn parses_srt_and_vtt() {
        let cues = parse_subtitles("1\r\n00:00:01,200 --> 00:00:03,000\r\nhello\r\n").unwrap();
        assert!((cues[0].start_seconds - 1.2).abs() < f64::EPSILON);
        assert_eq!(
            parse_subtitles("WEBVTT\n\n00:01.200 --> 00:03.000 align:start\nhello").unwrap(),
            cues
        );
        assert!(parse_subtitles("00:04.000 --> 00:03.000\nwrong").is_err());
    }
    #[test]
    fn intersects_trailing_youtube_caption_with_media() {
        let cues = vec![SubtitleCue {
            start_seconds: 825.680,
            end_seconds: 828.440,
            text: "last words".into(),
        }];
        let segments = transcript_segments(&cues, 826_514).unwrap();
        assert_eq!(segments[0].range.start_ms, 825_680);
        assert_eq!(segments[0].range.end_ms, 826_514);
        assert!(transcript_segments(&cues, 800_000).is_err());
    }
    #[test]
    fn rejects_asset_escape() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(other.path(), b"video").unwrap();
        assert!(owned_file(root.path(), other.path(), 100).is_err());
    }
}
