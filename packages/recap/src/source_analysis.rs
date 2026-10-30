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
/// Prefer a nearby cue boundary, not a guessed topic boundary. Search only the
/// final quarter (at most eight cues), retaining at least three quarters of the
/// admitted text bytes. Unpunctuated speech falls back to the hard cutoff.
fn natural_end(transcript: &[TranscriptSegment], start: usize, end: usize, bytes: usize) -> usize {
    if end == transcript.len() {
        return end;
    }
    let earliest = end - ((end - start) / 4).min(8);
    let mut removed_bytes = 0;
    for boundary in (earliest.max(start + 1)..=end).rev() {
        if removed_bytes > bytes / 4 {
            break;
        }
        let previous = &transcript[boundary - 1];
        let next = &transcript[boundary];
        let text = previous
            .text
            .trim_end()
            .trim_end_matches(['\"', '\'', '”', '’', ')', ']', '」', '』']);
        let sentence = text.ends_with(['.', '?', '!', '。', '？', '！']);
        let pause = next.range.start_ms.saturating_sub(previous.range.end_ms) >= 1_000;
        if sentence || pause {
            return boundary;
        }
        removed_bytes += previous.text.len();
    }
    end
}

/// Partition the full transcript without dropping tails or changing segment IDs.
///
/// Prefer nearby sentence/pause boundaries within hard byte and segment limits.
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
        end = natural_end(transcript, start, end, bytes);
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

/// Plan Detailed's temporal source coverage using the same semantic cue planner.
///
/// Primary cue identities belong to exactly one window; temporal overlap does not
/// confer transcript ownership. Caller admission has already validated the document.
pub(crate) fn detailed_windows(
    document: &crate::contracts::RecapDocument,
    policy: &crate::analysis_contracts::DetailedPolicy,
    cancel: &crate::process::Cancellation,
) -> Result<Vec<crate::analysis_contracts::SourceWindow>, crate::composition::CompositionError> {
    use crate::composition::CompositionError;
    policy.validate()?;
    if cancel.is_cancelled() {
        return Err(crate::model::ModelError::Cancelled.into());
    }
    let source_limit = policy.allowances.max_input_bytes / 4;
    let total_limit = source_limit.saturating_mul(if policy.allowances.max_source_windows == 0 {
        usize::MAX
    } else {
        policy.allowances.max_source_windows
    });
    // Admission precedes cloning; never truncate a cue or a source tail.
    transcript_windows(
        &document.transcript,
        WindowLimits {
            max_primary_bytes: source_limit,
            max_primary_segments: document.transcript.len().max(1),
            max_total_bytes: total_limit,
            max_total_segments: document.transcript.len(),
            ..WindowLimits::default()
        },
    )
    .map_err(|_| CompositionError::Budget)?;
    let mut transcript = document.transcript.clone();
    transcript.sort_by_key(|cue| (cue.range.start_ms, cue.range.end_ms));
    let mut windows = Vec::new();
    let mut first = 0;
    let mut start = 0;
    while start < document.source.duration_ms {
        if cancel.is_cancelled() {
            return Err(crate::model::ModelError::Cancelled.into());
        }
        if policy.allowances.max_source_windows > 0
            && windows.len() >= policy.allowances.max_source_windows
        {
            return Err(CompositionError::Limit {
                resource: "explicit source-window allowance",
                actual: windows.len().saturating_add(1),
                limit: policy.allowances.max_source_windows,
            });
        }
        let end = start
            .saturating_add(policy.window_duration_ms)
            .min(document.source.duration_ms);
        let eligible = transcript[first..]
            .iter()
            .take_while(|cue| cue.range.end_ms <= end)
            .count();
        let count = detailed_primary_count(&transcript, first, eligible, source_limit)?;
        let window = fit_detailed_window(
            document,
            &transcript,
            first..first + count,
            start,
            end,
            windows.len(),
            source_limit,
        )?;
        let next = first + window.transcript_ids.len();
        let covered_end = window.range.end_ms;
        windows.push(window);
        if covered_end == document.source.duration_ms && next == transcript.len() {
            break;
        }
        // Preserve the requested temporal overlap where possible. A crossing cue
        // may require more overlap to keep its complete range in its owning window.
        let mut next_start = covered_end
            .saturating_sub(policy.window_overlap_ms)
            .max(start);
        if let Some(cue) = transcript.get(next) {
            next_start = next_start.min(cue.range.start_ms).max(start);
        }
        if next_start == start && next == first {
            // A byte-driven short visual window may not admit the full requested
            // overlap. Advance to its end rather than lose the remaining video.
            next_start = covered_end;
            if transcript
                .get(next)
                .is_some_and(|cue| cue.range.start_ms < next_start)
            {
                return Err(CompositionError::Budget);
            }
        }
        start = next_start;
        first = next;
    }
    Ok(windows)
}

fn detailed_primary_count(
    transcript: &[TranscriptSegment],
    first: usize,
    eligible: usize,
    source_limit: usize,
) -> Result<usize, crate::composition::CompositionError> {
    if eligible == 0 {
        return Ok(0);
    }
    // One lookahead cue lets the existing planner recognize a normal temporal
    // cutoff as a boundary, rather than mistake it for the end of the source.
    let through = (first + eligible).saturating_add(1).min(transcript.len());
    let windows = transcript_windows(
        &transcript[first..through],
        WindowLimits {
            max_primary_bytes: source_limit,
            max_primary_segments: eligible,
            max_overlap_bytes: 0,
            max_overlap_segments: 0,
            max_total_bytes: usize::MAX,
            max_total_segments: through - first,
        },
    )
    .map_err(|_| crate::composition::CompositionError::Budget)?;
    Ok(windows[0].primary.len())
}

fn fit_detailed_window(
    document: &crate::contracts::RecapDocument,
    transcript: &[TranscriptSegment],
    primary_indices: std::ops::Range<usize>,
    start: u64,
    mut end: u64,
    index: usize,
    source_limit: usize,
) -> Result<crate::analysis_contracts::SourceWindow, crate::composition::CompositionError> {
    use crate::{
        analysis_contracts::SourceWindow, composition::CompositionError, contracts::TimeRange,
    };
    let first = primary_indices.start;
    let mut count = primary_indices.len();
    loop {
        let primary = &transcript[first..first + count];
        if let Some(next) = transcript.get(first + count) {
            let primary_end = primary
                .iter()
                .map(|cue| cue.range.end_ms)
                .max()
                .unwrap_or(start);
            end = end.min(next.range.start_ms.max(primary_end));
        }
        if end <= start {
            return Err(CompositionError::Budget);
        }
        let range = TimeRange {
            start_ms: start,
            end_ms: end,
        };
        let window = SourceWindow {
            id: format!("window-{index}"),
            range,
            transcript_ids: primary.iter().map(|cue| cue.id.clone()).collect(),
            evidence_ids: document
                .evidence
                .iter()
                .filter(|e| e.range.start_ms < end && start < e.range.end_ms)
                .map(|e| e.id.clone())
                .collect(),
            asset_ids: document
                .assets
                .iter()
                .filter(|a| {
                    a.mime_type.starts_with("image/")
                        && a.range.start_ms < end
                        && start < a.range.end_ms
                })
                .map(|a| a.id.clone())
                .collect(),
        };
        // Include identity envelopes, escaped transcript, context, evidence and
        // asset metadata, not only raw transcript bytes. Model image bytes remain
        // independently checked by the operation executor.
        let payload = serde_json::json!({
            "window": &window,
            "untrusted_source": detailed_window_source(document, &window),
        });
        if serde_json::to_vec(&payload)?.len() <= source_limit {
            return Ok(window);
        }
        match count {
            0 => {
                let middle = start + (end - start) / 2;
                if middle == start {
                    return Err(CompositionError::Budget);
                }
                end = middle;
            }
            1 => {
                let cue_end = primary[0].range.end_ms;
                if end <= cue_end {
                    return Err(CompositionError::Budget);
                }
                end = cue_end;
            }
            _ => count /= 2,
        }
    }
}

/// Construct the exact source payload used both by admission and every stage.
///
/// Context is separately labeled and never inserted into primary transcript IDs
/// or granted additional evidence identities.
pub(crate) fn detailed_window_source(
    document: &crate::contracts::RecapDocument,
    window: &crate::analysis_contracts::SourceWindow,
) -> serde_json::Value {
    let mut transcript: Vec<_> = document.transcript.iter().collect();
    transcript.sort_by_key(|cue| (cue.range.start_ms, cue.range.end_ms));
    let first = transcript
        .iter()
        .position(|cue| window.transcript_ids.contains(&cue.id))
        .unwrap_or_else(|| {
            transcript.partition_point(|cue| cue.range.start_ms < window.range.start_ms)
        });
    let limits = WindowLimits::default();
    let mut context_start = first;
    // Include the JSON array brackets and separators in the context byte limit.
    let mut bytes = 2;
    while context_start > 0 && first - context_start < limits.max_overlap_segments {
        let cue = transcript[context_start - 1];
        let size =
            serde_json::to_vec(cue).map_or(usize::MAX, |value| value.len().saturating_add(1));
        if size > limits.max_overlap_bytes - bytes {
            break;
        }
        bytes += size;
        context_start -= 1;
    }
    serde_json::json!({
        "range": window.range,
        "transcript": transcript.iter().filter(|cue| window.transcript_ids.contains(&cue.id)).collect::<Vec<_>>(),
        "preceding_context": {
            "role": "Context only, not newly covered transcript; do not inventory context-only information.",
            "transcript": &transcript[context_start..first],
        },
        "evidence": document.evidence.iter().filter(|e| window.evidence_ids.contains(&e.id)).collect::<Vec<_>>(),
        "assets": document.assets.iter().filter(|a| window.asset_ids.contains(&a.id)).collect::<Vec<_>>(),
    })
}

/// Adapt the existing window planner to immutable, direct-source passage inputs.
///
/// Caller admission validates the document. Context retains exact preceding cues,
/// but grants neither new primary ownership nor evidence/asset authority. Image
/// records identify source assets; actual bytes must be supplied by the executor.
///
/// Primary evidence must intersect the window and either reference at least one
/// primary cue or have no transcript references (potential visual support).
/// Context-only transcript evidence is excluded. Mixed primary/context evidence
/// remains an unchanged source record, not authority to narrate its nonprimary
/// cues as newly covered material. Context evidence/assets are always empty.
/// Missing primary evidence is not synthesized or borrowed from context: callers
/// must reject unsupported output or report the missing support explicitly.
///
/// Whole-cue ownership and source references establish selection mechanics, not
/// preservation of every explanation, example or qualification. Independent
/// source-based review remains necessary; image presence does not prove inspection.
pub(crate) fn passage_units(
    document: &crate::contracts::RecapDocument,
    policy: &crate::analysis_contracts::DetailedPolicy,
    cancel: &crate::process::Cancellation,
) -> Result<Vec<crate::passage_contracts::SourceUnit>, crate::composition::CompositionError> {
    use crate::{
        composition::CompositionError,
        contracts::TimeRange,
        passage_contracts::{PassageSource, SourceUnit},
    };
    let windows = detailed_windows(document, policy, cancel)?;
    let mut units = Vec::with_capacity(windows.len());
    for window in windows {
        if cancel.is_cancelled() {
            return Err(crate::model::ModelError::Cancelled.into());
        }
        // Use the admitted payload's exact ordering and bounded context selection.
        let payload = detailed_window_source(document, &window);
        let transcript: Vec<TranscriptSegment> =
            serde_json::from_value(payload["transcript"].clone())?;
        let context_transcript: Vec<TranscriptSegment> =
            serde_json::from_value(payload["preceding_context"]["transcript"].clone())?;
        let context_range = context_transcript
            .iter()
            .fold(None::<TimeRange>, |range, cue| {
                Some(range.map_or(cue.range, |range| TimeRange {
                    start_ms: range.start_ms.min(cue.range.start_ms),
                    end_ms: range.end_ms.max(cue.range.end_ms),
                }))
            })
            .unwrap_or(TimeRange {
                start_ms: window.range.start_ms,
                end_ms: window.range.start_ms,
            });
        let primary = PassageSource {
            range: window.range,
            transcript,
            // Temporal overlap alone must not promote context-only transcript
            // evidence to primary support. Mixed evidence remains unchanged;
            // only the primary transcript, not every referenced cue, is owned.
            evidence: document
                .evidence
                .iter()
                .filter(|evidence| {
                    window.evidence_ids.contains(&evidence.id)
                        && (evidence.transcript_segment_ids.is_empty()
                            || evidence
                                .transcript_segment_ids
                                .iter()
                                .any(|id| window.transcript_ids.contains(id)))
                })
                .cloned()
                .collect(),
            assets: serde_json::from_value(payload["assets"].clone())?,
        };
        let unit = SourceUnit {
            source_context: document.source.context.for_composition(),
            id: window.id,
            primary,
            context: PassageSource {
                range: context_range,
                transcript: context_transcript,
                evidence: Vec::new(),
                assets: Vec::new(),
            },
        };
        // Budget the actual typed source envelope as well as planner admission.
        if serde_json::to_vec(&unit)?.len() > policy.allowances.max_input_bytes / 4 {
            return Err(CompositionError::Budget);
        }
        units.push(unit);
    }
    Ok(units)
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
    fn prefers_nearby_sentence_or_pause_without_changing_primary_text() {
        for sentence in [true, false] {
            let mut source = segments(12);
            for (index, cue) in source.iter_mut().enumerate() {
                cue.text = "cueword ".into();
                cue.range.start_ms = u64::try_from(index).unwrap() * 1_000;
                cue.range.end_ms = cue.range.start_ms + 500;
            }
            if sentence {
                source[5].text = "Finish. ".into();
            } else {
                for cue in &mut source[6..] {
                    cue.range.start_ms += 500;
                    cue.range.end_ms += 500;
                }
            }
            let limits = WindowLimits {
                max_primary_bytes: 64,
                max_primary_segments: 8,
                max_overlap_bytes: 16,
                max_overlap_segments: 2,
                ..WindowLimits::default()
            };
            let windows = transcript_windows(&source, limits).unwrap();
            assert_eq!(windows[0].primary.len(), 6);
            assert_eq!(windows[1].context, &source[4..6]);
            let primary: Vec<_> = windows.iter().flat_map(|w| w.primary).collect();
            assert_eq!(primary, source.iter().collect::<Vec<_>>());
            for window in windows {
                assert!(window.primary.len() <= limits.max_primary_segments);
                assert!(window.primary.iter().map(|s| s.text.len()).sum::<usize>() <= 64);
                assert!(window.context.iter().map(|s| s.text.len()).sum::<usize>() <= 16);
            }
        }
    }

    #[test]
    fn recognizes_quoted_cjk_sentence_endings() {
        let mut source = segments(10);
        source[5].text = "終わり。」".into();
        let windows = transcript_windows(
            &source,
            WindowLimits {
                max_primary_bytes: 64,
                max_primary_segments: 8,
                ..WindowLimits::default()
            },
        )
        .unwrap();
        assert_eq!(windows[0].primary, &source[..6]);
    }

    #[test]
    fn ignores_distant_boundaries_and_keeps_a_complete_tail_together() {
        let mut source = segments(12);
        source[0].text = "Done.".into();
        let limits = WindowLimits {
            max_primary_bytes: 100,
            max_primary_segments: 8,
            ..WindowLimits::default()
        };
        let windows = transcript_windows(&source, limits).unwrap();
        assert_eq!(windows[0].primary.len(), 8);
        assert_eq!(windows[1].primary, &source[8..]);
        source[9].text = "終わり。\"".into();
        let windows = transcript_windows(&source, limits).unwrap();
        assert_eq!(windows[1].primary, &source[8..]);
    }

    #[test]
    fn natural_boundary_never_discards_most_of_a_byte_limited_window() {
        let mut source = segments(10);
        source[5].text = "Done.".into();
        source[6].text = "🙂".repeat(30);
        let windows = transcript_windows(
            &source,
            WindowLimits {
                max_primary_bytes: 200,
                max_primary_segments: 8,
                ..WindowLimits::default()
            },
        )
        .unwrap();
        assert_eq!(windows[0].primary.len(), 8);
    }

    fn detailed_source(
        transcript: Vec<TranscriptSegment>,
        duration_ms: u64,
    ) -> crate::contracts::RecapDocument {
        use crate::contracts::{Evidence, GenerationIntent, RecapDocument, Source};
        let evidence = transcript
            .iter()
            .map(|cue| Evidence {
                id: format!("evidence-{}", cue.id),
                range: cue.range,
                transcript_segment_ids: vec![cue.id.clone()],
            })
            .collect();
        RecapDocument {
            schema_version: 1,
            id: "source-test".into(),
            revision: 1,
            title: "Source".into(),
            source: Source {
                context: crate::source_context::SourceContext::default(),
                id: "video".into(),
                title: "Video".into(),
                url: None,
                duration_ms,
            },
            intent: GenerationIntent::default(),
            transcript,
            evidence,
            assets: vec![],
            sections: vec![],
        }
    }

    fn timed_segments(count: usize) -> Vec<TranscriptSegment> {
        segments(count)
            .into_iter()
            .enumerate()
            .map(|(index, mut cue)| {
                cue.range.start_ms = u64::try_from(index).unwrap() * 1_000;
                cue.range.end_ms = cue.range.start_ms + 1_000;
                cue.text = "continued explanation".into();
                cue
            })
            .collect()
    }

    fn assert_detailed_coverage(
        document: &crate::contracts::RecapDocument,
        windows: &[crate::analysis_contracts::SourceWindow],
    ) {
        let mut covered_until = 0;
        for window in windows {
            assert!(window.range.start_ms <= covered_until);
            assert!(window.range.end_ms > window.range.start_ms);
            covered_until = covered_until.max(window.range.end_ms);
            for id in &window.transcript_ids {
                let cue = document
                    .transcript
                    .iter()
                    .find(|cue| &cue.id == id)
                    .unwrap();
                assert!(window.range.start_ms <= cue.range.start_ms);
                assert!(cue.range.end_ms <= window.range.end_ms);
            }
            for id in &window.evidence_ids {
                assert!(document.evidence.iter().any(|evidence| &evidence.id == id));
            }
        }
        assert_eq!(covered_until, document.source.duration_ms);
        let mut actual: Vec<_> = windows
            .iter()
            .flat_map(|window| window.transcript_ids.iter())
            .collect();
        let mut expected: Vec<_> = document.transcript.iter().map(|cue| &cue.id).collect();
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected);
    }

    #[test]
    fn detailed_normal_duration_split_uses_sentence_boundary_and_labeled_context() {
        use crate::{analysis_contracts::DetailedPolicy, process::Cancellation};
        let mut cues = timed_segments(12);
        cues[5].text = "The explanation ends here.".into();
        let document = detailed_source(cues, 12_000);
        let policy = DetailedPolicy {
            window_duration_ms: 8_000,
            window_overlap_ms: 1_000,
            ..DetailedPolicy::default()
        };
        let windows = detailed_windows(&document, &policy, &Cancellation::default()).unwrap();
        assert_eq!(windows[0].transcript_ids.len(), 6);
        assert_eq!(windows[0].range.end_ms, 6_000);
        assert_eq!(windows[1].range.start_ms, 5_000);
        assert_detailed_coverage(&document, &windows);
        let source = detailed_window_source(&document, &windows[1]);
        assert_eq!(source["transcript"][0]["id"], "original-6");
        let context = source["preceding_context"]["transcript"]
            .as_array()
            .unwrap();
        assert_eq!(context.last().unwrap()["id"], "original-5");
        assert!(context.len() <= WindowLimits::default().max_overlap_segments);
        assert!(
            serde_json::to_vec(context).unwrap().len()
                <= WindowLimits::default().max_overlap_bytes + 2
        );
        assert!(!windows[1].transcript_ids.contains(&"original-5".into()));
    }

    #[test]
    fn detailed_covers_visual_only_sources_and_transcript_gaps() {
        use crate::{
            analysis_contracts::DetailedPolicy,
            contracts::{Evidence, TimeRange},
            process::Cancellation,
        };
        for cues in [
            vec![],
            vec![TranscriptSegment {
                id: "middle".into(),
                range: TimeRange {
                    start_ms: 4_000,
                    end_ms: 5_000,
                },
                text: "One explanation.".into(),
            }],
        ] {
            let mut document = detailed_source(cues, 12_000);
            document.evidence.push(Evidence {
                id: "visual-gap".into(),
                range: TimeRange {
                    start_ms: 8_000,
                    end_ms: 9_000,
                },
                transcript_segment_ids: vec![],
            });
            let policy = DetailedPolicy {
                window_duration_ms: 3_000,
                window_overlap_ms: 500,
                ..DetailedPolicy::default()
            };
            let windows = detailed_windows(&document, &policy, &Cancellation::default()).unwrap();
            assert_detailed_coverage(&document, &windows);
            assert!(windows.iter().any(|window| window.transcript_ids.is_empty()
                && window.evidence_ids.contains(&"visual-gap".into())));
            assert!(
                windows
                    .iter()
                    .all(|window| window.range.end_ms - window.range.start_ms
                        <= policy.window_duration_ms)
            );
        }
    }

    #[test]
    fn detailed_crossing_cues_are_owned_whole_once_even_with_temporal_overlap() {
        use crate::{analysis_contracts::DetailedPolicy, process::Cancellation};
        let mut cues = timed_segments(8);
        cues[3].range.end_ms = 5_000;
        let document = detailed_source(cues, 8_000);
        let policy = DetailedPolicy {
            window_duration_ms: 4_000,
            window_overlap_ms: 500,
            ..DetailedPolicy::default()
        };
        let windows = detailed_windows(&document, &policy, &Cancellation::default()).unwrap();
        assert_detailed_coverage(&document, &windows);
        assert_eq!(windows[0].transcript_ids.len(), 3);
        assert!(
            windows
                .iter()
                .all(|window| window.range.end_ms - window.range.start_ms <= 4_000)
        );
    }

    #[test]
    fn detailed_budgets_include_escaped_text_context_and_identity_envelopes() {
        use crate::{analysis_contracts::DetailedPolicy, process::Cancellation};
        let mut cues = timed_segments(16);
        for cue in &mut cues {
            cue.text = "\"\\\n".repeat(15);
        }
        let document = detailed_source(cues, 16_000);
        let mut policy = DetailedPolicy {
            window_duration_ms: 8_000,
            window_overlap_ms: 500,
            ..DetailedPolicy::default()
        };
        policy.allowances.max_input_bytes = 16_000;
        let windows = detailed_windows(&document, &policy, &Cancellation::default()).unwrap();
        assert_detailed_coverage(&document, &windows);
        for window in &windows {
            let payload = serde_json::json!({ "window": window, "untrusted_source": detailed_window_source(&document, window) });
            assert!(
                serde_json::to_vec(&payload).unwrap().len()
                    <= policy.allowances.max_input_bytes / 4
            );
        }
        policy.allowances.max_source_windows = 1;
        assert!(detailed_windows(&document, &policy, &Cancellation::default()).is_err());
    }

    #[test]
    fn detailed_visual_metadata_is_budgeted_and_unshrinkable_payload_fails() {
        use crate::{
            analysis_contracts::DetailedPolicy,
            contracts::{Asset, AssetPath, TimeRange},
            process::Cancellation,
        };
        let mut document = detailed_source(vec![], 10_000);
        document.assets.push(Asset {
            illustration: None,
            id: "large-metadata".repeat(300),
            path: AssetPath::new("frame.jpg").unwrap(),
            mime_type: "image/jpeg".into(),
            range: TimeRange {
                start_ms: 0,
                end_ms: 10_000,
            },
        });
        let mut policy = DetailedPolicy::default();
        policy.allowances.max_input_bytes = 8_000;
        assert!(matches!(
            detailed_windows(&document, &policy, &Cancellation::default()),
            Err(crate::composition::CompositionError::Budget
                | crate::composition::CompositionError::Limit { .. })
        ));
    }

    #[test]
    fn detailed_never_invents_context_evidence_or_silently_truncates_indivisible_source() {
        use crate::{analysis_contracts::DetailedPolicy, process::Cancellation};
        let document = detailed_source(timed_segments(3), 3_000);
        let policy = DetailedPolicy {
            window_duration_ms: 1_000,
            window_overlap_ms: 0,
            ..DetailedPolicy::default()
        };
        let windows = detailed_windows(&document, &policy, &Cancellation::default()).unwrap();
        let source = detailed_window_source(&document, &windows[1]);
        assert_eq!(source["evidence"].as_array().unwrap().len(), 1);
        assert_eq!(source["evidence"][0]["id"], "evidence-original-1");
        assert_eq!(
            source["preceding_context"]["transcript"][0]["id"],
            "original-0"
        );
        let mut oversized = document.clone();
        oversized.transcript[0].text = "x".repeat(policy.allowances.max_input_bytes);
        assert!(detailed_windows(&oversized, &policy, &Cancellation::default()).is_err());
        let cancel = Cancellation::default();
        cancel.cancel();
        assert!(matches!(
            detailed_windows(&document, &policy, &cancel),
            Err(crate::composition::CompositionError::Model(
                crate::model::ModelError::Cancelled
            ))
        ));
    }

    #[test]
    fn passage_primary_and_context_retain_exact_source_without_context_authority() {
        use crate::{analysis_contracts::DetailedPolicy, process::Cancellation};
        let mut cues = timed_segments(8);
        cues[0].text = "Attributed example: 17 units, except when cold.".into();
        let mut document = detailed_source(cues, 8_000);
        // Valid broad evidence overlaps later windows but supports only cue zero.
        document.evidence[0].range.end_ms = 8_000;
        document.validate().unwrap();
        let policy = DetailedPolicy {
            window_duration_ms: 3_000,
            window_overlap_ms: 500,
            ..DetailedPolicy::default()
        };
        let cancel = Cancellation::default();
        let windows = detailed_windows(&document, &policy, &cancel).unwrap();
        let units = passage_units(&document, &policy, &cancel).unwrap();
        let owned: Vec<_> = units
            .iter()
            .flat_map(|unit| unit.primary.transcript.iter())
            .collect();
        assert_eq!(owned, document.transcript.iter().collect::<Vec<_>>());
        for (unit, window) in units.iter().zip(&windows) {
            let payload = detailed_window_source(&document, window);
            assert_eq!(unit.id, window.id);
            assert_eq!(unit.primary.range, window.range);
            assert_eq!(
                serde_json::to_value(&unit.primary.transcript).unwrap(),
                payload["transcript"]
            );
            assert_eq!(
                serde_json::to_value(&unit.context.transcript).unwrap(),
                payload["preceding_context"]["transcript"]
            );
            assert!(unit.context.evidence.is_empty());
            assert!(unit.context.assets.is_empty());
            for cue in &unit.context.transcript {
                assert!(
                    !unit
                        .primary
                        .transcript
                        .iter()
                        .any(|primary| primary.id == cue.id)
                );
                assert!(unit.context.range.start_ms <= cue.range.start_ms);
                assert!(cue.range.end_ms <= unit.context.range.end_ms);
            }
        }
        assert!(units[0].primary.evidence.contains(&document.evidence[0]));
        assert!(
            units[1]
                .context
                .transcript
                .contains(&document.transcript[0])
        );
        assert!(!units[1].primary.evidence.contains(&document.evidence[0]));
    }

    #[test]
    fn passage_visual_only_gaps_retain_actual_asset_and_evidence_records() {
        use crate::{
            analysis_contracts::DetailedPolicy,
            contracts::{Asset, AssetPath, Evidence},
            process::Cancellation,
        };
        let mut document = detailed_source(vec![], 8_000);
        document.evidence.push(Evidence {
            id: "diagram".into(),
            range: TimeRange {
                start_ms: 4_000,
                end_ms: 4_001,
            },
            transcript_segment_ids: vec![],
        });
        document.assets.push(Asset {
            illustration: None,
            id: "frame".into(),
            path: AssetPath::new("frame.jpg").unwrap(),
            mime_type: "image/jpeg".into(),
            range: document.evidence[0].range,
        });
        let policy = DetailedPolicy {
            window_duration_ms: 3_000,
            window_overlap_ms: 500,
            ..DetailedPolicy::default()
        };
        let units = passage_units(&document, &policy, &Cancellation::default()).unwrap();
        assert!(units.iter().all(|unit| unit.primary.transcript.is_empty()));
        assert!(
            units
                .iter()
                .any(|unit| unit.primary.assets == document.assets
                    && unit.primary.evidence == document.evidence)
        );
        assert_eq!(units.first().unwrap().primary.range.start_ms, 0);
        assert_eq!(units.last().unwrap().primary.range.end_ms, 8_000);
    }

    #[test]
    fn passage_crossing_cue_is_owned_whole_once() {
        use crate::{analysis_contracts::DetailedPolicy, process::Cancellation};
        let mut cues = timed_segments(8);
        cues[3].range.end_ms = 5_000;
        let document = detailed_source(cues, 8_000);
        let policy = DetailedPolicy {
            window_duration_ms: 4_000,
            window_overlap_ms: 500,
            ..DetailedPolicy::default()
        };
        let units = passage_units(&document, &policy, &Cancellation::default()).unwrap();
        assert_eq!(
            units
                .iter()
                .flat_map(|unit| &unit.primary.transcript)
                .collect::<Vec<_>>(),
            document.transcript.iter().collect::<Vec<_>>()
        );
        for unit in units {
            for cue in unit.primary.transcript {
                assert!(unit.primary.range.start_ms <= cue.range.start_ms);
                assert!(cue.range.end_ms <= unit.primary.range.end_ms);
            }
        }
    }

    #[test]
    fn passage_long_crossing_cue_preserves_planner_failure_instead_of_truncating() {
        use crate::{analysis_contracts::DetailedPolicy, process::Cancellation};
        let mut cues = timed_segments(1);
        cues[0].range.end_ms = 5_000;
        let document = detailed_source(cues, 10_000);
        let policy = DetailedPolicy {
            window_duration_ms: 4_000,
            window_overlap_ms: 500,
            ..DetailedPolicy::default()
        };
        assert!(matches!(
            passage_units(&document, &policy, &Cancellation::default()),
            Err(crate::composition::CompositionError::Budget
                | crate::composition::CompositionError::Limit { .. })
        ));
    }

    #[test]
    fn passage_limits_and_cancellation_fail_without_partial_source() {
        use crate::{analysis_contracts::DetailedPolicy, process::Cancellation};
        let mut document = detailed_source(timed_segments(8), 8_000);
        for cue in &mut document.transcript {
            cue.text = "語🙂\\\"\\n".repeat(15);
        }
        let mut policy = DetailedPolicy {
            window_duration_ms: 3_000,
            window_overlap_ms: 500,
            ..DetailedPolicy::default()
        };
        policy.allowances.max_input_bytes = 16_000;
        let cancel = Cancellation::default();
        let units = passage_units(&document, &policy, &cancel).unwrap();
        for unit in units {
            assert!(
                serde_json::to_vec(&unit).unwrap().len() <= policy.allowances.max_input_bytes / 4
            );
        }
        policy.allowances.max_source_windows = 1;
        assert!(matches!(
            passage_units(&document, &policy, &cancel),
            Err(crate::composition::CompositionError::Budget
                | crate::composition::CompositionError::Limit { .. })
        ));
        policy.allowances.max_source_windows = 100;
        document.transcript[0].text = "x".repeat(policy.allowances.max_input_bytes);
        assert!(matches!(
            passage_units(&document, &policy, &cancel),
            Err(crate::composition::CompositionError::Budget
                | crate::composition::CompositionError::Limit { .. })
        ));
        cancel.cancel();
        assert!(matches!(
            passage_units(&document, &policy, &cancel),
            Err(crate::composition::CompositionError::Model(
                crate::model::ModelError::Cancelled
            ))
        ));
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
