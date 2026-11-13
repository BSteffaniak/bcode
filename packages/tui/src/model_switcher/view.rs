//! Drawing and pointer geometry of the model switcher, composed from BMUX components.
//!
//! One component tree is built per frame from the switcher's state. Painting and pointer
//! routing both use the layout of that tree, so clicks always land on what was drawn.
//!
//! Layout, inside a centered modal:
//!
//! * a search row with the result count and a loading spinner;
//! * the body: in Browse, the grouped list beside a details pane (stacked into a two-line
//!   summary below the list when narrow); in Compare, one sortable table of every model;
//! * a notice row, when there is one, and a footer of key hints.

use super::inventory::{
    self, Access, Entry, Fix, Readiness, Row, SourceKey, capability_labels, price_lines,
    token_count,
};
use super::{Layout, ModelSwitcher, Painted};
use bmux_tui::component::{Component, Constraints, EventCx, LayoutCx, LayoutId, LayoutNode};
use bmux_tui::composition::{Column, Flex, Row as BoxRow, SizeBox, Surface, TextBlock};
use bmux_tui::event::{Event, MouseButton, MouseEventKind};
use bmux_tui::geometry::{Insets, Point, Rect, Size};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::style::{Modifier, Style};
use bmux_tui::text::{Line, Span, Text, TextWrap, TrailingAlign, TrailingPlacement};
use bmux_tui_components::key_hint_bar::{
    KeyHint, KeyHintBarComponent, KeyHintBarPolicy, KeyHintOverflow,
};
use bmux_tui_components::labeled_details::{
    DetailItem, LabelPlacement, LabeledDetailsComponent, LabeledDetailsStyles,
};
use bmux_tui_components::modal_frame::{
    ModalFrame, ModalFrameComponent, ModalPlacement, ModalSizing, ModalTheme,
};
use bmux_tui_components::scrollbar_layout::ScrollbarAxisLayoutMode;
use bmux_tui_components::selectable_list::{
    SelectableListComponent, SelectableListHighlightPolicy, SelectableListItem,
    SelectableListPolicy, SelectableListStyles,
};
use bmux_tui_components::table::{
    TableAlign, TableColumn, TableComponent, TableHit, TablePolicy, TableRow, TableSortDirection,
    TableState, TableStyles,
};
use bmux_tui_components::text_input::TextInputComponent;
use bmux_tui_components::theme::{ComponentSurfaceDepth, ComponentTheme};
use std::cell::RefCell;

const MODAL_ID: &str = "switcher";
const SEARCH_ID: &str = "switcher.search";
const LIST_ID: &str = "switcher.list";
const TABLE_ID: &str = "switcher.table";
const ACTION_PREFIX: &str = "switcher.action.";
const EFFORT_PREFIX: &str = "switcher.effort.";
/// Widest modal, in cells.
const MAX_WIDTH: u16 = 132;
/// Tallest modal, in cells.
const MAX_HEIGHT: u16 = 40;
/// Narrowest body that keeps the details pane beside the list.
const SPLIT_MIN_WIDTH: u16 = 96;

/// Styles the switcher paints with, resolved from the active theme's semantic roles.
#[derive(Debug, Clone, Copy)]
pub struct SwitcherTheme {
    component: ComponentTheme,
    modal: ModalTheme,
    chrome: bmux_tui_components::theme::ComponentChrome,
}

impl SwitcherTheme {
    /// Resolve the switcher's styles from the presented theme.
    #[must_use]
    pub fn from_presented(theme: &crate::theme::PresentedTheme) -> Self {
        Self {
            component: theme
                .component_theme()
                .for_surface(ComponentSurfaceDepth::Overlay),
            modal: crate::render::TuiTheme::from_presented(theme).modal_theme(),
            chrome: crate::render::dialog_chrome(),
        }
    }

    const fn text(&self) -> Style {
        self.component.text
    }

    const fn muted(&self) -> Style {
        self.component.muted
    }

    const fn heading(&self) -> Style {
        self.component.title.add_modifier(Modifier::BOLD)
    }

    fn list_styles(&self) -> SelectableListStyles {
        let theme = self.component;
        SelectableListStyles {
            background: theme.surfaces.overlay,
            scrollbar: theme.scrollbar_styles(),
            normal: theme.text,
            focused: theme.selected,
            selected: theme.selected,
            hovered: theme.hover,
            pressed: theme.selected.add_modifier(Modifier::BOLD),
            // Headings are disabled rows: they keep their own styles rather than dimming.
            disabled: theme.text,
        }
    }

    fn readiness(&self, readiness: &Readiness) -> Span {
        match readiness {
            Readiness::Confirmed => Span::styled("✓", self.component.success),
            Readiness::Available => Span::styled("✓", self.component.muted),
            Readiness::Action { fix, .. } => Span::styled(
                match fix {
                    Fix::Connect => "Connect ›",
                    Fix::Reapprove => "Approve ›",
                    Fix::Setup => "Set up ›",
                },
                self.component.warning,
            ),
            Readiness::Unavailable(_) => Span::styled("✕", self.component.error),
        }
    }
}

/// A pointer target outside the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// An effort chip in the details pane.
    Effort(String),
    /// A button.
    Action(Action),
    /// A compare-table column heading.
    SortColumn(usize),
    /// Outside the modal.
    Outside,
}

/// Buttons the switcher offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Use the selected model, or start its fix.
    Use,
    /// Add or remove the selected model from favorites.
    Favorite,
    /// Open native tool setup.
    Setup,
    /// Switch between Browse and Compare.
    Compare,
    /// Close the switcher.
    Close,
}

impl Action {
    const fn id(self) -> &'static str {
        match self {
            Self::Use => "use",
            Self::Favorite => "favorite",
            Self::Setup => "setup",
            Self::Compare => "compare",
            Self::Close => "close",
        }
    }

    const ALL: [Self; 5] = [
        Self::Use,
        Self::Favorite,
        Self::Setup,
        Self::Compare,
        Self::Close,
    ];
}

/// Columns of the compare table, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Column_ {
    Model,
    Source,
    Context,
    Output,
    InputPrice,
    OutputPrice,
    Status,
}

const COLUMNS: [Column_; 7] = [
    Column_::Model,
    Column_::Source,
    Column_::Context,
    Column_::Output,
    Column_::InputPrice,
    Column_::OutputPrice,
    Column_::Status,
];

/// Sorting of the compare table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompareState {
    column: Option<usize>,
    descending: bool,
}

impl CompareState {
    /// Sort by the next sortable column; `reverse` flips the direction instead.
    pub const fn cycle(&mut self, reverse: bool) {
        if reverse {
            self.descending = !self.descending;
            return;
        }
        self.column = match self.column {
            None => Some(0),
            Some(index) if index + 1 < COLUMNS.len() => Some(index + 1),
            Some(_) => None,
        };
    }

    /// Sort by `column`, flipping the direction when it is already the sort column.
    pub fn sort_by(&mut self, column: usize) {
        if self.column == Some(column) {
            self.descending = !self.descending;
        } else {
            self.column = Some(column);
            self.descending = matches!(
                COLUMNS.get(column),
                Some(Column_::Context | Column_::Output)
            );
        }
    }
}

/// What a pointer event did to the list or table.
#[derive(Debug, Default)]
pub struct Routed {
    /// Row id under a left-button press, when the press landed on a choice.
    pub pressed: Option<String>,
    /// Whether the event changed presentation (hover, scroll).
    pub handled: bool,
}

/// Everything one frame draws, owned so components can borrow it.
struct Frame {
    items: Vec<SelectableListItem>,
    item_rows: Vec<String>,
    active_index: Option<usize>,
    details: Vec<DetailItem>,
    table_columns: Vec<TableColumn<'static>>,
    table_rows: Vec<TableRow>,
    table_ids: Vec<String>,
    table_state: RefCell<TableState>,
    hints: Vec<(String, String)>,
}

fn frame(switcher: &ModelSwitcher, theme: &SwitcherTheme) -> Frame {
    let (items, item_rows) = list_items(switcher, theme);
    let active_index = switcher
        .active
        .as_ref()
        .and_then(|active| item_rows.iter().position(|id| id == active));
    let (table_columns, table_rows, table_ids) = compare_table(switcher, theme);
    let selected = switcher
        .active
        .as_ref()
        .and_then(|active| table_ids.iter().position(|id| id == active));
    Frame {
        items,
        item_rows,
        active_index,
        details: switcher
            .active_entry()
            .map(|entry| detail_items(switcher, entry))
            .unwrap_or_default(),
        table_columns,
        table_rows,
        table_ids,
        table_state: RefCell::new(TableState::new(selected)),
        hints: hints(switcher),
    }
}

fn list_items(
    switcher: &ModelSwitcher,
    theme: &SwitcherTheme,
) -> (Vec<SelectableListItem>, Vec<String>) {
    let mut items = Vec::new();
    let mut ids = Vec::new();
    let spinner = spinner_glyph(switcher.started.elapsed());
    for (index, row) in switcher.rows.iter().enumerate() {
        match row {
            Row::Heading {
                title,
                detail,
                loading,
                failure,
                ..
            } => {
                let mut spans = vec![Span::styled(title.to_uppercase(), theme.heading())];
                if !detail.is_empty() {
                    spans.push(Span::styled(format!("  {detail}"), theme.muted()));
                }
                if *loading {
                    spans.push(Span::styled(
                        format!("  {spinner} checking…"),
                        theme.muted(),
                    ));
                }
                if let Some(failure) = failure {
                    spans.push(Span::styled(
                        format!("  {failure}"),
                        theme.component.warning,
                    ));
                }
                let mut lines = Vec::new();
                if index > 0 {
                    lines.push(Line::new());
                }
                lines.push(Line::from_spans(spans));
                items.push(
                    SelectableListItem::multiline(format!("heading.{index}"), lines).disabled(true),
                );
                ids.push(format!("heading.{index}"));
            }
            Row::Choice {
                id,
                entry,
                current,
                favorite,
            } => {
                let entry = &switcher.inventory.entries()[*entry];
                items.push(choice_item(id, entry, *current, *favorite, theme));
                ids.push(id.clone());
            }
        }
    }
    (items, ids)
}

fn choice_item(
    id: &str,
    entry: &Entry,
    current: bool,
    favorite: bool,
    theme: &SwitcherTheme,
) -> SelectableListItem {
    let marker = if current {
        Span::styled("● ", theme.component.success)
    } else if favorite {
        Span::styled("★ ", theme.component.warning)
    } else {
        Span::raw("  ")
    };
    let title_style = if current {
        theme.text().add_modifier(Modifier::BOLD)
    } else {
        theme.text()
    };
    let mut spans = vec![marker, Span::styled(entry.title().to_owned(), title_style)];
    if let Some(identifier) = entry.identifier() {
        spans.push(Span::styled(format!("  {identifier}"), theme.muted()));
    }
    let mut trailing = Vec::new();
    if let Some(summary) = entry.summary() {
        trailing.push(Span::styled(summary, theme.muted()));
        trailing.push(Span::raw("  "));
    }
    trailing.push(theme.readiness(&entry.readiness));
    SelectableListItem::rich(id.to_owned(), Line::from_spans(spans)).trailing(
        Line::from_spans(trailing),
        TrailingPlacement {
            align: TrailingAlign::Right,
            keep_visible: true,
            gap: 2,
        },
    )
}

fn detail_items(switcher: &ModelSwitcher, entry: &Entry) -> Vec<DetailItem> {
    let model = &entry.model.model;
    let mut items = Vec::new();
    let source = switcher
        .inventory
        .sources()
        .find(|source| source.key == entry.source);
    match &entry.access {
        Access::Connect(candidate) => {
            items.push(DetailItem::new(
                "Credentials",
                candidate.application.clone(),
            ));
            return items;
        }
        Access::Setup { tool } => {
            items.push(DetailItem::new("Tool", tool.clone()));
            return items;
        }
        Access::Direct | Access::Connection { .. } => {}
    }
    if let Some(source) = source {
        let runs = match &entry.source {
            SourceKey::Native { .. } => format!("{} (runs the tool)", source.title),
            SourceKey::Connection { .. } => format!("{} {}", source.title, source.detail),
            SourceKey::Provider { .. } | SourceKey::Connect => source.title.clone(),
        };
        items.push(DetailItem::new("Runs in", runs));
    }
    if let Some(billing) = inventory::billing_label(entry.model.billing_route) {
        items.push(DetailItem::new("Billed to", billing));
    }
    let mut size = Vec::new();
    if let Some(context) = model.context_window {
        size.push(format!("{} context", token_count(context)));
    }
    if let Some(output) = model.max_output_tokens {
        size.push(format!("{} output", token_count(output)));
    }
    if !size.is_empty() {
        items.push(DetailItem::new("Size", size.join(" · ")));
    }
    let prices = price_lines(model);
    if !prices.is_empty() {
        items.push(DetailItem::new(
            "Price",
            prices
                .into_iter()
                .map(|(label, price)| format!("{label} {price}"))
                .collect::<Vec<_>>()
                .join(" · "),
        ));
    }
    let supports: Vec<_> = capability_labels(model).collect();
    if !supports.is_empty() {
        items.push(DetailItem::new("Supports", supports.join(" · ")));
    }
    items
}

fn readiness_text(entry: &Entry) -> Option<(String, bool)> {
    match &entry.readiness {
        Readiness::Confirmed => None,
        Readiness::Available => matches!(entry.source, SourceKey::Native { .. }).then(|| {
            (
                "Signed in. The tool checks access to this model when you start.".to_owned(),
                false,
            )
        }),
        Readiness::Action { reason, .. } => Some((reason.clone(), false)),
        Readiness::Unavailable(reason) => Some((reason.clone(), true)),
    }
}

const fn use_label(entry: &Entry) -> &'static str {
    match (&entry.access, &entry.readiness) {
        (Access::Connect(_), _) => "Connect",
        (Access::Setup { .. }, _)
        | (
            _,
            Readiness::Action {
                fix: Fix::Setup, ..
            },
        ) => "Set up",
        (
            _,
            Readiness::Action {
                fix: Fix::Reapprove,
                ..
            },
        ) => "Approve again",
        _ => "Use model",
    }
}

fn compare_table(
    switcher: &ModelSwitcher,
    theme: &SwitcherTheme,
) -> (Vec<TableColumn<'static>>, Vec<TableRow>, Vec<String>) {
    let sources: std::collections::BTreeMap<_, _> = switcher
        .inventory
        .sources()
        .map(|source| (source.key.clone(), source))
        .collect();
    let source_label = |entry: &Entry| compare_source(&sources, entry);
    let rows = compare_rows(switcher, &source_label);
    let columns = compare_columns(switcher);
    let mut ids = Vec::new();
    let table_rows = rows
        .into_iter()
        .map(|(id, entry)| {
            ids.push(id);
            let model = &entry.model.model;
            let currency = model
                .pricing
                .as_ref()
                .map_or("USD", |pricing| pricing.currency.as_str());
            let cell = |text: Option<String>| {
                Line::from_spans(vec![text.map_or_else(
                    || Span::styled("—", theme.muted()),
                    |text| Span::styled(text, theme.text()),
                )])
            };
            TableRow::rich(vec![
                Line::from_spans(vec![Span::styled(entry.title().to_owned(), theme.text())]),
                Line::from_spans(vec![Span::styled(source_label(entry), theme.muted())]),
                cell(model.context_window.map(token_count)),
                cell(model.max_output_tokens.map(token_count)),
                cell(entry_price(entry, false).map(|micros| {
                    inventory::token_price(currency, bcode_model::ModelTokenPrice { micros })
                })),
                cell(entry_price(entry, true).map(|micros| {
                    inventory::token_price(currency, bcode_model::ModelTokenPrice { micros })
                })),
                Line::from_spans(vec![theme.readiness(&entry.readiness)]),
            ])
        })
        .collect();
    (columns, table_rows, ids)
}

/// Unique model rows of the compare table, in the chosen sort order.
fn compare_rows<'a>(
    switcher: &'a ModelSwitcher,
    source_label: &dyn Fn(&Entry) -> String,
) -> Vec<(String, &'a Entry)> {
    let mut rows: Vec<(String, &Entry)> = switcher
        .rows
        .iter()
        .filter_map(|row| match row {
            Row::Choice { id, entry, .. } => {
                Some((id.clone(), &switcher.inventory.entries()[*entry]))
            }
            Row::Heading { .. } => None,
        })
        .filter(|(_, entry)| !matches!(entry.access, Access::Connect(_) | Access::Setup { .. }))
        .collect();
    // A model listed in several sections (current, favorite, its source) appears once.
    let mut seen = std::collections::BTreeSet::new();
    rows.retain(|(_, entry)| seen.insert(entry.key.clone()));
    if let Some(column) = switcher.compare.column.and_then(|index| COLUMNS.get(index)) {
        let descending = switcher.compare.descending;
        rows.sort_by(|(_, left), (_, right)| {
            let (left_model, right_model) = (&left.model.model, &right.model.model);
            let ordering = match column {
                Column_::Model => left.title().cmp(right.title()),
                Column_::Source => source_label(left).cmp(&source_label(right)),
                Column_::Context => left_model.context_window.cmp(&right_model.context_window),
                Column_::Output => left_model
                    .max_output_tokens
                    .cmp(&right_model.max_output_tokens),
                Column_::InputPrice => entry_price(left, false).cmp(&entry_price(right, false)),
                Column_::OutputPrice => entry_price(left, true).cmp(&entry_price(right, true)),
                Column_::Status => left
                    .readiness
                    .selectable()
                    .cmp(&right.readiness.selectable()),
            };
            if descending {
                ordering.reverse()
            } else {
                ordering
            }
        });
    }
    rows
}

/// The source column of a compare row.
fn compare_source(
    sources: &std::collections::BTreeMap<SourceKey, &inventory::Source>,
    entry: &Entry,
) -> String {
    sources
        .get(&entry.source)
        .map_or_else(String::new, |source| {
            if source.detail.is_empty() || matches!(entry.source, SourceKey::Provider { .. }) {
                source.title.clone()
            } else {
                format!("{} · {}", source.title, source.detail)
            }
        })
}

/// Input or output price per million tokens, in micro-units.
fn entry_price(entry: &Entry, output: bool) -> Option<u64> {
    entry.model.model.pricing.as_ref().and_then(|pricing| {
        if output {
            pricing.output
        } else {
            pricing.input
        }
        .map(|price| price.micros)
    })
}

/// Compare-table columns, with the sort indicator on the sort column.
fn compare_columns(switcher: &ModelSwitcher) -> Vec<TableColumn<'static>> {
    COLUMNS
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let (title, align, width) = match column {
                Column_::Model => ("Model", TableAlign::Left, 0),
                Column_::Source => ("Source", TableAlign::Left, 0),
                Column_::Context => ("Context", TableAlign::Right, 9),
                Column_::Output => ("Max out", TableAlign::Right, 8),
                Column_::InputPrice => ("In $/M", TableAlign::Right, 10),
                Column_::OutputPrice => ("Out $/M", TableAlign::Right, 10),
                Column_::Status => ("Ready", TableAlign::Center, 11),
            };
            let sort = (switcher.compare.column == Some(index)).then_some(
                if switcher.compare.descending {
                    TableSortDirection::Descending
                } else {
                    TableSortDirection::Ascending
                },
            );
            let column = TableColumn::new(title).align(align).sort(sort);
            if width == 0 {
                column.flex(1)
            } else {
                column.fixed(width)
            }
        })
        .collect()
}

fn hints(switcher: &ModelSwitcher) -> Vec<(String, String)> {
    let mut hints = vec![(switcher.hint("tui.control.submit"), "use".to_owned())];
    if switcher
        .active_entry()
        .is_some_and(|entry| switcher.efforts_of(entry).is_some())
    {
        hints.push((
            format!(
                "{}{}",
                switcher.hint("tui.control.left"),
                switcher.hint("tui.control.right")
            ),
            "effort".into(),
        ));
    }
    hints.push((switcher.hint("tui.model.favorite"), "favorite".into()));
    hints.push((
        switcher.hint("tui.model.compare"),
        match switcher.layout {
            Layout::Browse => "compare".into(),
            Layout::Compare => "browse".into(),
        },
    ));
    if switcher.layout == Layout::Compare {
        hints.push((switcher.hint("tui.model.sort"), "sort".into()));
    }
    hints.push((switcher.hint("tui.model.setup"), "setup".into()));
    hints.push((
        switcher.hint("tui.control.cancel"),
        if switcher.query_text().is_empty() {
            "close".into()
        } else {
            "clear".into()
        },
    ));
    hints
}

fn button<'a>(action: Action, label: &str, primary: bool, theme: &SwitcherTheme) -> Surface<'a> {
    let style = if primary {
        theme.component.selected.add_modifier(Modifier::BOLD)
    } else {
        theme.component.muted.add_modifier(Modifier::REVERSED)
    };
    Surface::new(TextBlock::new(Text::from_lines(vec![Line::from_spans(
        vec![Span::styled(format!(" {label} "), style)],
    )])))
    .id(format!("{ACTION_PREFIX}{}", action.id()))
}

fn details_pane<'a>(
    switcher: &'a ModelSwitcher,
    frame: &'a Frame,
    theme: &SwitcherTheme,
) -> Column<'a> {
    let Some(entry) = switcher.active_entry() else {
        return Column::new().child(
            TextBlock::new(Text::from_lines(vec![Line::from_spans(vec![
                Span::styled(
                    if switcher.loading() {
                        "Checking what you can use…"
                    } else {
                        "No models match."
                    },
                    theme.muted(),
                ),
            ])]))
            .wrap(TextWrap::Word),
        );
    };
    let mut title = vec![Span::styled(
        entry.title().to_owned(),
        theme.text().add_modifier(Modifier::BOLD),
    )];
    if let Some(identifier) = entry.identifier() {
        title.push(Span::styled(format!("  {identifier}"), theme.muted()));
    }
    let mut column = Column::new()
        .child(TextBlock::new(Text::from_lines(vec![Line::from_spans(title)])).wrap(TextWrap::Word))
        .child(TextBlock::new(Text::from_lines(vec![Line::new()])));
    column = column.child(
        LabeledDetailsComponent::new("switcher.details", &frame.details)
            .label_placement(LabelPlacement::Beside)
            .item_spacing(false)
            .styles(LabeledDetailsStyles {
                label: theme.muted(),
                value: theme.text(),
                continuation: theme.muted(),
            }),
    );
    if let Some((values, picked)) = switcher.efforts_of(entry) {
        let mut chips = BoxRow::new()
            .gap(1)
            .child(TextBlock::new(Text::from_lines(vec![Line::from_spans(
                vec![Span::styled("Effort", theme.muted())],
            )])));
        for value in values {
            let style = if value == picked {
                theme.component.selected.add_modifier(Modifier::BOLD)
            } else {
                theme.muted()
            };
            chips = chips.child(
                Surface::new(TextBlock::new(Text::from_lines(vec![Line::from_spans(
                    vec![Span::styled(format!(" {value} "), style)],
                )])))
                .id(format!("{EFFORT_PREFIX}{value}")),
            );
        }
        column = column
            .child(TextBlock::new(Text::from_lines(vec![Line::new()])))
            .child(chips);
    }
    if let Some((text, error)) = readiness_text(entry) {
        column = column
            .child(TextBlock::new(Text::from_lines(vec![Line::new()])))
            .child(
                TextBlock::new(Text::from_lines(vec![Line::from_spans(vec![
                    Span::styled(
                        text,
                        if error {
                            theme.component.error
                        } else {
                            theme.component.warning
                        },
                    ),
                ])]))
                .wrap(TextWrap::Word),
            );
    }
    let mut buttons =
        BoxRow::new()
            .gap(2)
            .child(button(Action::Use, use_label(entry), true, theme));
    if !matches!(entry.access, Access::Connect(_) | Access::Setup { .. }) {
        let favorite = switcher.choices.favorites.contains(&entry.key);
        buttons = buttons.child(button(
            Action::Favorite,
            if favorite {
                "★ Unfavorite"
            } else {
                "☆ Favorite"
            },
            false,
            theme,
        ));
    }
    column
        .child(TextBlock::new(Text::from_lines(vec![Line::new()])))
        .child(buttons)
}

/// A two-line summary of the selection, shown under the list when there is no room beside it.
fn summary(switcher: &ModelSwitcher, theme: &SwitcherTheme) -> TextBlock {
    let lines = switcher.active_entry().map_or_else(Vec::new, |entry| {
        let mut facts = Vec::new();
        if let Some(summary) = entry.summary() {
            facts.push(summary);
        }
        if let Some((_, picked)) = switcher.efforts_of(entry) {
            facts.push(format!("effort {picked}"));
        }
        let mut lines = vec![Line::from_spans(vec![
            Span::styled(
                entry.title().to_owned(),
                theme.text().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {}", facts.join(" · ")), theme.muted()),
        ])];
        if let Some((text, error)) = readiness_text(entry) {
            lines.push(Line::from_spans(vec![Span::styled(
                text,
                if error {
                    theme.component.error
                } else {
                    theme.component.warning
                },
            )]));
        }
        lines
    });
    TextBlock::new(Text::from_lines(lines)).wrap(TextWrap::None)
}

/// Rows around the body: border, top padding, search row and its gap, notice, and hints.
const CHROME_ROWS: u16 = 7;
/// Fewest body rows, so the details pane fits beside a short list.
const MIN_BODY_ROWS: u16 = 18;

fn modal_frame(theme: &SwitcherTheme, area: Size, body_rows: u16) -> ModalFrame {
    let width = area.width.saturating_sub(4).min(MAX_WIDTH);
    // Tall enough for the body (and no taller), within the screen.
    let height = body_rows
        .max(MIN_BODY_ROWS)
        .saturating_add(CHROME_ROWS)
        .min(MAX_HEIGHT)
        .min(area.height.saturating_sub(2));
    ModalFrame::new(
        ModalSizing::new(
            Size::new(width.min(24), height),
            Size::new(width, height),
            Insets::all(0),
        ),
        theme.modal,
    )
    .title(Line::from_spans(vec![Span::styled(
        " Switch model ",
        theme.heading(),
    )]))
    .chrome(theme.chrome)
    .padding(Insets::new(1, 2, 0, 2))
    .placement(ModalPlacement::Centered)
}

fn tree<'a>(
    switcher: &'a ModelSwitcher,
    frame: &'a Frame,
    theme: &'a SwitcherTheme,
    area: Size,
) -> ModalFrameComponent<'a> {
    let body_rows = match switcher.layout {
        Layout::Browse => frame
            .items
            .iter()
            .map(SelectableListItem::height)
            .sum::<u64>(),
        Layout::Compare => u64::try_from(frame.table_rows.len()).unwrap_or(u64::MAX) + 2,
    };
    let modal = modal_frame(theme, area, u16::try_from(body_rows).unwrap_or(u16::MAX));
    let content_width = modal
        .content_area(Rect::new(0, 0, area.width, area.height))
        .width;
    let count = switcher
        .rows
        .iter()
        .filter_map(|row| match row {
            Row::Choice { entry, .. } => Some(*entry),
            Row::Heading { .. } => None,
        })
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let mut status = vec![Span::styled(format!("{count} models"), theme.muted())];
    if switcher.loading() {
        status.push(Span::styled(
            format!("  {}", spinner_glyph(switcher.started.elapsed())),
            theme.component.accent,
        ));
    }
    let search = BoxRow::new()
        .gap(2)
        .flex(Flex::new(
            1,
            TextInputComponent::new(SEARCH_ID, &switcher.query, &switcher.input_policy)
                .style(theme.text())
                .placeholder("Search models, tools, and providers", theme.muted())
                .focused(true),
        ))
        .child(TextBlock::new(Text::from_lines(vec![Line::from_spans(
            status,
        )])));
    let body = body(switcher, frame, theme, content_width);
    let mut column = Column::new()
        .child(search)
        .child(TextBlock::new(Text::from_lines(vec![Line::new()])))
        .flex(Flex::new(1, BoxedComponent(body)));
    if let Some(notice) = &switcher.notice {
        column = column.child(
            TextBlock::new(Text::from_lines(vec![Line::from_spans(vec![
                Span::styled(
                    notice.text.clone(),
                    if notice.error {
                        theme.component.error
                    } else {
                        theme.component.success
                    },
                ),
            ])]))
            .wrap(TextWrap::Word),
        );
    }
    let hints: Vec<KeyHint<'_>> = frame
        .hints
        .iter()
        .map(|(key, label)| KeyHint::new(key, label))
        .collect();
    column = column.child(SizeBox::new(OwnedHints(hints, theme.component)).height(1));
    ModalFrameComponent::new(MODAL_ID, modal, column)
}

/// The list beside its details, the list above a summary when narrow, or the compare table.
fn body<'a>(
    switcher: &'a ModelSwitcher,
    frame: &'a Frame,
    theme: &'a SwitcherTheme,
    content_width: u16,
) -> Box<dyn Component + 'a> {
    match switcher.layout {
        Layout::Browse => {
            let list = SelectableListComponent::new(LIST_ID, &frame.items, &switcher.list)
                .policy(
                    SelectableListPolicy::interactive()
                        .scrollbar(ScrollbarAxisLayoutMode::OverlayOnOverflow),
                )
                .styles(theme.list_styles());
            let mut policy = SelectableListPolicy::interactive()
                .scrollbar(ScrollbarAxisLayoutMode::OverlayOnOverflow);
            policy.highlight = SelectableListHighlightPolicy::new("▍", true);
            let list = list.policy(policy);
            let mut state = switcher.list.get();
            state.set_selected(frame.active_index);
            state.set_focused(frame.active_index);
            switcher.list.set(state);
            let list = Revealing(list);
            if content_width >= SPLIT_MIN_WIDTH {
                Box::new(
                    BoxRow::new()
                        .gap(2)
                        .flex(Flex::new(3, list))
                        .flex(Flex::new(
                            2,
                            Surface::new(details_pane(switcher, frame, theme))
                                .border(
                                    bmux_tui::chrome::Border::single()
                                        .sides(bmux_tui::chrome::BorderSides::LEFT)
                                        .style(theme.component.border),
                                )
                                .padding(Insets::new(0, 0, 0, 2)),
                        )),
                )
            } else {
                Box::new(
                    Column::new()
                        .flex(Flex::new(1, list))
                        .child(TextBlock::new(Text::from_lines(vec![Line::new()])))
                        .child(SizeBox::new(summary(switcher, theme)).height(2)),
                )
            }
        }
        Layout::Compare => Box::new(Revealing(
            TableComponent::new(
                TABLE_ID,
                &frame.table_columns,
                &frame.table_rows,
                &frame.table_state,
            )
            .policy(compare_policy())
            .styles(compare_styles(theme))
            .empty("No models match."),
        )),
    }
}

/// Owns the hint list for the key-hint bar it builds.
struct OwnedHints<'a>(Vec<KeyHint<'a>>, ComponentTheme);

impl Component for OwnedHints<'_> {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.bar().layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.bar().paint(layout, cx);
    }
}

impl OwnedHints<'_> {
    fn bar(&self) -> KeyHintBarComponent<'_> {
        KeyHintBarComponent::new("switcher.hints", &self.0)
            .policy(
                KeyHintBarPolicy::compact()
                    .separator("  ")
                    .overflow(KeyHintOverflow::Hide)
                    .background(false),
            )
            .styles(self.1.key_hint_bar_styles())
    }
}

/// A boxed child.
struct BoxedComponent<'a>(Box<dyn Component + 'a>);

impl Component for BoxedComponent<'_> {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.0.layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.0.paint(layout, cx);
    }

    fn event(
        &self,
        event: &Event,
        layout: &LayoutNode,
        cx: &mut EventCx<'_>,
    ) -> bmux_tui::event::EventOutcome {
        self.0.event(event, layout, cx)
    }
}

/// Keeps the selected row or table row scrolled into view when painted.
struct Revealing<C>(C);

impl Component for Revealing<SelectableListComponent<'_, '_>> {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.0.layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.0.reveal_focus(layout);
        self.0.paint(layout, cx);
    }

    fn event(
        &self,
        event: &Event,
        layout: &LayoutNode,
        cx: &mut EventCx<'_>,
    ) -> bmux_tui::event::EventOutcome {
        self.0.event(event, layout, cx)
    }
}

impl Component for Revealing<TableComponent<'_, '_>> {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.0.layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.0.paint(layout, cx);
    }

    fn event(
        &self,
        event: &Event,
        layout: &LayoutNode,
        cx: &mut EventCx<'_>,
    ) -> bmux_tui::event::EventOutcome {
        self.0.event(event, layout, cx)
    }
}

const fn compare_policy() -> TablePolicy {
    let mut policy = TablePolicy::interactive();
    policy.header = true;
    policy.header_separator = true;
    policy.vertical_scrollbar = ScrollbarAxisLayoutMode::OverlayOnOverflow;
    policy
}

fn compare_styles(theme: &SwitcherTheme) -> TableStyles {
    let component = theme.component;
    TableStyles {
        header: component.muted.add_modifier(Modifier::BOLD),
        row: component.text,
        selected: component.selected,
        hovered: component.hover,
        ..TableStyles::from(component)
    }
}

/// Paint the switcher centered in `area` and return the layout it was painted with.
pub(super) fn paint(
    switcher: &ModelSwitcher,
    theme: &SwitcherTheme,
    area: Rect,
    cx: &mut PaintCx<'_, '_>,
) -> LayoutNode {
    let frame = frame(switcher, theme);
    let tree = tree(switcher, &frame, theme, area.size());
    let layout = tree.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    cx.with_child(
        i32::from(area.x),
        i64::from(area.y),
        LocalRect::new(0, 0, area.width, area.height),
        |cx| tree.paint(&layout, cx),
    );
    layout
}

/// Route a pointer event to the list or table, through the layout last painted.
pub(super) fn route_list_event(
    switcher: &ModelSwitcher,
    theme: &SwitcherTheme,
    painted: &Painted,
    event: &Event,
) -> Routed {
    let frame = frame(switcher, theme);
    let tree = tree(switcher, &frame, theme, painted.area.size());
    let area = painted.area;
    let press = matches!(
        event,
        Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
    );
    if press && switcher.layout == Layout::Browse {
        // A press focuses the row it lands on; clearing focus first tells a press on a row
        // from a press on a heading, gap, or scrollbar.
        let mut state = switcher.list.get();
        state.set_focused(None);
        switcher.list.set(state);
    }
    let mut cx = EventCx::new(&painted.layout);
    let outcome = cx.with_transform(0, 0, i32::from(area.x), i64::from(area.y), area, |cx| {
        tree.event(event, &painted.layout, cx)
    });
    let pressed = match (press, switcher.layout, event) {
        (true, Layout::Browse, _) => switcher
            .list
            .get()
            .focused()
            .and_then(|index| frame.item_rows.get(index).cloned())
            .filter(|id| !id.starts_with("heading.")),
        (true, Layout::Compare, Event::Mouse(mouse)) => {
            compare_row_at(&frame, &painted.layout, area, mouse.position)
        }
        _ => None,
    };
    Routed {
        pressed,
        handled: outcome != bmux_tui::event::EventOutcome::Ignored,
    }
}

/// The compare row under `point`, using the table's own geometry.
fn compare_row_at(frame: &Frame, layout: &LayoutNode, area: Rect, point: Point) -> Option<String> {
    let local = Point::new(
        point.x.saturating_sub(area.x),
        point.y.saturating_sub(area.y),
    );
    let table = layout.find_rect(&LayoutId::new(TABLE_ID))?;
    let state = frame.table_state.borrow();
    match bmux_tui_components::table::Table::new(&frame.table_columns, &frame.table_rows)
        .policy(compare_policy())
        .hit_test(table, &state, local)?
    {
        TableHit::Row { row } | TableHit::Cell { row, .. } => frame.table_ids.get(row).cloned(),
        TableHit::Header { .. } => None,
    }
}

/// What lies under `point` other than list rows, from the layout last painted.
pub(super) fn target_at(
    switcher: &ModelSwitcher,
    layout: &LayoutNode,
    area: Rect,
    point: Point,
) -> Option<Target> {
    let local = Point::new(
        point.x.saturating_sub(area.x),
        point.y.saturating_sub(area.y),
    );
    let surface = LayoutId::new(format!("{MODAL_ID}.surface"));
    let inside = layout
        .find_rect(&surface)
        .is_some_and(|rect| rect.contains(local));
    if !inside {
        return Some(Target::Outside);
    }
    let under = |id: String| {
        layout
            .find_rect(&LayoutId::new(id))
            .is_some_and(|rect| rect.contains(local))
    };
    if let Some((values, _)) = switcher
        .active_entry()
        .and_then(|entry| switcher.efforts_of(entry))
        && let Some(value) = values
            .into_iter()
            .find(|value| under(format!("{EFFORT_PREFIX}{value}")))
    {
        return Some(Target::Effort(value));
    }
    if let Some(action) = Action::ALL
        .into_iter()
        .find(|action| under(format!("{ACTION_PREFIX}{}", action.id())))
    {
        return Some(Target::Action(action));
    }
    if switcher.layout == super::Layout::Compare
        && let Some(table) = layout.find_rect(&LayoutId::new(TABLE_ID))
        && local.y == table.y
        && table.contains(local)
    {
        return compare_column_at(switcher, table, local).map(Target::SortColumn);
    }
    None
}

/// Rows of the list viewport last painted, for paging.
pub(super) fn list_rows(layout: &LayoutNode) -> Option<usize> {
    layout
        .find_rect(&LayoutId::new(LIST_ID))
        .map(|rect| usize::from(rect.height))
}

static SPINNER: std::sync::LazyLock<bmux_tui_components::spinner::Spinner> =
    std::sync::LazyLock::new(bmux_tui_components::spinner::Spinner::braille);

fn spinner_glyph(elapsed: std::time::Duration) -> &'static str {
    SPINNER.frame(elapsed)
}

/// Time until the spinner's next frame.
pub(super) fn spinner_interval(elapsed: std::time::Duration) -> std::time::Duration {
    SPINNER
        .next_change(elapsed)
        .unwrap_or(std::time::Duration::from_millis(100))
}

/// The compare column whose heading contains `point`, using the table's own geometry.
fn compare_column_at(switcher: &ModelSwitcher, table: Rect, point: Point) -> Option<usize> {
    let frame_columns = compare_columns(switcher);
    let rows: Vec<TableRow> = Vec::new();
    let state = TableState::new(None);
    match bmux_tui_components::table::Table::new(&frame_columns, &rows)
        .policy(compare_policy())
        .hit_test(table, &state, point)
    {
        Some(TableHit::Header { column }) => Some(column),
        _ => None,
    }
}
