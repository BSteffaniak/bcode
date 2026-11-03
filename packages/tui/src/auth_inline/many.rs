//! Multi-selection policy for the shared inline auth surface.

use super::{AuthChoiceOutcome, Choice, run};
use bmux_keyboard::KeyCode;
use bmux_tui::event::Event;
use std::{collections::BTreeSet, io};

/// Result of a multi-choice auth step. Back and cancellation authorize no effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMultiChoiceOutcome {
    /// Nonempty indices into the original labels, in source order.
    Selected(Vec<usize>),
    /// Return to the preceding step without committing the selection.
    Back,
    /// Abort the journey without committing the selection.
    Cancelled,
}

/// Select one or more auth choices without entering the alternate screen.
///
/// Space toggles the focused choice; Ctrl-A explicitly selects every source
/// choice, including choices hidden by the filter. Enter confirms only a
/// nonempty selection. Filtering preserves checked source indices. Esc goes
/// back, and Ctrl-C, Ctrl-D, or end of input cancels.
///
/// # Errors
/// Returns `InvalidInput` for empty labels, or terminal, input, and output errors.
/// The BMUX guard restores terminal state.
pub fn choose_many(
    title: &str,
    labels: &[String],
    config: &bcode_config::TuiConfig,
) -> io::Result<AuthMultiChoiceOutcome> {
    run(
        title,
        labels,
        config,
        new_choice(labels),
        update,
        AuthMultiChoiceOutcome::Cancelled,
    )
}

fn new_choice(labels: &[String]) -> Choice {
    Choice {
        checked: Some(BTreeSet::new()),
        ..Choice::new(labels)
    }
}

fn update(choice: &mut Choice, event: &Event, labels: &[String]) -> Option<AuthMultiChoiceOutcome> {
    if let Event::Key(key) = event {
        match key.key {
            KeyCode::Char('a') if key.modifiers.ctrl && !key.modifiers.alt => {
                choice.checked.as_mut()?.extend(0..labels.len());
                return None;
            }
            KeyCode::Char(' ') if !key.modifiers.ctrl && !key.modifiers.alt => {
                if let Some(&index) = choice.indices.get(choice.selected) {
                    let checked = choice.checked.as_mut()?;
                    if !checked.remove(&index) {
                        checked.insert(index);
                    }
                }
                return None;
            }
            KeyCode::Enter => {
                let checked = choice.checked.as_ref()?;
                return (!checked.is_empty())
                    .then(|| AuthMultiChoiceOutcome::Selected(checked.iter().copied().collect()));
            }
            _ => {}
        }
    }
    match choice.update(event, labels)? {
        AuthChoiceOutcome::Back => Some(AuthMultiChoiceOutcome::Back),
        AuthChoiceOutcome::Cancelled => Some(AuthMultiChoiceOutcome::Cancelled),
        AuthChoiceOutcome::Selected(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_keyboard::{KeyStroke, Modifiers};
    use bmux_tui::geometry::{Rect, Size};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyStroke::simple(code))
    }

    fn ctrl(character: char) -> Event {
        Event::Key(KeyStroke::with_modifiers(
            KeyCode::Char(character),
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        ))
    }

    #[test]
    fn toggles_are_explicit_and_empty_selection_cannot_continue() {
        let labels = vec!["one".to_owned(), "two".to_owned()];
        let mut choice = new_choice(&labels);
        assert_eq!(update(&mut choice, &key(KeyCode::Enter), &labels), None);
        update(&mut choice, &key(KeyCode::Down), &labels);
        assert_eq!(update(&mut choice, &key(KeyCode::Enter), &labels), None);
        update(&mut choice, &key(KeyCode::Char(' ')), &labels);
        assert_eq!(
            update(&mut choice, &key(KeyCode::Enter), &labels),
            Some(AuthMultiChoiceOutcome::Selected(vec![1]))
        );
        update(&mut choice, &key(KeyCode::Char(' ')), &labels);
        assert_eq!(update(&mut choice, &key(KeyCode::Enter), &labels), None);
        assert_eq!(choice.filter, "");
    }

    #[test]
    fn unicode_filtering_keeps_checked_and_focused_source_identity() {
        let labels = vec![
            "中文".to_owned(),
            "👩‍💻 e\u{301}".to_owned(),
            "中文".to_owned(),
        ];
        let mut choice = new_choice(&labels);
        update(&mut choice, &key(KeyCode::End), &labels);
        update(&mut choice, &key(KeyCode::Char(' ')), &labels);
        update(&mut choice, &Event::Paste("中".to_owned()), &labels);
        assert_eq!(choice.indices, [0, 2]);
        assert_eq!(choice.indices[choice.selected], 2);
        update(&mut choice, &key(KeyCode::Backspace), &labels);
        assert_eq!(choice.indices[choice.selected], 2);
        update(
            &mut choice,
            &Event::Paste("👩‍💻 e\u{301}".to_owned()),
            &labels,
        );
        assert_eq!(choice.indices, [1]);
        update(&mut choice, &key(KeyCode::Char(' ')), &labels);
        update(&mut choice, &Event::Paste("missing".to_owned()), &labels);
        assert_eq!(choice.indices, Vec::<usize>::new());
        update(&mut choice, &key(KeyCode::Char(' ')), &labels);
        assert_eq!(
            update(&mut choice, &key(KeyCode::Enter), &labels),
            Some(AuthMultiChoiceOutcome::Selected(vec![1, 2]))
        );
    }

    #[test]
    fn select_all_is_explicit_and_includes_filtered_out_choices() {
        let labels = vec!["first".to_owned(), "second".to_owned()];
        let mut choice = new_choice(&labels);
        update(&mut choice, &Event::Paste("second".to_owned()), &labels);
        assert_eq!(update(&mut choice, &key(KeyCode::Enter), &labels), None);
        update(&mut choice, &ctrl('a'), &labels);
        update(&mut choice, &ctrl('a'), &labels);
        assert_eq!(choice.filter, "second");
        assert_eq!(
            update(&mut choice, &key(KeyCode::Enter), &labels),
            Some(AuthMultiChoiceOutcome::Selected(vec![0, 1]))
        );
        update(&mut choice, &key(KeyCode::Char(' ')), &labels);
        assert_eq!(
            update(&mut choice, &key(KeyCode::Enter), &labels),
            Some(AuthMultiChoiceOutcome::Selected(vec![0]))
        );
    }

    #[test]
    fn back_and_cancel_never_submit_checked_choices() {
        let labels = vec!["account".to_owned()];
        let mut choice = new_choice(&labels);
        update(&mut choice, &ctrl('a'), &labels);
        assert_eq!(
            update(&mut choice, &key(KeyCode::Escape), &labels),
            Some(AuthMultiChoiceOutcome::Back)
        );
        for character in ['c', 'd'] {
            assert_eq!(
                update(&mut choice, &ctrl(character), &labels),
                Some(AuthMultiChoiceOutcome::Cancelled)
            );
        }
    }

    #[test]
    fn empty_catalog_is_rejected_without_entering_terminal() {
        assert_eq!(
            choose_many("Auth", &[], &bcode_config::TuiConfig::default())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn checkbox_rendering_is_bounded_across_unicode_and_resize() {
        let labels = (0..50)
            .map(|index| format!("Account {index} 中文 👩‍💻 e\u{301}"))
            .collect::<Vec<_>>();
        let mut choice = new_choice(&labels);
        update(&mut choice, &key(KeyCode::End), &labels);
        update(&mut choice, &key(KeyCode::Char(' ')), &labels);
        let resolved = crate::theme::resolve_initial_theme();
        let theme = resolved.presented(resolved.accent);
        for width in [0, 1, 2, 8, 30, 80, 1, 80] {
            for height in [0, 1, 2, 5, 12] {
                let buffer = super::super::render(
                    "Authentication 中文 👩‍💻",
                    &labels,
                    &choice,
                    Size::new(width, height),
                    &theme,
                );
                assert_eq!(buffer.area(), Rect::new(0, 0, width, height));
                assert_eq!(
                    buffer.cells().len(),
                    usize::from(width) * usize::from(height)
                );
                if width == 80 && height == 12 {
                    let text = buffer
                        .cells()
                        .iter()
                        .map(|cell| cell.symbol.as_str())
                        .collect::<String>();
                    assert!(text.contains("[x] Account 49"), "{text}");
                }
            }
        }
        assert_eq!(
            update(&mut choice, &key(KeyCode::Enter), &labels),
            Some(AuthMultiChoiceOutcome::Selected(vec![49]))
        );
        update(&mut choice, &key(KeyCode::Home), &labels);
        let buffer = super::super::render("Auth", &labels, &choice, Size::new(80, 12), &theme);
        let text = buffer
            .cells()
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect::<String>();
        assert!(text.contains("[ ] Account 0"), "{text}");
    }
}
