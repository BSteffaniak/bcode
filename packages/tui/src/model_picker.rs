//! TUI model picker state.

use bcode_model::{ModelInfo, ModelPricingInfo, ModelTokenPrice};
use bmux_tui::list::{ListItem, ListState};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui_components::text_input::TextInputState;

use super::filtered_list::FilteredListState;

/// Model picker state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPickerApp {
    models: Vec<ModelInfo>,
    filter: TextInputState,
    list: FilteredListState,
    status: String,
}

impl ModelPickerApp {
    /// Create a model picker with status text.
    #[must_use]
    pub fn new_with_status(models: Vec<ModelInfo>, status: impl Into<String>) -> Self {
        let list = FilteredListState::new(models.len());
        Self {
            models,
            filter: super::text_input_flow::empty_state(),
            list,
            status: status.into(),
        }
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

    /// Return selected model id.
    #[must_use]
    pub fn selected_model_id(&self) -> Option<String> {
        let index = self.list.selected_source_index()?;
        Some(self.models[index].model_id.clone())
    }

    /// Refresh filter.
    pub fn refresh_filter(&mut self) {
        let query = self.filter.buffer().text().trim().to_ascii_lowercase();
        let filtered_indices = self
            .models
            .iter()
            .enumerate()
            .filter_map(|(index, model)| model_matches(model, &query).then_some(index))
            .collect();
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

fn model_item(model: &ModelInfo) -> ListItem {
    let marker = if model.is_default { "* " } else { "  " };
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

fn model_matches(model: &ModelInfo, query: &str) -> bool {
    query.is_empty()
        || model.model_id.to_ascii_lowercase().contains(query)
        || model.display_name.to_ascii_lowercase().contains(query)
        || model
            .pricing
            .as_ref()
            .and_then(model_pricing_summary)
            .is_some_and(|pricing| pricing.to_ascii_lowercase().contains(query))
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
