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

const LIST_HIGHLIGHT_WIDTH: usize = 2;
const CELL_GAP: &str = "  ";

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

    /// Return an aligned header line for the visible model columns.
    #[must_use]
    pub fn header_line(&self, width: u16) -> Line {
        let rows = self.visible_rows();
        let widths = ModelPickerColumnWidths::from_rows(&rows, usable_list_width(width));
        Line::from_spans(vec![Span::styled(
            format_header(&widths, self.sort_key, self.sort_direction),
            Style::new()
                .fg(Color::BrightBlack)
                .add_modifier(Modifier::BOLD),
        )])
    }

    /// Return visible list items.
    #[must_use]
    pub fn list_items(&self, width: u16) -> Vec<ListItem> {
        if self.list.indices().is_empty() {
            return vec![empty_item("No matching models.")];
        }
        let rows = self.visible_rows();
        let widths = ModelPickerColumnWidths::from_rows(&rows, usable_list_width(width));
        rows.iter().map(|row| model_item(row, &widths)).collect()
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
            sort_key_label(self.sort_key),
            sort_direction_label(self.sort_direction)
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

    fn visible_rows(&self) -> Vec<ModelPickerRow> {
        self.list
            .indices()
            .iter()
            .map(|index| ModelPickerRow::from_model(&self.models[*index]))
            .collect()
    }

    #[cfg(test)]
    fn row_strings_for_test(&self, width: u16) -> Vec<String> {
        let rows = self.visible_rows();
        let widths = ModelPickerColumnWidths::from_rows(&rows, usable_list_width(width));
        rows.iter().map(|row| format_row(row, &widths)).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelPickerRow {
    marker: &'static str,
    model_id: String,
    context: String,
    max_output: String,
    input_price: String,
    cached_price: String,
    output_price: String,
    state: String,
}

impl ModelPickerRow {
    fn from_model(model: &ModelInfo) -> Self {
        Self {
            marker: if model_is_ignored(model) {
                "×"
            } else if model.is_default {
                "*"
            } else {
                " "
            },
            model_id: model.model_id.clone(),
            context: model
                .context_window
                .map_or_else(|| "?".to_string(), format_token_count),
            max_output: model
                .max_output_tokens
                .map_or_else(|| "?".to_string(), format_token_count),
            input_price: model_price_cell(model, |pricing| pricing.input),
            cached_price: model_price_cell(model, |pricing| pricing.cached_input),
            output_price: model_price_cell(model, |pricing| pricing.output),
            state: model_ignore_summary(model).unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModelPickerColumnWidths {
    model_id: usize,
    context: usize,
    max_output: usize,
    input_price: usize,
    cached_price: usize,
    output_price: usize,
    state: usize,
}

impl ModelPickerColumnWidths {
    fn from_rows(rows: &[ModelPickerRow], available_width: usize) -> Self {
        let mut widths = Self {
            model_id: "Model".len(),
            context: "Ctx".len(),
            max_output: "Max out".len(),
            input_price: "Input".len(),
            cached_price: "Cached".len(),
            output_price: "Output".len(),
            state: "State".len(),
        };
        for row in rows {
            widths.model_id = widths.model_id.max(display_width(&row.model_id));
            widths.context = widths.context.max(display_width(&row.context));
            widths.max_output = widths.max_output.max(display_width(&row.max_output));
            widths.input_price = widths.input_price.max(display_width(&row.input_price));
            widths.cached_price = widths.cached_price.max(display_width(&row.cached_price));
            widths.output_price = widths.output_price.max(display_width(&row.output_price));
            widths.state = widths.state.max(display_width(&row.state));
        }
        widths.model_id = widths
            .model_id
            .min(max_model_width(widths, available_width));
        widths
    }
}

fn max_model_width(mut widths: ModelPickerColumnWidths, available_width: usize) -> usize {
    widths.model_id = 0;
    let non_model = row_width(widths);
    available_width.saturating_sub(non_model).max("Model".len())
}

const fn row_width(widths: ModelPickerColumnWidths) -> usize {
    1 + 7 * CELL_GAP.len()
        + widths.model_id
        + widths.context
        + widths.max_output
        + widths.input_price
        + widths.cached_price
        + widths.output_price
        + widths.state
}

fn usable_list_width(width: u16) -> usize {
    usize::from(width).saturating_sub(LIST_HIGHLIGHT_WIDTH)
}

fn model_item(row: &ModelPickerRow, widths: &ModelPickerColumnWidths) -> ListItem {
    ListItem::new(Line::from_spans(vec![
        Span::styled(row.marker, Style::new().fg(Color::BrightBlack)),
        Span::raw(CELL_GAP),
        Span::styled(
            pad_right(
                &truncate_ascii(&row.model_id, widths.model_id),
                widths.model_id,
            ),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::raw(CELL_GAP),
        Span::styled(
            pad_left(&row.context, widths.context),
            Style::new().fg(Color::Cyan),
        ),
        Span::raw(CELL_GAP),
        Span::styled(
            pad_left(&row.max_output, widths.max_output),
            Style::new().fg(Color::Cyan),
        ),
        Span::raw(CELL_GAP),
        Span::styled(
            pad_right(&row.input_price, widths.input_price),
            Style::new().fg(Color::Cyan),
        ),
        Span::raw(CELL_GAP),
        Span::styled(
            pad_right(&row.cached_price, widths.cached_price),
            Style::new().fg(Color::Cyan),
        ),
        Span::raw(CELL_GAP),
        Span::styled(
            pad_right(&row.output_price, widths.output_price),
            Style::new().fg(Color::Cyan),
        ),
        Span::raw(CELL_GAP),
        Span::styled(
            pad_right(&row.state, widths.state),
            Style::new().fg(Color::Yellow),
        ),
    ]))
}

fn format_header(
    widths: &ModelPickerColumnWidths,
    sort_key: ModelSortKey,
    direction: ModelSortDirection,
) -> String {
    format_cells(
        " ",
        &sort_header("Model", ModelSortKey::ModelId, sort_key, direction),
        &sort_header("Ctx", ModelSortKey::ContextWindow, sort_key, direction),
        &sort_header(
            "Max out",
            ModelSortKey::MaxOutputTokens,
            sort_key,
            direction,
        ),
        &sort_header("Input", ModelSortKey::InputPrice, sort_key, direction),
        &sort_header(
            "Cached",
            ModelSortKey::CachedInputPrice,
            sort_key,
            direction,
        ),
        &sort_header("Output", ModelSortKey::OutputPrice, sort_key, direction),
        "State",
        widths,
    )
}

#[cfg(test)]
fn format_row(row: &ModelPickerRow, widths: &ModelPickerColumnWidths) -> String {
    format_cells(
        row.marker,
        &row.model_id,
        &row.context,
        &row.max_output,
        &row.input_price,
        &row.cached_price,
        &row.output_price,
        &row.state,
        widths,
    )
}

#[allow(clippy::too_many_arguments)]
fn format_cells(
    marker: &str,
    model_id: &str,
    context: &str,
    max_output: &str,
    input_price: &str,
    cached_price: &str,
    output_price: &str,
    state: &str,
    widths: &ModelPickerColumnWidths,
) -> String {
    [
        marker.to_string(),
        pad_right(&truncate_ascii(model_id, widths.model_id), widths.model_id),
        pad_left(context, widths.context),
        pad_left(max_output, widths.max_output),
        pad_right(input_price, widths.input_price),
        pad_right(cached_price, widths.cached_price),
        pad_right(output_price, widths.output_price),
        pad_right(state, widths.state),
    ]
    .join(CELL_GAP)
}

fn sort_header(
    label: &str,
    key: ModelSortKey,
    current: ModelSortKey,
    direction: ModelSortDirection,
) -> String {
    if key == current {
        format!("{label}{}", sort_direction_label(direction))
    } else {
        label.to_string()
    }
}

const fn sort_key_label(key: ModelSortKey) -> &'static str {
    match key {
        ModelSortKey::Default => "default",
        ModelSortKey::ModelId => "model",
        ModelSortKey::ContextWindow => "ctx",
        ModelSortKey::MaxOutputTokens => "max-out",
        ModelSortKey::InputPrice => "input",
        ModelSortKey::CachedInputPrice => "cached",
        ModelSortKey::OutputPrice => "output",
    }
}

const fn sort_direction_label(direction: ModelSortDirection) -> &'static str {
    match direction {
        ModelSortDirection::Asc => "↑",
        ModelSortDirection::Desc => "↓",
    }
}

fn pad_right(text: &str, width: usize) -> String {
    let padding = width.saturating_sub(display_width(text));
    format!("{text}{}", " ".repeat(padding))
}

fn pad_left(text: &str, width: usize) -> String {
    let padding = width.saturating_sub(display_width(text));
    format!("{}{text}", " ".repeat(padding))
}

fn truncate_ascii(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    format!("{}…", text.chars().take(width - 1).collect::<String>())
}

fn display_width(text: &str) -> usize {
    text.chars().count()
}

fn model_price_cell(
    model: &ModelInfo,
    price: impl Fn(&ModelPricingInfo) -> Option<ModelTokenPrice>,
) -> String {
    model.pricing.as_ref().and_then(price).map_or_else(
        || "?".to_string(),
        |price| {
            format!(
                "{}/M",
                format_token_price(
                    model
                        .pricing
                        .as_ref()
                        .map_or("USD", |pricing| pricing.currency.as_str()),
                    price
                )
            )
        },
    )
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

fn format_token_count(value: u32) -> String {
    if value >= 1_000_000 {
        let whole = value / 1_000_000;
        let decimal = (value % 1_000_000) / 100_000;
        if decimal == 0 {
            format!("{whole}m")
        } else {
            format!("{whole}.{decimal}m")
        }
    } else if value >= 1_000 {
        format!("{}k", value / 1_000)
    } else {
        value.to_string()
    }
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

    fn model(
        model_id: &str,
        context_window: Option<u32>,
        input: Option<u64>,
        output: Option<u64>,
    ) -> ModelInfo {
        ModelInfo {
            model_id: model_id.to_string(),
            display_name: model_id.to_string(),
            is_default: false,
            context_window,
            max_output_tokens: Some(16_000),
            capabilities: std::collections::BTreeSet::new(),
            reasoning: None,
            cache: bcode_model::ModelCacheInfo::default(),
            metadata_source: None,
            pricing: Some(ModelPricingInfo {
                currency: "USD".to_string(),
                unit: bcode_model::ModelPricingUnit::PerMillionTokens,
                input: input.map(price),
                cached_input: Some(price(125_000)),
                cache_write_input: None,
                output: output.map(price),
                source: bcode_model::ModelPricingSource::PatternMatch,
            }),
            visibility: ModelVisibility::Visible,
        }
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

    #[test]
    fn aligns_price_columns_across_visible_rows() {
        let app = ModelPickerApp::new_with_status(
            vec![
                model("gpt-5", Some(272_000), Some(3_500_000), Some(28_000_000)),
                model(
                    "very-long-model-name-mini",
                    Some(128_000),
                    Some(250_000),
                    Some(2_000_000),
                ),
            ],
            "Select",
        );
        let rows = app.row_strings_for_test(120);
        let input_offsets = rows
            .iter()
            .map(|row| row.find('$').expect("row should contain input price"))
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(input_offsets.len(), 1);
    }

    #[test]
    fn truncates_model_column_for_narrow_rows() {
        let app = ModelPickerApp::new_with_status(
            vec![model(
                "very-long-model-name-that-needs-truncation",
                Some(128_000),
                Some(250_000),
                Some(2_000_000),
            )],
            "Select",
        );
        let rows = app.row_strings_for_test(64);

        assert!(rows[0].contains('…'));
    }
}
