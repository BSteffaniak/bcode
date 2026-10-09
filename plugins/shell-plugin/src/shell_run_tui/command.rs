//! One measured command presentation for draft, request, live and result cards.

use super::{format_shell_command_for_display, prompt_spans, prompt_style, shell_command_spans};
#[cfg(test)]
use super::{partial_json_string, shell_command_projection};
use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::composition::TextBlock;
use bmux_tui::prelude::{Line, Span};
use bmux_tui::text::{Text, TextWrap};

pub(super) fn project(
    command: &str,
    cwd: Option<&str>,
    format_commands: bool,
    width: u16,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
) -> bcode_plugin_sdk::tui::PluginTuiVisualProjection {
    let mut result = bcode_plugin_sdk::tui::PluginTuiVisualProjection::default();
    if width == 0 || command.is_empty() {
        return result;
    }
    let display = if format_commands {
        format_shell_command_for_display(command)
    } else {
        command.to_owned()
    };
    let prompt = command_prompt(cwd, width, context);
    let mut logical = Vec::new();
    let mut source_lines = Vec::new();
    let mut display_offset = 0;
    let mut decorated_offset = 0;
    for (index, source) in display.split('\n').enumerate() {
        let prefix = if index == 0 {
            prompt.clone()
        } else {
            Line::default()
        };
        let prefix_bytes = prefix.plain_text().len();
        let mut spans = prefix.spans;
        spans.extend(shell_command_spans(source));
        source_lines.push((
            display_offset,
            decorated_offset + prefix_bytes,
            source.len(),
        ));
        logical.push(Line::from_spans(spans));
        display_offset += source.len() + 1;
        decorated_offset += prefix_bytes + source.len() + 1;
    }
    // Character wrapping preserves every source whitespace unit; BMUX retains source ranges.
    // Selection cells use BMUX's standard geometry. Its text projection treats tabs as
    // zero-width source bytes whereas plain_text_fragments expands tab stops; tab rows retain
    // source ranges but deliberately omit cells rather than claiming inconsistent hit geometry.
    let block = TextBlock::new(Text { lines: logical }).wrap(TextWrap::Character);
    let layout = block.layout(
        Constraints::for_width(u64::from(width)),
        &mut LayoutCx::new(),
    );
    let source_revision = |source: &str| {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        source.hash(&mut hash);
        hash.finish()
    };
    let identity = if display == command {
        "shell-command:raw".to_owned()
    } else {
        // Formatter output has its own source, never invented original-command byte mappings.
        format!("shell-command:formatted:{:016x}", source_revision(&display))
    };
    for measured in block.projection(&layout) {
        let row = result.rows.len();
        let visible = measured.line.width() <= usize::from(width);
        let line = measured.line.viewport(0, usize::from(width));
        let (base, decorated_start, length) = source_lines[measured.source_line];
        let selection_identity = format!("{identity}:line:{}", measured.source_line);
        let revision = source_revision(display.get(base..base + length).unwrap_or_default());
        let start = measured.source_range.start.max(decorated_start);
        let end = measured.source_range.end.min(decorated_start + length);
        if visible && start <= end {
            let source_start = base + start - decorated_start;
            let source_end = base + end - decorated_start;
            result
                .anchors
                .push(bcode_plugin_sdk::tui_visual::TuiVisualAnchor {
                    key: format!("command-byte:{source_start}"),
                    row,
                    source: Some(bcode_plugin_sdk::tui_visual::TuiVisualSourceRange {
                        identity: identity.clone(),
                        start: source_start,
                        end: source_end,
                    }),
                });
            let text = display[source_start..source_end].to_owned();
            let prefix_bytes = start.saturating_sub(measured.source_range.start);
            let rendered = measured.line.plain_text();
            let column = bmux_tui::text_width::display_width(&rendered[..prefix_bytes]);
            let column = u16::try_from(column).unwrap_or(width).min(width);
            let cells = command_selection_cells(
                &text,
                &selection_identity,
                source_start,
                revision,
                column,
                width,
            );
            result.selection.push((
                row,
                bcode_plugin_sdk::tui::PluginTuiSelectionRow {
                    identity: selection_identity,
                    byte_start: source_start,
                    text,
                    cells,
                    revision,
                },
            ));
        }
        result.rows.push(line);
    }
    result
}

fn command_prompt(
    cwd: Option<&str>,
    width: u16,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
) -> Line {
    let full_prompt = Line::from_spans(prompt_spans(cwd, context));
    // Prefer source over decorative cwd. At tiny widths even the marker is omitted.
    if full_prompt.width() <= usize::from(width / 2) {
        full_prompt
    } else if width >= 4 {
        Line::from_spans(vec![Span::styled("❯ ", prompt_style())])
    } else {
        Line::default()
    }
}

fn command_selection_cells(
    text: &str,
    identity: &str,
    source_start: usize,
    revision: u64,
    column: u16,
    width: u16,
) -> Vec<bcode_plugin_sdk::tui::PluginTuiSelectionCell> {
    if text.contains('\t') {
        Vec::new()
    } else {
        bmux_tui::selection::plain_text_fragments(
            "shell-command",
            identity,
            bmux_tui::geometry::Rect::new(column, 0, width - column, 1),
            0,
            text,
            source_start,
            revision,
        )
        .into_iter()
        .filter(|fragment| !fragment.source_range.is_empty())
        .map(|fragment| bcode_plugin_sdk::tui::PluginTuiSelectionCell {
            column: fragment.area.x,
            width: fragment.area.width,
            bytes: fragment.source_range,
        })
        .collect()
    }
}

pub(super) fn append_metadata(
    target: &mut bcode_plugin_sdk::tui::PluginTuiVisualProjection,
    command: bcode_plugin_sdk::tui::PluginTuiVisualProjection,
) {
    target.anchors.extend(command.anchors);
    target.selection.extend(command.selection);
}

#[cfg(test)]
pub(super) fn declared_copy(
    projection: &bcode_plugin_sdk::tui::PluginTuiVisualProjection,
) -> String {
    let mut copied = String::new();
    let mut previous = None;
    for (_, row) in &projection.selection {
        if previous.is_some_and(|identity| identity != row.identity.as_str()) {
            copied.push('\n');
        }
        copied.push_str(&row.text);
        previous = Some(row.identity.as_str());
    }
    copied
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(width: u16) -> bcode_plugin_sdk::tui::PluginTuiVisualRenderContext {
        bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
            width,
            bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
            None,
        )
    }

    #[test]
    fn wrapping_preserves_source_and_selection_at_every_width() {
        let source = "python -c 'print(\"a  b\")'\n  界😀e\u{301}\tend\n";
        for width in [0, 1, 2, 3, 4, 8, 16, 80] {
            let projected = project(
                source,
                Some("/very/long/界/😀/working/directory"),
                false,
                width,
                &context(width),
            );
            assert!(
                projected
                    .rows
                    .iter()
                    .all(|line| line.width() <= usize::from(width))
            );
            for (row, selection) in &projected.selection {
                assert!(selection.is_valid());
                let end = selection.byte_start + selection.text.len();
                assert_eq!(selection.text, source[selection.byte_start..end]);
                assert!(
                    selection
                        .cells
                        .iter()
                        .all(|cell| cell.column + cell.width <= width)
                );
                assert!(projected.anchors.iter().any(|anchor| anchor.row == *row));
            }
            if width >= 4 {
                let recovered = projected
                    .selection
                    .iter()
                    .map(|(_, row)| row.text.as_str())
                    .collect::<String>();
                assert_eq!(recovered, source.replace('\n', ""));
            }
        }
    }

    #[test]
    fn long_scripts_heredocs_and_invalid_shell_are_wrapped_without_formatting() {
        for source in [
            "python -c 'import pathlib; print(\"many   quoted spaces preserved\")'",
            "cat <<'PY'\n    print('hello  world')\nPY",
            "if unfinished '  quoted",
        ] {
            let projected = project(source, None, false, 12, &context(12));
            assert!(projected.rows.len() > 1);
            assert!(projected.rows.iter().all(|line| line.width() <= 12));
            assert_eq!(
                projected
                    .selection
                    .iter()
                    .map(|(_, row)| row.text.as_str())
                    .collect::<String>(),
                source.replace('\n', "")
            );
        }
    }

    #[test]
    fn streamed_drafts_never_format_even_when_json_is_complete() {
        let context = context(24);
        let source = "if true;then echo 'a  b';fi";
        let json = serde_json::json!({"command": source, "format_commands": true}).to_string();
        for end in (0..=json.len()).filter(|end| json.is_char_boundary(*end)) {
            let payload = serde_json::json!({"preview": &json[..end]});
            let projected = shell_command_projection(&payload, &context);
            let decoded = partial_json_string(&json[..end], "command").unwrap_or_default();
            let recovered = projected
                .selection
                .iter()
                .map(|(_, row)| row.text.as_str())
                .collect::<String>();
            assert_eq!(recovered, decoded);
            assert!(projected.rows.iter().all(|line| line.width() <= 24));
        }
        let final_payload = serde_json::json!({"command": source});
        let final_projection = shell_command_projection(&final_payload, &context);
        assert!(
            final_projection
                .selection
                .iter()
                .all(|(_, row)| row.identity.starts_with("shell-command:formatted:"))
        );
    }

    #[test]
    fn styling_partitions_source_without_normalizing_whitespace() {
        for source in [
            "  echo  'a  b'\t--flag",
            "printf \"quoted  text\"",
            "echo escaped\\ space",
            "  界😀e\u{301} ",
        ] {
            assert_eq!(
                Line::from_spans(shell_command_spans(source)).plain_text(),
                source
            );
        }
    }

    #[test]
    fn formatted_selection_maps_to_display_not_execution_source() {
        let source = "if true;then echo hi;fi\ncat <<'PY'\nprint('long  quoted whitespace in a heredoc')\nPY";
        let display = format_shell_command_for_display(source);
        assert_ne!(source, display);
        let projected = project(source, None, true, 10, &context(10));
        for (_, selection) in &projected.selection {
            let end = selection.byte_start + selection.text.len();
            assert_eq!(selection.text, display[selection.byte_start..end]);
            assert!(selection.identity.starts_with("shell-command:formatted:"));
        }
        // Same logical line identity joins soft rows; newline-separated lines differ.
        let same_line = projected
            .selection
            .windows(2)
            .any(|pair| pair[0].1.identity == pair[1].1.identity);
        let different_line = projected
            .selection
            .windows(2)
            .any(|pair| pair[0].1.identity != pair[1].1.identity);
        assert!(same_line && different_line);
        assert_eq!(declared_copy(&projected), display);
    }

    #[test]
    fn literal_copy_keeps_indentation_tabs_blank_lines_and_trailing_newlines() {
        let source = "  python - <<'PY'\n\tprint('a  b')\n\nPY\n";
        for width in [4, 12, 80] {
            let projected = project(source, Some("/tmp/long-cwd"), false, width, &context(width));
            assert_eq!(declared_copy(&projected), source);
            for (_, row) in &projected.selection {
                if row.text.contains('\t') {
                    // Locked BMUX does not expose tab-expanded measured source geometry.
                    // Do not publish its incompatible eight-cell selection approximation.
                    assert!(row.cells.is_empty());
                }
            }
        }
    }

    #[test]
    fn width_changes_keep_source_identity_and_containment() {
        let source = "echo 'one  two three four five six seven'";
        let narrow = project(source, None, false, 12, &context(12));
        let wide = project(source, None, false, 28, &context(28));
        for anchor in narrow.anchors {
            let source = anchor.source.unwrap();
            assert!(
                wide.anchors
                    .iter()
                    .any(|anchor| anchor.source.as_ref().is_some_and(|candidate| {
                        candidate.identity == source.identity
                            && (candidate.start..candidate.end).contains(&source.start)
                    }))
            );
        }
    }
}
