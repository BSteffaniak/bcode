//! Bounded editable filter form using BMUX text editing and layout.
use bcode_usage_models::UsageQuery;
use bmux_keyboard::KeyCode;
use bmux_tui::{
    component::{Component, Constraints, LayoutCx},
    event::Event,
    geometry::{Rect, Size},
    paint::{LocalRect, PaintCx},
    prelude::Line,
    style::Style,
};
use bmux_tui_components::text_input::{
    TextInputComponent, TextInputControl, TextInputPolicy, TextInputState,
};
use std::cell::RefCell;

const LABELS: [&str; 6] = [
    "From UTC milliseconds",
    "To UTC milliseconds",
    "Provider IDs (JSON array; [] = all)",
    "Models (JSON array of {provider,model}; [] = all)",
    "Session UUID (empty = all)",
    "Bucket milliseconds",
];

pub(super) struct Filters {
    fields: [RefCell<TextInputState>; 6],
    selected: usize,
    pub error: String,
}
impl Filters {
    pub fn new(query: &UsageQuery) -> Self {
        let values = [
            query.range.from_timestamp_ms.to_string(),
            query.range.to_timestamp_ms.to_string(),
            serde_json::to_string(&query.providers).unwrap_or_default(),
            serde_json::to_string(&query.models).unwrap_or_default(),
            query.session_id.map_or(String::new(), |id| id.to_string()),
            query.bucket_ms.to_string(),
        ];
        Self {
            fields: values.map(|value| {
                let mut state = TextInputState::default();
                state.buffer_mut().paste(&value);
                RefCell::new(state)
            }),
            selected: 0,
            error: String::new(),
        }
    }
    pub fn apply(&self, base: &UsageQuery) -> Result<UsageQuery, String> {
        let values: Vec<_> = self
            .fields
            .iter()
            .map(|field| field.borrow().buffer().text().to_owned())
            .collect();
        if values.iter().any(|value| value.len() > 65536) {
            return Err("Filter field exceeds 64 KiB".into());
        }
        let mut query = base.clone();
        query.range.from_timestamp_ms = values[0]
            .trim()
            .parse()
            .map_err(|_| "Invalid start timestamp")?;
        query.range.to_timestamp_ms = values[1]
            .trim()
            .parse()
            .map_err(|_| "Invalid end timestamp")?;
        query.providers = serde_json::from_str(&values[2])
            .map_err(|_| "Providers must be a JSON array of strings")?;
        query.models = serde_json::from_str(&values[3])
            .map_err(|_| "Models must be a JSON array of provider/model objects")?;
        query.session_id = if values[4].trim().is_empty() {
            None
        } else {
            Some(
                values[4]
                    .trim()
                    .parse()
                    .map_err(|_| "Invalid session UUID")?,
            )
        };
        query.bucket_ms = values[5]
            .trim()
            .parse()
            .map_err(|_| "Invalid bucket duration")?;
        query.after = None;
        query.revision = None;
        query.validate()?;
        Ok(query)
    }
    pub fn event(&mut self, event: &Event) {
        if let Event::Key(stroke) = event
            && stroke.key == KeyCode::Tab
        {
            self.selected = (self.selected + 1) % self.fields.len();
            return;
        }
        let mut state = self.fields[self.selected].borrow_mut();
        let before = state.clone();
        TextInputControl::new(&TextInputPolicy::chat_composer()).handle_event(&mut state, event);
        if state.buffer().text().len() > 65536 {
            *state = before;
            self.error = "Filter field exceeds 64 KiB".into();
        }
    }
    pub fn paint(&self, area: Rect, frame: &mut PaintCx<'_, '_>) {
        let label = format!(
            "Filters {}/6: {} | Tab next; Enter apply; Esc cancel",
            self.selected + 1,
            LABELS[self.selected]
        );
        for (offset, text) in [label.as_str(), self.error.as_str()]
            .into_iter()
            .enumerate()
        {
            let offset = u16::try_from(offset).unwrap_or_default();
            if offset < area.height {
                frame.write_line_with_fallback_style(
                    LocalRect::terminal(Rect::new(area.x, area.y + offset, area.width, 1)),
                    &Line::from(text),
                    Style::new(),
                );
            }
        }
        let input_area = Rect::new(
            area.x,
            area.y.saturating_add(area.height.min(2)),
            area.width,
            area.height.saturating_sub(2),
        );
        let policy = TextInputPolicy::chat_composer();
        let input = TextInputComponent::new("usage-filter", &self.fields[self.selected], &policy)
            .focused(true);
        let layout = input.layout(
            Constraints::tight(Size::new(input_area.width, input_area.height)),
            &mut LayoutCx::new(),
        );
        frame.with_child(
            i32::from(input_area.x),
            i64::from(input_area.y),
            LocalRect::new(0, 0, input_area.width, input_area.height),
            |cx| input.paint(&layout, cx),
        );
    }
}
