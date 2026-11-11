//! Publisher and importer options (spec A.10 INV-PUB-06).
//!
//! A form generated from a provider's JSON-schema-like `options_schema`, built from BMUX `form`
//! (focus traversal and required validation) and `form_field` (label, required marker, help)
//! around a BMUX text input or a choice value.
//!
//! * String properties are text inputs; typing always edits the value (`j`/`k` included).
//! * `enum` strings and booleans are choices: `←`/`→`/`space` cycle them.
//! * `tab`/`shift+tab` (and `↑`/`↓`) move between fields, `enter` submits once every required
//!   field has a value, `esc` cancels.
//! * Values are sent as strings (booleans as `"true"`/`"false"`); empty values are omitted, so
//!   providers apply their own defaults.

use super::theme::ReviewTheme;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::composition::{Column, TextBlock};
use bmux_tui::text::{Line, Span, Text};
use bmux_tui_components::form::{Form, FormFieldItem, FormOutcome, FormPolicy, FormState};
use bmux_tui_components::form_field::FormFieldComponent;
use bmux_tui_components::text_input::{
    EdgePolicy, TextInputComponent, TextInputControl, TextInputOutcome, TextInputPolicy,
    TextInputState,
};
use std::cell::RefCell;

/// Kind of one option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldKind {
    /// Free text.
    Text,
    /// One of these values.
    Choice(Vec<String>),
}

/// One option field.
pub struct OptionField {
    /// Option key.
    pub key: String,
    label: String,
    help: Option<String>,
    /// Kind.
    pub kind: FieldKind,
    required: bool,
    input: RefCell<TextInputState>,
    choice: usize,
}

impl std::fmt::Debug for OptionField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OptionField")
            .field("key", &self.key)
            .field("value", &self.value())
            .finish_non_exhaustive()
    }
}

impl OptionField {
    /// Current value.
    #[must_use]
    pub fn value(&self) -> String {
        match &self.kind {
            FieldKind::Text => self.input.borrow().buffer().text().to_string(),
            FieldKind::Choice(choices) => choices.get(self.choice).cloned().unwrap_or_default(),
        }
    }

    fn set_value(&mut self, value: &str) {
        match &self.kind {
            FieldKind::Text => {
                *self.input.borrow_mut() =
                    TextInputState::new(bmux_text_edit::TextEditBuffer::from_text(value));
            }
            FieldKind::Choice(choices) => {
                if let Some(index) = choices.iter().position(|choice| choice == value) {
                    self.choice = index;
                }
            }
        }
    }
}

/// What a key did to the form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormKey {
    /// Every required field has a value; continue.
    Submitted,
    /// Leave the form.
    Cancelled,
    /// Something changed.
    Redraw,
    /// Not handled.
    Ignored,
}

/// An options form.
#[derive(Debug)]
pub struct OptionsForm {
    fields: Vec<OptionField>,
    items: Vec<FormFieldItem>,
    state: FormState,
    policy: TextInputPolicy,
    /// Required fields left empty by the last submit.
    invalid: Vec<usize>,
}

impl OptionsForm {
    /// The form for `schema` (`{"properties": {...}, "required": [...]}`); string and boolean
    /// properties become fields, in the schema's order.
    #[must_use]
    pub fn from_schema(schema: &serde_json::Value) -> Self {
        let required: Vec<&str> = schema
            .get("required")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .collect();
        let mut fields = Vec::new();
        for (key, property) in schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .into_iter()
            .flatten()
        {
            let kind = match property.get("type").and_then(serde_json::Value::as_str) {
                Some("boolean") => FieldKind::Choice(vec!["true".into(), "false".into()]),
                Some("string") => property
                    .get("enum")
                    .and_then(serde_json::Value::as_array)
                    .map_or(FieldKind::Text, |choices| {
                        FieldKind::Choice(
                            choices
                                .iter()
                                .filter_map(serde_json::Value::as_str)
                                .map(str::to_string)
                                .collect(),
                        )
                    }),
                _ => continue,
            };
            let mut field = OptionField {
                key: key.clone(),
                label: key.replace('_', " "),
                help: property
                    .get("description")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                kind,
                required: required.contains(&key.as_str()),
                input: RefCell::new(TextInputState::new(bmux_text_edit::TextEditBuffer::new())),
                choice: 0,
            };
            if let Some(default) = property.get("default").and_then(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| value.as_bool().map(|value| value.to_string()))
            }) {
                field.set_value(&default);
            }
            fields.push(field);
        }
        let items = fields
            .iter()
            .map(|field| FormFieldItem::new(field.key.clone()).required(field.required))
            .collect();
        let mut policy = TextInputPolicy::chat_composer();
        policy.keyboard.shift_enter = None;
        policy.viewport.max_rows = Some(1);
        policy.edge = EdgePolicy::disabled();
        Self {
            state: FormState::new((!fields.is_empty()).then_some(0)),
            fields,
            items,
            policy,
            invalid: Vec::new(),
        }
    }

    /// The fields.
    #[must_use]
    pub fn fields(&self) -> &[OptionField] {
        &self.fields
    }

    /// Index of the focused field.
    #[must_use]
    pub const fn focused(&self) -> Option<usize> {
        self.state.focused()
    }

    /// Set a field's value (ignored for unknown keys and choices without that value).
    pub fn set(&mut self, key: &str, value: &str) {
        if let Some(field) = self.fields.iter_mut().find(|field| field.key == key) {
            field.set_value(value);
        }
    }

    /// The options to send: every non-empty value, as strings.
    #[must_use]
    pub fn values(&self) -> serde_json::Value {
        serde_json::Value::Object(
            self.fields
                .iter()
                .map(|field| (field.key.clone(), field.value()))
                .filter(|(_, value)| !value.is_empty())
                .map(|(key, value)| (key, serde_json::Value::String(value)))
                .collect(),
        )
    }

    /// Required fields without a value.
    fn invalid_fields(&self) -> Vec<usize> {
        let values: Vec<String> = self.fields.iter().map(OptionField::value).collect();
        let values: Vec<Option<&str>> = values.iter().map(|value| Some(value.as_str())).collect();
        Form::new(&self.items, &values)
            .policy(FormPolicy::interactive())
            .validate()
    }

    /// Handle a key.
    pub fn key(&mut self, stroke: KeyStroke) -> FormKey {
        let plain = stroke.modifiers.is_empty();
        let focused = self.state.focused();
        let field_kind = focused
            .and_then(|index| self.fields.get(index))
            .map(|field| &field.kind);
        match stroke.key {
            KeyCode::Escape if plain => return FormKey::Cancelled,
            KeyCode::Enter if plain => {
                let invalid = self.invalid_fields();
                self.invalid.clone_from(&invalid);
                return if invalid.is_empty() {
                    FormKey::Submitted
                } else {
                    self.state.set_focused(invalid.first().copied());
                    FormKey::Redraw
                };
            }
            KeyCode::Tab | KeyCode::Up | KeyCode::Down => {
                let backwards = stroke.key == KeyCode::Up
                    || (stroke.key == KeyCode::Tab && stroke.modifiers.shift);
                let tab = KeyStroke {
                    key: KeyCode::Tab,
                    modifiers: bmux_keyboard::Modifiers {
                        shift: backwards,
                        ..bmux_keyboard::Modifiers::default()
                    },
                };
                let values: Vec<Option<&str>> = vec![None; self.items.len()];
                let form = Form::new(&self.items, &values).policy(FormPolicy::interactive());
                return match form.handle_event(&mut self.state, &bmux_tui::event::Event::Key(tab)) {
                    FormOutcome::Focused(_) => FormKey::Redraw,
                    _ => FormKey::Ignored,
                };
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Space
                if plain && matches!(field_kind, Some(FieldKind::Choice(_))) =>
            {
                let index = focused.unwrap_or_default();
                let field = &mut self.fields[index];
                if let FieldKind::Choice(choices) = &field.kind
                    && !choices.is_empty()
                {
                    let len = choices.len();
                    field.choice = if stroke.key == KeyCode::Left {
                        (field.choice + len - 1) % len
                    } else {
                        (field.choice + 1) % len
                    };
                }
                return FormKey::Redraw;
            }
            _ => {}
        }
        let Some(index) = focused else {
            return FormKey::Ignored;
        };
        if self.fields[index].kind != FieldKind::Text {
            return FormKey::Ignored;
        }
        let outcome = TextInputControl::new(&self.policy)
            .handle_key(&mut self.fields[index].input.borrow_mut(), stroke);
        if outcome == TextInputOutcome::Ignored {
            FormKey::Ignored
        } else {
            self.invalid.retain(|invalid| *invalid != index);
            FormKey::Redraw
        }
    }

    /// Paste into the focused text field.
    pub fn paste(&mut self, text: &str) -> FormKey {
        let Some(index) = self.state.focused() else {
            return FormKey::Ignored;
        };
        if self.fields[index].kind != FieldKind::Text {
            return FormKey::Ignored;
        }
        TextInputControl::new(&self.policy)
            .handle_paste(&mut self.fields[index].input.borrow_mut(), text);
        FormKey::Redraw
    }

    /// The form's fields as BMUX form fields in a column.
    #[must_use]
    pub fn component<'a>(&'a self, id: &str, theme: &ReviewTheme) -> Column<'a> {
        let mut column = Column::new().id(format!("{id}.fields"));
        let focused = self.state.focused();
        for (index, field) in self.fields.iter().enumerate() {
            let is_focused = focused == Some(index);
            let field_id = format!("{id}.field.{}", field.key);
            let mut component = match &field.kind {
                FieldKind::Text => FormFieldComponent::new(
                    field_id.clone(),
                    &field.label,
                    TextInputComponent::new(
                        format!("{field_id}.input"),
                        &field.input,
                        &self.policy,
                    )
                    .style(theme.input)
                    .placeholder("(default)", theme.placeholder)
                    .focused(is_focused),
                ),
                FieldKind::Choice(_) => {
                    let style = if is_focused { theme.accent } else { theme.text };
                    let value = Line::from_spans(vec![
                        Span::styled("‹ ", theme.muted),
                        Span::styled(field.value(), style),
                        Span::styled(" ›", theme.muted),
                    ]);
                    FormFieldComponent::new(
                        field_id.clone(),
                        &field.label,
                        TextBlock::new(Text::from_lines(vec![value]))
                            .wrap(bmux_tui::text::TextWrap::None),
                    )
                }
            }
            .required(field.required)
            .styles(theme.component.form_field_styles());
            if let Some(help) = &field.help {
                component = component.help(help);
            }
            if self.invalid.contains(&index) {
                component = component.error("required");
            }
            column = column.child(component);
        }
        column
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(form: &mut OptionsForm, key: KeyCode) -> FormKey {
        form.key(KeyStroke::simple(key))
    }

    #[test]
    fn schema_fields_cycle_choices_type_any_letter_and_require_values() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "repository": { "type": "string", "description": "owner/repo" },
                "event": { "type": "string", "default": "COMMENT", "enum": ["COMMENT", "APPROVE"] },
                "fallback": { "type": "boolean", "default": true },
                "count": { "type": "integer" }
            },
            "required": ["repository"]
        });
        let mut form = OptionsForm::from_schema(&schema);
        let keys: Vec<&str> = form
            .fields()
            .iter()
            .map(|field| field.key.as_str())
            .collect();
        assert_eq!(keys, ["event", "fallback", "repository"]);
        assert_eq!(
            form.values(),
            serde_json::json!({"event": "COMMENT", "fallback": "true"})
        );
        // Choices cycle in both directions.
        assert_eq!(key(&mut form, KeyCode::Right), FormKey::Redraw);
        assert_eq!(form.values()["event"], "APPROVE");
        key(&mut form, KeyCode::Left);
        assert_eq!(form.values()["event"], "COMMENT");
        // A required field blocks submitting and takes focus.
        assert_eq!(key(&mut form, KeyCode::Enter), FormKey::Redraw);
        assert_eq!(form.focused(), Some(2));
        // `j` and `k` are typed into text fields.
        for ch in "o/jk".chars() {
            form.key(KeyStroke::simple(KeyCode::Char(ch)));
        }
        assert_eq!(form.values()["repository"], "o/jk");
        assert_eq!(key(&mut form, KeyCode::Enter), FormKey::Submitted);
        key(&mut form, KeyCode::Up);
        key(&mut form, KeyCode::Space);
        assert_eq!(form.values()["fallback"], "false");
        assert_eq!(key(&mut form, KeyCode::Escape), FormKey::Cancelled);
    }
}
