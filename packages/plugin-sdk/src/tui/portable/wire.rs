//! Explicit v2 terminal operation DTOs. Remote derives are field adapters, not a
//! serialization requirement on BMUX runtime types. Unknown variants fail closed.
use bmux_text_edit as edit;
use bmux_tui::{event, geometry};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(remote = "event::EventOutcome", rename_all = "snake_case")]
pub enum EventOutcome {
    Ignored,
    Handled,
    Redraw,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "edit::TextMotion", rename_all = "snake_case")]
enum Motion {
    Left,
    Right,
    WordLeft,
    WordRight,
    Start,
    End,
    LineStart,
    Absolute(usize),
    VisualUp,
    VisualDown,
    LineEnd,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "edit::TextDelete", rename_all = "snake_case")]
enum Delete {
    Backward,
    Forward,
    WordBackward,
    WordForward,
    ToLineStart,
    ToLineEnd,
    ToStart,
    ToEnd,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "edit::TextEditCommand", rename_all = "snake_case")]
enum Command {
    Insert(char),
    InsertString(String),
    Paste(String),
    Move(#[serde(with = "Motion")] edit::TextMotion),
    Delete(#[serde(with = "Delete")] edit::TextDelete),
    Clear,
}

#[derive(Serialize, Deserialize)]
struct CommandValue(#[serde(with = "Command")] edit::TextEditCommand);
#[derive(Serialize, Deserialize)]
struct MotionValue(#[serde(with = "Motion")] edit::TextMotion);

pub(super) mod optional_command {
    use super::{CommandValue, Deserialize, Serialize, edit};
    use std::borrow::Borrow;
    pub fn serialize<S: serde::Serializer, V: Borrow<Option<edit::TextEditCommand>>>(
        value: &V,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value
            .borrow()
            .clone()
            .map(CommandValue)
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<edit::TextEditCommand>, D::Error> {
        Option::<CommandValue>::deserialize(deserializer).map(|value| value.map(|value| value.0))
    }
}

pub(super) mod optional_motion {
    use super::{Deserialize, MotionValue, Serialize, edit};
    use std::borrow::Borrow;
    pub fn serialize<S: serde::Serializer, V: Borrow<Option<edit::TextMotion>>>(
        value: &V,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.borrow().map(MotionValue).serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<edit::TextMotion>, D::Error> {
        Option::<MotionValue>::deserialize(deserializer).map(|value| value.map(|value| value.0))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum EventDto {
    Key(bmux_keyboard::KeyStroke),
    Mouse(MouseValue),
    Resize(SizeValue),
    Paste(String),
    Focus(FocusValue),
    Tick,
    User(String),
}
#[derive(Serialize, Deserialize)]
struct MouseValue(#[serde(with = "Mouse")] event::MouseEvent);
#[derive(Serialize, Deserialize)]
struct SizeValue(#[serde(with = "Size")] geometry::Size);
#[derive(Serialize, Deserialize)]
struct FocusValue(#[serde(with = "Focus")] event::FocusEvent);

pub(super) mod input {
    use super::{Deserialize, EventDto, FocusValue, MouseValue, Serialize, SizeValue, event};
    pub fn serialize<S: serde::Serializer>(
        value: &event::Event,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let dto = match value {
            event::Event::Key(value) => EventDto::Key(*value),
            event::Event::Mouse(value) => EventDto::Mouse(MouseValue(*value)),
            event::Event::Resize(value) => EventDto::Resize(SizeValue(*value)),
            event::Event::Paste(value) => EventDto::Paste(value.clone()),
            event::Event::Focus(value) => EventDto::Focus(FocusValue(*value)),
            event::Event::Tick => EventDto::Tick,
            event::Event::User(value) => EventDto::User(value.clone()),
        };
        dto.serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<event::Event, D::Error> {
        Ok(match EventDto::deserialize(deserializer)? {
            EventDto::Key(value) => event::Event::Key(value),
            EventDto::Mouse(value) => event::Event::Mouse(value.0),
            EventDto::Resize(value) => event::Event::Resize(value.0),
            EventDto::Paste(value) => event::Event::Paste(value),
            EventDto::Focus(value) => event::Event::Focus(value.0),
            EventDto::Tick => event::Event::Tick,
            EventDto::User(value) => event::Event::User(value),
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "event::FocusEvent", rename_all = "snake_case")]
enum Focus {
    Gained,
    Lost,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "geometry::Size", deny_unknown_fields)]
struct Size {
    width: u16,
    height: u16,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "geometry::Point", deny_unknown_fields)]
struct Point {
    x: u16,
    y: u16,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "event::MouseEvent", deny_unknown_fields)]
struct Mouse {
    #[serde(with = "MouseKind")]
    kind: event::MouseEventKind,
    #[serde(with = "Point")]
    position: geometry::Point,
    #[serde(with = "Modifiers")]
    modifiers: event::MouseModifiers,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "event::MouseModifiers", deny_unknown_fields)]
struct Modifiers {
    shift: bool,
    alt: bool,
    ctrl: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "event::MouseEventKind", rename_all = "snake_case")]
enum MouseKind {
    Down(#[serde(with = "Button")] event::MouseButton),
    Up(#[serde(with = "Button")] event::MouseButton),
    Drag(#[serde(with = "Button")] event::MouseButton),
    Move,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "event::MouseButton", rename_all = "snake_case")]
enum Button {
    Left,
    Right,
    Middle,
    Other(u8),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct Input(#[serde(with = "input")] event::Event);

    #[test]
    fn terminal_input_dto_round_trips_and_rejects_unknown_variants() {
        for event in [
            event::Event::Paste("界e\u{301}".to_owned()),
            event::Event::Resize(geometry::Size::new(3, 2)),
            event::Event::Focus(event::FocusEvent::Lost),
            event::Event::Tick,
            event::Event::User("local".to_owned()),
            event::Event::Mouse(
                event::MouseEvent::new(
                    event::MouseEventKind::Drag(event::MouseButton::Other(4)),
                    geometry::Point::new(2, 1),
                )
                .with_modifiers(event::MouseModifiers {
                    shift: true,
                    alt: false,
                    ctrl: true,
                }),
            ),
        ] {
            let input = Input(event);
            let bytes = serde_json::to_vec(&input).unwrap();
            assert_eq!(serde_json::from_slice::<Input>(&bytes).unwrap(), input);
        }
        assert!(serde_json::from_str::<Input>(r#"{"kind":"future"}"#).is_err());
        assert!(serde_json::from_str::<Input>(r#"{"kind":"tick","unexpected":true}"#).is_err());
    }

    #[test]
    fn mouse_and_focus_dtos_preserve_each_explicit_variant() {
        use event::{MouseButton, MouseEventKind};
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Right),
            MouseEventKind::Drag(MouseButton::Middle),
            MouseEventKind::Down(MouseButton::Other(255)),
            MouseEventKind::Move,
            MouseEventKind::ScrollUp,
            MouseEventKind::ScrollDown,
            MouseEventKind::ScrollLeft,
            MouseEventKind::ScrollRight,
        ] {
            let input = Input(event::Event::Mouse(event::MouseEvent {
                kind,
                position: geometry::Point::new(u16::MAX, 0),
                modifiers: event::MouseModifiers {
                    shift: true,
                    alt: true,
                    ctrl: true,
                },
            }));
            let wire = serde_json::to_value(&input).unwrap();
            assert_eq!(serde_json::from_value::<Input>(wire).unwrap(), input);
        }
        for focus in [event::FocusEvent::Gained, event::FocusEvent::Lost] {
            let input = Input(event::Event::Focus(focus));
            let wire = serde_json::to_value(&input).unwrap();
            assert_eq!(serde_json::from_value::<Input>(wire).unwrap(), input);
        }
        let wire = serde_json::json!({"kind":"focus","value":"gained"});
        assert_eq!(
            serde_json::from_value::<Input>(wire).unwrap(),
            Input(event::Event::Focus(event::FocusEvent::Gained))
        );
        assert!(
            serde_json::from_value::<Input>(serde_json::json!({"kind":"focus","value":"future"}))
                .is_err()
        );
    }

    #[test]
    fn editor_dto_covers_each_motion_and_deletion() {
        use crate::tui::portable::Bindings;
        for motion in [
            edit::TextMotion::Left,
            edit::TextMotion::Right,
            edit::TextMotion::WordLeft,
            edit::TextMotion::WordRight,
            edit::TextMotion::Start,
            edit::TextMotion::End,
            edit::TextMotion::LineStart,
            edit::TextMotion::LineEnd,
            edit::TextMotion::Absolute(13),
            edit::TextMotion::VisualUp,
            edit::TextMotion::VisualDown,
        ] {
            let bindings = Bindings {
                edit: Some(edit::TextEditCommand::Move(motion)),
                selection: Some(motion),
                ..Bindings::default()
            };
            let bytes = serde_json::to_vec(&bindings).unwrap();
            assert_eq!(
                serde_json::from_slice::<Bindings>(&bytes).unwrap(),
                bindings
            );
        }
        for delete in [
            edit::TextDelete::Backward,
            edit::TextDelete::Forward,
            edit::TextDelete::WordBackward,
            edit::TextDelete::WordForward,
            edit::TextDelete::ToLineStart,
            edit::TextDelete::ToLineEnd,
            edit::TextDelete::ToStart,
            edit::TextDelete::ToEnd,
        ] {
            let bindings = Bindings {
                edit: Some(edit::TextEditCommand::Delete(delete)),
                ..Bindings::default()
            };
            let bytes = serde_json::to_vec(&bindings).unwrap();
            assert_eq!(
                serde_json::from_slice::<Bindings>(&bytes).unwrap(),
                bindings
            );
        }
        for command in [
            edit::TextEditCommand::Insert('界'),
            edit::TextEditCommand::InsertString("e\u{301}".to_owned()),
            edit::TextEditCommand::Paste("text".to_owned()),
            edit::TextEditCommand::Clear,
        ] {
            let bindings = Bindings {
                edit: Some(command),
                ..Bindings::default()
            };
            let bytes = serde_json::to_vec(&bindings).unwrap();
            assert_eq!(
                serde_json::from_slice::<Bindings>(&bytes).unwrap(),
                bindings
            );
        }
    }

    #[test]
    fn outcome_dto_preserves_consumption_independent_of_effect() {
        for outcome in [
            event::EventOutcome::Ignored,
            event::EventOutcome::Handled,
            event::EventOutcome::Redraw,
        ] {
            let result = crate::tui::PluginTuiEventResult {
                outcome,
                action: crate::tui::PluginTuiAction::None,
            };
            let bytes = serde_json::to_vec(&result).unwrap();
            assert_eq!(
                serde_json::from_slice::<crate::tui::PluginTuiEventResult>(&bytes).unwrap(),
                result
            );
        }
    }
}
