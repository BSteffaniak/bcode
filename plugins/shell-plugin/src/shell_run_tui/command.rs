//! One measured, source-backed command presentation for every lifecycle state.

use crate::presentation::document::{Document, Provenance, Role, SourceOrigin};
use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::composition::TextBlock;
use bmux_tui::prelude::{Line, Span};
use bmux_tui::text::{Text, TextWrap};
use std::ops::Range;
#[cfg(feature = "static-bundled")]
use std::sync::{Arc, Mutex};

/// One retained semantic document; rendering geometry remains BMUX-owned and width-specific.
#[cfg(feature = "static-bundled")]
#[derive(Default)]
pub(super) struct PresentationState {
    state: Mutex<AcceptedDocument>,
}

#[cfg(feature = "static-bundled")]
#[derive(Default)]
struct AcceptedDocument {
    generation: u64,
    accepted: Option<(bcode_config::ShellOutputPresentationPolicy, Arc<Document>)>,
}

#[cfg(feature = "static-bundled")]
impl PresentationState {
    pub(super) fn invalidate(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.generation = state.generation.saturating_add(1);
            state.accepted = None;
        }
    }

    fn prepare(
        &self,
        source: &str,
        policy: &bcode_config::ShellOutputPresentationPolicy,
    ) -> Arc<Document> {
        let generation = if let Ok(mut state) = self.state.lock() {
            if let Some((accepted_policy, document)) = &state.accepted
                && accepted_policy == policy
                && document.execution_text() == source
            {
                return Arc::clone(document);
            }
            state.generation = state.generation.saturating_add(1);
            state.generation
        } else {
            return Arc::new(crate::presentation::present(source, policy));
        };
        let document = Arc::new(crate::presentation::present(source, policy));
        self.accept(generation, policy, Arc::clone(&document));
        document
    }

    fn accept(
        &self,
        generation: u64,
        policy: &bcode_config::ShellOutputPresentationPolicy,
        document: Arc<Document>,
    ) -> bool {
        // Oversized literal fallback is never retained as a semantic parser cache entry.
        if document.execution_text().len() > policy.max_source_bytes.min(64 * 1024) {
            return false;
        }
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.generation != generation {
            return false;
        }
        state.accepted = Some((policy.clone(), document));
        true
    }
}

#[cfg(all(test, feature = "static-bundled"))]
use super::{partial_json_string, shell_command_projection};

struct Segment {
    display: Range<usize>,
    provenance: Provenance,
}

#[cfg(all(test, feature = "static-bundled"))]
pub(super) fn project(
    command: &str,
    cwd: Option<&str>,
    format_commands: bool,
    width: u16,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
) -> bcode_plugin_sdk::tui::PluginTuiVisualProjection {
    let policy = bcode_config::ShellToolOutputConfig::default()
        .resolve_presentation_policy(Some(format_commands));
    project_with_policy(command, cwd, &policy, width, context)
}

#[cfg(all(test, feature = "static-bundled"))]
pub(super) fn project_with_policy(
    command: &str,
    cwd: Option<&str>,
    policy: &bcode_config::ShellOutputPresentationPolicy,
    width: u16,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
) -> bcode_plugin_sdk::tui::PluginTuiVisualProjection {
    project_with_state(
        command,
        cwd,
        policy,
        width,
        context,
        &PresentationState::default(),
    )
}

#[cfg(feature = "static-bundled")]
pub(super) fn project_with_state(
    command: &str,
    cwd: Option<&str>,
    policy: &bcode_config::ShellOutputPresentationPolicy,
    width: u16,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    state: &PresentationState,
) -> bcode_plugin_sdk::tui::PluginTuiVisualProjection {
    let document = state.prepare(command, policy);
    let context = crate::shell_portable::CommandContext::from_native(context);
    let projection = project_document(command, cwd, width, &context, &document);
    bcode_plugin_sdk::tui::PluginTuiVisualProjection {
        rows: projection.rows,
        anchors: projection.anchors,
        selection: projection.selection,
    }
}

/// Project the exact producer semantic document through BMUX once.
pub fn project_document(
    command: &str,
    cwd: Option<&str>,
    width: u16,
    context: &crate::shell_portable::CommandContext,
    document: &Document,
) -> crate::shell_portable::CommandProjection {
    let mut result = crate::shell_portable::CommandProjection::default();
    if width == 0 || command.is_empty() {
        return result;
    }
    let (logical, segments) = logical_document(document, cwd, width, context);
    let block = TextBlock::new(Text { lines: logical }).wrap(TextWrap::Character);
    let layout = block.layout(
        Constraints::for_width(u64::from(width)),
        &mut LayoutCx::new(),
    );
    let revision = source_revision(command);
    let identity = "shell-command:original";
    let mut frontier = 0;
    let mut segment_cursor = 0;
    let mut anchor_end = 0;
    let mut anchor_start = None;
    for measured in block.projection(&layout) {
        let row = result.rows.len();
        while segments
            .get(segment_cursor)
            .is_some_and(|segment| segment.display.end <= measured.source_range.start)
        {
            segment_cursor += 1;
        }
        let relevant = &segments[segment_cursor..];
        let anchor = measured_anchor(document, relevant, &measured.source_range);
        let mut cells = measured_cells(document, relevant, &measured, width, revision);
        let end = relevant
            .iter()
            .take_while(|segment| segment.display.start < measured.source_range.end)
            .filter_map(|segment| {
                let Provenance::Source { id, range } = &segment.provenance else {
                    return None;
                };
                (id.0 == 0).then(|| {
                    range.start + measured.source_range.end.min(segment.display.end)
                        - segment.display.start
                })
            })
            .max()
            .unwrap_or(frontier)
            .max(frontier)
            .max(
                cells
                    .iter()
                    .map(|cell| cell.bytes.end)
                    .max()
                    .unwrap_or(frontier),
            );
        // Copy chunks retain omitted original whitespace, not generated indentation or decoded
        // source. One source identity joins soft and structural rows without fabricated newlines.
        cells.retain(|cell| cell.bytes.start >= frontier && cell.bytes.end <= end);
        cells.truncate(4096);
        if end - frontier <= 256 * 1024 {
            result.selection.push((
                row,
                bcode_plugin_models::tui_selection::PluginTuiSelectionRow {
                    identity: identity.to_owned(),
                    byte_start: frontier,
                    text: command[frontier..end].to_owned(),
                    cells,
                    revision,
                },
            ));
        }
        result
            .anchors
            .push(bcode_plugin_models::tui_visual::TuiVisualAnchor {
                key: format!("shell-command:{row}"),
                row,
                source: {
                    admitted_anchor(
                        identity,
                        anchor.unwrap_or(frontier),
                        end,
                        &mut anchor_start,
                        &mut anchor_end,
                    )
                },
            });
        frontier = end;
        result
            .rows
            .push(measured.line.viewport(0, usize::from(width)));
    }
    if let Some((_, last)) = result.selection.last_mut()
        && command.len() - last.byte_start <= 256 * 1024
    {
        command[last.byte_start..].clone_into(&mut last.text);
    }
    if !document.issues.is_empty() {
        result.rows.extend(bmux_tui::text::wrap_line_bounded(
            &Line::from_spans(vec![Span::styled(
                "  embedded presentation limited · original source retained",
                context.muted_style(),
            )]),
            bmux_tui::text::TextWrapGeometry::uniform(usize::from(width)),
            TextWrap::Character,
        ));
    }
    result
}

// Rewritten/nested rows can share a source association. Preserve only non-overlapping
// source anchors; other rows retain generated-row keys rather than invented byte geometry.
fn admitted_anchor(
    identity: &str,
    start: usize,
    end: usize,
    previous_start: &mut Option<usize>,
    previous_end: &mut usize,
) -> Option<bcode_plugin_models::tui_visual::TuiVisualSourceRange> {
    if start < *previous_end || *previous_start == Some(start) {
        return None;
    }
    let end = end.max(start);
    *previous_start = Some(start);
    *previous_end = end;
    Some(bcode_plugin_models::tui_visual::TuiVisualSourceRange {
        identity: identity.to_owned(),
        start,
        end,
    })
}

fn measured_anchor(
    document: &Document,
    relevant: &[Segment],
    measured: &Range<usize>,
) -> Option<usize> {
    relevant
        .iter()
        .take_while(|segment| segment.display.start < measured.end)
        .find_map(|segment| {
            let Provenance::Source { id, range } = &segment.provenance else {
                return None;
            };
            let start = measured.start.max(segment.display.start);
            let end = measured.end.min(segment.display.end);
            let provenance = Provenance::Source {
                id: *id,
                range: range.start + start - segment.display.start
                    ..range.start + end - segment.display.start,
            };
            original_range(document, &provenance).map(|range| range.start)
        })
}

fn logical_document(
    document: &Document,
    cwd: Option<&str>,
    width: u16,
    context: &crate::shell_portable::CommandContext,
) -> (Vec<Line>, Vec<Segment>) {
    let mut logical = Vec::new();
    let mut segments = Vec::new();
    let mut offset = 0;
    for (index, line) in document.lines().into_iter().enumerate() {
        let mut spans = if index == 0 {
            command_prompt(cwd, width, context).spans
        } else {
            Vec::new()
        };
        offset += spans.iter().map(|span| span.content.len()).sum::<usize>();
        for fragment in line.fragments {
            let end = offset + fragment.text.len();
            segments.push(Segment {
                display: offset..end,
                provenance: fragment.provenance,
            });
            spans.push(Span::styled(
                fragment.text,
                role_style(fragment.role, context),
            ));
            offset = end;
        }
        logical.push(Line::from_spans(spans));
        offset += 1;
    }
    (logical, segments)
}

fn measured_cells(
    document: &Document,
    relevant: &[Segment],
    measured: &bmux_tui::composition::TextProjectionRow,
    width: u16,
    revision: u64,
) -> Vec<bcode_plugin_models::tui_selection::PluginTuiSelectionCell> {
    let rendered = measured.line.plain_text();
    // BMUX text projection and selection disagree on tab expansion; do not invent geometry.
    if measured.line.width() > usize::from(width) || rendered.contains('\t') {
        return Vec::new();
    }
    let mut cells = Vec::new();
    for fragment in bmux_tui::selection::plain_text_fragments(
        "shell-command",
        "shell-command:original",
        bmux_tui::geometry::Rect::new(0, 0, width, 1),
        0,
        &rendered,
        measured.source_range.start,
        revision,
    ) {
        if fragment.source_range.is_empty() {
            continue;
        }
        if let Some(segment) = relevant
            .iter()
            .take_while(|segment| segment.display.start < fragment.source_range.end)
            .find(|segment| {
                segment.display.start <= fragment.source_range.start
                    && segment.display.end >= fragment.source_range.end
            })
            && let Provenance::Source { id, range } = &segment.provenance
        {
            let decoded = range.start + fragment.source_range.start - segment.display.start
                ..range.start + fragment.source_range.end - segment.display.start;
            if let Some(ranges) = document.original_ranges(*id, decoded)
                && let (Some(first), Some(last)) = (ranges.first(), ranges.last())
                && ranges.windows(2).all(|pair| pair[0].end == pair[1].start)
            {
                cells.push(bcode_plugin_models::tui_selection::PluginTuiSelectionCell {
                    column: fragment.area.x,
                    width: fragment.area.width,
                    bytes: first.start..last.end,
                });
            }
        }
    }
    cells
}

fn original_range(document: &Document, provenance: &Provenance) -> Option<Range<usize>> {
    let Provenance::Source { id, range } = provenance else {
        return None;
    };
    let mut id = *id;
    let mut range = range.clone();
    if let Some(ranges) = document.original_ranges(id, range.clone())
        && let (Some(first), Some(last)) = (ranges.first(), ranges.last())
    {
        return Some(first.start..last.end);
    }
    for _ in 0..document.sources.len() {
        match &document.sources.get(id.0)?.origin {
            SourceOrigin::Original => return Some(range),
            SourceOrigin::Formatted { .. } => return document.original_region(id),
            SourceOrigin::Decoded { parent, encoded } => {
                id = *parent;
                range = encoded.clone();
            }
        }
    }
    None
}

fn source_revision(source: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hash);
    hash.finish()
}

fn role_style(
    role: Role,
    context: &crate::shell_portable::CommandContext,
) -> bmux_tui::prelude::Style {
    match role {
        Role::Command | Role::Function => context.command_style(),
        Role::Keyword | Role::Flag => context.flag_style(),
        Role::Variable | Role::Identifier => {
            context.syntax_style(|syntax| syntax.variable, bmux_tui::prelude::Color::Blue)
        }
        Role::Number => {
            context.syntax_style(|syntax| syntax.number, bmux_tui::prelude::Color::Magenta)
        }
        Role::Operator => context.operator_style(),
        Role::String => context.string_style(),
        Role::Comment => context.muted_style(),
        Role::Literal | Role::Whitespace => context.argument_style(),
    }
}

fn command_prompt(
    cwd: Option<&str>,
    width: u16,
    context: &crate::shell_portable::CommandContext,
) -> Line {
    let full_prompt = Line::from_spans(context.prompt_spans(cwd));
    if full_prompt.width() <= usize::from(width / 2) {
        full_prompt
    } else if width >= 4 {
        Line::from_spans(vec![Span::styled("❯ ", context.prompt_style())])
    } else {
        Line::default()
    }
}

#[cfg(feature = "static-bundled")]
pub(super) fn append_metadata(
    target: &mut bcode_plugin_sdk::tui::PluginTuiVisualProjection,
    command: bcode_plugin_sdk::tui::PluginTuiVisualProjection,
) {
    target.anchors.extend(command.anchors);
    target.selection.extend(command.selection);
}

#[cfg(all(test, feature = "static-bundled"))]
pub(super) fn declared_copy(
    projection: &bcode_plugin_sdk::tui::PluginTuiVisualProjection,
) -> String {
    projection
        .selection
        .iter()
        .map(|(_, row)| row.text.as_str())
        .collect()
}

#[cfg(all(test, feature = "static-bundled"))]
mod tests {
    use super::*;

    const fn context(width: u16) -> bcode_plugin_sdk::tui::PluginTuiVisualRenderContext {
        bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
            width,
            bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
            None,
        )
    }

    #[test]
    fn accepted_document_reuses_exact_source_and_rejects_stale_completion() {
        let state = PresentationState::default();
        let policy = bcode_config::ShellOutputPresentationPolicy::default();
        let first = state.prepare("echo first", &policy);
        assert!(Arc::ptr_eq(&first, &state.prepare("echo first", &policy)));
        let stale_generation = state.state.lock().unwrap().generation;
        let latest = state.prepare("echo latest", &policy);
        assert!(!state.accept(stale_generation, &policy, first));
        assert!(Arc::ptr_eq(&latest, &state.prepare("echo latest", &policy)));
        state.invalidate();
        assert!(!state.accept(stale_generation + 1, &policy, Arc::clone(&latest)));
        assert!(!Arc::ptr_eq(
            &latest,
            &state.prepare("echo latest", &policy)
        ));
        let mut literal = policy.clone();
        literal.enabled = false;
        let changed_policy = state.prepare("echo latest", &literal);
        assert!(!Arc::ptr_eq(&latest, &changed_policy));
        let oversized = "x".repeat(64 * 1024 + 1);
        assert!(!Arc::ptr_eq(
            &state.prepare(&oversized, &policy),
            &state.prepare(&oversized, &policy)
        ));
    }

    #[test]
    fn measured_geometry_and_copy_preserve_original_at_every_width() {
        for source in [
            "if true;then echo 'a  b';else echo 界😀e\u{301};fi",
            "  python - <<'PY'\n\tprint('a  b')\n\nPY\n",
            "node -e 'console.log(\"界😀\"); console.log(2)'",
            "bash -c 'if true;then python -c '\"'\"'print(1)'\"'\"';fi'",
            "if unfinished '  quoted",
        ] {
            for width in [0, 1, 2, 3, 4, 8, 16, 80] {
                for format in [false, true] {
                    let projected = project(
                        source,
                        Some("/long/界/😀/cwd"),
                        format,
                        width,
                        &context(width),
                    );
                    assert!(
                        projected
                            .rows
                            .iter()
                            .all(|line| line.width() <= usize::from(width))
                    );
                    if width == 0 {
                        assert!(projected.selection.is_empty());
                        continue;
                    }
                    assert_eq!(declared_copy(&projected), source);
                    for (row, selection) in &projected.selection {
                        assert!(selection.is_valid());
                        assert_eq!(
                            selection.text,
                            source
                                [selection.byte_start..selection.byte_start + selection.text.len()]
                        );
                        assert!(
                            selection
                                .cells
                                .iter()
                                .all(|cell| cell.column + cell.width <= width)
                        );
                        assert!(projected.anchors.iter().any(|anchor| anchor.row == *row));
                    }
                }
            }
        }
    }

    #[test]
    fn progressive_drafts_keep_source_copy_authority() {
        let source = "if true;then echo 'a  b';else echo 界;fi";
        for (end, _) in source.char_indices().skip(1) {
            let source = &source[..end];
            let preview = format!("{{\"command\":{}", serde_json::to_string(source).unwrap());
            let decoded = partial_json_string(&preview, "command").unwrap();
            let projected =
                shell_command_projection(&serde_json::json!({"preview": preview}), &context(20));
            assert_eq!(declared_copy(&projected), decoded);
            assert!(projected.rows.iter().all(|line| line.width() <= 20));
        }
    }

    #[test]
    fn selection_cells_use_measured_painted_graphemes_and_original_bytes() {
        let source = "echo 界😀e\u{301} 'a  b'";
        for width in [2, 3, 4, 9, 80] {
            let projected = project(source, None, false, width, &context(width));
            for (index, selection) in &projected.selection {
                for cell in &selection.cells {
                    let painted = projected.rows[*index]
                        .viewport(usize::from(cell.column), usize::from(cell.width))
                        .plain_text();
                    assert_eq!(painted, source[cell.bytes.clone()]);
                }
            }
            assert_eq!(declared_copy(&projected), source);
        }
    }

    #[test]
    fn width_changes_keep_original_identity_and_revision() {
        let source = "echo 'one  two three four five six seven'";
        let narrow = project(source, None, true, 12, &context(12));
        let wide = project(source, None, true, 28, &context(28));
        assert_eq!(declared_copy(&narrow), declared_copy(&wide));
        for (_, row) in &narrow.selection {
            assert!(
                wide.selection
                    .iter()
                    .all(|(_, other)| other.identity == row.identity
                        && other.revision == row.revision)
            );
        }
        let changed = project("echo changed", None, true, 12, &context(12));
        assert_ne!(
            narrow.selection[0].1.revision,
            changed.selection[0].1.revision
        );
    }
}
