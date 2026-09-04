//! Host-side prompt-cache breakpoint planning.
//!
//! The planner decides which stable request sections should end with provider cache points and
//! which hints to send. It works only on normalized [`bcode_model`] types; provider adapters
//! translate the resulting hints and [`ContentBlock::CachePoint`] blocks into their wire formats
//! and may drop points to satisfy their own budgets, reporting drops through request projection.

use crate::estimated_tokens_from_chars;
use bcode_model::{
    ContentBlock, MessageRole, ModelCacheCapability, ModelCacheInfo, ModelMessage,
    PromptCacheHints, PromptCacheMode, PromptCachePoint,
};
use std::collections::BTreeSet;

/// Maximum rolling conversation breakpoints the host places in one request.
///
/// Bounded independently of the provider budget so write churn stays low: each round moves at
/// most this many points forward through the conversation.
pub const MAX_CONVERSATION_CACHE_POINTS: usize = 3;

/// Explicit cache-point budget assumed for one request when the model does not declare one.
///
/// Anthropic-style explicit caches allow four breakpoints per request. The planner spends one on
/// the system prompt and one on tool definitions when present, and gives the remainder to the
/// rolling conversation prefix so no request exceeds the budget and forces provider-side drops.
pub const DEFAULT_MAX_CACHE_POINTS: usize = 4;

/// Minimum message count before the conversation prefix is worth a breakpoint.
pub const MIN_MESSAGES_FOR_CONVERSATION_CACHE: usize = 3;

/// Label the host attaches to conversation-prefix cache points.
pub const CONVERSATION_PREFIX_LABEL: &str = "conversation_prefix";

/// Inputs for [`plan_prompt_cache`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptCachePlanInput<'a> {
    /// Cache mode selected by the host configuration.
    pub mode: PromptCacheMode,
    /// Stable partition key for this conversation's cache entries.
    pub cache_key: String,
    /// Resolved cache capabilities for the selected model.
    pub cache: &'a ModelCacheInfo,
    /// Stable system prompt that precedes the conversation, when present.
    ///
    /// Only its length matters to planning: together with the tool definitions it determines
    /// whether the request prefix is long enough to be cacheable at all.
    pub system_prompt: Option<&'a str>,
    /// Estimated tokens contributed by tool definitions ahead of the conversation.
    pub tool_definition_tokens: u64,
}

/// Plan prompt-cache hints and place conversation cache points into `messages`.
///
/// Any cache points already present are removed first: they are projection artifacts from an
/// earlier round and must not accumulate. When `mode` is [`PromptCacheMode::Off`] no hints or
/// points are produced. Conversation points are placed only when the model advertises explicit
/// cache points and the stable prefix up to each point is at least the model's minimum cacheable
/// prefix; shorter prefixes would consume breakpoints without ever producing a cache hit.
#[must_use]
pub fn plan_prompt_cache(
    messages: &mut [ModelMessage],
    input: &PromptCachePlanInput<'_>,
) -> PromptCacheHints {
    for message in messages.iter_mut() {
        message
            .content
            .retain(|block| !matches!(block, ContentBlock::CachePoint { .. }));
    }

    if !input.mode.is_enabled() {
        return PromptCacheHints::default();
    }

    let explicit_cache_points = input
        .cache
        .capabilities
        .contains(&ModelCacheCapability::ExplicitCachePoints);
    let ttl_seconds = explicit_cache_points
        .then(|| input.cache.ttl_seconds.iter().next_back().copied())
        .flatten();
    if explicit_cache_points {
        let min_prefix_tokens = input
            .cache
            .min_prefix_tokens
            .unwrap_or(crate::DEFAULT_MIN_PREFIX_TOKENS);
        let has_system_prompt = input
            .system_prompt
            .is_some_and(|prompt| !prompt.trim().is_empty());
        let static_prefix_tokens = input
            .system_prompt
            .map_or(0, |prompt| {
                estimated_tokens_from_chars(prompt.chars().count())
            })
            .saturating_add(input.tool_definition_tokens);
        // Stay inside the provider's per-request budget: the adapter spends one point on the
        // system prompt and one on tool definitions when present.
        let reserved =
            usize::from(has_system_prompt) + usize::from(input.tool_definition_tokens > 0);
        let conversation_budget = DEFAULT_MAX_CACHE_POINTS
            .saturating_sub(reserved)
            .min(MAX_CONVERSATION_CACHE_POINTS);
        for index in conversation_cache_point_indices(
            messages,
            static_prefix_tokens,
            min_prefix_tokens,
            conversation_budget,
        ) {
            messages[index].content.push(ContentBlock::CachePoint {
                hint: PromptCachePoint {
                    label: Some(CONVERSATION_PREFIX_LABEL.to_string()),
                    ttl_seconds,
                },
            });
        }
    }

    PromptCacheHints {
        mode: input.mode,
        key: Some(input.cache_key.clone()),
        ttl_seconds,
        supported_ttl_seconds: input.cache.ttl_seconds.clone(),
        cache_system_prompt: true,
        cache_tools: true,
    }
}

/// Return message indices that should end with a conversation-prefix cache point.
///
/// Candidates are completed user messages and completed tool results, excluding the mutable tail
/// (the newest non-tool message, which changes every round). The newest candidates win so the
/// rolling breakpoint advances through tool-loop history. A candidate is skipped when the
/// estimated prefix through it is shorter than `min_prefix_tokens`, and at most `max_points`
/// indices are returned.
#[must_use]
pub fn conversation_cache_point_indices(
    messages: &[ModelMessage],
    static_prefix_tokens: u64,
    min_prefix_tokens: u64,
    max_points: usize,
) -> Vec<usize> {
    if messages.len() < MIN_MESSAGES_FOR_CONVERSATION_CACHE || max_points == 0 {
        return Vec::new();
    }
    let mut prefix_tokens = Vec::with_capacity(messages.len());
    let mut running = static_prefix_tokens;
    for message in messages {
        running = running.saturating_add(estimated_message_tokens(message));
        prefix_tokens.push(running);
    }
    let mutable_tail = messages
        .last()
        .is_some_and(|message| message.role != MessageRole::Tool);
    let mut indices = messages
        .iter()
        .enumerate()
        .rev()
        .skip(usize::from(mutable_tail))
        .filter(|(index, _)| prefix_tokens[*index] >= min_prefix_tokens)
        .filter_map(|(index, message)| {
            let cacheable_user = matches!(message.role, MessageRole::User)
                && message.content.iter().any(|block| {
                    matches!(
                        block,
                        ContentBlock::Text { text } if !text.trim().is_empty()
                    )
                });
            let completed_tool_result = matches!(message.role, MessageRole::Tool)
                && message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolResult { .. }));
            (cacheable_user || completed_tool_result).then_some(index)
        })
        .take(max_points)
        .collect::<Vec<_>>();
    indices.reverse();
    indices
}

/// Count cache points currently present in `messages`.
#[must_use]
pub fn cache_point_count(messages: &[ModelMessage]) -> usize {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter(|block| matches!(block, ContentBlock::CachePoint { .. }))
        .count()
}

fn estimated_message_tokens(message: &ModelMessage) -> u64 {
    serde_json::to_string(message).map_or(u64::MAX, |serialized| {
        estimated_tokens_from_chars(serialized.chars().count())
    })
}

/// Estimate the tokens contributed by tool definitions ahead of the conversation.
#[must_use]
pub fn estimated_tool_definition_tokens(tools: &[bcode_model::ToolDefinition]) -> u64 {
    serde_json::to_string(tools).map_or(u64::MAX, |serialized| {
        estimated_tokens_from_chars(serialized.chars().count())
    })
}

/// Convenience set of TTLs to request: the longest supported TTL, if any.
#[must_use]
pub fn preferred_ttl_seconds(supported: &BTreeSet<u64>) -> Option<u64> {
    supported.iter().next_back().copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_model::{ToolCall, ToolResult};

    fn explicit_cache(min_prefix_tokens: Option<u64>) -> ModelCacheInfo {
        ModelCacheInfo {
            capabilities: BTreeSet::from([
                ModelCacheCapability::ExplicitCachePoints,
                ModelCacheCapability::PromptCacheKey,
            ]),
            ttl_seconds: BTreeSet::from([30 * 60]),
            min_prefix_tokens,
        }
    }

    fn plan(
        messages: &mut [ModelMessage],
        mode: PromptCacheMode,
        cache: &ModelCacheInfo,
    ) -> PromptCacheHints {
        plan_prompt_cache(
            messages,
            &PromptCachePlanInput {
                mode,
                cache_key: "bcode:test".into(),
                cache,
                system_prompt: None,
                tool_definition_tokens: 0,
            },
        )
    }

    fn user_messages(count: usize) -> Vec<ModelMessage> {
        (0..count)
            .map(|index| ModelMessage {
                role: MessageRole::User,
                content: vec![ContentBlock::Text {
                    text: format!("message {index}"),
                }],
            })
            .collect()
    }

    fn point_indices(messages: &[ModelMessage]) -> Vec<usize> {
        messages
            .iter()
            .enumerate()
            .filter_map(|(index, message)| {
                message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::CachePoint { .. }))
                    .then_some(index)
            })
            .collect()
    }

    #[test]
    fn off_mode_emits_no_hints_and_strips_stale_points() {
        let mut messages = user_messages(4);
        messages[0].content.push(ContentBlock::CachePoint {
            hint: PromptCachePoint::default(),
        });

        let hints = plan(
            &mut messages,
            PromptCacheMode::Off,
            &explicit_cache(Some(1)),
        );

        assert_eq!(hints, PromptCacheHints::default());
        assert_eq!(cache_point_count(&messages), 0);
    }

    #[test]
    fn auto_mode_marks_stable_sections_without_history() {
        let mut messages = Vec::new();
        let hints = plan(
            &mut messages,
            PromptCacheMode::Auto,
            &explicit_cache(Some(1)),
        );
        assert!(hints.cache_system_prompt);
        assert!(hints.cache_tools);
        assert_eq!(hints.key.as_deref(), Some("bcode:test"));
        assert!(messages.is_empty());
    }

    #[test]
    fn stale_points_are_removed_before_planning() {
        let mut messages = vec![ModelMessage {
            role: MessageRole::User,
            content: vec![
                ContentBlock::Text {
                    text: "stable message".into(),
                },
                ContentBlock::CachePoint {
                    hint: PromptCachePoint::default(),
                },
            ],
        }];
        let hints = plan(
            &mut messages,
            PromptCacheMode::Auto,
            &explicit_cache(Some(1)),
        );
        assert_eq!(hints.mode, PromptCacheMode::Auto);
        assert_eq!(cache_point_count(&messages), 0);
    }

    #[test]
    fn auto_mode_rolls_breakpoints_without_marking_mutable_tail() {
        let cache = explicit_cache(Some(1));
        let mut first_round = user_messages(6);
        let first_hints = plan(&mut first_round, PromptCacheMode::Auto, &cache);
        let first_points = point_indices(&first_round);

        let mut second_round = first_round;
        second_round.push(ModelMessage {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: "mutable tail".into(),
            }],
        });
        let second_hints = plan(&mut second_round, PromptCacheMode::Auto, &cache);
        let second_points = point_indices(&second_round);

        assert_eq!(first_points, vec![2, 3, 4]);
        assert_eq!(second_points, vec![3, 4, 5]);
        assert_eq!(first_hints.key, second_hints.key);
        assert_eq!(second_hints.ttl_seconds, Some(30 * 60));
        assert!(!matches!(
            second_round
                .last()
                .and_then(|message| message.content.last()),
            Some(ContentBlock::CachePoint { .. })
        ));
    }

    #[test]
    fn auto_mode_advances_breakpoint_through_tool_loop_history() {
        let cache = explicit_cache(Some(1));
        let mut messages = vec![ModelMessage {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: "implement the requested change".into(),
            }],
        }];
        let mut planned_prefix_ends = Vec::new();

        for round in 0..8 {
            messages.push(ModelMessage {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::ToolCall {
                    call: ToolCall {
                        id: format!("call-{round}"),
                        name: "filesystem.read".into(),
                        arguments: serde_json::json!({"path": format!("src/file-{round}.rs")}),
                    },
                }],
            });
            messages.push(ModelMessage {
                role: MessageRole::Tool,
                content: vec![ContentBlock::ToolResult {
                    result: ToolResult {
                        call_id: format!("call-{round}"),
                        output: format!("stable tool output for round {round} ").repeat(128),
                        is_error: false,
                        content: Vec::new(),
                    },
                }],
            });

            let hints = plan(&mut messages, PromptCacheMode::Auto, &cache);
            assert_eq!(hints.key.as_deref(), Some("bcode:test"));
            let newest = point_indices(&messages)
                .last()
                .copied()
                .expect("tool-loop history needs a cache point");
            assert!(newest < messages.len());
            planned_prefix_ends.push(newest);
        }

        assert!(
            planned_prefix_ends.windows(2).all(|pair| pair[1] > pair[0]),
            "newest cacheable prefix must advance: {planned_prefix_ends:?}"
        );
        assert!(planned_prefix_ends.last().copied().unwrap_or_default() >= messages.len() - 4);
    }

    #[test]
    fn auto_mode_does_not_add_points_without_explicit_support() {
        let cache = ModelCacheInfo {
            capabilities: BTreeSet::from([ModelCacheCapability::AutomaticPrefixCache]),
            ..ModelCacheInfo::default()
        };
        let mut messages = user_messages(6);
        let hints = plan(&mut messages, PromptCacheMode::Auto, &cache);
        assert_eq!(hints.mode, PromptCacheMode::Auto);
        assert_eq!(hints.ttl_seconds, None);
        assert_eq!(cache_point_count(&messages), 0);
    }

    #[test]
    fn aggressive_mode_marks_conversation_prefix() {
        let mut messages = user_messages(6);
        let hints = plan(
            &mut messages,
            PromptCacheMode::Aggressive,
            &explicit_cache(Some(1)),
        );
        assert!(hints.cache_system_prompt);
        assert_eq!(hints.mode, PromptCacheMode::Aggressive);
        assert_eq!(cache_point_count(&messages), 3);
        assert!(!matches!(
            messages[5].content.last(),
            Some(ContentBlock::CachePoint { .. })
        ));
    }

    #[test]
    fn points_are_withheld_until_prefix_reaches_minimum() {
        // Six tiny messages are far below a 1024-token minimum with no system prompt.
        let mut messages = user_messages(6);
        let hints = plan(
            &mut messages,
            PromptCacheMode::Auto,
            &explicit_cache(Some(1_024)),
        );
        assert!(hints.cache_system_prompt);
        assert_eq!(cache_point_count(&messages), 0);

        // A long stable system prompt makes the same conversation cacheable from the first
        // candidate onward.
        let system_prompt = "stable instructions ".repeat(400);
        let hints = plan_prompt_cache(
            &mut messages,
            &PromptCachePlanInput {
                mode: PromptCacheMode::Auto,
                cache_key: "bcode:test".into(),
                cache: &explicit_cache(Some(1_024)),
                system_prompt: Some(&system_prompt),
                tool_definition_tokens: 0,
            },
        );
        assert_eq!(hints.mode, PromptCacheMode::Auto);
        assert_eq!(point_indices(&messages), vec![2, 3, 4]);
    }

    #[test]
    fn only_sufficiently_long_prefixes_receive_points() {
        // Messages 0..3 are tiny; message 3 is a large tool result. Only candidates at or after
        // the large result satisfy the minimum.
        let mut messages = vec![
            ModelMessage {
                role: MessageRole::User,
                content: vec![ContentBlock::Text { text: "a".into() }],
            },
            ModelMessage {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::ToolCall {
                    call: ToolCall {
                        id: "c1".into(),
                        name: "t".into(),
                        arguments: serde_json::json!({}),
                    },
                }],
            },
            ModelMessage {
                role: MessageRole::Tool,
                content: vec![ContentBlock::ToolResult {
                    result: ToolResult {
                        call_id: "c1".into(),
                        output: "short".into(),
                        is_error: false,
                        content: Vec::new(),
                    },
                }],
            },
            ModelMessage {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::ToolCall {
                    call: ToolCall {
                        id: "c2".into(),
                        name: "t".into(),
                        arguments: serde_json::json!({}),
                    },
                }],
            },
            ModelMessage {
                role: MessageRole::Tool,
                content: vec![ContentBlock::ToolResult {
                    result: ToolResult {
                        call_id: "c2".into(),
                        output: "x".repeat(8_000),
                        is_error: false,
                        content: Vec::new(),
                    },
                }],
            },
        ];
        plan(
            &mut messages,
            PromptCacheMode::Auto,
            &explicit_cache(Some(1_024)),
        );
        assert_eq!(point_indices(&messages), vec![4]);
    }

    #[test]
    fn unknown_minimum_uses_conservative_default() {
        let mut messages = user_messages(6);
        plan(&mut messages, PromptCacheMode::Auto, &explicit_cache(None));
        assert_eq!(cache_point_count(&messages), 0);
    }

    #[test]
    fn conversation_points_stay_within_the_request_budget() {
        // System prompt and tools each reserve one of the four per-request points, leaving two
        // for the rolling conversation prefix.
        let mut messages = user_messages(8);
        let system_prompt = "stable instructions ".repeat(8);
        let hints = plan_prompt_cache(
            &mut messages,
            &PromptCachePlanInput {
                mode: PromptCacheMode::Auto,
                cache_key: "bcode:test".into(),
                cache: &explicit_cache(Some(1)),
                system_prompt: Some(&system_prompt),
                tool_definition_tokens: 200,
            },
        );
        assert!(hints.cache_system_prompt && hints.cache_tools);
        assert_eq!(point_indices(&messages), vec![5, 6]);

        // Without tools the conversation gets three.
        let mut messages = user_messages(8);
        let _ = plan_prompt_cache(
            &mut messages,
            &PromptCachePlanInput {
                mode: PromptCacheMode::Auto,
                cache_key: "bcode:test".into(),
                cache: &explicit_cache(Some(1)),
                system_prompt: Some(&system_prompt),
                tool_definition_tokens: 0,
            },
        );
        assert_eq!(point_indices(&messages), vec![4, 5, 6]);
    }
}
