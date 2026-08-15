//! Deterministic provider-style source simulation for the streaming configurator.

use std::time::{Duration, Instant};

/// Named provider-delivery simulation preset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StreamingSourcePreset {
    /// Moderate variation representative of ordinary provider delivery.
    #[default]
    Balanced,
    /// Small, frequent, strongly varying chunks.
    Choppy,
    /// Alternating rapid clusters and large chunks.
    Bursty,
    /// Infrequent larger chunks with conspicuous gaps.
    Sparse,
    /// User-adjusted values not matching a named preset.
    Custom,
}

impl StreamingSourcePreset {
    /// Return every selectable named preset.
    pub const NAMED: [Self; 4] = [Self::Balanced, Self::Choppy, Self::Bursty, Self::Sparse];

    /// Return the policy represented by this preset.
    #[must_use]
    pub const fn policy(self) -> StreamingSourcePolicy {
        match self {
            Self::Balanced | Self::Custom => StreamingSourcePolicy {
                preset: self,
                target_chunk_chars: 24,
                chunk_size_variation_percent: 55,
                base_interval_ms: 90,
                interval_variation_percent: 55,
            },
            Self::Choppy => StreamingSourcePolicy {
                preset: self,
                target_chunk_chars: 8,
                chunk_size_variation_percent: 75,
                base_interval_ms: 45,
                interval_variation_percent: 65,
            },
            Self::Bursty => StreamingSourcePolicy {
                preset: self,
                target_chunk_chars: 34,
                chunk_size_variation_percent: 90,
                base_interval_ms: 75,
                interval_variation_percent: 100,
            },
            Self::Sparse => StreamingSourcePolicy {
                preset: self,
                target_chunk_chars: 72,
                chunk_size_variation_percent: 45,
                base_interval_ms: 360,
                interval_variation_percent: 85,
            },
        }
    }
}

/// Bounded source-delivery policy for the deterministic provider simulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamingSourcePolicy {
    /// Named preset or custom marker.
    pub preset: StreamingSourcePreset,
    /// Target Unicode scalar values per provider chunk.
    pub target_chunk_chars: u16,
    /// Deterministic chunk-size variation strength.
    pub chunk_size_variation_percent: u8,
    /// Nominal interval between provider chunks.
    pub base_interval_ms: u16,
    /// Deterministic interval variation strength.
    pub interval_variation_percent: u8,
}

impl StreamingSourcePolicy {
    /// Minimum target chunk size.
    pub const MIN_CHUNK_CHARS: u16 = 1;
    /// Maximum target chunk size.
    pub const MAX_CHUNK_CHARS: u16 = 256;
    /// Minimum base source interval.
    pub const MIN_INTERVAL_MS: u16 = 10;
    /// Maximum base source interval.
    pub const MAX_INTERVAL_MS: u16 = 2_000;

    /// Return bounded policy values.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.target_chunk_chars = self
            .target_chunk_chars
            .clamp(Self::MIN_CHUNK_CHARS, Self::MAX_CHUNK_CHARS);
        self.chunk_size_variation_percent = self.chunk_size_variation_percent.min(100);
        self.base_interval_ms = self
            .base_interval_ms
            .clamp(Self::MIN_INTERVAL_MS, Self::MAX_INTERVAL_MS);
        self.interval_variation_percent = self.interval_variation_percent.min(100);
        self
    }

    /// Return this policy marked custom after direct numeric editing.
    #[must_use]
    pub const fn custom(mut self) -> Self {
        self.preset = StreamingSourcePreset::Custom;
        self
    }
}

impl Default for StreamingSourcePolicy {
    fn default() -> Self {
        StreamingSourcePreset::Balanced.policy()
    }
}

const SIZE_FACTORS: [u16; 16] = [
    35, 80, 140, 55, 190, 100, 45, 165, 70, 125, 30, 210, 95, 150, 60, 115,
];
const INTERVAL_FACTORS: [u16; 16] = [
    20, 45, 180, 30, 600, 75, 25, 130, 40, 260, 55, 350, 90, 35, 170, 65,
];

/// Long fixed Markdown response used by every source policy.
pub const LONG_RESPONSE: &str = concat!(
    "# Designing a resilient streaming interface\n\n",
    "A useful streaming interface has to remain understandable while the underlying provider behaves unpredictably. Text may arrive one character at a time, in complete sentences, or in several rapid bursts followed by silence. The presentation layer should absorb those differences without inventing content, losing ordering, or delaying authoritative completion. This long response exists to make those trade-offs visible for more than a fleeting moment.\n\n",
    "## Start with explicit ownership\n\n",
    "The provider owns accepted bytes and terminal outcomes. The shared semantic projection owns grapheme-safe visible prefixes, interpolation, backlog accounting, and convergence. The terminal frontend owns layout, focus, scrolling, input, timers, and damage. Keeping those responsibilities separate makes the comparison honest: both panes receive the same source event, while only one projection applies smoothing.\n\n",
    "A practical implementation should preserve several properties:\n\n",
    "* accepted text is monotonic and never silently rewritten;\n",
    "* cancellation and completion remain absorbing terminal states;\n",
    "* source timing is deterministic enough for repeatable tests;\n",
    "* ordinary rendering work stays bounded even for long responses; and\n",
    "* interactive presentation choices never mutate declarative configuration.\n\n",
    "## Bursts, gaps, and catch-up\n\n",
    "Imagine a provider sending tiny fragments for a few hundred milliseconds, then combining an entire paragraph into one chunk. A short interval can make raw output feel frantic. A long interval can make it appear frozen. Smoothing should expose accepted text progressively, but it must accelerate when the backlog would otherwise exceed the configured maximum age. That catch-up behavior is why both the nominal rate and lag bound matter.\n\n",
    "The simulator varies chunk size and timing independently. At zero variation, delivery is intentionally steady. At full variation, fixed factor sequences create clusters, medium pauses, and conspicuous gaps throughout the response. Because the pattern is deterministic, restarting with the same policy produces the same boundaries and deadlines. Changing a setting affects only the undispatched suffix.\n\n",
    "## Unicode is ordinary text\n\n",
    "Correctness cannot assume one byte, scalar value, and visible grapheme are interchangeable. Consider cafe\u{301}, the emoji 👩🏽‍💻, flags such as 🇯🇵, and scripts such as 東京, हिन्दी, and العربية. Provider chunks may end at valid UTF-8 scalar boundaries while splitting a multi-scalar grapheme. The shared projection must still reveal only complete grapheme clusters. Both panes must eventually converge on the exact original bytes.\n\n",
    "## A small implementation sketch\n\n",
    "```rust\n",
    "while let Some(deadline) = controller.next_deadline(now) {\n",
    "    schedule(deadline);\n",
    "    let event = source.next_typed_event();\n",
    "    raw.apply_live_event(&event);\n",
    "    smoothed.apply_live_event(&event);\n",
    "}\n",
    "```\n\n",
    "The important detail is not the loop syntax. It is that the event is constructed once and delivered to both projections. Display metadata, hit targets, or viewport state must never influence accepted bytes, authorization, dispatch, or durable outcomes.\n\n",
    "## Viewports for long output\n\n",
    "A realistic response quickly exceeds a preview pane. While the user remains at the bottom, each pane should follow its latest visible text. Page Up suspends that behavior so earlier material can be inspected; Page Down eventually returns to the bottom and restores follow mode. The two panes should navigate together, because comparing unrelated regions defeats the purpose of the tool. Restart returns both panes to follow mode.\n\n",
    "## Operational failure modes\n\n",
    "There are several tempting shortcuts that produce misleading results. A permanent animation interval leaks work after a surface closes. Replaying the source after every control change hides whether live adoption works. Reimplementing interpolation in the TUI creates a preview-only approximation. Persisting simulator settings beside product smoothing policy confuses a diagnostic scenario with an actual presentation preference. Each shortcut makes the demonstration easier while making the product less truthful.\n\n",
    "Instead, use one earliest semantic deadline, preserve accepted prefixes, and retain source controls only for the lifetime of the configurator. Apply persists smoothing settings alone. Reset clears that override alone. Cancel writes nothing. If persistence fails, report the failure rather than claiming success.\n\n",
    "## Testing the complete path\n\n",
    "Unit tests should prove deterministic chunk schedules, valid UTF-8 boundaries, normalization, and exact concatenation. Controller tests should change source policy mid-stream and verify that accepted text never moves backward. Renderer tests should cover wide and stacked layouts, long-text following, suspended follow mode, Unicode, hover and pressed component states, and the minimum-size fallback. Root-runtime tests should prove that policy changes replace deadlines and that close, pause, Apply, Reset, and Cancel remove them.\n\n",
    "Automated checks are necessary but not sufficient. A person should watch the raw pane jump in visibly different ways under Balanced, Choppy, Bursty, and Sparse presets. The smoothed pane should remain calmer, respond immediately to rate and lag adjustments, and finish with byte-identical content. Scrolling should remain predictable throughout.\n\n",
    "## Why deterministic simulation matters\n\n",
    "Random traffic can look convincing, but it makes regressions difficult to reproduce. A fixed pattern provides the visual variety of irregular traffic while preserving exact expectations. Intermediate variation values interpolate between a steady baseline and the complete pattern. This also makes performance measurements comparable across runs and avoids introducing a random-number dependency into terminal presentation.\n\n",
    "## Final convergence\n\n",
    "At the end of the turn, authoritative completion wins. Any remaining accepted backlog converges according to the shared terminal contract, stale updates cannot reopen the stream, and both panes contain the same response. The raw pane documents what the simulated provider delivered; the smoothed pane demonstrates how Bcode presents those same facts. Neither pane owns canonical session history, and closing the configurator discards both projections.\n\n",
    "A good configurator therefore does more than animate text. It exposes the relationship between source behavior and presentation policy while preserving every architectural boundary that protects the real application. The response is intentionally long enough to adjust several controls, switch presets, pause, scroll, resume, and observe convergence without racing a two-second demonstration.\n"
);

/// Deterministic on-demand source partitioner and deadline planner.
pub struct StreamingSourceScenario {
    policy: StreamingSourcePolicy,
    accepted_bytes: usize,
    delivery_index: usize,
    next_deadline: Instant,
}

impl StreamingSourceScenario {
    /// Create a source scenario whose first chunk is due after its configured interval.
    #[must_use]
    pub fn new(now: Instant, policy: StreamingSourcePolicy) -> Self {
        let policy = policy.normalized();
        Self {
            policy,
            accepted_bytes: 0,
            delivery_index: 0,
            next_deadline: now + interval_for(policy, 0),
        }
    }

    /// Return current policy.
    #[must_use]
    pub const fn policy(&self) -> StreamingSourcePolicy {
        self.policy
    }

    /// Return accepted byte count.
    #[must_use]
    pub const fn accepted_bytes(&self) -> usize {
        self.accepted_bytes
    }

    /// Return delivered chunk count.
    #[must_use]
    pub const fn delivered_chunks(&self) -> usize {
        self.delivery_index
    }

    /// Return whether the complete response has been delivered.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.accepted_bytes >= LONG_RESPONSE.len()
    }

    /// Return next source deadline when content remains.
    #[must_use]
    pub const fn next_deadline(&self) -> Option<Instant> {
        if self.is_complete() {
            None
        } else {
            Some(self.next_deadline)
        }
    }

    /// Apply policy to the undispatched suffix and replace the next source deadline.
    pub fn set_policy(&mut self, policy: StreamingSourcePolicy, now: Instant) {
        self.policy = policy.normalized();
        self.next_deadline = now + interval_for(self.policy, self.delivery_index);
    }

    /// Shift the next deadline after a pause.
    pub fn shift_deadline(&mut self, duration: Duration) {
        self.next_deadline += duration;
    }

    /// Deliver the next due UTF-8 chunk and schedule its successor.
    pub fn take_due(&mut self, now: Instant) -> Option<&'static str> {
        if self.is_complete() || now < self.next_deadline {
            return None;
        }
        let start = self.accepted_bytes;
        let target_chars = chunk_chars_for(self.policy, self.delivery_index);
        let suffix = &LONG_RESPONSE[start..];
        let byte_len = suffix
            .char_indices()
            .nth(target_chars)
            .map_or(suffix.len(), |(offset, _)| offset);
        self.accepted_bytes = start.saturating_add(byte_len);
        self.delivery_index = self.delivery_index.saturating_add(1);
        self.next_deadline = now + interval_for(self.policy, self.delivery_index);
        Some(&LONG_RESPONSE[start..self.accepted_bytes])
    }
}

fn varied(base: u32, factor: u16, variation: u8) -> u32 {
    let factor_delta = i32::from(factor) - 100;
    let adjusted = 100 + factor_delta * i32::from(variation) / 100;
    base.saturating_mul(u32::try_from(adjusted.max(1)).unwrap_or(1)) / 100
}

fn chunk_chars_for(policy: StreamingSourcePolicy, index: usize) -> usize {
    usize::try_from(
        varied(
            u32::from(policy.target_chunk_chars),
            SIZE_FACTORS[index % SIZE_FACTORS.len()],
            policy.chunk_size_variation_percent,
        )
        .max(1),
    )
    .unwrap_or(usize::MAX)
}

fn interval_for(policy: StreamingSourcePolicy, index: usize) -> Duration {
    Duration::from_millis(u64::from(
        varied(
            u32::from(policy.base_interval_ms),
            INTERVAL_FACTORS[index % INTERVAL_FACTORS.len()],
            policy.interval_variation_percent,
        )
        .max(1),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(policy: StreamingSourcePolicy) -> Vec<(u128, String)> {
        let start = Instant::now();
        let mut scenario = StreamingSourceScenario::new(start, policy);
        let mut chunks = Vec::new();
        while let Some(deadline) = scenario.next_deadline() {
            let now = deadline;
            let chunk = scenario.take_due(now).expect("due chunk");
            chunks.push((now.duration_since(start).as_millis(), chunk.to_owned()));
        }
        chunks
    }

    #[test]
    fn long_response_and_every_schedule_are_exact_valid_and_deterministic() {
        assert!(LONG_RESPONSE.len() >= 5_000);
        for preset in StreamingSourcePreset::NAMED {
            let first = drain(preset.policy());
            let second = drain(preset.policy());
            assert_eq!(first, second);
            assert_eq!(
                first
                    .iter()
                    .map(|(_, chunk)| chunk.as_str())
                    .collect::<String>(),
                LONG_RESPONSE
            );
            assert!(first.len() < LONG_RESPONSE.len());
        }
    }

    #[test]
    fn variation_and_live_policy_changes_affect_only_future_delivery() {
        let steady = drain(StreamingSourcePolicy {
            chunk_size_variation_percent: 0,
            interval_variation_percent: 0,
            ..StreamingSourcePolicy::default()
        });
        assert!(
            steady
                .windows(2)
                .take(10)
                .all(|pair| pair[0].1.chars().count() == pair[1].1.chars().count())
        );
        let varied = drain(StreamingSourcePreset::Bursty.policy());
        assert!(
            varied
                .windows(2)
                .any(|pair| pair[0].1.len() != pair[1].1.len())
        );
        assert!(
            varied
                .windows(2)
                .any(|pair| pair[1].0 - pair[0].0 != varied[1].0 - varied[0].0)
        );

        let start = Instant::now();
        let mut scenario = StreamingSourceScenario::new(start, StreamingSourcePolicy::default());
        let first_due = scenario.next_deadline().expect("deadline");
        let first = scenario.take_due(first_due).expect("first").to_owned();
        let accepted = scenario.accepted_bytes();
        scenario.set_policy(StreamingSourcePreset::Sparse.policy(), first_due);
        let second_due = scenario.next_deadline().expect("new deadline");
        assert!(second_due > first_due);
        let second = scenario.take_due(second_due).expect("second");
        assert_eq!(&LONG_RESPONSE[..accepted], first);
        assert_eq!(&LONG_RESPONSE[accepted..accepted + second.len()], second);
    }

    #[test]
    fn normalization_and_presets_are_bounded() {
        let normalized = StreamingSourcePolicy {
            preset: StreamingSourcePreset::Custom,
            target_chunk_chars: 0,
            chunk_size_variation_percent: u8::MAX,
            base_interval_ms: 0,
            interval_variation_percent: u8::MAX,
        }
        .normalized();
        assert_eq!(
            normalized.target_chunk_chars,
            StreamingSourcePolicy::MIN_CHUNK_CHARS
        );
        assert_eq!(normalized.chunk_size_variation_percent, 100);
        assert_eq!(
            normalized.base_interval_ms,
            StreamingSourcePolicy::MIN_INTERVAL_MS
        );
        assert_eq!(normalized.interval_variation_percent, 100);
        assert_ne!(
            StreamingSourcePreset::Balanced.policy(),
            StreamingSourcePreset::Bursty.policy()
        );
    }
}
