//! Rows of an AI card: a live, abbreviated feed of the question's turn in its linked session.
//!
//! * It is a card on the same frame as comment threads ([`super::thread_card`]): folded with
//!   `z` or its chevron, a Viewed checkbox (a new answer makes it unviewed again).
//! * The header carries the card's state: a spinner while the session works, the status, the
//!   session's tokens and cost, and the time the turn has run.
//! * The body is the turn: the question (`▍you`), reasoning collapsed to `▸ thinking · N words`,
//!   tool lines (`◌` running, `✓` done, `✗` failed) with the last lines of their output and an
//!   `All N lines` expander, the answer as streaming markdown, notices, and errors.
//! * A pending permission request of the session replaces the card's actions with its prompt
//!   (Allow, Always, Decline, Never); while a turn runs the actions are Stop and Open in Bcode.
//!
//! Every row comes from the bounded [`Feed`](crate::agent::Feed), so a card's height is bounded
//! whatever the session did.
//!
//! In the review assistant's conversation ([`dock_rows`]), a turn that writes the guide on
//! screen shows one live guide card ([`GuideCard`]) in place of its guide tool lines: the
//! guide's sections as the review reads them, each one a click away in the diff. Turns of other
//! guides, and failed guide calls, keep their ordinary tool lines.

use super::{
    ActionCells, AgentAction, Folds, ItemAction, ItemRows, Look, SuggestionAction, action_row,
    indented, line_reference, markdown, markdown_all_rows, outdated_label, suggestion_rows,
    thread_card,
};
use crate::agent::feed::{FeedItem, PendingPermission, TOOL_TAIL_LINES, ToolLine, ToolState, Turn};
use crate::agent::{AgentPhase, AgentState, AgentStreams};
use crate::guide::{CardSectionState, CardStatus, GuideCard};
use crate::theme::ReviewTheme;
use bcode_code_review_models::{ReviewAiExchange, ReviewAiExchangeStatus};
use bmux_tui::style::Modifier;
use bmux_tui::text::{Line, Span};
use bmux_tui::text_width::display_width;
use bmux_tui_components::spinner::{DEFAULT_INTERVAL, Spinner};

/// Rows of open reasoning.
const REASONING_ROWS: usize = 40;
/// Lines of one error.
const ERROR_ROWS: usize = 3;
/// Cells of a card's `model · effort` label at most.
const MODEL_LABEL_CELLS: usize = 40;
/// Cells continuation rows of an error are indented by (under its text, past the icon).
const ERROR_INDENT: usize = 2;

/// Cells of indentation per section depth on a guide card (deeper sections indent no further
/// than [`GUIDE_CARD_MAX_DEPTH`]).
const GUIDE_CARD_INDENT: usize = 2;
/// Depth past which guide card rows stop indenting.
const GUIDE_CARD_MAX_DEPTH: u16 = 3;
/// Tools whose lines a guide card replaces (the guide tools).
const GUIDE_TOOL_PREFIX: &str = "code_review.guide.";
/// Guide tools that write; a turn calling one of them shows a guide card.
const GUIDE_WRITE_TOOLS: [&str; 2] = ["code_review.guide.start", "code_review.guide.write"];

/// Separator between a feed item's key and the exchange it belongs to, in [`Folds::feed_open`].
const OPEN_SEPARATOR: char = '\u{1f}';

/// The key of an opened feed item (reasoning or tool output) of `exchange_id`.
#[must_use]
pub fn open_key(exchange_id: &str, item_id: &str) -> String {
    format!("{exchange_id}{OPEN_SEPARATOR}{item_id}")
}

/// The stable id of a feed item that opens (reasoning, tool output).
#[must_use]
pub fn item_id(item: &FeedItem) -> Option<&str> {
    match item {
        FeedItem::Reasoning { id, .. } => Some(id),
        FeedItem::Tool(tool) => Some(&tool.id),
        _ => None,
    }
}

/// The `guide_id` a guide tool call's JSON arguments name. Arguments are kept bounded, so a
/// long call may be cut: the id is then read from the text before the cut.
#[must_use]
pub fn guide_id_of(arguments: &str) -> Option<String> {
    if let Ok(serde_json::Value::Object(object)) =
        serde_json::from_str::<serde_json::Value>(arguments)
    {
        return object
            .get("guide_id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .map(str::to_owned);
    }
    let rest = &arguments[arguments.find("\"guide_id\"")? + "\"guide_id\"".len()..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let id: String = rest
        .chars()
        .take_while(|c| *c != '"' && *c != '\\')
        .collect();
    (!id.trim().is_empty() && rest[id.len()..].starts_with('"')).then_some(id)
}

/// Whether `turn` writes the guide `card` shows, so the card stands in for its guide tool
/// lines: it calls a guide write tool naming the card's guide, or (when none of its writes
/// names a guide, a guide started without an id) the card's version is written by `session`.
fn writes_guide(turn: &Turn, card: &GuideCard, session: &str) -> bool {
    let mut writes = false;
    let mut named = false;
    for item in &turn.items {
        let FeedItem::Tool(tool) = item else {
            continue;
        };
        if !GUIDE_WRITE_TOOLS.contains(&tool.name.as_str()) {
            continue;
        }
        writes = true;
        if let Some(id) = guide_id_of(&tool.arguments) {
            if id == card.guide_id {
                return true;
            }
            named = true;
        }
    }
    writes && !named && card.session.as_deref() == Some(session)
}

/// The turn an exchange's card shows, and whether it is the session's newest turn.
///
/// It is the newest turn asking the exchange's question, else (when the question is no longer
/// recognizable in the observed window) the session's newest turn when this is the newest
/// exchange of the session.
#[must_use]
pub fn exchange_turn<'a>(
    exchange: &ReviewAiExchange,
    state: &'a AgentState,
    newest_exchange: bool,
) -> Option<(&'a Turn, bool)> {
    let last = state.feed.turns.len().checked_sub(1)?;
    if let Some((index, turn)) = state.feed.turn_for(&exchange.question) {
        return Some((turn, index == last));
    }
    if newest_exchange && exchange.status == ReviewAiExchangeStatus::Linked {
        return state.feed.turns.last().map(|turn| (turn, true));
    }
    None
}

/// Whether `exchange` is the newest exchange of its session.
#[must_use]
pub fn is_newest(exchange: &ReviewAiExchange, notes: &crate::notes::ReviewNotes) -> bool {
    let Some(session) = exchange.session_id.as_deref() else {
        return true;
    };
    notes
        .exchanges()
        .iter()
        .filter(|other| other.session_id.as_deref() == Some(session))
        .max_by_key(|other| (other.created_at_ms, other.exchange_id.as_str()))
        .is_none_or(|newest| newest.exchange_id == exchange.exchange_id)
}

/// Header chips of an AI card after its lead: spinner and status, usage, and an outdated label.
/// Returns whether the question or the observation of its session failed (a retry applies),
/// and whether it needs you.
fn header(
    agent: &bcode_code_review::document::AgentThreadItem,
    status: Option<ReviewAiExchangeStatus>,
    live: Option<(&AgentState, crate::agent::LiveClock)>,
    theme: &ReviewTheme,
) -> (Vec<Vec<Span>>, bool, bool) {
    let icons = &theme.icons;
    let (label, style, failed) = match (status, live) {
        (Some(ReviewAiExchangeStatus::Failed), _) => ("failed", theme.error, true),
        (Some(ReviewAiExchangeStatus::Pending), _) => ("asking…", theme.badge_draft, false),
        (_, Some((state, _))) if state.needs_you() => ("needs you", theme.pill_warning, false),
        (_, Some((state, _))) => match state.phase {
            AgentPhase::Waiting => ("connecting…", theme.muted, false),
            AgentPhase::Working if state.cancelling => ("stopping…", theme.badge_draft, false),
            AgentPhase::Working => ("working…", theme.badge_draft, false),
            AgentPhase::Answered if state.status == "stopped" => ("stopped", theme.muted, false),
            AgentPhase::Answered => ("answered", theme.badge_resolved, false),
            AgentPhase::Failed if state.disconnected => ("stream stopped", theme.error, true),
            AgentPhase::Failed => ("failed", theme.error, false),
        },
        _ => ("suggestion", theme.badge_ai, false),
    };
    let needs_you = live.is_some_and(|(state, _)| state.needs_you());
    let mut state_chip = Vec::new();
    let spinning = matches!(status, Some(ReviewAiExchangeStatus::Pending))
        || live.is_some_and(|(state, _)| state.working && !state.needs_you());
    if spinning {
        let spinner = Spinner::named(icons.spinner).unwrap_or_else(Spinner::braille);
        let step = live.map_or(0, |(_, clock)| clock.step);
        let elapsed = DEFAULT_INTERVAL.saturating_mul(u32::try_from(step % 64).unwrap_or(0));
        state_chip.push(spinner.span(elapsed, theme.accent));
        state_chip.push(Span::raw(" "));
    }
    state_chip.push(Span::styled(label, style));
    let mut chips = vec![state_chip];
    if let Some((state, clock)) = live {
        let mut usage = Vec::new();
        if state.feed.tokens > 0 {
            usage.push(crate::agent::feed::tokens_label(state.feed.tokens));
        }
        if let Some(cost) = &state.feed.cost {
            usage.push(cost.clone());
        }
        if state.working && clock.seconds > 0 {
            usage.push(format!("{}s", clock.seconds));
        }
        if !usage.is_empty() {
            chips.push(vec![Span::styled(usage.join(icons.separator), theme.muted)]);
        }
    }
    if let bcode_code_review::document::AnchorPlacement::Outdated(reason) = agent.placement {
        chips.push(vec![Span::styled(
            outdated_label(reason),
            theme.badge_outdated,
        )]);
    }
    (chips, failed, needs_you)
}

/// Builds a card's rows while tracking clickable cells.
struct Card<'a> {
    theme: &'a ReviewTheme,
    width: usize,
    pad: String,
    body_width: usize,
    lines: Vec<Line>,
    actions: Vec<ActionCells<ItemAction>>,
}

impl Card<'_> {
    fn row(&self) -> u16 {
        u16::try_from(self.lines.len()).unwrap_or(u16::MAX)
    }

    fn push(&mut self, spans: Vec<Span>) {
        let mut line = vec![Span::raw(self.pad.clone())];
        line.extend(spans);
        self.lines.push(Line::from_spans(line).truncate(self.width));
    }

    /// A row whose `label` (after the padding and `lead` cells) runs `action` on click.
    fn push_toggle(
        &mut self,
        lead: Vec<Span>,
        label: String,
        style: bmux_tui::style::Style,
        action: ItemAction,
    ) {
        let start = self.pad.len()
            + lead
                .iter()
                .map(|span| display_width(&span.content))
                .sum::<usize>();
        let end = (start + display_width(&label)).min(self.width);
        if start < end {
            self.actions.push(ActionCells {
                row: self.row(),
                cells: u16::try_from(start).unwrap_or(u16::MAX)
                    ..u16::try_from(end).unwrap_or(u16::MAX),
                action,
            });
        }
        let mut spans = lead;
        spans.push(Span::styled(label, style));
        self.push(spans);
    }

    fn extend_indented(&mut self, lines: Vec<Line>, extra: usize) {
        let prefix = " ".repeat(self.pad.len() + extra);
        self.lines.extend(indented(lines, &prefix, self.width));
    }

    fn buttons(&mut self, buttons: &[(String, ItemAction)]) {
        if buttons.is_empty() {
            return;
        }
        let buttons: Vec<(&str, ItemAction)> = buttons
            .iter()
            .map(|(label, action)| (label.as_str(), *action))
            .collect();
        let (rows, placed) = action_row(
            &buttons,
            self.row(),
            self.pad.len().min(self.width),
            self.width,
            self.theme,
        );
        self.lines.extend(rows);
        self.actions.extend(placed);
    }

    /// The reviewer's message under `▍you`, in full: every line word-wrapped under the bar,
    /// blank lines between paragraphs kept.
    fn question(&mut self, question: &str) {
        let theme = self.theme;
        let bar = theme.icons.accent_bar;
        self.push(vec![
            Span::styled(bar, theme.accent),
            Span::styled("you", theme.accent.add_modifier(Modifier::BOLD)),
        ]);
        let prefix = || vec![Span::styled(bar, theme.accent), Span::raw(" ")];
        let prefix_width = display_width(bar) + 1;
        let width = self
            .width
            .saturating_sub(self.pad.len() + prefix_width)
            .max(1);
        for line in question.trim().lines() {
            if line.trim().is_empty() {
                self.push(vec![Span::styled(bar, theme.accent)]);
                continue;
            }
            let rows = bmux_tui::text::wrap_line_bounded(
                &Line::from_spans(vec![Span::styled(line.to_owned(), theme.tiers.strong)]),
                bmux_tui::text::TextWrapGeometry::with_continuation(width, width),
                bmux_tui::text::TextWrap::Word,
            );
            for row in rows {
                let mut spans = prefix();
                spans.extend(row.spans);
                self.push(spans);
            }
        }
    }

    /// The items of `turn`; opened items are keyed by `exchange_id`, and the toggles of item `i`
    /// carry `base + i`. With `guide`, the turn's guide tool lines (except failed ones) give way
    /// to that guide card, shown where the first of them was.
    fn turn(
        &mut self,
        turn: &Turn,
        exchange_id: &str,
        folds: &Folds,
        base: usize,
        guide: Option<&GuideCard>,
    ) {
        let theme = self.theme;
        let mut card_shown = false;
        if turn.earlier > 0 {
            self.push(vec![Span::styled(
                format!("+{} earlier", turn.earlier),
                theme.tiers.dim,
            )]);
        }
        for (index, item) in turn.items.iter().enumerate() {
            let open = item_id(item)
                .is_some_and(|id| folds.feed_open.contains(&open_key(exchange_id, id)));
            let toggle = ItemAction::Agent(AgentAction::Toggle(
                u16::try_from(base + index).unwrap_or(u16::MAX),
            ));
            match item {
                FeedItem::Reasoning { words, text, .. } => {
                    let marker = if open {
                        theme.icons.menu
                    } else {
                        theme.icons.collapsed
                    };
                    let style = theme.tiers.dim.add_modifier(Modifier::ITALIC);
                    self.push_toggle(
                        Vec::new(),
                        format!("{marker} thinking · {words} words"),
                        style,
                        toggle,
                    );
                    if open && !text.is_empty() {
                        let rows = markdown(
                            text,
                            self.body_width.saturating_sub(2),
                            REASONING_ROWS,
                            theme,
                        );
                        self.extend_indented(rows, 2);
                    }
                }
                FeedItem::Tool(tool)
                    if guide.is_some()
                        && tool.name.starts_with(GUIDE_TOOL_PREFIX)
                        && tool.state != ToolState::Failed =>
                {
                    if let Some(card) = guide.filter(|_| !card_shown) {
                        card_shown = true;
                        self.guide_card(card);
                    }
                }
                FeedItem::Tool(tool) => self.tool(tool, open, toggle),
                FeedItem::Answer {
                    text,
                    streaming,
                    clipped,
                } => {
                    // Every answer shows in full; the feed's byte bound is the only limit.
                    let rows = markdown_all_rows(text, self.body_width, *streaming);
                    self.extend_indented(rows, 0);
                    if *clipped {
                        self.push(vec![Span::styled(
                            format!(
                                "{} answer too long; the rest is in Bcode",
                                theme.icons.ellipsis
                            ),
                            theme.muted,
                        )]);
                    }
                }
                FeedItem::Notice(text) => {
                    self.push(vec![Span::styled(
                        format!("· {}", text.lines().next().unwrap_or_default()),
                        theme.tiers.dim,
                    )]);
                }
                FeedItem::Error(text) => self.error(text),
            }
        }
    }

    /// An error, each of its first lines wrapped under its text so the reason and what to do
    /// stay readable in a narrow panel.
    fn error(&mut self, text: &str) {
        let theme = self.theme;
        let width = self.width.saturating_sub(self.pad.len());
        let rows = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .take(ERROR_ROWS)
            .flat_map(|line| {
                bmux_tui::text::wrap_line_bounded(
                    &Line::from_spans(vec![
                        Span::styled(theme.icons.alert, theme.error),
                        Span::raw(" "),
                        Span::styled(line.to_owned(), theme.error),
                    ]),
                    bmux_tui::text::TextWrapGeometry::with_continuation(
                        width,
                        width.saturating_sub(ERROR_INDENT),
                    ),
                    bmux_tui::text::TextWrap::Word,
                )
                .into_iter()
                .enumerate()
                .map(|(index, row)| {
                    if index == 0 {
                        row.spans
                    } else {
                        std::iter::once(Span::raw(" ".repeat(ERROR_INDENT)))
                            .chain(row.spans)
                            .collect()
                    }
                })
            })
            .take(ERROR_ROWS * 2)
            .collect::<Vec<_>>();
        for row in rows {
            self.push(row);
        }
    }

    /// A guide card: the guide's title, version and progress, its sections (each a click away
    /// in the diff), and an Open button that shows the guide.
    fn guide_card(&mut self, card: &GuideCard) {
        let theme = self.theme;
        let icons = &theme.icons;
        let (status, style) = match card.status {
            CardStatus::Writing => (
                format!("{} writing {}/{}", icons.running, card.written, card.total),
                theme.badge_draft,
            ),
            CardStatus::Finishing => (format!("{} finishing…", icons.running), theme.badge_draft),
            CardStatus::Complete => (
                format!(
                    "{} {} section{}",
                    icons.viewed,
                    card.total,
                    if card.total == 1 { "" } else { "s" }
                ),
                theme.badge_resolved,
            ),
            CardStatus::Stopped => (
                format!("stopped · {}/{} written", card.written, card.total),
                theme.muted,
            ),
            CardStatus::Failed => (
                format!("failed · {}/{} written", card.written, card.total),
                theme.error,
            ),
        };
        self.push(vec![
            Span::styled(icons.guide_marker, theme.accent),
            Span::raw(" "),
            Span::styled(
                format!("Guide v{} ", card.version),
                theme.accent.add_modifier(Modifier::BOLD),
            ),
            // Progress before the title, so a narrow panel cuts the title, not the progress.
            Span::styled(status, style),
            Span::styled(theme.icons.separator, theme.tiers.dim),
            Span::styled(card.title.clone(), theme.tiers.strong),
        ]);
        let mut writing_marked = !card.busy();
        for section in &card.sections {
            let (glyph, glyph_style, title_style) = match section.state {
                CardSectionState::Planned => (icons.running, theme.tiers.dim, theme.tiers.dim),
                CardSectionState::Written => (icons.unviewed, theme.muted, theme.text),
                CardSectionState::Reviewed => (icons.viewed, theme.viewed, theme.text),
                CardSectionState::Provisional => (icons.unviewed, theme.tiers.dim, theme.muted),
            };
            let depth = usize::from(section.depth.min(GUIDE_CARD_MAX_DEPTH));
            let mut lead = vec![Span::raw(" ".repeat(GUIDE_CARD_INDENT * (depth + 1)))];
            if section.current {
                lead[0] = Span::raw(" ".repeat(GUIDE_CARD_INDENT * (depth + 1) - 1));
                lead.push(Span::styled(icons.accent_bar, theme.accent));
            }
            lead.push(Span::styled(glyph, glyph_style));
            lead.push(Span::raw(" "));
            let mut label = format!("{} {}", section.number, section.title);
            if !writing_marked && section.state == CardSectionState::Planned {
                writing_marked = true;
                label.push_str(" · writing…");
            }
            let style = if section.current {
                title_style.add_modifier(Modifier::BOLD)
            } else {
                title_style
            };
            self.push_toggle(
                lead,
                label,
                style,
                ItemAction::Agent(AgentAction::GuideSection(
                    u16::try_from(section.entry).unwrap_or(u16::MAX),
                )),
            );
        }
        if card.more > 0 {
            self.push(vec![Span::styled(
                format!(
                    "{}+{} more section{}",
                    " ".repeat(GUIDE_CARD_INDENT),
                    card.more,
                    if card.more == 1 { "" } else { "s" }
                ),
                theme.tiers.dim,
            )]);
        }
        self.buttons(&[(
            format!("Open guide {}", icons.external),
            ItemAction::Agent(AgentAction::GuideOpen),
        )]);
    }

    fn tool(&mut self, tool: &ToolLine, open: bool, toggle: ItemAction) {
        let theme = self.theme;
        let (icon, style) = match tool.state {
            ToolState::Running => (theme.icons.running, theme.accent),
            ToolState::Done => (theme.icons.viewed, theme.viewed),
            ToolState::Failed => (theme.icons.failed, theme.error),
        };
        let mut spans = vec![
            Span::styled(icon, style),
            Span::raw(" "),
            Span::styled(tool.name.clone(), theme.muted.add_modifier(Modifier::BOLD)),
        ];
        if !tool.summary.is_empty() {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(tool.summary.clone(), theme.tiers.dim));
        }
        self.push(spans);
        let shown = if open {
            tool.output.len()
        } else {
            TOOL_TAIL_LINES.min(tool.output.len())
        };
        let output_style = if tool.state == ToolState::Failed {
            theme.error
        } else {
            theme.muted
        };
        if open && tool.total_lines > tool.output.len() {
            self.push(vec![
                Span::raw("  "),
                Span::styled(
                    format!("+{} earlier lines", tool.total_lines - tool.output.len()),
                    theme.tiers.dim,
                ),
            ]);
        }
        for line in &tool.output[tool.output.len() - shown..] {
            self.push(vec![
                Span::raw("  "),
                Span::styled(theme.icons.output, theme.tiers.dim),
                Span::raw(" "),
                Span::styled(line.clone(), output_style),
            ]);
        }
        if tool.total_lines > TOOL_TAIL_LINES {
            let label = if open {
                " Fewer lines ".to_owned()
            } else {
                format!(" All {} lines ", tool.total_lines)
            };
            self.push_toggle(vec![Span::raw("  ")], label, theme.pill, toggle);
        }
    }

    /// The question, its error, and the exchange's turn (or what stands in for it). Returns
    /// whether the card shows the session's newest turn, where prompts and stop apply.
    fn exchange(
        &mut self,
        exchange: &ReviewAiExchange,
        state: Option<&AgentState>,
        newest_exchange: bool,
        folds: &Folds,
    ) -> bool {
        let theme = self.theme;
        if let Some(error) = &exchange.error {
            self.alert(error.lines().next().unwrap_or_default());
        }
        if let Some((turn, newest)) =
            state.and_then(|state| exchange_turn(exchange, state, newest_exchange))
        {
            self.turn(turn, &exchange.exchange_id, folds, 0, None);
            return newest;
        }
        if let Some(state) = state.filter(|_| exchange.status != ReviewAiExchangeStatus::Pending) {
            if newest_exchange && state.phase == AgentPhase::Failed {
                self.alert(&state.status);
            } else if !newest_exchange {
                self.push(vec![Span::styled(
                    "earlier in the session; open it in Bcode to read it",
                    theme.muted,
                )]);
            } else if !state.status.is_empty() {
                self.push(vec![Span::styled(state.status.clone(), theme.muted)]);
            }
        }
        newest_exchange
    }

    /// An error row.
    fn alert(&mut self, text: &str) {
        let theme = self.theme;
        self.push(vec![
            Span::styled(theme.icons.alert, theme.error),
            Span::raw(" "),
            Span::styled(text.to_owned(), theme.error),
        ]);
    }

    fn permission(
        &mut self,
        permission: &PendingPermission,
        answering: bool,
        keys: &crate::agent::FeedKeys,
    ) {
        let theme = self.theme;
        let warning = theme.component.warning.add_modifier(Modifier::BOLD);
        self.push(vec![Span::styled(
            format!("? Use {}?", permission.tool),
            warning,
        )]);
        if !permission.summary.is_empty() {
            self.push(vec![
                Span::raw("  "),
                Span::styled(permission.summary.clone(), theme.text),
            ]);
        }
        if let Some(detail) = &permission.detail {
            self.push(vec![
                Span::raw("  "),
                Span::styled(detail.clone(), theme.muted),
            ]);
        }
        if permission.more > 0 {
            self.push(vec![
                Span::raw("  "),
                Span::styled(
                    format!(
                        "+{} more request{}",
                        permission.more,
                        if permission.more == 1 { "" } else { "s" }
                    ),
                    theme.tiers.dim,
                ),
            ]);
        }
        if answering {
            self.push(vec![Span::styled("answering…", theme.muted)]);
            return;
        }
        let mut buttons = vec![(
            labelled("Allow", &keys.allow),
            ItemAction::Agent(AgentAction::Allow),
        )];
        if permission.can_remember {
            buttons.push((
                labelled("Always", &keys.always),
                ItemAction::Agent(AgentAction::Always),
            ));
        }
        buttons.push((
            labelled("Decline", &keys.decline),
            ItemAction::Agent(AgentAction::Decline),
        ));
        if permission.can_remember {
            buttons.push((
                labelled("Never", &keys.never),
                ItemAction::Agent(AgentAction::Never),
            ));
        }
        self.buttons(&buttons);
    }
}

/// The key opened items of the review assistant's session `session` are kept under in
/// [`Folds::feed_open`] (with [`open_key`]).
#[must_use]
pub fn dock_key(session: &str) -> String {
    format!("assistant:{session}")
}

/// Rows of the review assistant's conversation with its session.
///
/// Every observed turn (the question as `▍you`, then reasoning, tool lines and answers as on an
/// AI card), the pending permission prompt, and a status line while the session works. Toggles
/// carry `turn index * MAX_TURN_ITEMS + item index`. A turn writing `guide` (the guide on
/// screen) shows its card in place of its guide tool lines.
#[must_use]
pub fn dock_rows(
    session: &str,
    agents: &AgentStreams,
    folds: &Folds,
    theme: &ReviewTheme,
    width: usize,
    (draft, guide): (bool, Option<&GuideCard>),
) -> (Vec<Line>, Vec<ActionCells<ItemAction>>) {
    let mut card = Card {
        theme,
        width,
        pad: String::new(),
        body_width: width,
        lines: Vec::new(),
        actions: Vec::new(),
    };
    let Some(state) = agents.state(session) else {
        card.push(vec![Span::styled("connecting…", theme.muted)]);
        return (card.lines, card.actions);
    };
    let key = dock_key(session);
    for (index, turn) in state.feed.turns.iter().enumerate() {
        if index > 0 {
            card.lines.push(Line::default());
        }
        if !turn.question.is_empty() {
            card.question(&turn.question);
        }
        let guide = guide.filter(|guide| writes_guide(turn, guide, session));
        card.turn(
            turn,
            &key,
            folds,
            index * crate::agent::feed::MAX_TURN_ITEMS,
            guide,
        );
    }
    if let Some(permission) = &state.feed.permission {
        card.lines.push(Line::default());
        card.permission(
            permission,
            agents.is_answering(&permission.permission_id),
            agents.keys(),
        );
    }
    if !state.working && state.feed.permission.is_none() && !state.answer.trim().is_empty() {
        let agent = ItemAction::Agent;
        let mut buttons = vec![("Copy".to_owned(), agent(AgentAction::Copy))];
        if draft {
            buttons.push(("Draft comment".to_owned(), agent(AgentAction::Draft)));
        }
        card.buttons(&buttons);
    }
    let clock = agents.clock(session);
    if state.working && state.feed.permission.is_none() {
        let spinner = Spinner::named(theme.icons.spinner).unwrap_or_else(Spinner::braille);
        let elapsed = DEFAULT_INTERVAL.saturating_mul(u32::try_from(clock.step % 64).unwrap_or(0));
        let mut spans = vec![
            spinner.span(elapsed, theme.accent),
            Span::raw(" "),
            Span::styled(
                if state.cancelling {
                    "stopping…"
                } else {
                    "working…"
                },
                theme.badge_draft,
            ),
        ];
        if clock.seconds > 0 {
            spans.push(Span::styled(theme.icons.separator, theme.tiers.dim));
            spans.push(Span::styled(format!("{}s", clock.seconds), theme.muted));
        }
        card.lines.push(Line::default());
        card.push(spans);
    } else if state.phase == AgentPhase::Failed && state.feed.turns.is_empty() {
        card.alert(&state.status);
    }
    (card.lines, card.actions)
}

/// A button label with its key, when it has one.
fn labelled(label: &str, key: &str) -> String {
    if key.is_empty() {
        label.to_owned()
    } else {
        format!("{label} {key}")
    }
}

/// A body builder whose lines are `width` cells (the card's inner width).
const fn body(theme: &ReviewTheme, width: usize) -> Card<'_> {
    Card {
        theme,
        width,
        pad: String::new(),
        body_width: width,
        lines: Vec::new(),
        actions: Vec::new(),
    }
}

/// An AI card's header chips: the lead (chevron, `✦ Bcode`), what is new, then `chips`.
fn title(chips: Vec<Vec<Span>>, look: &Look, theme: &ReviewTheme) -> Vec<Vec<Span>> {
    let mut title = vec![vec![
        look.chevron(theme),
        Span::raw(" "),
        Span::styled(theme.icons.ai, theme.badge_ai),
        Span::raw(" "),
        Span::styled("Bcode", theme.badge_ai),
    ]];
    title.extend(look.new_chip(theme));
    title.extend(chips);
    title
}

/// `card` as a section of the frame, when it has lines.
fn section(card: Card<'_>, sections: &mut Vec<thread_card::Section>) {
    if !card.lines.is_empty() {
        sections.push(thread_card::Section {
            lines: card.lines,
            actions: card.actions,
        });
    }
}

/// Rows of an AI card: a card on the threads' frame (see [`thread_card`]), its sections the
/// question, the turn, a permission prompt, and suggestions.
pub(super) fn agent_rows(
    agent: &bcode_code_review::document::AgentThreadItem,
    notes: &crate::notes::ReviewNotes,
    agents: &AgentStreams,
    folds: &Folds,
    layout: thread_card::Layout,
    theme: &ReviewTheme,
) -> ItemRows {
    let inner = layout.inner();
    let exchange = agent
        .exchange_id
        .as_deref()
        .and_then(|id| notes.exchange(id));
    let session = exchange.and_then(|exchange| exchange.session_id.as_deref());
    let state = session.and_then(|session| agents.state(session));
    let live = state.zip(session.map(|session| agents.clock(session)));
    let (chips, failed, needs_you) =
        header(agent, exchange.map(|exchange| exchange.status), live, theme);
    let look = Look::of(&thread_card::ai_key(&agent.id), folds, theme);
    let title = title(chips, &look, theme);
    let mut sections = Vec::new();
    if let Some(exchange) = exchange {
        let mut question = body(theme, inner);
        question.question(&exchange.question);
        section(question, &mut sections);
    }
    let mut turn = body(theme, inner);
    let active = exchange
        .is_some_and(|exchange| turn.exchange(exchange, state, is_newest(exchange, notes), folds));
    section(turn, &mut sections);
    let permission = state
        .filter(|_| active)
        .and_then(|state| state.feed.permission.as_ref());
    if let Some(permission) = permission {
        let mut prompt = body(theme, inner);
        prompt.permission(
            permission,
            agents.is_answering(&permission.permission_id),
            agents.keys(),
        );
        section(prompt, &mut sections);
    }
    let (lines, actions) =
        suggestion_rows(&agent.suggestion_ids, notes, theme, 0, inner, 0, |action| {
            ItemAction::Agent(match action {
                SuggestionAction::Accept => AgentAction::Accept,
                SuggestionAction::Refine => AgentAction::Refine,
                SuggestionAction::Reject => AgentAction::Reject,
            })
        });
    if !lines.is_empty() {
        sections.push(thread_card::Section { lines, actions });
    }
    let working = active && state.is_some_and(|state| state.working);
    let answered = exchange
        .zip(state)
        .and_then(|(exchange, state)| exchange_turn(exchange, state, active))
        .is_some_and(|(turn, _)| !turn.answer().is_empty());
    let mode = if permission.is_some() {
        CardMode::Prompting
    } else if working {
        CardMode::Working
    } else {
        CardMode::Idle {
            answered,
            retry: failed,
        }
    };
    let owned = card_buttons(&CardButtons {
        keys: agents.keys(),
        model: session.and_then(|session| agents.model_label(session)),
        linked: session.is_some(),
        exchange: exchange.is_some(),
        mode,
    });
    let buttons: Vec<(&str, ItemAction)> = owned
        .iter()
        .map(|(label, action)| (label.as_str(), *action))
        .collect();
    let border = if failed {
        theme.error
    } else if needs_you {
        theme.threads.waiting
    } else {
        look.border(false, false, theme)
    };
    thread_card::Card {
        title,
        viewed: exchange.map(|_| look.checkbox.clone()),
        sections,
        buttons,
        footer: line_reference(agent.lines, theme),
        border,
        folded: look.folded,
    }
    .rows(layout, theme)
}

/// What an AI card offers besides opening its session.
#[derive(Clone, Copy)]
enum CardMode {
    /// A permission prompt shows; its answers are the actions.
    Prompting,
    /// The turn runs; it can be stopped.
    Working,
    /// Nothing runs.
    Idle {
        /// The turn has a finished answer.
        answered: bool,
        /// Following the session stopped and can be retried.
        retry: bool,
    },
}

/// What decides an AI card's buttons.
struct CardButtons<'a> {
    keys: &'a crate::agent::FeedKeys,
    /// The session's `model · effort`, once known.
    model: Option<&'a str>,
    linked: bool,
    exchange: bool,
    mode: CardMode,
}

/// An AI card's buttons: Open in Bcode, then Stop while the turn runs, nothing more while a
/// permission prompt shows, else the answer's actions.
fn card_buttons(card: &CardButtons<'_>) -> Vec<(String, ItemAction)> {
    let agent = ItemAction::Agent;
    let mut buttons = Vec::new();
    if card.linked {
        buttons.push((
            labelled("Open in Bcode", &card.keys.open),
            agent(AgentAction::Open),
        ));
        if let Some(model) = card.model {
            let model =
                Line::from_spans(vec![Span::raw(model.to_owned())]).truncate(MODEL_LABEL_CELLS);
            let model: String = model
                .spans
                .iter()
                .map(|span| span.content.as_str())
                .collect();
            buttons.push((format!("● {model}"), agent(AgentAction::Model)));
        }
    }
    let (answered, retry) = match card.mode {
        CardMode::Prompting => return buttons,
        CardMode::Working => {
            buttons.push((labelled("Stop", &card.keys.stop), agent(AgentAction::Stop)));
            return buttons;
        }
        CardMode::Idle { answered, retry } => (answered, retry),
    };
    if card.linked {
        buttons.push(("Follow up".to_owned(), agent(AgentAction::FollowUp)));
    }
    if answered {
        buttons.push(("Copy".to_owned(), agent(AgentAction::Copy)));
        buttons.push(("Suggest comment".to_owned(), agent(AgentAction::Suggest)));
        buttons.push(("Draft".to_owned(), agent(AgentAction::Draft)));
    }
    if retry && card.linked {
        buttons.push(("Retry".to_owned(), agent(AgentAction::Retry)));
    }
    if card.exchange {
        buttons.push(("Delete".to_owned(), agent(AgentAction::Delete)));
    }
    buttons
}

#[cfg(test)]
mod tests {
    use super::guide_id_of;

    #[test]
    fn a_guide_id_is_read_from_whole_or_cut_arguments() {
        assert_eq!(
            guide_id_of(r#"{"guide_id":"g-1","ops":[]}"#).as_deref(),
            Some("g-1")
        );
        // Arguments are kept bounded; a cut call still names its guide before the cut.
        assert_eq!(
            guide_id_of(r#"{"guide_id": "g-2", "ops":[{"op":"put_section","body":"long…"#)
                .as_deref(),
            Some("g-2")
        );
        assert_eq!(guide_id_of(r#"{"title":"x"}"#), None);
        assert_eq!(guide_id_of(r#"{"guide_id":"#), None);
        assert_eq!(guide_id_of(r#"{"guide_id":"unterminated"#), None);
        assert_eq!(guide_id_of(r#"{"guide_id":""}"#), None);
    }
}
