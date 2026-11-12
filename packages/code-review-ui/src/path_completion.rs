//! `@path` completion in the comment composer.
//!
//! Typing `@` followed by a query at the editor's cursor offers repository paths that match it
//! (tracked and untracked, ignoring ignored files) from the data layer's file search. The
//! suggestions show inside the composer, between the editor and its action row, so they share
//! the composer's single layout. The interaction is BMUX's completion controller
//! ([`CompletionState`], [`CompletionInteraction`]) over a [`SelectableListComponent`]:
//! `up`/`down` (or `ctrl+p`/`ctrl+n`) move, `tab`/`enter` accept, `esc` dismisses, and clicking a
//! suggestion accepts it. Accepting replaces the whole `@word` under the cursor with
//! `@<path>` as one undoable edit. Searches are tagged with a generation so a late answer for an
//! older query is ignored.

use std::ops::Range;

use bcode_code_review_models::ReviewFilesSearchResponse;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::TextEditBuffer;
use bmux_tui::event::EventOutcome;
use bmux_tui::text::{Line, Span};
use bmux_tui_components::completion::{
    CompletionCommand, CompletionInteraction, CompletionState, CompletionStatus,
};
use bmux_tui_components::selectable_list::{
    SelectableListComponent, SelectableListItem, SelectableListStyles,
};

use crate::theme::ReviewTheme;

/// Layout id of the suggestion list.
pub const LIST_ID: &str = "review.composer.suggestions";
/// Suggestions requested and shown at most.
pub const MAX_SUGGESTIONS: u16 = 6;

/// The `@word` at a cursor: its byte range (including `@`) and the query after `@`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// Bytes of the whole word, from `@` to the next whitespace.
    pub range: Range<usize>,
    /// Text between `@` and the cursor.
    pub query: String,
}

/// The `@word` the cursor at byte `cursor` of `text` is in or right after, when the word starts
/// with `@` at the start of the text or after whitespace.
#[must_use]
pub fn token_at(text: &str, cursor: usize) -> Option<Token> {
    let cursor = cursor.min(text.len());
    if !text.is_char_boundary(cursor) {
        return None;
    }
    let start = text[..cursor]
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace())
        .map_or(0, |(index, character)| index + character.len_utf8());
    let word = &text[start..cursor];
    let query = word.strip_prefix('@')?;
    let end = text[cursor..]
        .char_indices()
        .find(|(_, character)| character.is_whitespace())
        .map_or(text.len(), |(index, _)| cursor + index);
    Some(Token {
        range: start..end,
        query: query.to_owned(),
    })
}

/// A search the surface should run for the composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    /// Path query.
    pub query: String,
    /// Generation the answer must carry.
    pub generation: u64,
}

/// Open `@path` completion.
#[derive(Debug)]
pub struct PathCompletion {
    token: Token,
    generation: u64,
    status: CompletionStatus,
    items: Vec<SelectableListItem>,
    total: u32,
    error: Option<String>,
    state: CompletionState,
}

impl PathCompletion {
    /// Completion of `token`, waiting for the search tagged `generation`.
    #[must_use]
    pub const fn new(token: Token, generation: u64) -> Self {
        let mut state = CompletionState::new();
        state.activate();
        Self {
            token,
            generation,
            status: CompletionStatus::Loading,
            items: Vec::new(),
            total: 0,
            error: None,
            state,
        }
    }

    /// The word being completed.
    #[must_use]
    pub const fn token(&self) -> &Token {
        &self.token
    }

    /// Follow the word to `token` when an edit moved it without changing its query.
    pub fn retarget(&mut self, token: Token) {
        self.token = token;
    }

    /// Apply the answer of the search tagged `generation`; answers of older searches are
    /// ignored. Returns whether it applied.
    pub fn answer(
        &mut self,
        generation: u64,
        result: Result<ReviewFilesSearchResponse, String>,
    ) -> bool {
        if generation != self.generation {
            return false;
        }
        match result {
            Ok(response) => {
                self.items = response
                    .files
                    .into_iter()
                    .take(usize::from(MAX_SUGGESTIONS))
                    .map(|file| SelectableListItem::new(file.path.clone(), file.path))
                    .collect();
                self.total = response.total_matches;
                self.error = None;
                self.status = if self.items.is_empty() {
                    CompletionStatus::Empty
                } else {
                    CompletionStatus::Candidates
                };
            }
            Err(error) => {
                self.items.clear();
                self.error = Some(error);
                self.status = CompletionStatus::Error;
            }
        }
        self.state.reconcile(&self.items);
        true
    }

    /// The path at suggestion `index`.
    #[must_use]
    pub fn path(&self, index: usize) -> Option<&str> {
        self.items.get(index).map(|item| item.id.as_str())
    }

    /// Whether suggestions can be accepted now.
    #[must_use]
    pub fn accepts(&self) -> bool {
        self.status == CompletionStatus::Candidates
    }

    /// Route `command`; returns the path to insert when it accepted one.
    pub fn command(&mut self, command: CompletionCommand) -> (EventOutcome, Option<String>) {
        let mut accepted = None;
        let outcome = CompletionInteraction::new(&self.items, self.status).handle_command(
            command,
            &mut self.state,
            |id| {
                accepted = Some(id.to_owned());
                true
            },
        );
        (outcome, accepted)
    }

    /// Whether the completion was dismissed.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.state.is_active()
    }

    /// Rows the suggestions take: a header and one row per suggestion.
    #[must_use]
    pub fn rows(&self) -> u16 {
        1 + u16::try_from(self.items.len()).unwrap_or(MAX_SUGGESTIONS)
    }

    /// The header row: what the suggestions are, or why there are none.
    #[must_use]
    pub fn header(&self, theme: &ReviewTheme) -> Line {
        let detail = match self.status {
            CompletionStatus::Loading => "searching…".to_owned(),
            CompletionStatus::Empty => format!("no paths match @{}", self.token.query),
            CompletionStatus::Error => self.error.clone().unwrap_or_default(),
            CompletionStatus::Candidates if self.total > u32::from(MAX_SUGGESTIONS) => {
                format!("{} of {}", self.items.len(), self.total)
            }
            CompletionStatus::Candidates => String::new(),
        };
        let mut spans = vec![Span::styled("Suggestions", theme.title)];
        if !detail.is_empty() {
            spans.push(Span::styled(theme.icons.separator, theme.muted));
            let style = if self.status == CompletionStatus::Error {
                theme.error
            } else {
                theme.muted
            };
            spans.push(Span::styled(detail, style));
        }
        Line::from_spans(spans)
    }

    /// The suggestion list component (shared by painting and tests).
    #[must_use]
    pub fn list<'a>(&'a self, theme: &ReviewTheme) -> SelectableListComponent<'a, 'a> {
        SelectableListComponent::new(LIST_ID, &self.items, self.state.list_state()).styles(
            SelectableListStyles {
                background: theme.input,
                normal: theme.path,
                focused: theme.path.patch(theme.selection),
                selected: theme.path.patch(theme.selection),
                ..SelectableListStyles::default()
            },
        )
    }
}

/// The completion command a key means while suggestions are open.
#[must_use]
pub const fn command_for(stroke: KeyStroke) -> Option<CompletionCommand> {
    let plain = stroke.modifiers.is_empty();
    let ctrl = stroke.modifiers.ctrl
        && !stroke.modifiers.alt
        && !stroke.modifiers.shift
        && !stroke.modifiers.super_key;
    match stroke.key {
        KeyCode::Up if plain => Some(CompletionCommand::Previous),
        KeyCode::Down if plain => Some(CompletionCommand::Next),
        KeyCode::Char('p') if ctrl => Some(CompletionCommand::Previous),
        KeyCode::Char('n') if ctrl => Some(CompletionCommand::Next),
        KeyCode::Tab | KeyCode::Enter if plain => Some(CompletionCommand::Accept),
        KeyCode::Escape if plain => Some(CompletionCommand::Dismiss),
        _ => None,
    }
}

/// Replace `token` in `buffer` with `@path`, leaving the cursor after it, as one undoable edit.
/// Returns whether the text changed.
pub fn insert(buffer: &mut TextEditBuffer, token: &Token, path: &str) -> bool {
    let replacement = format!("@{path}");
    let cursor = token.range.start + replacement.len();
    let state = buffer.edit_state();
    buffer
        .replace_byte_range(&state, token.range.clone(), &replacement, cursor, None)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_code_review_models::ReviewFileSearchEntry;

    fn response(paths: &[&str], total: u32) -> ReviewFilesSearchResponse {
        ReviewFilesSearchResponse {
            files: paths
                .iter()
                .map(|path| ReviewFileSearchEntry {
                    path: (*path).to_owned(),
                })
                .collect(),
            next_cursor: None,
            total_matches: total,
        }
    }

    #[test]
    fn tokens_start_at_an_at_sign_after_whitespace() {
        assert_eq!(
            token_at("see @src/ma", 11),
            Some(Token {
                range: 4..11,
                query: "src/ma".to_owned()
            })
        );
        // The whole word is replaced even with the cursor inside it.
        assert_eq!(
            token_at("@src/main.rs tail", 4),
            Some(Token {
                range: 0..12,
                query: "src".to_owned()
            })
        );
        assert_eq!(token_at("mail me@host", 12), None);
        assert_eq!(token_at("@done ", 6), None);
        assert_eq!(token_at("plain", 5), None);
        // Multi-byte text before the word keeps byte ranges on character boundaries.
        assert_eq!(
            token_at("日本 @ü", "日本 @ü".len()),
            Some(Token {
                range: 7..10,
                query: "ü".to_owned()
            })
        );
    }

    #[test]
    fn late_answers_are_ignored_and_accepting_replaces_the_word() {
        let mut buffer = TextEditBuffer::from_text("look at @comp now");
        let token = token_at(buffer.text(), 13).unwrap();
        let mut completion = PathCompletion::new(token.clone(), 2);
        assert!(!completion.accepts());
        // Enter while searching is consumed without accepting or submitting.
        assert_eq!(
            completion.command(CompletionCommand::Accept),
            (EventOutcome::Handled, None)
        );
        assert!(!completion.answer(1, Ok(response(&["stale.txt"], 1))));
        assert!(completion.answer(2, Ok(response(&["a/comp.rs", "completion-target.txt"], 9))));
        assert!(completion.accepts());
        assert_eq!(completion.rows(), 3);
        let (_, none) = completion.command(CompletionCommand::Next);
        assert_eq!(none, None);
        let (outcome, path) = completion.command(CompletionCommand::Accept);
        assert_eq!(outcome, EventOutcome::Redraw);
        let path = path.unwrap();
        assert_eq!(path, "completion-target.txt");
        assert!(!completion.is_active());
        assert!(insert(&mut buffer, &token, &path));
        assert_eq!(buffer.text(), "look at @completion-target.txt now");
        assert_eq!(
            buffer.cursor_byte_index(),
            "look at @completion-target.txt".len()
        );
    }

    #[test]
    fn empty_and_failed_searches_explain_themselves() {
        let theme = ReviewTheme::derive(None, crate::theme::IconStyle::Ascii);
        let text = |line: Line| {
            line.spans
                .iter()
                .map(|span| span.content.as_str())
                .collect::<String>()
        };
        let token = token_at("@zz", 3).unwrap();
        let mut completion = PathCompletion::new(token.clone(), 1);
        assert!(text(completion.header(&theme)).contains("searching"));
        completion.answer(1, Ok(response(&[], 0)));
        assert!(text(completion.header(&theme)).contains("no paths match @zz"));
        let mut failed = PathCompletion::new(token, 1);
        failed.answer(1, Err("not a git repository".to_owned()));
        assert!(text(failed.header(&theme)).contains("not a git repository"));
        assert_eq!(failed.rows(), 1);
    }

    #[test]
    fn keys_map_to_completion_commands() {
        let ctrl = bmux_keyboard::Modifiers {
            ctrl: true,
            ..bmux_keyboard::Modifiers::NONE
        };
        assert_eq!(
            command_for(KeyStroke::simple(KeyCode::Enter)),
            Some(CompletionCommand::Accept)
        );
        assert_eq!(
            command_for(KeyStroke::with_modifiers(KeyCode::Char('n'), ctrl)),
            Some(CompletionCommand::Next)
        );
        assert_eq!(command_for(KeyStroke::simple(KeyCode::Char('n'))), None);
    }
}
