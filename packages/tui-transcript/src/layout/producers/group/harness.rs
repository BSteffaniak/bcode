//! Test harness shared by the group, tool and visual producer tests: resolve real structure
//! for a list of items, derive units, produce each unit through the standard registry, lay it
//! out once and paint it into a buffer of exactly the measured size.

use std::sync::Arc;

use bcode_presentation_policy::models::{LayerSource, PresentationRule, RuleLayer};
use bcode_presentation_policy::{CompiledPolicy, PolicyRegistry};
use bcode_session_view_models::{
    ChatMessageView, PermissionView, ToolInvocationView, ToolInvocationViewStatus, ToolResultView,
    ToolTimingView, TranscriptChanges, TranscriptViewItem, TranscriptViewItemId,
    TranscriptViewItemKind,
};
use bmux_tui::buffer::Buffer;
use bmux_tui::component::{Constraints, LayoutCx, LayoutNode, LogicalRect};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::paint::PaintCx;

use crate::layout::context::RenderConfig;
use crate::layout::geometry::{ItemAction, ItemGeometry};
use crate::layout::producers::{ProducerRegistry, UnitProducer, UnitView};
use crate::units::structure::{DisclosureTarget, StructureConfig, StructureResolver};
use crate::units::{UnitKey, Units};
use crate::window::WindowMode;

pub fn id(text: &str) -> TranscriptViewItemId {
    TranscriptViewItemId::new(text)
}

pub fn item(name: &str, kind: TranscriptViewItemKind) -> TranscriptViewItem {
    TranscriptViewItem {
        id: id(name),
        revision: 1,
        sequence: Some(1),
        timestamp_ms: None,
        output_location: None,
        streaming: false,
        kind,
    }
}

pub fn user(name: &str) -> TranscriptViewItem {
    item(
        name,
        TranscriptViewItemKind::UserMessage {
            message: ChatMessageView::markdown(name),
        },
    )
}

pub fn invocation(
    tool: &str,
    status: ToolInvocationViewStatus,
    arguments: Option<&str>,
    output: Option<&str>,
) -> ToolInvocationView {
    ToolInvocationView {
        tool_call_id: format!("call-{tool}"),
        producer_plugin_id: None,
        tool_name: Some(tool.to_owned()),
        arguments_json: arguments.map(str::to_owned),
        working_directory: None,
        request_draft: None,
        status,
        result_text: None,
        is_error: matches!(status, ToolInvocationViewStatus::Failed).then_some(true),
        result: output.map(|text| ToolResultView::Text {
            text: text.to_owned(),
        }),
        presentation: None,
        timing: ToolTimingView::default(),
    }
}

pub fn tool_item(name: &str, tool: ToolInvocationView) -> TranscriptViewItem {
    item(
        name,
        TranscriptViewItemKind::ToolInvocation {
            tool: Box::new(tool),
        },
    )
}

pub fn tool(name: &str, status: ToolInvocationViewStatus) -> TranscriptViewItem {
    let mut tool = invocation("read", status, Some(r#"{"path":"a.rs"}"#), Some("line 1"));
    tool.tool_call_id = format!("call-{name}");
    tool_item(name, tool)
}

pub fn permission(name: &str) -> TranscriptViewItem {
    item(
        name,
        TranscriptViewItemKind::Permission {
            permission: PermissionView {
                permission_id: format!("perm-{name}"),
                session_id: None,
                tool_call_id: format!("call-{name}"),
                tool_name: "shell".to_owned(),
                producer_plugin_id: None,
                arguments_json: r#"{"command":"rm -rf build"}"#.to_owned(),
                batch: None,
                agent_id: String::new(),
                title: Some("Run command".to_owned()),
                policy_source: None,
                detail: None,
                resolved: false,
                approved: None,
                can_remember: false,
            },
        },
    )
}

/// Structure configuration compiled from configuration-layer rules.
pub fn config(rules: serde_json::Value) -> StructureConfig {
    let rules: Vec<PresentationRule> = serde_json::from_value(rules).expect("rules");
    let (policy, diagnostics) = CompiledPolicy::compile(
        &[RuleLayer {
            source: LayerSource::Config,
            rules,
        }],
        &PolicyRegistry::with_host_parts(),
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    StructureConfig {
        policy: Arc::new(policy),
        ..StructureConfig::default()
    }
}

/// One produced, measured and painted unit.
#[derive(Debug)]
pub struct Rendered {
    pub key: UnitKey,
    pub node: LayoutNode,
    pub rows: Vec<String>,
    pub geometry: ItemGeometry,
}

impl Rendered {
    pub fn height(&self) -> u64 {
        self.node.size.height
    }

    pub fn text(&self) -> String {
        self.rows.join("\n")
    }

    /// Row whose painted text contains `needle`.
    pub fn row_of(&self, needle: &str) -> Option<u64> {
        self.rows
            .iter()
            .position(|row| row.contains(needle))
            .map(|row| row as u64)
    }

    /// Hit region toggling `target`.
    pub fn hit(&self, target: &DisclosureTarget) -> Option<LogicalRect> {
        self.geometry
            .hit_regions
            .iter()
            .find(|region| region.action == ItemAction::ToggleDisclosure(target.clone()))
            .map(|region| region.rect)
    }

    /// First row of the focus target for `target`.
    pub fn focus_row(&self, target: &DisclosureTarget) -> Option<u64> {
        self.geometry
            .focus_targets
            .iter()
            .find(|focus| focus.disclosure.as_ref() == Some(target))
            .map(|focus| focus.rows.start)
    }
}

/// Resolved structure and derived units for `items`.
pub struct Fixture {
    pub items: Vec<TranscriptViewItem>,
    pub resolver: StructureResolver,
    pub units: Units,
    pub render: RenderConfig,
}

impl Fixture {
    pub fn new(config: StructureConfig, items: Vec<TranscriptViewItem>) -> Self {
        let mut fixture = Self {
            items,
            resolver: StructureResolver::new(config),
            units: Units::new(),
            render: RenderConfig::default(),
        };
        fixture.refresh();
        fixture
    }

    /// Re-resolve and re-derive after an override or configuration change.
    pub fn refresh(&mut self) {
        let _ = self
            .resolver
            .update(&TranscriptChanges::default(), &self.items);
        let _ = self.units.apply(
            &TranscriptChanges::default(),
            &self.items,
            WindowMode::Live,
            &self.resolver,
        );
    }

    pub fn toggle(&mut self, target: &DisclosureTarget) {
        let _ = self.resolver.toggle(target);
        self.refresh();
    }

    pub fn keys(&self) -> Vec<UnitKey> {
        self.units.keys().to_vec()
    }

    /// Produce, lay out (once) and paint `key` at `width`.
    pub fn render(&self, key: &UnitKey, width: u16) -> Rendered {
        let unit = self.units.entry(key).expect("unit");
        let item = key
            .item_id()
            .and_then(|id| self.units.item(&self.items, id));
        let structure = self.resolver.unit_presentation(key);
        let cx = self.render.context(key, u64::from(width), structure);
        let produced = ProducerRegistry::standard().produce(UnitView { unit, item }, &cx);
        let node = produced.content.layout(
            Constraints::for_width(u64::from(width)),
            &mut LayoutCx::new(),
        );
        assert!(
            node.size.width <= u64::from(width),
            "{key:?} wider than {width}"
        );
        let height = u16::try_from(node.size.height).expect("height");
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        let mut frame = Frame::new(&mut buffer);
        if width > 0 && height > 0 {
            produced.content.paint(&node, &mut PaintCx::new(&mut frame));
        }
        let rows = (0..height)
            .map(|row| frame.buffer().row_symbols(row).unwrap_or_default())
            .collect();
        let geometry = produced.content.geometry(&node);
        Rendered {
            key: key.clone(),
            node,
            rows,
            geometry,
        }
    }

    pub fn render_item(&self, name: &str, width: u16) -> Rendered {
        self.render(&UnitKey::Item(id(name)), width)
    }
}

/// Geometry agreement every rendered unit must satisfy at its width (B3, B7, B12): hit regions,
/// focus targets, selection fragments, content keys and paint slots lie inside the measured
/// node, and every disclosure hit region sits on its focus target's first row.
pub fn assert_geometry_inside(rendered: &Rendered, width: u16) {
    let height = rendered.height();
    let width = u64::from(width);
    let geometry = &rendered.geometry;
    for region in &geometry.hit_regions {
        let rect = region.rect;
        assert!(
            rect.x + rect.width <= width && rect.y + rect.height <= height && rect.width > 0,
            "{:?}: hit region {rect:?} outside {width}x{height}",
            rendered.key
        );
        if let ItemAction::ToggleDisclosure(target) = &region.action {
            assert_eq!(
                rendered.focus_row(target),
                Some(rect.y),
                "{:?}: hit region and focus target rows agree",
                rendered.key
            );
        }
    }
    for focus in &geometry.focus_targets {
        assert!(focus.rows.end <= height, "{focus:?} outside {height}");
    }
    for fragment in &geometry.selection_fragments {
        assert!(fragment.rows.end <= height, "{fragment:?} outside {height}");
    }
    for key in &geometry.content_keys {
        assert!(key.rows.end <= height, "{key:?} outside {height}");
    }
    for slot in &geometry.paint_slots {
        let rect = slot.rect;
        assert!(
            rect.x + rect.width <= width && rect.y + rect.height <= height,
            "{slot:?} outside {width}x{height}"
        );
    }
    for row in geometry.source_map.rows() {
        assert!(row.row < height.max(1), "{row:?} outside {height}");
    }
}
