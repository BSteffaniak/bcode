//! TUI model picker state.

use std::cmp::Ordering;

use bcode_model::{
    ModelInfo, ModelPricingInfo, ModelTokenPrice, ModelVisibility, ModelVisibilitySource,
};
use bmux_tui::list::{ListItem, ListState};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui_components::text_input::TextInputState;

use super::filtered_list::FilteredListState;

/// Model sort key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSortKey {
    Default,
    ModelId,
    ContextWindow,
    MaxOutputTokens,
    InputPrice,
    CachedInputPrice,
    OutputPrice,
}

/// Model sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSortDirection {
    Asc,
    Desc,
}

/// Model picker state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPickerApp {
    models: Vec<ModelInfo>,
    filter: TextInputState,
    list: FilteredListState,
    status: String,
    show_ignored: bool,
    sort_key: ModelSortKey,
    sort_direction: ModelSortDirection,
}

impl ModelPickerApp {
    /// Create a model picker with status text.
    #[must_use]
    pub fn new_with_status(models: Vec<ModelInfo>, status: impl Into<String>) -> Self {
        let list = FilteredListState::new(models.len());
        let mut app = Self {
            models,
            filter: super::text_input_flow::empty_state(),
            list,
            status: status.into(),
            show_ignored: false,
            sort_key: ModelSortKey::Default,
            sort_direction: ModelSortDirection::Desc,
        };
        app.refresh_filter();
        app
    }

    /// Return filter input mutably.
    pub const fn filter_mut(&mut self) -> &mut TextInputState {
        &mut self.filter
    }

    /// Return list state mutably.
    pub const fn list_state_mut(&mut self) -> &mut ListState {
        self.list.list_state_mut()
    }

    /// Return status.
    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Return visible list items.
    #[must_use]
    pub fn list_items(&self) -> Vec<ListItem> {
        if self.list.indices().is_empty() {
            return vec![empty_item("No matching models.")];
        }
        self.list
            .indices()
            .iter()
            .map(|index| model_item(&self.models[*index]))
            .collect()
    }

    /// Set status text.
    pub fn set_status(&mut self, status: impl Into<String>) {
        self.status = status.into();
    }

    /// Cycle model sort key.
    pub fn cycle_sort_key(&mut self) {
        self.sort_key = match self.sort_key {
            ModelSortKey::Default => ModelSortKey::ModelId,
            ModelSortKey::ModelId => ModelSortKey::ContextWindow,
            ModelSortKey::ContextWindow => ModelSortKey::MaxOutputTokens,
            ModelSortKey::MaxOutputTokens => ModelSortKey::InputPrice,
            ModelSortKey::InputPrice => ModelSortKey::CachedInputPrice,
            ModelSortKey::CachedInputPrice => ModelSortKey::OutputPrice,
            ModelSortKey::OutputPrice => ModelSortKey::Default,
        };
        self.refresh_filter();
    }

    /// Reverse sort direction.
    pub fn reverse_sort_direction(&mut self) {
        self.sort_direction = match self.sort_direction {
            ModelSortDirection::Asc => ModelSortDirection::Desc,
            ModelSortDirection::Desc => ModelSortDirection::Asc,
        };
        self.refresh_filter();
    }

    /// Return current sort label.
    #[must_use]
    pub fn sort_label(&self) -> String {
        format!(
            "sort {} {}",
            match self.sort_key {
                ModelSortKey::Default => "default",
                ModelSortKey::ModelId => "model",
                ModelSortKey::ContextWindow => "ctx",
                ModelSortKey::MaxOutputTokens => "max-out",
                ModelSortKey::InputPrice => "input",
                ModelSortKey::CachedInputPrice => "cached",
                ModelSortKey::OutputPrice => "output",
            },
            match self.sort_direction {
                ModelSortDirection::Asc => "↑",
                ModelSortDirection::Desc => "↓",
            }
        )
    }

    /// Toggle whether ignored models are visible.
    pub fn toggle_show_ignored(&mut self) {
        self.show_ignored = !self.show_ignored;
        self.refresh_filter();
    }

    /// Return whether ignored models are visible.
    #[must_use]
    pub const fn show_ignored(&self) -> bool {
        self.show_ignored
    }

    /// Return selected model id.
    #[must_use]
    pub fn selected_model_id(&self) -> Option<String> {
        let index = self.list.selected_source_index()?;
        Some(self.models[index].model_id.clone())
    }

    /// Return selected ignored model id.
    #[must_use]
    pub fn selected_ignored_model_id(&self) -> Option<String> {
        let index = self.list.selected_source_index()?;
        model_is_ignored(&self.models[index]).then(|| self.models[index].model_id.clone())
    }

    /// Mark a model ignored by runtime state.
    pub fn mark_state_ignored(&mut self, model_id: &str) {
        if let Some(model) = self
            .models
            .iter_mut()
            .find(|model| model.model_id == model_id)
        {
            model.visibility = match &model.visibility {
                ModelVisibility::Ignored {
                    source: ModelVisibilitySource::Config,
                    rule,
                } => ModelVisibility::Ignored {
                    source: ModelVisibilitySource::Both,
                    rule: rule.clone(),
                },
                ModelVisibility::Ignored { .. } => model.visibility.clone(),
                _ => ModelVisibility::Ignored {
                    source: ModelVisibilitySource::State,
                    rule: model_id.to_string(),
                },
            };
        }
        self.refresh_filter();
    }

    /// Remove runtime-state ignore marker for a model.
    pub fn mark_state_unignored(&mut self, model_id: &str) {
        if let Some(model) = self
            .models
            .iter_mut()
            .find(|model| model.model_id == model_id)
        {
            model.visibility = match &model.visibility {
                ModelVisibility::Ignored {
                    source: ModelVisibilitySource::State,
                    ..
                } => ModelVisibility::Visible,
                ModelVisibility::Ignored {
                    source: ModelVisibilitySource::Both,
                    rule,
                } => ModelVisibility::Ignored {
                    source: ModelVisibilitySource::Config,
                    rule: rule.clone(),
                },
                _ => model.visibility.clone(),
            };
        }
        self.refresh_filter();
    }

    /// Refresh filter.
    pub fn refresh_filter(&mut self) {
        let query = self.filter.buffer().text().trim().to_ascii_lowercase();
        let mut filtered_indices = self
            .models
            .iter()
            .enumerate()
            .filter_map(|(index, model)| {
                (model_matches(model, &query) && (self.show_ignored || !model_is_ignored(model)))
                    .then_some(index)
            })
            .collect::<Vec<_>>();
        let sort_key = self.sort_key;
        let sort_direction = self.sort_direction;
        filtered_indices.sort_by(|left, right| {
            compare_models(
                &self.models[*left],
                &self.models[*right],
                sort_key,
                sort_direction,
            )
        });
        self.list.replace_indices(filtered_indices);
    }

    /// Move selection down.
    pub fn select_next(&mut self) {
        self.list.select_next();
    }

    /// Move selection up.
    pub fn select_previous(&mut self) {
        self.list.select_previous();
    }

    /// Select a visible row by zero-based index.
    pub const fn select_visible(&mut self, row: usize) -> bool {
        self.list.select_visible(row)
    }
}

fn compare_models(
    left: &ModelInfo,
    right: &ModelInfo,
    sort_key: ModelSortKey,
    direction: ModelSortDirection,
) -> Ordering {
    let ordering = match sort_key {
        ModelSortKey::Default => Ordering::Equal,
        ModelSortKey::ModelId => left.model_id.cmp(&right.model_id),
        ModelSortKey::ContextWindow => {
            compare_option_u32(left.context_window, right.context_window, direction)
        }
        ModelSortKey::MaxOutputTokens => {
            compare_option_u32(left.max_output_tokens, right.max_output_tokens, direction)
        }
        ModelSortKey::InputPrice => compare_price(left, right, direction, |pricing| pricing.input),
        ModelSortKey::CachedInputPrice => {
            compare_price(left, right, direction, |pricing| pricing.cached_input)
        }
        ModelSortKey::OutputPrice => {
            compare_price(left, right, direction, |pricing| pricing.output)
        }
    };
    let ordering =
        if sort_key == ModelSortKey::ModelId && matches!(direction, ModelSortDirection::Desc) {
            ordering.reverse()
        } else {
            ordering
        };
    ordering.then_with(|| left.model_id.cmp(&right.model_id))
}

fn compare_option_u32(
    left: Option<u32>,
    right: Option<u32>,
    direction: ModelSortDirection,
) -> Ordering {
    compare_optional(left, right, direction)
}

fn compare_optional<T: Ord>(
    left: Option<T>,
    right: Option<T>,
    direction: ModelSortDirection,
) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => match direction {
            ModelSortDirection::Asc => left.cmp(&right),
            ModelSortDirection::Desc => right.cmp(&left),
        },
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn compare_price(
    left: &ModelInfo,
    right: &ModelInfo,
    direction: ModelSortDirection,
    price: impl Fn(&ModelPricingInfo) -> Option<ModelTokenPrice>,
) -> Ordering {
    let left = left
        .pricing
        .as_ref()
        .and_then(&price)
        .map(|price| price.micros);
    let right = right
        .pricing
        .as_ref()
        .and_then(price)
        .map(|price| price.micros);
    compare_optional(left, right, direction)
}

fn model_item(model: &ModelInfo) -> ListItem {
    let marker = if model_is_ignored(model) {
        "× "
    } else if model.is_default {
        "* "
    } else {
        "  "
    };
    let mut spans = vec![
        Span::styled(marker, Style::new().fg(Color::BrightBlack)),
        Span::styled(
            model.model_id.clone(),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            model.display_name.clone(),
            Style::new().fg(Color::BrightBlack),
        ),
    ];
    if let Some(pricing) = model.pricing.as_ref().and_then(model_pricing_summary) {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(pricing, Style::new().fg(Color::Cyan)));
    }
    if let Some(reason) = model_ignore_summary(model) {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(reason, Style::new().fg(Color::Yellow)));
    }
    ListItem::new(Line::from_spans(spans))
}

fn model_pricing_summary(pricing: &ModelPricingInfo) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(input) = pricing.input {
        parts.push(format!(
            "in {}/M",
            format_token_price(&pricing.currency, input)
        ));
    }
    if let Some(cached) = pricing.cached_input {
        parts.push(format!(
            "cached {}/M",
            format_token_price(&pricing.currency, cached)
        ));
    }
    if let Some(cache_write) = pricing.cache_write_input {
        parts.push(format!(
            "write {}/M",
            format_token_price(&pricing.currency, cache_write)
        ));
    }
    if let Some(output) = pricing.output {
        parts.push(format!(
            "out {}/M",
            format_token_price(&pricing.currency, output)
        ));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn format_token_price(currency: &str, price: ModelTokenPrice) -> String {
    let amount = format_micros_decimal(price.micros);
    if currency == "USD" {
        format!("${amount}")
    } else {
        format!("{currency} {amount}")
    }
}

fn format_micros_decimal(micros: u64) -> String {
    let whole = micros / 1_000_000;
    let fractional = micros % 1_000_000;
    if fractional == 0 {
        return whole.to_string();
    }
    let mut value = format!("{whole}.{fractional:06}");
    while value.ends_with('0') {
        value.pop();
    }
    value
}

fn model_ignore_summary(model: &ModelInfo) -> Option<String> {
    match &model.visibility {
        ModelVisibility::Ignored { source, .. } => {
            Some(format!("ignored {}", visibility_source_label(*source)))
        }
        ModelVisibility::Unsupported { reason } => Some(format!("unsupported {reason}")),
        ModelVisibility::Visible => None,
    }
}

const fn visibility_source_label(source: ModelVisibilitySource) -> &'static str {
    match source {
        ModelVisibilitySource::Config => "config",
        ModelVisibilitySource::State => "state",
        ModelVisibilitySource::Both => "config+state",
    }
}

const fn model_is_ignored(model: &ModelInfo) -> bool {
    matches!(model.visibility, ModelVisibility::Ignored { .. })
}

fn model_matches(model: &ModelInfo, query: &str) -> bool {
    query.is_empty()
        || model.model_id.to_ascii_lowercase().contains(query)
        || model.display_name.to_ascii_lowercase().contains(query)
        || model
            .pricing
            .as_ref()
            .and_then(model_pricing_summary)
            .is_some_and(|pricing| pricing.to_ascii_lowercase().contains(query))
        || model_ignore_summary(model)
            .is_some_and(|summary| summary.to_ascii_lowercase().contains(query))
}

fn empty_item(message: &str) -> ListItem {
    ListItem::new(Line::from_spans(vec![Span::styled(
        message.to_owned(),
        Style::new().fg(Color::BrightBlack),
    )]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price(micros: u64) -> ModelTokenPrice {
        ModelTokenPrice::from_micros(micros)
    }

    #[test]
    fn formats_model_pricing_summary() {
        let pricing = ModelPricingInfo {
            currency: "USD".to_string(),
            unit: bcode_model::ModelPricingUnit::PerMillionTokens,
            input: Some(price(1_250_000)),
            cached_input: Some(price(125_000)),
            cache_write_input: None,
            output: Some(price(10_000_000)),
            source: bcode_model::ModelPricingSource::PatternMatch,
        };

        assert_eq!(
            model_pricing_summary(&pricing).as_deref(),
            Some("in $1.25/M · cached $0.125/M · out $10/M")
        );
    }

    #[test]
    fn formats_non_usd_model_pricing_summary() {
        let pricing = ModelPricingInfo {
            currency: "EUR".to_string(),
            unit: bcode_model::ModelPricingUnit::PerMillionTokens,
            input: Some(price(2_000_000)),
            cached_input: None,
            cache_write_input: None,
            output: Some(price(8_500_000)),
            source: bcode_model::ModelPricingSource::ProviderApi,
        };

        assert_eq!(
            model_pricing_summary(&pricing).as_deref(),
            Some("in EUR 2/M · out EUR 8.5/M")
        );
    }
}
