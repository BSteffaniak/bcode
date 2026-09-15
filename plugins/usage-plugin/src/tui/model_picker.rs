//! Searchable, bounded model comparison selection using BMUX controls.
use bcode_usage_models::UsageModel;
use bmux_keyboard::KeyCode;
use bmux_tui::{
    component::{Component, Constraints, LayoutCx},
    event::Event,
    geometry::{Rect, Size},
    paint::{LocalRect, PaintCx},
    prelude::Line,
};
use bmux_tui_components::{
    table::{Table, TableColumn, TableRow, TableState},
    text_input::{TextInputComponent, TextInputControl, TextInputPolicy, TextInputState},
};
use std::{cell::RefCell, collections::BTreeSet};

pub(super) struct ModelPicker {
    models: Vec<UsageModel>,
    selected: BTreeSet<UsageModel>,
    search: RefCell<TextInputState>,
    table: TableState,
    area: Rect,
}
impl ModelPicker {
    pub fn new(models: impl Iterator<Item = UsageModel>, selected: BTreeSet<UsageModel>) -> Self {
        let models: BTreeSet<_> = models.take(256).chain(selected.iter().cloned()).collect();
        Self {
            models: models.into_iter().collect(),
            selected,
            search: RefCell::new(TextInputState::default()),
            table: TableState::default(),
            area: Rect::new(0, 0, 0, 0),
        }
    }
    pub fn selection(&self) -> BTreeSet<UsageModel> {
        self.selected.clone()
    }
    fn visible(&self) -> Vec<&UsageModel> {
        let search = self.search.borrow().buffer().text().to_lowercase();
        self.models
            .iter()
            .filter(|model| label(model).to_lowercase().contains(&search))
            .collect()
    }
    fn rows(&self) -> Vec<TableRow> {
        self.visible()
            .into_iter()
            .map(|model| {
                TableRow::rich(vec![
                    Line::from(if self.selected.contains(model) {
                        "[x]"
                    } else {
                        "[ ]"
                    }),
                    Line::from(label(model)),
                ])
            })
            .collect()
    }
    pub fn event(&mut self, event: &Event) {
        if let Event::Key(stroke) = event {
            match stroke.key {
                KeyCode::Char(' ') => {
                    if let Some(model) = self
                        .visible()
                        .get(self.table.selected().unwrap_or(0))
                        .map(|model| (*model).clone())
                        && !self.selected.remove(&model)
                        && self.selected.len() < 256
                    {
                        self.selected.insert(model);
                    }
                    return;
                }
                KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                    Table::new(&columns(), &self.rows()).handle_event(
                        self.area,
                        &mut self.table,
                        event,
                    );
                    return;
                }
                _ => {}
            }
        }
        let mut search = self.search.borrow_mut();
        let previous = search.clone();
        TextInputControl::new(&TextInputPolicy::chat_composer()).handle_event(&mut search, event);
        if search.buffer().text().len() > 1024 {
            *search = previous;
        }
        self.table = TableState::default();
    }
    pub fn paint(&mut self, area: Rect, frame: &mut PaintCx<'_, '_>) {
        let height = area.height.min(2);
        let policy = TextInputPolicy::chat_composer();
        let input =
            TextInputComponent::new("usage-model-search", &self.search, &policy).focused(true);
        let layout = input.layout(
            Constraints::tight(Size::new(area.width, height)),
            &mut LayoutCx::new(),
        );
        frame.with_child(
            i32::from(area.x),
            i64::from(area.y),
            LocalRect::new(0, 0, area.width, height),
            |cx| input.paint(&layout, cx),
        );
        self.area = Rect::new(
            area.x,
            area.y.saturating_add(height),
            area.width,
            area.height.saturating_sub(height),
        );
        Table::new(&columns(), &self.rows()).paint(self.area, &self.table, frame);
    }
}
const fn columns() -> [TableColumn<'static>; 2] {
    [
        TableColumn::new("Select").fixed(4),
        TableColumn::new("Models in report page; type search, Space toggle, Enter accept"),
    ]
}
fn label(model: &UsageModel) -> String {
    format!(
        "{}/{}",
        model.provider.as_deref().unwrap_or("Unknown"),
        model.model.as_deref().unwrap_or("Unknown")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn search_preserves_exact_selection_and_unknown_identity() {
        let model = UsageModel {
            provider: Some("Provider".into()),
            model: Some("模型/Name".into()),
        };
        let picker = ModelPicker::new(
            [model.clone(), UsageModel::default()].into_iter(),
            BTreeSet::from([model.clone()]),
        );
        picker.search.borrow_mut().buffer_mut().paste("name");
        assert_eq!(picker.visible(), vec![&model]);
        assert_eq!(picker.selection(), BTreeSet::from([model]));
    }
}
