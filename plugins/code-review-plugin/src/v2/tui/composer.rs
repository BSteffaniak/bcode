//! The inline comment composer (spec 6.2, inventory A.7).
//!
//! The composer is the document's `Composer` item, shown under its anchor. Its editor is a BMUX
//! `TextInputComponent` over a retained `TextInputState` with the shared chat-composer editing
//! policy (`enter` submits, `shift+enter` inserts a newline, readline editing keys), the same
//! generic pieces the workflow prose editor composes. A markdown preview replaces the editor
//! while toggled.
//!
//! [`Composer::layout`] is the single geometry of the item: its height, the editor's rows, and
//! the clickable action buttons. Measuring, painting, and click routing all read it.

use super::rows::ActionCells;
use super::theme::ReviewTheme;
use crate::v2::document::{AnchorTarget, DocAnchor};
use bcode_code_review_models::{ReviewThreadKind, ReviewThreadSeverity};
use bmux_keyboard::KeyStroke;
use bmux_tui::component::{Component, Constraints, LayoutCx, LayoutNode};
use bmux_tui::text::{Line, Span};
use bmux_tui::text_width::display_width;
use bmux_tui_components::text_input::{
    TextInputComponent, TextInputControl, TextInputOutcome, TextInputPolicy, TextInputState,
};
use std::cell::RefCell;

/// Layout id of the composer's editor.
pub const EDITOR_ID: &str = "review.composer.editor";
/// Editor rows shown at least and at most.
const EDITOR_MIN_ROWS: u16 = 3;
const EDITOR_MAX_ROWS: u16 = 8;
/// Preview rows shown at most.
const PREVIEW_MAX_ROWS: usize = 12;

/// What submitting the composer does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerMode {
    /// A new thread at the anchor.
    Comment,
    /// A reply to a thread.
    Reply {
        /// Thread id.
        thread_id: String,
    },
    /// Edit an existing comment.
    Edit {
        /// Comment id.
        comment_id: String,
    },
    /// Ask Bcode about the anchor.
    Ask,
}

/// A clickable composer button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerButton {
    /// Submit (save, reply, update, or ask).
    Submit,
    /// Switch between saving a comment and asking Bcode.
    Switch,
    /// Toggle the markdown preview.
    Preview,
    /// Cancel.
    Cancel,
}

/// Outcome of a key or paste in the composer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerInput {
    /// Not handled.
    Ignored,
    /// The text or cursor changed.
    Edited,
    /// Submit was requested.
    Submit,
}

/// Rows of the composer at one width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerLayout {
    /// Total rows.
    pub height: u64,
    /// Title row.
    pub title: Line,
    /// Cells before the editor and action rows (aligned with diff content).
    pub indent: u16,
    /// Editor rows (row 1 onward); 0 in preview.
    pub editor_rows: u16,
    /// Preview rows, in preview mode.
    pub preview: Vec<Line>,
    /// Action row (last row).
    pub actions: Line,
    /// Clickable buttons on the action row.
    pub buttons: Vec<ActionCells<ComposerButton>>,
}

/// The open composer.
#[derive(Debug)]
pub struct Composer {
    /// Anchor shown at.
    pub anchor: DocAnchor,
    /// Submit behavior.
    pub mode: ComposerMode,
    /// Editor state.
    pub input: RefCell<TextInputState>,
    /// Markdown preview instead of the editor.
    pub preview: bool,
    /// Kind of a new thread.
    pub kind: ReviewThreadKind,
    /// Severity of a new thread.
    pub severity: ReviewThreadSeverity,
    policy: TextInputPolicy,
    revision: u64,
}

impl Composer {
    /// A composer at `anchor` starting with `text`.
    #[must_use]
    pub fn new(anchor: DocAnchor, mode: ComposerMode, text: &str) -> Self {
        let mut policy = TextInputPolicy::chat_composer();
        policy.viewport.min_rows = EDITOR_MIN_ROWS;
        policy.viewport.max_rows = Some(EDITOR_MAX_ROWS);
        policy.edge = bmux_tui_components::text_input::EdgePolicy::disabled();
        Self {
            anchor,
            mode,
            input: RefCell::new(TextInputState::new(
                bmux_text_edit::TextEditBuffer::from_text(text),
            )),
            preview: false,
            kind: ReviewThreadKind::Note,
            severity: ReviewThreadSeverity::Info,
            policy,
            revision: 0,
        }
    }

    /// Editing policy of the editor.
    #[must_use]
    pub const fn policy(&self) -> &TextInputPolicy {
        &self.policy
    }

    /// Changes whenever the composer's rows can change.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Note a change of the composer's presentation.
    pub const fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// Current text.
    #[must_use]
    pub fn text(&self) -> String {
        self.input.borrow().buffer().text().to_string()
    }

    /// Handle a key in the editor.
    pub fn key(&mut self, stroke: KeyStroke) -> ComposerInput {
        if self.preview {
            return ComposerInput::Ignored;
        }
        let outcome =
            TextInputControl::new(&self.policy).handle_key(&mut self.input.borrow_mut(), stroke);
        self.outcome(outcome)
    }

    /// Insert pasted text.
    pub fn paste(&mut self, text: &str) -> ComposerInput {
        if self.preview {
            return ComposerInput::Ignored;
        }
        let outcome =
            TextInputControl::new(&self.policy).handle_paste(&mut self.input.borrow_mut(), text);
        self.outcome(outcome)
    }

    const fn outcome(&mut self, outcome: TextInputOutcome) -> ComposerInput {
        match outcome {
            TextInputOutcome::Submitted => ComposerInput::Submit,
            TextInputOutcome::Ignored => ComposerInput::Ignored,
            _ => {
                self.touch();
                ComposerInput::Edited
            }
        }
    }

    /// Switch between commenting and asking Bcode (only for new threads).
    pub fn switch(&mut self) {
        self.mode = match self.mode {
            ComposerMode::Comment => ComposerMode::Ask,
            ComposerMode::Ask => ComposerMode::Comment,
            ref other => other.clone(),
        };
        self.touch();
    }

    /// The editor component (shared by measuring, painting, and events).
    #[must_use]
    pub fn editor<'a>(&'a self, theme: &ReviewTheme) -> TextInputComponent<'a, 'a> {
        TextInputComponent::new(EDITOR_ID, &self.input, &self.policy)
            .style(theme.input)
            .placeholder(self.placeholder(), theme.placeholder)
            .focused(true)
    }

    const fn placeholder(&self) -> &'static str {
        match self.mode {
            ComposerMode::Ask => "Ask Bcode about this code…",
            ComposerMode::Reply { .. } => "Write a reply (markdown)…",
            _ => "Write a comment (markdown)…",
        }
    }

    fn title(&self, theme: &ReviewTheme) -> Line {
        let icons = &theme.icons;
        let (icon, style, verb) = match self.mode {
            ComposerMode::Comment => (icons.draft, theme.badge_draft, "Comment on"),
            ComposerMode::Reply { .. } => (icons.thread, theme.badge_open, "Reply on"),
            ComposerMode::Edit { .. } => (icons.draft, theme.badge_draft, "Edit comment on"),
            ComposerMode::Ask => (icons.ai, theme.badge_ai, "Ask Bcode about"),
        };
        let mut spans = vec![
            Span::styled(icon, style),
            Span::raw(" "),
            Span::styled(verb, theme.title),
            Span::raw(" "),
            Span::styled(anchor_label(&self.anchor), theme.path),
        ];
        if self.mode == ComposerMode::Comment {
            spans.push(Span::styled(icons.separator, theme.muted));
            spans.push(Span::styled(
                super::notes::kind_label(self.kind),
                theme.muted,
            ));
            spans.push(Span::styled(icons.separator, theme.muted));
            spans.push(Span::styled(
                super::notes::severity_label(self.severity),
                theme.muted,
            ));
        }
        if self.preview {
            spans.push(Span::styled(icons.separator, theme.muted));
            spans.push(Span::styled("preview", theme.accent));
        }
        Line::from_spans(spans)
    }

    /// Geometry of the composer at `width`, with diff-aligned `indent` (bounded so the editor
    /// keeps at least half the row).
    #[must_use]
    pub fn layout(&self, width: u16, indent: usize, theme: &ReviewTheme) -> ComposerLayout {
        let indent = u16::try_from(indent.min(usize::from(width) / 2)).unwrap_or(0);
        let inner = width.saturating_sub(indent);
        let pad = Span::raw(" ".repeat(usize::from(indent)));
        let title = Line::from_spans(
            std::iter::once(pad.clone())
                .chain(self.title(theme).spans.iter().cloned())
                .collect::<Vec<_>>(),
        )
        .truncate(usize::from(width));
        let (editor_rows, preview) = if self.preview {
            let text = self.text();
            let mut lines = if text.trim().is_empty() {
                vec![Line::from_spans(vec![Span::styled(
                    "Nothing to preview",
                    theme.muted,
                )])]
            } else {
                bcode_markdown_render::render_markdown(
                    &text,
                    &bcode_markdown_render::MarkdownRenderOptions::new(inner.max(1)),
                )
                .lines
            };
            lines.truncate(PREVIEW_MAX_ROWS);
            let lines = lines
                .into_iter()
                .map(|line| {
                    Line::from_spans(
                        std::iter::once(pad.clone())
                            .chain(line.spans.iter().cloned())
                            .collect::<Vec<_>>(),
                    )
                    .truncate(usize::from(width))
                })
                .collect();
            (0, lines)
        } else if inner == 0 {
            (EDITOR_MIN_ROWS, Vec::new())
        } else {
            let node: LayoutNode = self.editor(theme).layout(
                Constraints::for_width(u64::from(inner)),
                &mut LayoutCx::new(),
            );
            (
                u16::try_from(node.size.height)
                    .unwrap_or(EDITOR_MAX_ROWS)
                    .clamp(EDITOR_MIN_ROWS, EDITOR_MAX_ROWS),
                Vec::new(),
            )
        };
        let (actions, buttons) = self.actions(theme, width, indent);
        let body = if self.preview {
            preview.len() as u64
        } else {
            u64::from(editor_rows)
        };
        ComposerLayout {
            height: 2 + body,
            title,
            indent,
            editor_rows,
            preview,
            actions,
            buttons,
        }
    }

    /// The action row: buttons first (kept whole or dropped from the end), then hints.
    fn actions(
        &self,
        theme: &ReviewTheme,
        width: u16,
        indent: u16,
    ) -> (Line, Vec<ActionCells<ComposerButton>>) {
        let submit = match self.mode {
            ComposerMode::Comment => " Save draft ",
            ComposerMode::Reply { .. } => " Reply ",
            ComposerMode::Edit { .. } => " Save edit ",
            ComposerMode::Ask => " Ask Bcode ",
        };
        let mut parts: Vec<(String, Option<ComposerButton>)> =
            vec![(submit.to_owned(), Some(ComposerButton::Submit))];
        let switch = match self.mode {
            ComposerMode::Comment => Some(" Ask Bcode instead "),
            ComposerMode::Ask => Some(" Comment instead "),
            _ => None,
        };
        if let Some(switch) = switch {
            parts.push((switch.to_owned(), Some(ComposerButton::Switch)));
        }
        parts.push((
            if self.preview { " Edit " } else { " Preview " }.to_owned(),
            Some(ComposerButton::Preview),
        ));
        parts.push((" Cancel ".to_owned(), Some(ComposerButton::Cancel)));
        let width = usize::from(width);
        let mut x = usize::from(indent);
        let mut spans = vec![Span::raw(" ".repeat(x.min(width)))];
        let mut buttons = Vec::new();
        for (text, button) in parts {
            let cells = display_width(&text);
            if x + cells > width {
                break;
            }
            let style = if button == Some(ComposerButton::Submit) {
                theme.accent.patch(theme.selection)
            } else {
                theme.gap_action
            };
            spans.push(Span::styled(text, style));
            if let Some(button) = button {
                buttons.push(ActionCells {
                    row: 0,
                    cells: u16::try_from(x).unwrap_or(u16::MAX)
                        ..u16::try_from(x + cells).unwrap_or(u16::MAX),
                    action: button,
                });
            }
            x += cells;
            spans.push(Span::raw(" "));
            x += 1;
        }
        (Line::from_spans(spans).truncate(width), buttons)
    }
}

/// `path:12-14`, `path`, or `review` for an anchor.
#[must_use]
pub fn anchor_label(anchor: &DocAnchor) -> String {
    let path = anchor
        .file
        .as_ref()
        .map_or_else(|| "the review".to_owned(), |file| file.path.clone());
    match &anchor.target {
        AnchorTarget::Lines(lines) => {
            let (start, end) = (lines.start_line(), lines.end_line());
            if start == end {
                format!("{path}:{end}")
            } else {
                format!("{path}:{start}-{end}")
            }
        }
        AnchorTarget::File | AnchorTarget::Review => path,
    }
}
