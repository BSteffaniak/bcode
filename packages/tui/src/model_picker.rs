//! TUI model picker state.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use bcode_model::{
    ModelAvailability, ModelAvailabilityReason, ModelAvailabilityStatus, ModelInfo, ModelList,
    ModelPricingInfo, ModelTokenPrice, ModelVisibility, ModelVisibilitySource,
};
use bmux_tui::prelude::Line as ListItem;
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::Modifier;
use bmux_tui_components::selectable_list::SelectableListState as ListState;
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

/// Model picker interaction mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelPickerMode {
    /// Picker actions are focused.
    Actions,
    /// User is confirming a quota-consuming request for one selected model.
    ConfirmCheck,
    /// A bounded provider request is in progress.
    Checking,
    /// Filter text input is focused.
    Filter,
}

/// Model picker state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPickerApp {
    models: Vec<ModelInfo>,
    availability: BTreeMap<String, ModelAvailability>,
    rendered_item_area: Option<bmux_tui::geometry::Rect>,
    check_model_id: Option<String>,
    check_consent_visible: bool,
    filter: TextInputState,
    list: FilteredListState,
    status: String,
    show_ignored: bool,
    sort_key: ModelSortKey,
    sort_direction: ModelSortDirection,
    mode: ModelPickerMode,
}

impl ModelPickerApp {
    /// Create a model picker with status text.
    #[must_use]
    pub fn new_with_status(models: Vec<ModelInfo>, status: impl Into<String>) -> Self {
        let list = FilteredListState::new(models.len());
        let mut app = Self {
            models,
            availability: BTreeMap::new(),
            rendered_item_area: None,
            check_model_id: None,
            check_consent_visible: false,
            filter: super::text_input_flow::empty_state(),
            list,
            status: status.into(),
            show_ignored: false,
            sort_key: ModelSortKey::Default,
            sort_direction: ModelSortDirection::Desc,
            mode: ModelPickerMode::Actions,
        };
        app.refresh_filter();
        app
    }

    /// Create a picker retaining centralized, scope-specific availability decisions.
    #[must_use]
    pub fn from_model_list(models: ModelList, status: impl Into<String>) -> Self {
        let mut app = Self::new_with_status(models.models, status);
        app.availability = models.catalog.availability;
        app
    }

    /// Return selected availability scope without exposing endpoint or credential material.
    #[must_use]
    pub fn selected_scope_summary(&self) -> String {
        let Some(index) = self.list.selected_source_index() else {
            return "Scope: active provider configuration".to_owned();
        };
        self.availability
            .get(&self.models[index].model_id)
            .map_or_else(
                || "Scope: active provider configuration (no scoped evidence)".to_owned(),
                |availability| {
                    let target = &availability.target;
                    format!(
                        "Scope: {} / {} / {}{}",
                        target.provider,
                        target.auth_mode,
                        target.api_surface,
                        target
                            .integration
                            .as_ref()
                            .map_or_else(String::new, |value| format!(" / {value}"))
                    )
                },
            )
    }

    /// Return the selected model's normalized availability and serving scope.
    #[must_use]
    pub fn selected_availability_summary(&self) -> String {
        let Some(index) = self.list.selected_source_index() else {
            return "No model selected".to_owned();
        };
        let model_id = &self.models[index].model_id;
        self.availability.get(model_id).map_or_else(
            || "Unverified · no current evidence".to_owned(),
            |availability| {
                format!(
                    "{} · {}",
                    availability_label(availability.status),
                    availability_reason(availability.reason),
                )
            },
        )
    }

    /// Ask for explicit permission to check the selected model.
    pub fn request_check(&mut self) {
        if let Some(model_id) = self.selected_model_id() {
            self.check_consent_visible = false;
            self.check_model_id = Some(model_id.clone());
            self.mode = ModelPickerMode::ConfirmCheck;
            self.set_status(format!("Check {model_id}? One provider request consumes quota and may cost money. Enter confirms; Esc declines."));
        }
    }

    /// Enable confirmation only when the complete spending warning was painted.
    pub const fn set_check_consent_visible(&mut self, visible: bool) {
        self.check_consent_visible = visible;
    }

    /// Confirm only the model captured by the consent prompt.
    pub fn confirm_check(&mut self) -> Option<String> {
        if self.mode != ModelPickerMode::ConfirmCheck || !self.check_consent_visible {
            return None;
        }
        let model_id = self.check_model_id.take()?;
        self.mode = ModelPickerMode::Checking;
        self.set_status(format!("Checking {model_id}… one request, 20 second limit"));
        Some(model_id)
    }

    /// Decline a pending request without invoking the provider.
    pub fn decline_check(&mut self) {
        self.check_model_id = None;
        self.mode = ModelPickerMode::Actions;
        self.set_status("Check declined; no provider request sent");
    }

    /// Replace availability after a check without losing filter, sort, or selection.
    pub fn finish_check(&mut self, models: Option<ModelList>, status: impl Into<String>) {
        let selected = self.selected_model_id();
        if let Some(models) = models {
            self.models = models.models;
            self.availability = models.catalog.availability;
            self.refresh_filter();
            if let Some(model_id) = selected
                && let Some(row) = self
                    .list
                    .indices()
                    .iter()
                    .position(|index| self.models[*index].model_id == model_id)
            {
                self.select_visible(row);
            }
        }
        self.mode = ModelPickerMode::Actions;
        self.set_status(status);
    }

    /// Retain exactly the visible painted item area for mouse activation.
    pub fn set_rendered_item_area(&mut self, area: Option<bmux_tui::geometry::Rect>) {
        self.rendered_item_area = area.filter(|area| !area.is_empty());
    }

    /// Activate only a visible model row inside the retained paint geometry.
    pub fn select_mouse_row(&mut self, mouse: bmux_tui::event::MouseEvent) -> Option<String> {
        if !matches!(
            self.mode,
            ModelPickerMode::Actions | ModelPickerMode::Filter
        ) {
            return None;
        }
        let row =
            super::picker_mouse::command_palette_row_in_area(mouse, self.rendered_item_area?)?;
        self.select_visible(row)
            .then(|| self.selected_model_id())
            .flatten()
    }

    /// Return filter input mutably.
    pub const fn filter_mut(&mut self) -> &mut TextInputState {
        &mut self.filter
    }

    /// Synchronize list visibility before rendering and return its render state.
    pub fn list_render_state(&mut self, viewport_height: u16) -> &mut ListState {
        self.list.render_state(viewport_height)
    }

    /// Return status.
    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Return an aligned header line for the visible model columns.
    #[must_use]
    pub fn header_line(&self, width: u16, theme: super::render::TuiTheme) -> Line {
        let rows = self.visible_rows();
        let widths = ModelPickerColumnWidths::from_rows(&rows, usable_list_width(width));
        if row_width(widths) > usable_list_width(width) {
            return Line::from_spans(vec![Span::styled(
                truncate_column("Model", usable_list_width(width)),
                theme.muted,
            )]);
        }
        Line::from_spans(vec![Span::styled(
            format_header(&widths, self.sort_key, self.sort_direction),
            theme.muted.add_modifier(Modifier::BOLD),
        )])
    }

    /// Return visible list items.
    #[must_use]
    pub fn list_items(&self, width: u16, theme: super::render::TuiTheme) -> Vec<ListItem> {
        if self.list.indices().is_empty() {
            return vec![empty_item("No matching models.", theme)];
        }
        let rows = self.visible_rows();
        let widths = ModelPickerColumnWidths::from_rows(&rows, usable_list_width(width));
        if row_width(widths) > usable_list_width(width) {
            return rows
                .iter()
                .map(|row| {
                    Line::from_spans(vec![Span::styled(
                        truncate_column(
                            &format!("{} · {}", row.state, row.model_id),
                            usable_list_width(width),
                        ),
                        theme.text,
                    )])
                })
                .collect();
        }
        rows.iter()
            .map(|row| model_item(row, &widths, theme))
            .collect()
    }

    /// Return current interaction mode.
    #[must_use]
    pub const fn mode(&self) -> ModelPickerMode {
        self.mode
    }

    /// Focus the filter text input.
    pub const fn focus_filter(&mut self) {
        self.mode = ModelPickerMode::Filter;
    }

    /// Focus picker actions.
    pub const fn focus_actions(&mut self) {
        self.mode = ModelPickerMode::Actions;
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
            .map(|index| {
                let model = &self.models[*index];
                let mut row = ModelPickerRow::from_model(model);
                let status = self
                    .availability
                    .get(&model.model_id)
                    .map_or("Unverified", |availability| {
                        availability_label(availability.status)
                    });
                row.state = if row.state.is_empty() {
                    status.to_owned()
                } else {
                    format!("{status} · {}", row.state)
                };
                row
            })
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

fn model_item(
    row: &ModelPickerRow,
    widths: &ModelPickerColumnWidths,
    theme: super::render::TuiTheme,
) -> ListItem {
    Line::from_spans(vec![
        Span::styled(row.marker, theme.muted),
        Span::raw(CELL_GAP),
        Span::styled(
            pad_right(
                &truncate_column(&row.model_id, widths.model_id),
                widths.model_id,
            ),
            theme.text.add_modifier(Modifier::BOLD),
        ),
        Span::raw(CELL_GAP),
        Span::styled(pad_left(&row.context, widths.context), theme.border),
        Span::raw(CELL_GAP),
        Span::styled(pad_left(&row.max_output, widths.max_output), theme.border),
        Span::raw(CELL_GAP),
        Span::styled(
            pad_right(&row.input_price, widths.input_price),
            theme.border,
        ),
        Span::raw(CELL_GAP),
        Span::styled(
            pad_right(&row.cached_price, widths.cached_price),
            theme.border,
        ),
        Span::raw(CELL_GAP),
        Span::styled(
            pad_right(&row.output_price, widths.output_price),
            theme.border,
        ),
        Span::raw(CELL_GAP),
        Span::styled(pad_right(&row.state, widths.state), theme.muted),
    ])
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
        pad_right(&truncate_column(model_id, widths.model_id), widths.model_id),
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

fn truncate_column(text: &str, width: usize) -> String {
    bmux_tui::text_width::truncate_to_display_width(text, width)
}

fn display_width(text: &str) -> usize {
    bmux_tui::text_width::display_width(text)
}

const fn availability_label(status: ModelAvailabilityStatus) -> &'static str {
    match status {
        ModelAvailabilityStatus::Verified => "Verified",
        ModelAvailabilityStatus::CatalogDeclared => "Catalog-supported",
        ModelAvailabilityStatus::Unverified => "Unverified",
        ModelAvailabilityStatus::Unsupported | ModelAvailabilityStatus::AccountDenied => {
            "Unavailable"
        }
    }
}

const fn availability_reason(reason: ModelAvailabilityReason) -> &'static str {
    match reason {
        ModelAvailabilityReason::ObservedSuccess => "completed provider request",
        ModelAvailabilityReason::ProviderSurfaceUnsupported => {
            "not supported on this serving surface"
        }
        ModelAvailabilityReason::AccountDenied => "current account lacks access",
        ModelAvailabilityReason::CatalogDeclaration => "catalog declaration, not a live check",
        ModelAvailabilityReason::NoEvidence => "no current evidence",
    }
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

fn empty_item(message: &str, theme: super::render::TuiTheme) -> ListItem {
    Line::from_spans(vec![Span::styled(message.to_owned(), theme.muted)])
}

#[cfg(test)]
mod tests {
    #[test]
    fn unicode_columns_use_terminal_cells() {
        assert_eq!(super::display_width("界e\u{301}"), 3);
        for width in 0..10 {
            let text = super::truncate_column("界👩‍💻abcdef", width);
            assert!(bmux_tui::text_width::display_width(&text) <= width);
        }
    }

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
            max_image_input_base64_bytes: None,
            capabilities: std::collections::BTreeSet::new(),
            feature_support: bcode_model::ModelFeatureSupport::default(),
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
                context_threshold_tokens: None,
                rules: Vec::new(),
                revision: None,
                source: bcode_model::ModelPricingSource::PatternMatch,
            }),
            api_surface: None,
            visibility: ModelVisibility::Visible,
        }
    }

    #[test]
    fn availability_does_not_hide_unknown_or_denied_models() {
        let mut app = ModelPickerApp::new_with_status(
            vec![
                model("unknown", None, None, None),
                model("denied", None, None, None),
            ],
            "Select",
        );
        app.availability.insert(
            "denied".to_owned(),
            ModelAvailability {
                status: ModelAvailabilityStatus::AccountDenied,
                target: bcode_model::ModelCatalogSupportHint {
                    provider: "provider".to_owned(),
                    auth_mode: "subscription".to_owned(),
                    api_surface: "responses".to_owned(),
                    integration: None,
                },
                reason: ModelAvailabilityReason::AccountDenied,
                observed_at_ms: Some(1),
                expires_at_ms: Some(2),
            },
        );
        assert_eq!(app.visible_rows().len(), 2);
        let unknown_row = app
            .list
            .indices()
            .iter()
            .position(|index| app.models[*index].model_id == "unknown")
            .unwrap();
        app.select_visible(unknown_row);
        assert_eq!(app.selected_model_id().as_deref(), Some("unknown"));
        assert_eq!(
            app.selected_availability_summary(),
            "Unverified · no current evidence"
        );
        let denied_row = app
            .list
            .indices()
            .iter()
            .position(|index| app.models[*index].model_id == "denied")
            .unwrap();
        app.select_visible(denied_row);
        assert_eq!(app.selected_model_id().as_deref(), Some("denied"));
        assert_eq!(
            app.selected_availability_summary(),
            "Unavailable · current account lacks access"
        );
        assert_eq!(
            app.selected_scope_summary(),
            "Scope: provider / subscription / responses"
        );
        app.mark_state_ignored("denied");
        assert_eq!(app.visible_rows().len(), 1);
        app.toggle_show_ignored();
        assert_eq!(app.visible_rows().len(), 2);
    }

    #[test]
    fn selected_check_requires_visible_explicit_consent_and_captures_one_model() {
        let mut app = ModelPickerApp::new_with_status(
            vec![
                model("one", None, None, None),
                model("two", None, None, None),
            ],
            "Select",
        );
        assert!(app.confirm_check().is_none());
        app.request_check();
        assert!(app.confirm_check().is_none());
        app.set_check_consent_visible(true);
        app.select_next();
        assert_eq!(app.confirm_check().as_deref(), Some("one"));
        assert_eq!(app.mode(), ModelPickerMode::Checking);
        assert!(app.confirm_check().is_none());
        app.finish_check(None, "Check complete");
        app.request_check();
        app.decline_check();
        assert!(app.confirm_check().is_none());
        assert_eq!(app.mode(), ModelPickerMode::Actions);
    }

    #[test]
    fn mouse_activation_uses_retained_visible_geometry() {
        use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
        use bmux_tui::geometry::{Point, Rect};
        let mut app =
            ModelPickerApp::new_with_status(vec![model("one", None, None, None)], "Select");
        let mouse = MouseEvent::new(MouseEventKind::Down(MouseButton::Left), Point::new(2, 8));
        assert!(app.select_mouse_row(mouse).is_none());
        app.set_rendered_item_area(Some(Rect::new(1, 8, 10, 1)));
        assert_eq!(app.select_mouse_row(mouse).as_deref(), Some("one"));
        let header = MouseEvent {
            position: Point::new(2, 7),
            ..mouse
        };
        assert!(app.select_mouse_row(header).is_none());
        app.set_rendered_item_area(None);
        assert!(app.select_mouse_row(mouse).is_none());
    }

    #[test]
    fn compact_unicode_status_rows_stay_bounded() {
        let app = ModelPickerApp::new_with_status(
            vec![model("模型👩‍💻e\u{301}", None, None, None)],
            "Select",
        );
        let theme = super::super::render::TuiTheme::for_theme_id("terminal-native");
        for width in 0..80 {
            for item in app.list_items(width, theme) {
                assert!(item.width() <= usable_list_width(width));
            }
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
            context_threshold_tokens: None,
            rules: Vec::new(),
            revision: None,
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
            context_threshold_tokens: None,
            rules: Vec::new(),
            revision: None,
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
