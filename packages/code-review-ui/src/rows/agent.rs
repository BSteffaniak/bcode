//! Rows of an AI card: a live, abbreviated feed of the question's turn in its linked session.
//!
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

use super::{
    ActionCells, AgentAction, Folds, ItemAction, ItemRows, MAX_ANSWER_ROWS,
    MAX_EXPANDED_ANSWER_ROWS, SuggestionAction, action_row, indented, markdown_rows,
    outdated_label, suggestion_rows,
};
use crate::agent::feed::{FeedItem, PendingPermission, TOOL_TAIL_LINES, ToolLine, ToolState, Turn};
use crate::agent::{AgentPhase, AgentState, AgentStreams};
use crate::theme::ReviewTheme;
use bcode_code_review_models::{ReviewAiExchange, ReviewAiExchangeStatus};
use bmux_tui::style::Modifier;
use bmux_tui::text::{Line, Span};
use bmux_tui::text_width::display_width;
use bmux_tui_components::spinner::{DEFAULT_INTERVAL, Spinner};

/// Lines of the question shown under `▍you`.
const QUESTION_ROWS: usize = 3;
/// Rows of open reasoning.
const REASONING_ROWS: usize = 40;
/// Lines of one error.
const ERROR_ROWS: usize = 3;

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

/// Header of an AI card: icon, spinner and status, usage, and an outdated label. Returns
/// whether the question or the observation of its session failed (a retry applies).
fn header(
    agent: &bcode_code_review::document::AgentThreadItem,
    status: Option<ReviewAiExchangeStatus>,
    live: Option<(&AgentState, crate::agent::LiveClock)>,
    theme: &ReviewTheme,
    indent: usize,
) -> (Vec<Span>, bool) {
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
    let mut spans = vec![
        Span::raw(" ".repeat(indent)),
        Span::styled(icons.ai, theme.badge_ai),
        Span::raw(" "),
        Span::styled("Bcode", theme.badge_ai),
        Span::raw(" "),
    ];
    let spinning = matches!(status, Some(ReviewAiExchangeStatus::Pending))
        || live.is_some_and(|(state, _)| state.working && !state.needs_you());
    if spinning {
        let spinner = Spinner::named(icons.spinner).unwrap_or_else(Spinner::braille);
        let step = live.map_or(0, |(_, clock)| clock.step);
        let elapsed = DEFAULT_INTERVAL.saturating_mul(u32::try_from(step % 64).unwrap_or(0));
        spans.push(spinner.span(elapsed, theme.accent));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(label, style));
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
        for part in usage {
            spans.push(Span::styled(icons.separator, theme.tiers.dim));
            spans.push(Span::styled(part, theme.muted));
        }
    }
    if let bcode_code_review::document::AnchorPlacement::Outdated(reason) = agent.placement {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(outdated_label(reason), theme.badge_outdated));
    }
    (spans, failed)
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

    fn question(&mut self, question: &str) {
        let theme = self.theme;
        let bar = theme.icons.accent_bar;
        self.push(vec![
            Span::styled(bar, theme.accent),
            Span::styled("you", theme.accent.add_modifier(Modifier::BOLD)),
        ]);
        let lines: Vec<&str> = question
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        for line in lines.iter().take(QUESTION_ROWS) {
            self.push(vec![
                Span::styled(bar, theme.accent),
                Span::raw(" "),
                Span::styled((*line).to_owned(), theme.tiers.strong),
            ]);
        }
        if lines.len() > QUESTION_ROWS {
            self.push(vec![
                Span::styled(bar, theme.accent),
                Span::raw(" "),
                Span::styled(theme.icons.ellipsis, theme.muted),
            ]);
        }
    }

    /// The items of `turn`; opened items are keyed by `exchange_id`, and the toggles of item `i`
    /// carry `base + i`.
    fn turn(&mut self, turn: &Turn, exchange_id: &str, folds: &Folds, base: usize) {
        let theme = self.theme;
        if turn.earlier > 0 {
            self.push(vec![Span::styled(
                format!("+{} earlier", turn.earlier),
                theme.tiers.dim,
            )]);
        }
        let answers = turn
            .items
            .iter()
            .filter(|item| matches!(item, FeedItem::Answer { .. }))
            .count();
        let mut answer = 0;
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
                        let rows = markdown_rows(
                            text,
                            self.body_width.saturating_sub(2),
                            REASONING_ROWS,
                            false,
                            theme,
                        );
                        self.extend_indented(rows, 2);
                    }
                }
                FeedItem::Tool(tool) => self.tool(tool, open, toggle),
                FeedItem::Answer { text, streaming } => {
                    answer += 1;
                    // Earlier answers of the turn (between tool calls) are kept short; the
                    // last one is the answer.
                    let last = answer == answers;
                    let expanded = folds.expanded.contains(exchange_id);
                    let max = if !last {
                        MAX_ANSWER_ROWS / 2
                    } else if expanded {
                        MAX_EXPANDED_ANSWER_ROWS
                    } else {
                        MAX_ANSWER_ROWS
                    };
                    let rows = markdown_rows(text, self.body_width, max, *streaming && last, theme);
                    self.extend_indented(rows, 0);
                }
                FeedItem::Notice(text) => {
                    self.push(vec![Span::styled(
                        format!("· {}", text.lines().next().unwrap_or_default()),
                        theme.tiers.dim,
                    )]);
                }
                FeedItem::Error(text) => {
                    for line in text
                        .lines()
                        .filter(|line| !line.trim().is_empty())
                        .take(ERROR_ROWS)
                    {
                        self.push(vec![
                            Span::styled(theme.icons.alert, theme.error),
                            Span::raw(" "),
                            Span::styled(line.to_owned(), theme.error),
                        ]);
                    }
                }
            }
        }
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
        self.question(&exchange.question);
        if let Some(error) = &exchange.error {
            self.alert(error.lines().next().unwrap_or_default());
        }
        if let Some((turn, newest)) =
            state.and_then(|state| exchange_turn(exchange, state, newest_exchange))
        {
            self.turn(turn, &exchange.exchange_id, folds, 0);
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
/// carry `turn index * MAX_TURN_ITEMS + item index`.
#[must_use]
pub fn dock_rows(
    session: &str,
    agents: &AgentStreams,
    folds: &Folds,
    theme: &ReviewTheme,
    width: usize,
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
        card.turn(
            turn,
            &key,
            folds,
            index * crate::agent::feed::MAX_TURN_ITEMS,
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

/// Rows of an AI card.
pub(super) fn agent_rows(
    agent: &bcode_code_review::document::AgentThreadItem,
    notes: &crate::notes::ReviewNotes,
    agents: &AgentStreams,
    folds: &Folds,
    theme: &ReviewTheme,
    width: usize,
    indent: usize,
) -> ItemRows {
    let exchange = agent
        .exchange_id
        .as_deref()
        .and_then(|id| notes.exchange(id));
    let session = exchange.and_then(|exchange| exchange.session_id.as_deref());
    let state = session.and_then(|session| agents.state(session));
    let live = state.zip(session.map(|session| agents.clock(session)));
    let (header, failed) = header(
        agent,
        exchange.map(|exchange| exchange.status),
        live,
        theme,
        indent,
    );
    let body_indent = (indent + 2).min(width);
    let mut card = Card {
        theme,
        width,
        pad: " ".repeat(body_indent),
        body_width: width.saturating_sub(body_indent),
        lines: vec![Line::from_spans(header).truncate(width)],
        actions: Vec::new(),
    };
    let active = exchange
        .is_some_and(|exchange| card.exchange(exchange, state, is_newest(exchange, notes), folds));
    let permission = state
        .filter(|_| active)
        .and_then(|state| state.feed.permission.as_ref());
    if let Some(permission) = permission {
        card.permission(
            permission,
            agents.is_answering(&permission.permission_id),
            agents.keys(),
        );
    }
    let row = card.row();
    let (suggestion_lines, suggestion_actions) = suggestion_rows(
        &agent.suggestion_ids,
        notes,
        theme,
        body_indent,
        width,
        row,
        |action| {
            ItemAction::Agent(match action {
                SuggestionAction::Accept => AgentAction::Accept,
                SuggestionAction::Refine => AgentAction::Refine,
                SuggestionAction::Reject => AgentAction::Reject,
            })
        },
    );
    card.lines.extend(suggestion_lines);
    card.actions.extend(suggestion_actions);
    let working = active && state.is_some_and(|state| state.working);
    let answered = exchange
        .zip(state)
        .and_then(|(exchange, state)| exchange_turn(exchange, state, active))
        .is_some_and(|(turn, _)| !turn.answer().is_empty());
    let expanded = agent
        .exchange_id
        .as_ref()
        .is_some_and(|id| folds.expanded.contains(id));
    let mode = if permission.is_some() {
        CardMode::Prompting
    } else if working {
        CardMode::Working
    } else {
        CardMode::Idle {
            answered,
            expanded,
            retry: failed,
        }
    };
    let buttons = card_buttons(&CardButtons {
        keys: agents.keys(),
        linked: session.is_some(),
        exchange: exchange.is_some(),
        mode,
    });
    card.buttons(&buttons);
    ItemRows::lines(card.lines, card.actions)
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
        /// The whole answer shows.
        expanded: bool,
        /// Following the session stopped and can be retried.
        retry: bool,
    },
}

/// What decides an AI card's buttons.
struct CardButtons<'a> {
    keys: &'a crate::agent::FeedKeys,
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
    }
    let (answered, expanded, retry) = match card.mode {
        CardMode::Prompting => return buttons,
        CardMode::Working => {
            buttons.push((labelled("Stop", &card.keys.stop), agent(AgentAction::Stop)));
            return buttons;
        }
        CardMode::Idle {
            answered,
            expanded,
            retry,
        } => (answered, expanded, retry),
    };
    if card.linked {
        buttons.push(("Follow up".to_owned(), agent(AgentAction::FollowUp)));
    }
    if answered {
        let more = if expanded { "Less" } else { "More" };
        buttons.push((more.to_owned(), agent(AgentAction::Expand)));
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
