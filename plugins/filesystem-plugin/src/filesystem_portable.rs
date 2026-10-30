//! Producer-owned source/diff presentation shared by native and WASM BMUX hosts.
use bcode_tui_components::transcript::{
    tool_visual::{
        CanonicalPluginVisual,
        portable::{
            PortableArtifactChunk, PortableVisualAdapter, PortableVisualContext,
            PortableVisualDescriptor, PortableVisualOutput, PortableVisualRegistration,
        },
    },
    transcript_layout::TranscriptLayoutRows,
};
use bcode_tui_components::{
    diff_viewer::{DiffViewerLayout, DiffViewerStyle},
    source_viewer::SourceViewerStyle,
};
use bmux_tui::prelude::Style;
use std::collections::{BTreeMap, BTreeSet};

/// Local BMUX-boundary context, with no plugin SDK or host capability.
pub trait VisualContext {
    fn width(&self) -> u16;
    fn diff_layout(&self) -> DiffViewerLayout;
    fn theme(&self) -> Option<VisualTheme>;
    fn display_path(&self, path: impl AsRef<std::path::Path>) -> String;
}

#[derive(Clone, Copy)]
pub struct VisualTheme {
    pub text: Style,
    pub muted: Style,
    pub focused: Style,
    pub source: SourceViewerStyle,
    pub diff: DiffViewerStyle,
    pub syntax: bcode_syntax_render::SyntaxPalette,
}

impl VisualContext for PortableVisualContext<'_> {
    fn width(&self) -> u16 {
        self.width
    }
    fn diff_layout(&self) -> DiffViewerLayout {
        DiffViewerLayout::Auto { breakpoint: 120 }
    }
    fn display_path(&self, path: impl AsRef<std::path::Path>) -> String {
        path.as_ref().display().to_string()
    }
    fn theme(&self) -> Option<VisualTheme> {
        self.theme.map(|theme| VisualTheme {
            text: theme.text,
            muted: theme.muted,
            focused: theme.focused,
            source: SourceViewerStyle {
                source: theme.text,
                border: theme.border,
                gutter: theme.muted,
                truncated: theme.muted,
            },
            diff: DiffViewerStyle {
                text: theme.text,
                muted: theme.muted,
                title: theme.text,
                label: theme.muted,
                ..DiffViewerStyle::default()
            },
            syntax: bcode_syntax_render::SyntaxPalette::default(),
        })
    }
}

#[cfg(feature = "static-bundled")]
impl VisualContext for bcode_plugin_sdk::tui::PluginTuiVisualRenderContext {
    fn width(&self) -> u16 {
        self.width()
    }
    fn diff_layout(&self) -> DiffViewerLayout {
        use bcode_plugin_sdk::tui::PluginTuiDiffLayout;
        match self.diff_layout() {
            PluginTuiDiffLayout::Auto { breakpoint } => DiffViewerLayout::Auto { breakpoint },
            PluginTuiDiffLayout::Unified => DiffViewerLayout::Unified,
            PluginTuiDiffLayout::SideBySide => DiffViewerLayout::SideBySide,
        }
    }
    fn display_path(&self, path: impl AsRef<std::path::Path>) -> String {
        self.display_path(path).to_string()
    }
    fn theme(&self) -> Option<VisualTheme> {
        self.theme().map(|theme| VisualTheme {
            text: theme.text,
            muted: theme.muted,
            focused: theme.focused,
            source: SourceViewerStyle {
                source: theme.source.source,
                border: theme.source.border,
                gutter: theme.source.gutter,
                truncated: theme.source.truncated,
            },
            diff: DiffViewerStyle {
                text: theme.diff.text,
                muted: theme.diff.muted,
                title: theme.diff.title,
                label: theme.diff.label,
                added: theme.diff.added,
                removed: theme.diff.removed,
                hunk: theme.diff.hunk,
                added_row: theme.diff.added_row,
                removed_row: theme.diff.removed_row,
                added_emphasis: theme.diff.added_emphasis,
                removed_emphasis: theme.diff.removed_emphasis,
            },
            syntax: bcode_syntax_render::SyntaxPalette {
                text: bcode_syntax_render::SyntaxColor::from_tui(theme.syntax.text.into()),
                comment: bcode_syntax_render::SyntaxColor::from_tui(theme.syntax.comment.into()),
                keyword: bcode_syntax_render::SyntaxColor::from_tui(theme.syntax.keyword.into()),
                function: bcode_syntax_render::SyntaxColor::from_tui(theme.syntax.function.into()),
                variable: bcode_syntax_render::SyntaxColor::from_tui(theme.syntax.variable.into()),
                string: bcode_syntax_render::SyntaxColor::from_tui(theme.syntax.string.into()),
                number: bcode_syntax_render::SyntaxColor::from_tui(theme.syntax.number.into()),
                type_name: bcode_syntax_render::SyntaxColor::from_tui(
                    theme.syntax.type_name.into(),
                ),
                operator: bcode_syntax_render::SyntaxColor::from_tui(theme.syntax.operator.into()),
                punctuation: bcode_syntax_render::SyntaxColor::from_tui(
                    theme.syntax.punctuation.into(),
                ),
                heading: bcode_syntax_render::SyntaxColor::from_tui(theme.syntax.heading.into()),
                link: bcode_syntax_render::SyntaxColor::from_tui(theme.syntax.link.into()),
                raw: bcode_syntax_render::SyntaxColor::from_tui(theme.syntax.raw.into()),
            },
        })
    }
}

#[derive(Default)]
struct FilesystemVisual {
    change: crate::file_change_tui::FileChangeTuiVisualAdapter,
}
impl PortableVisualAdapter for FilesystemVisual {
    fn artifact_preview_limit(
        &self,
        reference_key: &str,
        content_type: Option<&str>,
    ) -> Option<u64> {
        crate::file_change_tui::source_preview_limit(reference_key, content_type)
    }

    fn artifact_chunk(&self, chunk: &PortableArtifactChunk<'_>) -> bool {
        self.change.deliver(chunk).is_ok()
    }

    fn render(
        &self,
        visual: &CanonicalPluginVisual,
        context: &PortableVisualContext<'_>,
    ) -> Option<PortableVisualOutput> {
        let mut payload = visual.payload.clone();
        let object = payload.as_object_mut()?;
        object.insert(
            "_bcode_presentation_revision".into(),
            visual.revision.into(),
        );
        if let Some(invocation) = &visual.invocation_id
            && let Some(artifact) = object
                .get_mut("_bcode_artifact")
                .and_then(serde_json::Value::as_object_mut)
        {
            artifact.insert("tool_call_id".into(), invocation.clone().into());
        }
        let payload = &payload;
        let kind = visual.schema.as_str();
        crate::filesystem_tui::ACTIVE_THEME.with(|theme| theme.set(VisualContext::theme(context)));
        let result = if matches!(
            kind,
            "bcode.filesystem.batch" | "bcode.filesystem.change" | "bcode.filesystem.file_change"
        ) {
            let (rows, anchors) = self.change.project(kind, payload, context);
            Some(TranscriptLayoutRows::Anchored { rows, anchors })
        } else {
            project(kind, payload, context)
        };
        crate::filesystem_tui::ACTIVE_THEME.with(|theme| theme.set(None));
        result.map(|rows| {
            let mut output = PortableVisualOutput {
                rows,
                ..PortableVisualOutput::default()
            }
            .with_source_copy_actions();
            attach_change_copy_button(&mut output, visual, context);
            output
        })
    }
}

fn attach_change_copy_button(
    output: &mut PortableVisualOutput,
    visual: &CanonicalPluginVisual,
    context: &PortableVisualContext<'_>,
) {
    use bcode_tui_components::transcript::actions::{TranscriptAction, TranscriptActionRegion};
    use bmux_tui::{
        buffer::Buffer,
        component::{Component, Constraints, LayoutCx},
        frame::Frame,
        geometry::Rect,
        paint::PaintCx,
        prelude::{Line, Span},
    };
    use bmux_tui_components::action_row::{
        ActionButton, ActionRowComponent, ActionRowState, ActionRowStyles,
    };

    if !matches!(
        visual.schema.as_str(),
        "bcode.filesystem.change" | "bcode.filesystem.file_change"
    ) || visual.schema_version != 1
        || visual.streaming
        || context.width == 0
        || visual
            .payload
            .get("truncated")
            .is_some_and(|value| value != false)
        || visual
            .payload
            .get("omitted")
            .is_some_and(|value| value != false)
    {
        return;
    }
    let new = visual
        .payload
        .get("new_text")
        .and_then(serde_json::Value::as_str);
    let old = visual
        .payload
        .get("old_text")
        .and_then(serde_json::Value::as_str);
    let (label, text) = match (old, new) {
        (_, Some(text)) if !text.is_empty() => ("Copy new source", text),
        (Some(text), Some("")) if !text.is_empty() => ("Copy old source", text),
        _ => return,
    };
    if text.len() > 64 * 1024 {
        return;
    }
    let TranscriptLayoutRows::Anchored { rows, anchors } = &mut output.rows else {
        return;
    };
    let actions = [ActionButton::new("copy-source", label)];
    let state = std::cell::Cell::new(ActionRowState::new());
    let theme = context.theme.copied().unwrap_or_default();
    let component = ActionRowComponent::new("filesystem.change.actions", &actions, &state).styles(
        ActionRowStyles {
            normal: theme.info,
            ..ActionRowStyles::default()
        },
    );
    let area = Rect::new(0, 0, context.width, 1);
    let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    let mut buffer = Buffer::empty(area);
    let hit = {
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        frame.hits().regions().first().map(|region| region.area)
    };
    let Some(hit) = hit.filter(|area| !area.is_empty()) else {
        return;
    };
    // Only toolbar presentation is captured here; source copy authority is the semantic
    // payload above, never the rendered diff cells or its line-only anchors.
    let toolbar = Line::from_spans(
        buffer
            .cells()
            .iter()
            .filter(|cell| !cell.is_wide_continuation())
            .map(|cell| Span::styled(cell.symbol.clone(), cell.style))
            .collect::<Vec<_>>(),
    );
    rows.insert(0, toolbar);
    for anchor in anchors {
        anchor.row = anchor.row.saturating_add(1);
    }
    output.actions.push(TranscriptActionRegion {
        id: "filesystem.change.copy-source".into(),
        area: hit,
        action: TranscriptAction::Copy(text.to_owned()),
    });
}

fn project(
    kind: &str,
    payload: &serde_json::Value,
    context: &impl VisualContext,
) -> Option<TranscriptLayoutRows> {
    use crate::filesystem_tui as fs;
    let (rows, anchors) = match kind {
        "bcode.filesystem.read" | "bcode.filesystem.artifact.read" => {
            let (rows, anchors, selection) =
                fs::read_layout(kind, payload, context.width(), context);
            let mut sources: BTreeMap<_, _> = selection
                .into_iter()
                .map(|row| ((row.identity.clone(), row.byte_start), row))
                .collect();
            let selection = anchors
                .iter()
                .filter_map(|anchor| {
                    let source = anchor.source.as_ref()?;
                    Some((
                        anchor.row,
                        sources.remove(&(source.identity.clone(), source.start))?,
                    ))
                })
                .collect();
            return Some(TranscriptLayoutRows::Selected {
                rows,
                anchors,
                selection,
            });
        }
        "bcode.filesystem.change" | "bcode.filesystem.file_change" => {
            crate::file_change_tui::file_change_layout(payload, context)
        }
        "bcode.filesystem.batch" => crate::file_change_tui::batch_layout(payload, context),
        "bcode.filesystem.request" => fs::request_layout(payload, context),
        "bcode.filesystem.request-draft.write" | "bcode.filesystem.request-draft.edit" => {
            fs::request_draft_layout(kind, payload, context)
        }
        "bcode.filesystem.request-draft.multi-edit" => {
            fs::multi_edit_draft_layout(payload, context)
        }
        "bcode.filesystem.image" => (fs::image_rows(payload, context), Vec::new()),
        "bcode.filesystem.exists" => (fs::exists_rows(payload, context), Vec::new()),
        "bcode.filesystem.list" => (fs::list_rows(payload, context), Vec::new()),
        "bcode.filesystem.find" => (fs::find_rows(payload, context), Vec::new()),
        "bcode.filesystem.grep" | "bcode.filesystem.artifact.grep" => {
            (fs::grep_rows(payload, context.width(), context), Vec::new())
        }
        "bcode.filesystem.stat" | "bcode.filesystem.artifact.metadata" => {
            (fs::metadata_rows(kind, payload, context), Vec::new())
        }
        _ => return None,
    };
    Some(TranscriptLayoutRows::Anchored { rows, anchors })
}

/// Static producer source/diff registrations.
/// Statically linked source/diff adapters. The host still applies enabled-plugin
/// and selected-adapter policy; linking does not grant filesystem capabilities.
#[must_use]
pub fn portable_visual_registrations() -> Vec<PortableVisualRegistration> {
    [
        ("filesystem-image-card", "bcode.filesystem.image"),
        ("filesystem-exists-card", "bcode.filesystem.exists"),
        ("filesystem-list-card", "bcode.filesystem.list"),
        ("filesystem-find-card", "bcode.filesystem.find"),
        ("filesystem-grep-card", "bcode.filesystem.grep"),
        (
            "filesystem-artifact-grep-card",
            "bcode.filesystem.artifact.grep",
        ),
        ("filesystem-stat-card", "bcode.filesystem.stat"),
        (
            "filesystem-artifact-metadata-card",
            "bcode.filesystem.artifact.metadata",
        ),
        ("filesystem-read-card", "bcode.filesystem.read"),
        (
            "filesystem-artifact-read-card",
            "bcode.filesystem.artifact.read",
        ),
        ("filesystem-change-card", "bcode.filesystem.change"),
        ("filesystem-batch-card", "bcode.filesystem.batch"),
        ("filesystem-request-card", "bcode.filesystem.request"),
        (
            "filesystem-write-request-draft",
            "bcode.filesystem.request-draft.write",
        ),
        (
            "filesystem-edit-request-draft",
            "bcode.filesystem.request-draft.edit",
        ),
        (
            "filesystem-multi-edit-request-draft",
            "bcode.filesystem.request-draft.multi-edit",
        ),
    ]
    .into_iter()
    .map(|(id, schema)| PortableVisualRegistration {
        descriptor: PortableVisualDescriptor {
            plugin_id: "bcode.filesystem".into(),
            adapter_id: id.into(),
            schemas: BTreeSet::from([(schema.into(), 1)]),
            priority: if schema.contains("request-draft") {
                110
            } else {
                100
            },
            producer_default: true,
        },
        adapter: Box::<FilesystemVisual>::default(),
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_tui_components::transcript::tool_visual::portable::{
        PortableVisualPolicy, PortableVisualRegistry,
    };
    fn visual(schema: &str, payload: serde_json::Value) -> CanonicalPluginVisual {
        CanonicalPluginVisual {
            invocation_id: Some("fixture".into()),
            revision: 1,
            producer_plugin_id: Some("bcode.filesystem".into()),
            schema: schema.into(),
            schema_version: 1,
            title: None,
            subtitle: None,
            payload,
            streaming: false,
        }
    }
    #[test]
    fn source_rows_retain_real_source_identity_through_unicode_reflow() {
        let visual = visual(
            "bcode.filesystem.read",
            serde_json::json!({"path":"src/example.rs", "contents":"let message = \"界🙂e\u{301}\";\nsecond line\n", "start_line":5}),
        );
        let adapter = FilesystemVisual::default();
        let wide = adapter
            .render(
                &visual,
                &PortableVisualContext {
                    width: 60,
                    theme: None,
                },
            )
            .unwrap();
        let narrow = adapter
            .render(
                &visual,
                &PortableVisualContext {
                    width: 20,
                    theme: None,
                },
            )
            .unwrap();
        let TranscriptLayoutRows::Selected {
            selection: wide, ..
        } = wide.rows
        else {
            panic!("source mappings required");
        };
        let TranscriptLayoutRows::Selected {
            selection: narrow, ..
        } = narrow.rows
        else {
            panic!("source mappings required");
        };
        assert!(!wide.is_empty());
        assert!(!narrow.is_empty());
        assert!(
            wide.values()
                .all(bcode_plugin_models::tui_selection::PluginTuiSelectionRow::is_valid)
        );
        assert!(
            narrow
                .values()
                .all(bcode_plugin_models::tui_selection::PluginTuiSelectionRow::is_valid)
        );
        assert_eq!(
            wide.values().next().unwrap().identity,
            narrow.values().next().unwrap().identity
        );
        assert_eq!(
            wide.values()
                .map(|row| row.text.as_str())
                .collect::<String>(),
            narrow
                .values()
                .map(|row| row.text.as_str())
                .collect::<String>()
        );
    }
    #[test]
    fn change_copy_button_uses_semantic_source_and_committed_component_hit_geometry() {
        use bcode_tui_components::transcript::actions::TranscriptAction;
        let source = "界👩‍💻e\u{301}\n\u{1b}[31mraw\u{1b}[0m\n";
        let input = visual(
            "bcode.filesystem.change",
            serde_json::json!({
                "path":"例.rs", "old_text":"old\n", "new_text":source
            }),
        );
        for width in [0, 1, 2, 12, 80, 160] {
            let context = PortableVisualContext { width, theme: None };
            let output = FilesystemVisual::default()
                .render(&input, &context)
                .unwrap();
            if width == 0 {
                assert!(output.actions.is_empty());
                continue;
            }
            assert_eq!(output.actions.len(), 1);
            let action = &output.actions[0];
            assert_eq!(action.action, TranscriptAction::Copy(source.into()));
            assert_eq!(action.area.y, 0);
            assert_eq!(action.area.height, 1);
            assert!(action.area.width > 0 && action.area.right() <= width);
            let TranscriptLayoutRows::Anchored { rows, .. } = output.rows else {
                panic!("no manufactured selection")
            };
            let toolbar = rows[0]
                .spans
                .iter()
                .map(|span| span.content.as_str())
                .collect::<String>();
            let complete_label = "[ Copy new source ]";
            let expected: String = complete_label.chars().take(usize::from(width)).collect();
            assert!(toolbar.starts_with(&expected));
        }
        for (field, value) in [("truncated", true), ("omitted", true)] {
            let mut incomplete = input.clone();
            incomplete.payload[field] = value.into();
            let output = FilesystemVisual::default()
                .render(
                    &incomplete,
                    &PortableVisualContext {
                        width: 80,
                        theme: None,
                    },
                )
                .unwrap();
            assert!(output.actions.is_empty());
        }
        let mut streaming = input;
        streaming.streaming = true;
        assert!(
            FilesystemVisual::default()
                .render(
                    &streaming,
                    &PortableVisualContext {
                        width: 80,
                        theme: None
                    }
                )
                .unwrap()
                .actions
                .is_empty()
        );
        let deleted = visual(
            "bcode.filesystem.change",
            serde_json::json!({"old_text":source,"new_text":""}),
        );
        let output = FilesystemVisual::default()
            .render(
                &deleted,
                &PortableVisualContext {
                    width: 80,
                    theme: None,
                },
            )
            .unwrap();
        assert_eq!(
            output.actions[0].action,
            TranscriptAction::Copy(source.into())
        );
        let TranscriptLayoutRows::Anchored { rows, .. } = output.rows else {
            panic!("diff rows")
        };
        assert!(
            rows[0]
                .spans
                .iter()
                .map(|span| span.content.as_str())
                .collect::<String>()
                .contains("Copy old source")
        );
    }

    #[test]
    fn diff_uses_native_shared_component_and_manifest_identity() {
        let input = visual(
            "bcode.filesystem.change",
            serde_json::json!({"path":"src/lib.rs", "old_text":"before 界\n", "new_text":"after 🙂\n"}),
        );
        let context = PortableVisualContext {
            width: 80,
            theme: None,
        };
        let output = FilesystemVisual::default()
            .render(&input, &context)
            .unwrap();
        let (rows, anchors) = crate::file_change_tui::file_change_layout(&input.payload, &context);
        let TranscriptLayoutRows::Anchored {
            rows: projected,
            anchors: projected_anchors,
        } = &output.rows
        else {
            panic!("diff anchors")
        };
        assert_eq!(&projected[1..], rows.as_slice());
        assert_eq!(
            projected_anchors
                .iter()
                .map(|anchor| (anchor.key.as_str(), anchor.row - 1))
                .collect::<Vec<_>>(),
            anchors
                .iter()
                .map(|anchor| (anchor.key.as_str(), anchor.row))
                .collect::<Vec<_>>()
        );
        assert_eq!(output.actions.len(), 1);
        assert_eq!(
            output.actions[0].action,
            bcode_tui_components::transcript::actions::TranscriptAction::Copy("after 🙂\n".into())
        );
        let registry = PortableVisualRegistry::new(
            1,
            PortableVisualPolicy {
                enabled_plugins: BTreeSet::from(["bcode.filesystem".into()]),
                ..PortableVisualPolicy::default()
            },
            portable_visual_registrations(),
        )
        .unwrap();
        assert_eq!(
            registry
                .prepare(&input, &context)
                .unwrap()
                .adapter_reference(),
            "bcode.filesystem/filesystem-change-card"
        );
    }
    #[cfg(feature = "static-bundled")]
    #[test]
    fn native_and_portable_source_projections_match_exactly() {
        use bcode_plugin_sdk::tui::{
            PluginTuiDiffLayout, PluginTuiVisualAdapter, PluginTuiVisualRenderContext,
        };
        let input = visual(
            "bcode.filesystem.read",
            serde_json::json!({"path":"src/lib.rs", "contents":"let x = \"界🙂\";\n"}),
        );
        let context = PluginTuiVisualRenderContext::new(60, PluginTuiDiffLayout::Unified, None);
        let native = crate::filesystem_tui::FilesystemTuiVisualAdapter::default().projection(
            &input.schema,
            &input.payload,
            &context,
        );
        let portable = FilesystemVisual::default()
            .render(
                &input,
                &PortableVisualContext {
                    width: 60,
                    theme: None,
                },
            )
            .unwrap();
        assert_eq!(
            portable.rows,
            TranscriptLayoutRows::Selected {
                rows: native.rows,
                anchors: native.anchors,
                selection: native.selection.into_iter().collect()
            }
        );
    }
}
