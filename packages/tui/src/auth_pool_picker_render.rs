//! Account-first subscription terminal rendering.

use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::composition::TextBlock;
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::Line;
use bmux_tui::text::TextWrap;

use super::auth_pool_picker::{AuthPoolPickerApp, Page};
use super::render::TuiTheme;

/// Render bounded account content; BMUX owns clipping, wrapping and list geometry.
pub fn render_auth_pool_picker_with_keymap(
    app: &mut AuthPoolPickerApp,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
    keymap: &super::keymap::BmuxKeyMap,
) {
    let area = Rect::new(0, 0, frame.area().width, frame.area().height);
    let inner = super::picker_render::render_picker_panel(" Accounts ", area, frame, theme);
    if inner.is_empty() {
        return;
    }
    if app.page == Page::Accounts {
        render_accounts(app, inner, frame, theme, keymap);
    } else {
        render_form(app, inner, frame, theme, keymap);
    }
}

fn render_accounts(
    app: &mut AuthPoolPickerApp,
    inner: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
    keymap: &super::keymap::BmuxKeyMap,
) {
    let headers = [
        format!(
            "This session: {}",
            app.route_label(&app.snapshot.session_route)
        ),
        format!(
            "New sessions: {} · {}",
            app.route_label(&app.snapshot.new_session_route),
            app.snapshot.model_profile.as_deref().unwrap_or("no preset")
        ),
    ];
    // At tiny heights prioritize an actionable list over chrome. No invisible hits.
    let header_count = if inner.height >= 5 { 2 } else { 0 };
    for (offset, text) in headers.into_iter().take(header_count).enumerate() {
        paint_line(
            frame,
            Rect::new(
                inner.x,
                inner.y + u16::try_from(offset).unwrap_or(0),
                inner.width,
                1,
            ),
            &Line::from(text),
            theme,
        );
    }
    let footer_height = u16::from(inner.height >= 3);
    let list_area = Rect::new(
        inner.x,
        inner.y + u16::try_from(header_count).unwrap_or(0),
        inner.width,
        inner
            .height
            .saturating_sub(u16::try_from(header_count).unwrap_or(0) + footer_height),
    );
    if app.snapshot.accounts.is_empty() {
        paint_line(
            frame,
            list_area,
            &Line::from("No accounts configured."),
            theme,
        );
    } else {
        app.render_list(list_area, frame, theme);
    }
    if footer_height > 0 {
        let hint = format!(
            "↑↓ · {} Use · {} Pause/Resume · {} Rename · {} Details · {} Reset · {} Close",
            keymap.hint("auth_pool_picker", "tui.control.submit"),
            keymap.hint("auth_pool_picker", "tui.auth.pause"),
            keymap.hint("auth_pool_picker", "tui.auth.rename"),
            keymap.hint("auth_pool_picker", "tui.auth.details"),
            keymap.hint("auth_pool_picker", "tui.auth.clear"),
            keymap.hint("auth_pool_picker", "tui.control.cancel")
        );
        paint_line(
            frame,
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            &Line::from(hint),
            theme,
        );
    }
}

fn render_form(
    app: &mut AuthPoolPickerApp,
    inner: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
    keymap: &super::keymap::BmuxKeyMap,
) {
    let text = app
        .form_lines()
        .iter()
        .map(Line::plain_text)
        .collect::<Vec<_>>()
        .join("\n");
    let text = TextBlock::new(text).style(theme.text).wrap(TextWrap::Word);
    let layout = text.layout(
        Constraints::new(0, u64::from(inner.width), 0, None),
        &mut LayoutCx::new(),
    );
    let rows = text.projection(&layout);
    let input_height = u16::from(app.page == Page::Rename);
    let footer_height = u16::from(inner.height >= 3);
    let height = inner.height.saturating_sub(input_height + footer_height);
    app.scroll = app
        .scroll
        .min(rows.len().saturating_sub(usize::from(height.max(1))));
    for (offset, row) in rows
        .iter()
        .skip(app.scroll)
        .take(usize::from(height))
        .enumerate()
    {
        paint_line(
            frame,
            Rect::new(
                inner.x,
                inner.y + u16::try_from(offset).unwrap_or(0),
                inner.width,
                1,
            ),
            &row.line,
            theme,
        );
    }
    if app.page == Page::Rename {
        super::picker_render::paint_picker_input(
            &mut app.label,
            Rect::new(
                inner.x,
                inner.bottom().saturating_sub(1 + footer_height),
                inner.width,
                1,
            ),
            "Friendly label",
            frame,
            theme,
        );
    }
    if footer_height > 0 {
        let hint = if app.submitting {
            "Saving account settings…".to_owned()
        } else if app.page == Page::Details {
            format!(
                "{} Back · PgUp/PgDn scroll",
                keymap.hint("auth_pool_picker", "tui.control.cancel")
            )
        } else {
            format!(
                "{} Apply · {} Back · PgUp/PgDn scroll",
                keymap.hint("auth_pool_picker", "tui.control.submit"),
                keymap.hint("auth_pool_picker", "tui.control.cancel")
            )
        };
        paint_line(
            frame,
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            &Line::from(hint),
            theme,
        );
    }
}

fn paint_line(frame: &mut PaintCx<'_, '_>, area: Rect, line: &Line, theme: TuiTheme) {
    if !area.is_empty() {
        frame.write_line_with_fallback_style(
            LocalRect::terminal(Rect::new(area.x, area.y, area.width, 1)),
            line,
            theme.text,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::{buffer::Buffer, frame::Frame};

    #[test]
    fn readiness_usage_and_route_selection_remain_independent() {
        let theme = TuiTheme::for_theme_id("terminal-native");
        let keymap =
            super::super::keymap::BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());
        let mut snapshot = super::super::auth_pool_picker::tests::snapshot();
        snapshot.last_used = Some(snapshot.accounts[1].account.clone());
        snapshot.accounts[0].readiness =
            bcode_provider_auth_models::AccountReadiness::Cooldown { until_unix: 1234 };
        snapshot.session_route = bcode_provider_auth_models::AccountRoute::Only {
            account: snapshot.accounts[0].account.clone(),
        };
        let mut app = AuthPoolPickerApp::new(snapshot);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 12));
        let mut frame = Frame::new(&mut buffer);
        render_auth_pool_picker_with_keymap(
            &mut app,
            &mut PaintCx::new(&mut frame),
            theme,
            &keymap,
        );
        let text = (0..12)
            .filter_map(|row| frame.buffer().row_symbols(row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("only account"));
        assert!(text.contains("Cooldown until 1234"));
        assert!(text.contains(
            "Work (provider.example / work) · Not paused · Readiness unverified · last used"
        ));
        assert!(!text.contains("Ready"));
    }

    #[test]
    fn stacked_list_reveals_selected_account_after_resize() {
        use bmux_keyboard::{KeyCode, KeyStroke};
        let theme = TuiTheme::for_theme_id("terminal-native");
        let keymap =
            super::super::keymap::BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());
        let mut app = AuthPoolPickerApp::new(super::super::auth_pool_picker::tests::snapshot());
        let _ = app.handle_event(
            &bmux_tui::event::Event::Key(KeyStroke::simple(KeyCode::Down)),
            &keymap,
        );
        for width in [40, 12, 40] {
            let area = Rect::new(0, 0, width, 1);
            let mut buffer = Buffer::empty(area);
            let mut frame = Frame::new(&mut buffer);
            app.render_list(area, &mut PaintCx::new(&mut frame), theme);
            assert!(
                frame
                    .buffer()
                    .row_symbols(0)
                    .expect("visible account row")
                    .contains("Work")
            );
            assert_eq!(
                app.selected()
                    .expect("selection survives resize")
                    .account
                    .profile,
                "work"
            );
        }
    }

    #[test]
    fn account_forms_render_at_zero_tiny_and_wide_sizes_with_unicode() {
        let theme = TuiTheme::for_theme_id("terminal-native");
        let keymap =
            super::super::keymap::BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());
        for (width, height) in [(0, 0), (1, 1), (4, 8), (12, 12), (32, 20), (100, 30)] {
            let mut app = AuthPoolPickerApp::new(super::super::auth_pool_picker::tests::snapshot());
            for page in [
                Page::Accounts,
                Page::Use,
                Page::Pause,
                Page::Rename,
                Page::Details,
                Page::Confirm,
            ] {
                app.page = page;
                let area = Rect::new(0, 0, width, height);
                let mut buffer = Buffer::empty(area);
                let mut frame = Frame::new(&mut buffer);
                render_auth_pool_picker_with_keymap(
                    &mut app,
                    &mut PaintCx::new(&mut frame),
                    theme,
                    &keymap,
                );
                assert_eq!(frame.area(), area);
                if width == 100 {
                    let text = (0..height)
                        .filter_map(|row| frame.buffer().row_symbols(row))
                        .collect::<Vec<_>>()
                        .join("\n");
                    match page {
                        Page::Accounts => {
                            assert!(text.contains("This session: Follow configuration"));
                            assert!(text.contains("New sessions: Follow configuration · sol"));
                            assert!(text.contains("個人 👩‍💻 e\u{301}"));
                            assert!(!text.contains("pool-one"));
                            assert!(!text.contains("last used"));
                        }
                        Page::Details => assert!(text.contains("pool-one, pool-two")),
                        Page::Use => assert!(text.contains("Model unchanged")),
                        _ => {}
                    }
                }
            }
        }
    }
}
