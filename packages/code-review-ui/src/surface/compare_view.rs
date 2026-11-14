//! The comparison on screen: the comparison row (baseline control, summary, push timeline),
//! the banner of a changed review, the "Changes since…" menu, the version picker, and the push
//! timeline's keys. Everything acts in place on the open review.

use super::ReviewSurface;
use super::compare::{CompareCounts, TimelineFocus, column_time};
use super::flows::Modal;
use crate::action::Action;
use crate::compare_bar::{self, BannerPart, BarPart, CompareBar};
use crate::compare_menu::{CompareMenu, MenuChoice, MenuOutcome, MenuState, VersionPicker};
use crate::config::compare::TimelineMode;
use crate::context::UiContext;
use bcode_code_review::document::CompareView;
use bcode_code_review_models::ReviewCompareBaseline as Baseline;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::composition::TextBlock;
use bmux_tui::event::{Event, MouseButton, MouseEventKind};
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::text::Text;
use bmux_tui_components::status_bar::StatusSeverity;

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// The summary of `counts`: in full and briefly.
fn summary(counts: CompareCounts, active: bool) -> [String; 2] {
    if !active {
        return [String::new(), String::new()];
    }
    if counts.pending > 0 {
        let done = counts.layers.saturating_sub(counts.pending);
        return [
            format!("comparing {done} of {}…", counts.layers),
            format!("{done}/{}…", counts.layers),
        ];
    }
    let mut parts = vec![format!(
        "{} of {} changed",
        counts.changed,
        plural(counts.layers, "PR", "PRs")
    )];
    if counts.rebased > 0 {
        parts.push(format!("{} rebased only", counts.rebased));
    }
    if counts.unchanged > 0 {
        parts.push(format!("{} unchanged", counts.unchanged));
    }
    if counts.new_to_you > 0 {
        parts.push(format!("{} new to you", counts.new_to_you));
    }
    if counts.unavailable > 0 {
        parts.push(format!("{} unavailable", counts.unavailable));
    }
    if counts.with_rebase > 0 {
        parts.push(format!("{} with the rebase", counts.with_rebase));
    }
    [
        parts.join(" · "),
        format!("{}/{} changed", counts.changed, counts.layers),
    ]
}

impl ReviewSurface {
    /// The comparison row's content.
    fn compare_bar_model(&self) -> CompareBar {
        let pushes: Vec<bool> = self
            .stack_history
            .as_ref()
            .map(|history| {
                history
                    .pushes
                    .iter()
                    .map(|push| !push.split().0.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let len = pushes.len();
        let timeline_mode = self.services.compare_config().timeline;
        let comparing = self.comparing.active();
        let focus = self.comparing.timeline;
        let range = focus
            .map(|focus| {
                (
                    focus.from.unwrap_or(focus.cursor),
                    focus.to.unwrap_or(len).max(focus.from.unwrap_or(0)),
                )
            })
            .or_else(|| self.compared_columns(len));
        CompareBar {
            baseline: self.baseline_labels(),
            summary: summary(self.compare_counts(), comparing),
            timeline: match timeline_mode {
                TimelineMode::Always => len > 0,
                TimelineMode::Auto => len >= 2 && (comparing || focus.is_some()),
            },
            pushes,
            range,
            cursor: focus.map(|focus| focus.cursor),
            note: self.last_push_summary(),
        }
    }

    /// The columns of the pushes compared, when the baseline is a time on the timeline.
    fn compared_columns(&self, len: usize) -> Option<(usize, usize)> {
        let history = self.stack_history.as_ref()?;
        let column_of = |at: u64| {
            history
                .pushes
                .iter()
                .position(|push| push.at_ms > at)
                .unwrap_or(len)
        };
        match self.comparing.baseline() {
            Baseline::PreviousPush if len > 0 && self.comparing.picks().is_empty() => {
                Some((len - 1, len))
            }
            Baseline::Between { from_ms, to_ms } if self.comparing.picks().is_empty() => {
                Some((column_of(from_ms), to_ms.map_or(len, column_of)))
            }
            _ => None,
        }
    }

    /// Whether the comparison row shows.
    pub(super) fn shows_compare_bar(&self) -> bool {
        self.compares_pulls()
    }

    /// Paint the comparison row on `area` (one row).
    pub(super) fn paint_compare_bar(&mut self, area: Rect, cx: &mut PaintCx<'_, '_>) {
        cx.fill(
            LocalRect::new(area.x.into(), area.y.into(), area.width, 1),
            " ",
            self.theme.header_bar,
        );
        let model = self.compare_bar_model();
        let plan = compare_bar::plan(&model, &self.theme, area.width);
        cx.paint_component(
            &TextBlock::new(Text::from_lines(vec![plan.line.clone()])),
            area,
        );
        self.comparing.bar = Some((area, plan));
    }

    /// Whether the comparison row and the banner show in a screen `height` rows tall (`ready`:
    /// the review is open, with a header).
    pub(super) fn compare_rows(&self, ready: bool, height: u16) -> (bool, bool) {
        let bar = ready && height >= 8 && self.shows_compare_bar();
        (bar, bar && height >= 12 && self.shows_compare_banner())
    }

    /// Paint the comparison row on `row` when `bar`, and the banner on the row under it when
    /// `banner` (forgetting what is not painted, so it takes no clicks).
    pub(super) fn paint_compare_rows(
        &mut self,
        row: Rect,
        bar: bool,
        banner: bool,
        cx: &mut PaintCx<'_, '_>,
    ) {
        if bar {
            let style = self.theme.header_bar;
            cx.with_style(style, |cx| self.paint_compare_bar(row, cx));
        } else {
            self.comparing.bar = None;
        }
        if banner {
            self.paint_compare_banner(Rect::new(row.x, row.y + 1, row.width, 1), cx);
        } else {
            self.comparing.banner_plan = None;
        }
    }

    /// The banner's content.
    fn banner_model(&self) -> compare_bar::Banner {
        let counts = self.compare_counts();
        let when = self.baseline_labels()[0]
            .strip_prefix("you last looked · ")
            .map(|ago| format!(" {ago}"))
            .unwrap_or_default();
        let mut found = vec![plural(
            counts.changed,
            "pull request changed",
            "pull requests changed",
        )];
        if counts.rebased > 0 {
            found.push(format!("{} only rebased", counts.rebased));
        }
        let key = |action: &str, fallback: &str| {
            self.action_keys
                .get(action)
                .cloned()
                .unwrap_or_else(|| fallback.to_owned())
        };
        compare_bar::Banner {
            text: [
                format!("Changed since you looked{when}: {}.", found.join(", ")),
                format!("{} changed, {} rebased", counts.changed, counts.rebased),
            ],
            view: (
                key("compare_view", "shift+l"),
                match self.comparing.view() {
                    CompareView::Full => "Only what changed".to_owned(),
                    CompareView::Changes => "Full change".to_owned(),
                },
            ),
            choose: key("compare_baseline", "b"),
        }
    }

    /// Whether the banner shows.
    pub(super) fn shows_compare_banner(&self) -> bool {
        self.comparing.banner_shown() && self.comparing.active()
    }

    /// Paint the banner on `area` (one row).
    pub(super) fn paint_compare_banner(&mut self, area: Rect, cx: &mut PaintCx<'_, '_>) {
        cx.fill(
            LocalRect::new(area.x.into(), area.y.into(), area.width, 1),
            " ",
            self.theme.canvas,
        );
        let plan = compare_bar::plan_banner(&self.banner_model(), &self.theme, area.width);
        cx.paint_component(
            &TextBlock::new(Text::from_lines(vec![plan.line.clone()])),
            area,
        );
        self.comparing.banner_plan = Some((area, plan));
    }

    /// Route a pointer event on the comparison row or the banner.
    pub(super) fn compare_mouse(&mut self, event: &Event, host: &UiContext) -> Option<Action> {
        let Event::Mouse(mouse) = event else {
            return None;
        };
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return None;
        }
        let modified = mouse.modifiers.shift || mouse.modifiers.ctrl || mouse.modifiers.alt;
        if let Some((area, plan)) = &self.comparing.bar
            && area.contains(mouse.position)
        {
            let part = plan.part_at(mouse.position.x.saturating_sub(area.x));
            match part {
                Some(BarPart::Control | BarPart::Summary) => self.open_compare_menu(),
                Some(BarPart::Tick { first, last }) => self.click_tick(first, last, modified, host),
                None => {}
            }
            return Some(Action::Redraw);
        }
        if let Some((area, plan)) = &self.comparing.banner_plan
            && area.contains(mouse.position)
        {
            match plan.part_at(mouse.position.x.saturating_sub(area.x)) {
                Some(BannerPart::View) => self.toggle_compare_view(),
                Some(BannerPart::Choose) => self.open_compare_menu(),
                Some(BannerPart::Dismiss) => self.dismiss_compare_banner(),
                None => {}
            }
            return Some(Action::Redraw);
        }
        None
    }

    /// A click on a tick: compare from before its first push to now (with a modifier: keep the
    /// start, and compare up to after its last push).
    fn click_tick(&mut self, first: usize, last: usize, modified: bool, host: &UiContext) {
        let Some(history) = self.stack_history.as_ref() else {
            return;
        };
        let len = history.pushes.len();
        let (from, to) = if modified {
            let from = self
                .compared_columns(len)
                .map_or(first, |(from, _)| from.min(first));
            (from, last + 1)
        } else {
            (first, len)
        };
        self.compare_columns(from, to, host);
    }

    /// Compare every layer between timeline columns `from` and `to`.
    fn compare_columns(&mut self, from: usize, to: usize, host: &UiContext) {
        let Some(history) = self.stack_history.as_ref() else {
            return;
        };
        let (from, to) = (from.min(to), from.max(to));
        let Some(from_ms) = column_time(history, from) else {
            self.set_status("nothing to compare: that is now", StatusSeverity::Info);
            return;
        };
        let to_ms = column_time(history, to);
        self.set_compare_baseline(Baseline::Between { from_ms, to_ms }, host);
    }

    /// Open the "Changes since…" menu.
    pub(super) fn open_compare_menu(&mut self) {
        if !self.compares_pulls() {
            self.set_status(
                "comparisons need pull request layers whose stack is known",
                StatusSeverity::Info,
            );
            return;
        }
        let current = if self.comparing.picks().is_empty() {
            Some(match self.comparing.baseline() {
                Baseline::LastLooked => MenuChoice::LastLooked,
                Baseline::PreviousPush => MenuChoice::PreviousPush,
                Baseline::Between { .. } => MenuChoice::Push,
                Baseline::Off => MenuChoice::Off,
            })
        } else {
            Some(MenuChoice::Version)
        };
        let number = self.current_layer_number();
        let state = MenuState {
            current,
            only_changed: self.comparing.only_changed(),
            changes_view: self.comparing.view() == CompareView::Changes,
            offers: crate::compare_menu::MenuOffers {
                pushes: self
                    .stack_history
                    .as_ref()
                    .is_some_and(|history| !history.pushes.is_empty()),
                versions: number.is_some_and(|number| {
                    self.stack_history.as_ref().is_some_and(|history| {
                        history
                            .layers
                            .iter()
                            .any(|layer| layer.number == number && layer.versions.len() >= 2)
                    })
                }),
            },
            view_key: self.action_keys.get("compare_view").cloned(),
        };
        self.modal = Some(Modal::Compare(CompareMenu::new(&state, &self.theme)));
    }

    /// What the menu chose.
    pub(super) fn compare_menu_outcome(
        &mut self,
        outcome: MenuOutcome<MenuChoice>,
        host: &UiContext,
    ) -> Action {
        let choice = match outcome {
            MenuOutcome::Close => {
                self.modal = None;
                return Action::Redraw;
            }
            MenuOutcome::Redraw => return Action::Redraw,
            MenuOutcome::Ignored => return Action::None,
            MenuOutcome::Chose(choice) => choice,
        };
        self.modal = None;
        match choice {
            MenuChoice::LastLooked => self.set_compare_baseline(Baseline::LastLooked, host),
            MenuChoice::PreviousPush => self.set_compare_baseline(Baseline::PreviousPush, host),
            MenuChoice::Ago(ms) => self.set_compare_baseline(
                Baseline::Between {
                    from_ms: super::super::history::now_ms().saturating_sub(ms),
                    to_ms: None,
                },
                host,
            ),
            MenuChoice::Off => self.set_compare_baseline(Baseline::Off, host),
            MenuChoice::Push => self.focus_timeline(),
            MenuChoice::Version => self.open_version_picker(host),
            MenuChoice::OnlyChanged => self.toggle_only_changed(),
            MenuChoice::View => self.toggle_compare_view(),
        }
        Action::Redraw
    }

    /// Give the push timeline the keyboard, on the newest push.
    pub(super) fn focus_timeline(&mut self) {
        let Some(len) = self
            .stack_history
            .as_ref()
            .map(|history| history.pushes.len())
            .filter(|len| *len > 0)
        else {
            self.set_status("no push of the stack is recorded yet", StatusSeverity::Info);
            return;
        };
        self.comparing.timeline = Some(TimelineFocus {
            cursor: len - 1,
            from: None,
            to: None,
        });
        self.set_status(
            "pushes: ←→ move · f from · t to · enter compare · esc leave",
            StatusSeverity::Info,
        );
    }

    /// Keys while the push timeline has the keyboard (it takes every plain key).
    pub(super) fn timeline_key(&mut self, stroke: KeyStroke, host: &UiContext) -> Option<Action> {
        let mut focus = self.comparing.timeline?;
        let len = self
            .stack_history
            .as_ref()
            .map_or(0, |history| history.pushes.len());
        if len == 0 {
            self.comparing.timeline = None;
            return None;
        }
        match stroke.key {
            KeyCode::Left | KeyCode::Char('h') => focus.cursor = focus.cursor.saturating_sub(1),
            KeyCode::Right | KeyCode::Char('l') => focus.cursor = (focus.cursor + 1).min(len - 1),
            KeyCode::Char('f') => focus.from = Some(focus.cursor),
            KeyCode::Char('t') => focus.to = Some(focus.cursor + 1),
            KeyCode::Enter => {
                let from = focus.from.unwrap_or(focus.cursor);
                let to = focus.to.unwrap_or(len);
                self.comparing.timeline = None;
                self.compare_columns(from, to, host);
                return Some(Action::Redraw);
            }
            KeyCode::Escape | KeyCode::Char('q') => {
                self.comparing.timeline = None;
                self.status = None;
                return Some(Action::Redraw);
            }
            _ => return Some(Action::None),
        }
        self.comparing.timeline = Some(focus);
        if let Some(push) = self
            .stack_history
            .as_ref()
            .and_then(|history| history.pushes.get(focus.cursor))
        {
            let text = format!(
                "push {} of {len}, {}: {}",
                focus.cursor + 1,
                super::super::history::ago(push.at_ms, super::super::history::now_ms()),
                bcode_code_review::history::push_summary(push)
            );
            self.set_status(text, StatusSeverity::Info);
        }
        Some(Action::Redraw)
    }

    /// Open the version picker of the current pull request.
    pub(super) fn open_version_picker(&mut self, host: &UiContext) {
        let Some(number) = self.current_layer_number() else {
            self.set_status(
                "put the cursor in a pull request first",
                StatusSeverity::Info,
            );
            return;
        };
        let Some(layer) = self
            .stack_history
            .as_ref()
            .and_then(|history| history.layers.iter().find(|layer| layer.number == number))
            .cloned()
        else {
            self.load_history_quietly(host);
            self.set_status(
                "reading the pull request's versions; try again in a moment",
                StatusSeverity::Info,
            );
            return;
        };
        if layer.versions.len() < 2 {
            self.set_status(
                format!("#{number} has one version recorded: nothing to compare yet"),
                StatusSeverity::Info,
            );
            return;
        }
        // Preselect what the layer is compared with now.
        let picked = self.comparing.picks().get(&number).cloned();
        let index_of = |head: &str| {
            layer
                .versions
                .iter()
                .rposition(|version| version.version.head_oid == head)
        };
        let from = picked
            .as_ref()
            .and_then(|pair| pair.from.as_ref())
            .and_then(|from| index_of(&from.head_oid));
        let to = picked.as_ref().and_then(|pair| index_of(&pair.to.head_oid));
        let looks = self.comparing.looks_of(number);
        self.modal = Some(Modal::Versions(VersionPicker::new(
            layer,
            looks,
            from,
            to,
            super::super::history::now_ms(),
            &self.theme,
        )));
    }

    /// What the version picker chose.
    pub(super) fn version_outcome(
        &mut self,
        outcome: MenuOutcome<crate::compare_menu::VersionPick>,
        number: u64,
        host: &UiContext,
    ) -> Action {
        match outcome {
            MenuOutcome::Close => {
                self.modal = None;
                Action::Redraw
            }
            MenuOutcome::Redraw => Action::Redraw,
            MenuOutcome::Ignored => Action::None,
            MenuOutcome::Chose(pick) => {
                self.modal = None;
                self.pick_versions(number, pick.from, pick.to, host);
                Action::Redraw
            }
        }
    }

    /// The comparison's actions (choosing the baseline, the filter, versions, pushes, and
    /// stepping a file through its versions); `None` for any other action.
    pub(super) fn compare_action(&mut self, action: &str, host: &UiContext) -> Option<Action> {
        Some(match action {
            "compare_baseline" => {
                self.open_compare_menu();
                Action::Redraw
            }
            "compare_only_changed" => {
                self.toggle_only_changed();
                Action::Redraw
            }
            "compare_off" => {
                self.set_compare_baseline(
                    bcode_code_review_models::ReviewCompareBaseline::Off,
                    host,
                );
                Action::Redraw
            }
            "compare_last_looked" => {
                self.set_compare_baseline(
                    bcode_code_review_models::ReviewCompareBaseline::LastLooked,
                    host,
                );
                Action::Redraw
            }
            "compare_versions" => {
                self.open_version_picker(host);
                Action::Redraw
            }
            "compare_push" => {
                self.focus_timeline();
                Action::Redraw
            }
            "file_version_previous" | "file_version_next" => {
                self.step_file_version(action == "file_version_next");
                Action::Redraw
            }
            _ => return None,
        })
    }

    /// Whether `file`'s lines refuse comments: it shows an older version.
    pub(super) fn refuse_historical(
        &mut self,
        file: &bcode_code_review::document::FileKey,
    ) -> bool {
        let historical = self
            .review
            .as_ref()
            .is_some_and(|review| review.doc.file_is_historical(file));
        if historical {
            self.set_status(
                "this file shows an older version: comment on the current code (b · Full change \
                 or You last looked)",
                StatusSeverity::Info,
            );
        }
        historical
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_summary_says_progress_then_what_was_found() {
        let counts = CompareCounts {
            layers: 21,
            pending: 15,
            ..CompareCounts::default()
        };
        assert_eq!(summary(counts, true)[0], "comparing 6 of 21…");
        let counts = CompareCounts {
            layers: 21,
            changed: 3,
            rebased: 18,
            ..CompareCounts::default()
        };
        assert_eq!(
            summary(counts, true),
            [
                "3 of 21 PRs changed · 18 rebased only".to_owned(),
                "3/21 changed".to_owned()
            ]
        );
        assert_eq!(summary(counts, false), [String::new(), String::new()]);
    }
}
