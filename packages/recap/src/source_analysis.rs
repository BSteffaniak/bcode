//! Deterministic, bounded Detailed transcript partitioning over document contracts.
//! Windows are analysis inputs, not trusted instructions or a replacement transcript.
use crate::contracts::TranscriptSegment;

/// Explicit per-request transcript budgets. They do not impose an editorial chapter cap.
#[derive(Clone, Copy, Debug)]
pub struct WindowLimits {
    /// Maximum primary text bytes in one window. An indivisible larger cue is rejected.
    pub max_primary_bytes: usize,
    /// Maximum primary segments per window.
    pub max_primary_segments: usize,
    /// Maximum preceding context bytes (in addition to primary bytes).
    pub max_overlap_bytes: usize,
    /// Maximum preceding context segments.
    pub max_overlap_segments: usize,
    /// Maximum accepted transcript bytes in total; exhaustion fails, never truncates.
    pub max_total_bytes: usize,
    /// Maximum accepted segment count.
    pub max_total_segments: usize,
}
impl Default for WindowLimits {
    fn default() -> Self {
        Self {
            max_primary_bytes: 24_000,
            max_primary_segments: 200,
            max_overlap_bytes: 2_000,
            max_overlap_segments: 8,
            max_total_bytes: 4 * 1024 * 1024,
            max_total_segments: 50_000,
        }
    }
}
/// A source-order window retaining exact document segment identity and text.
#[derive(Debug)]
pub struct TranscriptWindow<'a> {
    /// Zero-based deterministic window position, not a chapter assignment.
    pub index: usize,
    /// Preceding boundary context; never counted as newly covered source.
    pub context: &'a [TranscriptSegment],
    /// Each input segment appears in exactly one primary slice.
    pub primary: &'a [TranscriptSegment],
}
/// A transcript cannot be represented within explicit resource limits.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum WindowError {
    /// Limits must permit progress.
    #[error("invalid transcript window limits")]
    InvalidLimits,
    /// The entire source exceeds caller admission limits.
    #[error("transcript admission budget exceeded")]
    Budget,
    /// A cue cannot be silently cut to fit a request.
    #[error("indivisible transcript segment {index} exceeds window bytes")]
    IndivisibleCue {
        /// Original segment position.
        index: usize,
    },
}
/// Partition the full transcript without dropping tails or changing segment IDs.
/// Empty input yields no windows. A large source produces more windows, not fewer chapters.
///
/// # Errors
/// Rejects invalid limits, oversized complete input, and indivisible oversized cues.
pub fn transcript_windows(
    transcript: &[TranscriptSegment],
    limits: WindowLimits,
) -> Result<Vec<TranscriptWindow<'_>>, WindowError> {
    if limits.max_primary_bytes == 0 || limits.max_primary_segments == 0 {
        return Err(WindowError::InvalidLimits);
    }
    if transcript.len() > limits.max_total_segments {
        return Err(WindowError::Budget);
    }
    let mut total = 0_usize;
    for (index, segment) in transcript.iter().enumerate() {
        total = total
            .checked_add(segment.text.len())
            .ok_or(WindowError::Budget)?;
        if total > limits.max_total_bytes {
            return Err(WindowError::Budget);
        }
        if segment.text.len() > limits.max_primary_bytes {
            return Err(WindowError::IndivisibleCue { index });
        }
    }
    let mut windows = Vec::new();
    let mut start = 0;
    while start < transcript.len() {
        let mut end = start;
        let mut bytes = 0;
        while end < transcript.len()
            && end - start < limits.max_primary_segments
            && transcript[end].text.len() <= limits.max_primary_bytes - bytes
        {
            bytes += transcript[end].text.len();
            end += 1;
        }
        let mut context_start = start;
        let mut context_bytes = 0;
        while context_start > 0 && start - context_start < limits.max_overlap_segments {
            let size = transcript[context_start - 1].text.len();
            if size > limits.max_overlap_bytes - context_bytes {
                break;
            }
            context_bytes += size;
            context_start -= 1;
        }
        windows.push(TranscriptWindow {
            index: windows.len(),
            context: &transcript[context_start..start],
            primary: &transcript[start..end],
        });
        start = end;
    }
    Ok(windows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::TimeRange;

    fn segments(count: usize) -> Vec<TranscriptSegment> {
        (0..count)
            .map(|index| TranscriptSegment {
                id: format!("original-{index}"),
                range: TimeRange {
                    start_ms: 0,
                    end_ms: 1,
                },
                text: "語🙂".into(),
            })
            .collect()
    }
    #[test]
    fn covers_every_segment_once_including_tail_with_bounded_context() {
        let source = segments(17);
        let limits = WindowLimits {
            max_primary_bytes: 21,
            max_primary_segments: 3,
            max_overlap_bytes: 14,
            max_overlap_segments: 2,
            ..WindowLimits::default()
        };
        let windows = transcript_windows(&source, limits).unwrap();
        let ids: Vec<_> = windows
            .iter()
            .flat_map(|window| window.primary.iter().map(|s| &s.id))
            .collect();
        assert_eq!(ids, source.iter().map(|s| &s.id).collect::<Vec<_>>());
        assert_eq!(windows.len(), 6);
        assert_eq!(windows.last().unwrap().primary.len(), 2);
        for window in &windows {
            assert!(window.context.len() <= 2);
            assert!(window.context.iter().map(|s| s.text.len()).sum::<usize>() <= 14);
        }
        assert_eq!(windows[1].context[0].id, "original-1");
    }
    #[test]
    fn indivisible_cues_fail_instead_of_losing_unicode_or_tail() {
        let source = segments(1);
        assert_eq!(
            transcript_windows(
                &source,
                WindowLimits {
                    max_primary_bytes: 6,
                    ..WindowLimits::default()
                }
            )
            .unwrap_err(),
            WindowError::IndivisibleCue { index: 0 }
        );
        assert!(
            transcript_windows(&[], WindowLimits::default())
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn rejects_entire_source_over_admission_budget() {
        assert_eq!(
            transcript_windows(
                &segments(2),
                WindowLimits {
                    max_total_bytes: 7,
                    ..WindowLimits::default()
                }
            )
            .unwrap_err(),
            WindowError::Budget
        );
    }
}
