//! Terminal-native graph editor projection for portable workflow authoring documents.

use bcode_plugin_sdk::tui::{
    BoxedPluginTuiSurface, PluginTuiAction, PluginTuiHost, PluginTuiSurface,
    PluginTuiSurfaceFuture, PluginTuiSurfaceOpenRequest, PluginWorkflowAuthoringDraft,
    PluginWorkflowAuthoringEditResult, PluginWorkflowAuthoringPublishResult,
    PluginWorkflowAuthoringRevision, PluginWorkflowStartResponse,
};
use bcode_workflow::{
    EdgeDefinition, EdgeKind, NodeDefinition, NodeKind, WORKFLOW_AUTHORING_EDIT_VERSION,
    WORKFLOW_CALL_VERSION, WorkflowAuthoringCatalogSnapshot, WorkflowAuthoringDocument,
    WorkflowAuthoringEdgeSelector, WorkflowAuthoringEdit, WorkflowAuthoringEditBatch,
    WorkflowBlockDefinition, WorkflowCallConfiguration, WorkflowCallTarget,
    WorkflowCompilationPreview, WorkflowNodeDataflowPolicy, WorkflowProducerKind,
    WorkflowProducerProvenance, WorkflowSchemaFormControl, WorkflowSchemaFormDescription,
    WorkflowValidationDiagnostic,
};
use bmux_keyboard::KeyCode;
use bmux_tui::event::{Event, MouseButton, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::style::{Color, Style};
use bmux_tui::text::Line;
use std::collections::BTreeSet;
use tokio::sync::mpsc;

const PALETTE_HEADER_ROWS: u16 = 2;
const CANVAS_HEADER_ROWS: u16 = 2;
const GRAPH_PRESENTATION_NAMESPACE: &str = "bcode.graph";
const GRAPH_PRESENTATION_VERSION: u32 = 1;

/// Open the workflow authoring graph editor.
pub fn open(request: PluginTuiSurfaceOpenRequest) -> PluginTuiSurfaceFuture {
    Box::pin(async move {
        Ok(Box::new(WorkflowAuthorSurface::new(&request.options)) as BoxedPluginTuiSurface)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorPane {
    Palette,
    Canvas,
    Inspector,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingMutation {
    Add,
    Duplicate,
    Remove,
    Connect,
    Reposition,
    SetEntry,
    SetExit,
    RemoveEdge,
    ToggleGroup,
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    CycleAgentProfile,
    ToggleAgentReadOnly,
    IncreaseRepeatBound,
    CyclePredicate,
    SetNodeName,
    SetAgentModel,
    SetAgentSkills,
    SetRepeatBound,
    SetPredicatePath,
    SetSchemaField,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InspectorEditTarget {
    NodeName,
    AgentModel,
    AgentSkills,
    RepeatBound,
    PredicatePath,
    SchemaField {
        path: String,
        control: WorkflowSchemaFormControl,
        source: InspectorSchemaSource,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InspectorSchemaSource {
    NodeConfiguration,
    PluginInputDefaults,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InspectorTextEdit {
    target: InspectorEditTarget,
    buffer: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingConflict {
    expected_generation: u64,
    current_generation: u64,
    edits: Vec<WorkflowAuthoringEdit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum AuthoringOperation {
    CatalogLoad,
    DraftLoad,
    BaseLoad,
    Edit,
    Publish,
    Publishing,
    Start,
    Starting,
}

#[derive(Debug)]
enum AuthoringAsyncResult {
    Draft(Result<Option<Box<PluginWorkflowAuthoringDraft>>, String>),
    BaseRevision(Result<Option<Box<PluginWorkflowAuthoringRevision>>, String>),
    Edit(Result<PluginWorkflowAuthoringEditResult, String>),
    Publish(Result<PluginWorkflowAuthoringPublishResult, String>),
    Start(Result<PluginWorkflowStartResponse, String>),
}

impl EditorPane {
    const fn next(self) -> Self {
        match self {
            Self::Palette => Self::Canvas,
            Self::Canvas => Self::Inspector,
            Self::Inspector => Self::Palette,
        }
    }

    const fn previous(self) -> Self {
        match self {
            Self::Palette => Self::Inspector,
            Self::Canvas => Self::Palette,
            Self::Inspector => Self::Canvas,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PaletteEntryKind {
    NodeKind,
    PluginBlock,
    WorkflowCall,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaletteEntry {
    identity: String,
    label: String,
    kind: PaletteEntryKind,
}

#[derive(Debug)]
struct WorkflowAuthorSurface {
    document: Option<WorkflowAuthoringDocument>,
    catalog: Option<WorkflowAuthoringCatalogSnapshot>,
    preview: Option<WorkflowCompilationPreview>,
    diagnostics: Vec<WorkflowValidationDiagnostic>,
    palette: Vec<PaletteEntry>,
    focus: EditorPane,
    selected_palette: usize,
    selected_node: usize,
    selected_edge: usize,
    palette_scroll: usize,
    canvas_scroll: usize,
    inspector_scroll: usize,
    selected_schema_field: usize,
    last_area: Rect,
    workflow_id: Option<String>,
    draft_id: Option<String>,
    base_revision: Option<u64>,
    base_document: Option<WorkflowAuthoringDocument>,
    generation: Option<u64>,
    parent_session_id: Option<bcode_session_models::SessionId>,
    workspace_snapshot: Option<String>,
    published_revision: Option<u64>,
    producer: WorkflowProducerProvenance,
    operations: BTreeSet<AuthoringOperation>,
    pending_mutation: Option<PendingMutation>,
    pending_edit_batch: Option<WorkflowAuthoringEditBatch>,
    pending_conflict: Option<PendingConflict>,
    inspector_edit: Option<InspectorTextEdit>,
    connect_source: Option<String>,
    catalog_sender: mpsc::UnboundedSender<Result<WorkflowAuthoringCatalogSnapshot, String>>,
    catalog_receiver: mpsc::UnboundedReceiver<Result<WorkflowAuthoringCatalogSnapshot, String>>,
    authoring_sender: mpsc::UnboundedSender<AuthoringAsyncResult>,
    authoring_receiver: mpsc::UnboundedReceiver<AuthoringAsyncResult>,
    status: String,
}

impl WorkflowAuthorSurface {
    fn new(options: &serde_json::Value) -> Self {
        let document = authoring_document(options);
        let draft = options.get("draft");
        let workflow_id = draft
            .and_then(|draft| draft.get("identity"))
            .and_then(|identity| identity.get("workflow_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let draft_id = draft
            .and_then(|draft| draft.get("identity"))
            .and_then(|identity| identity.get("draft_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let generation = draft
            .and_then(|draft| draft.get("generation"))
            .and_then(serde_json::Value::as_u64);
        let base_revision = draft
            .and_then(|draft| draft.get("base_revision"))
            .and_then(serde_json::Value::as_u64);
        let parent_session_id = options
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|value| value.parse().ok());
        let workspace_snapshot = options
            .get("workspace_snapshot")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let producer = draft
            .and_then(|draft| draft.get("producer"))
            .and_then(|producer| serde_json::from_value(producer.clone()).ok())
            .unwrap_or_else(editor_producer);
        let diagnostics = document.as_ref().map_or_else(Vec::new, |document| {
            document.validation_report().diagnostics
        });
        let (catalog_sender, catalog_receiver) = mpsc::unbounded_channel();
        let (authoring_sender, authoring_receiver) = mpsc::unbounded_channel();
        let status = if workflow_id.is_some() && draft_id.is_some() {
            "Loading mutable authored draft and portable catalog…".to_string()
        } else if document.is_some() {
            "Template preview is read-only; instantiate it as a draft to edit".to_string()
        } else {
            "Select a maintainable authoring-document template or mutable draft".to_string()
        };
        Self {
            document,
            catalog: None,
            preview: None,
            diagnostics,
            palette: Vec::new(),
            focus: EditorPane::Canvas,
            selected_palette: 0,
            selected_node: 0,
            selected_edge: 0,
            palette_scroll: 0,
            canvas_scroll: 0,
            inspector_scroll: 0,
            selected_schema_field: 0,
            last_area: Rect::new(0, 0, 0, 0),
            workflow_id,
            draft_id,
            base_revision,
            base_document: None,
            generation,
            parent_session_id,
            workspace_snapshot,
            published_revision: None,
            producer,
            operations: BTreeSet::new(),
            pending_mutation: None,
            pending_edit_batch: None,
            pending_conflict: None,
            inspector_edit: None,
            connect_source: None,
            catalog_sender,
            catalog_receiver,
            authoring_sender,
            authoring_receiver,
            status,
        }
    }

    fn node_entries(&self) -> Vec<(&str, &NodeDefinition)> {
        self.document
            .as_ref()
            .map(|document| {
                document
                    .definition
                    .nodes
                    .iter()
                    .map(|(id, node)| (id.as_str(), node))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn selected_node(&self) -> Option<(&str, &NodeDefinition)> {
        self.node_entries().get(self.selected_node).copied()
    }

    fn clamp_selection(&mut self) {
        self.selected_palette = self
            .selected_palette
            .min(self.palette.len().saturating_sub(1));
        self.selected_node = self
            .selected_node
            .min(self.node_entries().len().saturating_sub(1));
        self.selected_edge = self.selected_edge.min(
            self.document
                .as_ref()
                .map_or(0, |document| document.definition.edges.len())
                .saturating_sub(1),
        );
        self.palette_scroll = self.palette_scroll.min(self.selected_palette);
        self.canvas_scroll = self.canvas_scroll.min(self.selected_node);
    }

    fn move_selection(&mut self, delta: isize) {
        let canvas_length = self.node_entries().len();
        let (selection, length) = match self.focus {
            EditorPane::Palette => (&mut self.selected_palette, self.palette.len()),
            EditorPane::Canvas => (&mut self.selected_node, canvas_length),
            EditorPane::Inspector => {
                if delta < 0 {
                    self.inspector_scroll = self.inspector_scroll.saturating_sub(1);
                } else {
                    self.inspector_scroll = self.inspector_scroll.saturating_add(1);
                }
                return;
            }
        };
        if length == 0 {
            *selection = 0;
        } else if delta < 0 {
            *selection = selection.saturating_sub(1);
        } else {
            *selection = selection.saturating_add(1).min(length - 1);
        }
    }

    fn install_catalog(&mut self, catalog: WorkflowAuthoringCatalogSnapshot) {
        self.palette = palette_entries(&catalog);
        self.catalog = Some(catalog);
        self.refresh_preview();
        self.clamp_selection();
    }

    fn install_draft(&mut self, draft: PluginWorkflowAuthoringDraft) {
        self.workflow_id = Some(draft.workflow_id);
        self.draft_id = Some(draft.draft_id);
        self.base_revision = draft.base_revision;
        self.generation = Some(draft.generation);
        self.producer = draft.producer;
        self.document = Some(draft.document);
        self.connect_source = None;
        self.refresh_preview();
        if let Some(conflict) = &self.pending_conflict {
            self.status = format!(
                "Reloaded generation {}; conflict from generation {} is awaiting explicit R resolution",
                draft.generation, conflict.expected_generation
            );
        }
        self.clamp_selection();
    }

    fn install_base_revision(&mut self, revision: PluginWorkflowAuthoringRevision) {
        self.base_revision = Some(revision.revision);
        self.base_document = Some(revision.document);
        self.refresh_preview();
    }

    fn semantic_diff(&self) -> Option<bcode_workflow::WorkflowAuthoringSemanticDiff> {
        let base = self.base_document.as_ref()?;
        let current = self.document.as_ref()?;
        let catalog = self.catalog.as_ref()?;
        bcode_workflow::workflow_authoring_semantic_diff(base, current, catalog).ok()
    }

    fn refresh_preview(&mut self) {
        let Some(document) = &self.document else {
            self.preview = None;
            self.diagnostics.clear();
            return;
        };
        self.preview = self.catalog.as_ref().map(|catalog| {
            document.compilation_preview(catalog, document.configuration_defaults.as_ref())
        });
        if let Some(preview) = &self.preview {
            self.diagnostics.clone_from(&preview.validation.diagnostics);
            self.status = if preview.is_compiled() {
                self.generation.map_or_else(
                    || "Portable template preview compiles; instantiate it to edit".to_string(),
                    |generation| format!("Mutable draft generation {generation} compiles"),
                )
            } else {
                format!(
                    "Draft has {} source-addressed diagnostic(s)",
                    preview.validation.diagnostics.len()
                )
            };
        } else {
            let report = document.validation_report();
            self.diagnostics = report.diagnostics;
            self.status = "Loading portable authoring catalog…".to_string();
        }
    }

    fn selected_palette_entry(&self) -> Option<&PaletteEntry> {
        self.palette.get(self.selected_palette)
    }

    fn selected_schema(&self) -> Option<(bcode_workflow::ValueSchema, InspectorSchemaSource)> {
        let (_, node) = self.selected_node()?;
        if node.kind == NodeKind::PluginBlock {
            let block =
                serde_json::from_value::<WorkflowBlockDefinition>(node.configuration.clone())
                    .ok()?;
            return Some((block.input, InspectorSchemaSource::PluginInputDefaults));
        }
        self.catalog.as_ref().and_then(|catalog| {
            catalog
                .node_configuration_schemas
                .get(node_kind_identity(node.kind))
                .cloned()
                .map(|schema| (schema, InspectorSchemaSource::NodeConfiguration))
        })
    }

    fn schema_fields(&self) -> Vec<bcode_workflow::WorkflowSchemaFormField> {
        self.selected_schema()
            .and_then(|(schema, _)| WorkflowSchemaFormDescription::from_schema(&schema).ok())
            .map(|form| form.fields)
            .unwrap_or_default()
    }

    fn begin_selected_schema_edit(&mut self) {
        let fields = self.schema_fields();
        let Some(field) = fields.get(self.selected_schema_field) else {
            self.status = "No schema-generated field is selected".to_string();
            return;
        };
        if field.path.is_empty() {
            self.status = "Select a concrete nested schema field".to_string();
            return;
        }
        let Some((_, source)) = self.selected_schema() else {
            self.status = "No authoritative configuration schema is available".to_string();
            return;
        };
        self.inspector_edit = Some(InspectorTextEdit {
            target: InspectorEditTarget::SchemaField {
                path: field.path.clone(),
                control: field.control,
                source,
            },
            buffer: self.selected_schema_value(&field.path, source),
        });
        self.status = "Editing bounded inspector value · Enter apply · Esc cancel".to_string();
    }

    fn selected_schema_value(&self, path: &str, source: InspectorSchemaSource) -> String {
        let Some((node_id, node)) = self.selected_node() else {
            return String::new();
        };
        let root = match source {
            InspectorSchemaSource::NodeConfiguration => &node.configuration,
            InspectorSchemaSource::PluginInputDefaults => self
                .document
                .as_ref()
                .and_then(|document| document.plugin_input_defaults.get(node_id))
                .unwrap_or(&serde_json::Value::Null),
        };
        json_path_value(root, path).map_or_else(String::new, schema_edit_value)
    }

    fn cycle_schema_field(&mut self) {
        let field_count = self.schema_fields().len();
        if field_count > 0 {
            self.selected_schema_field = self.selected_schema_field.saturating_add(1) % field_count;
            self.status = format!(
                "Selected schema field {} of {field_count}",
                self.selected_schema_field + 1
            );
        }
    }

    fn begin_inspector_edit(&mut self, target: InspectorEditTarget) {
        if self.generation.is_none() {
            self.status = "Open an instantiated mutable draft before editing fields".to_string();
            return;
        }
        let Some((_, node)) = self.selected_node() else {
            self.status = "Select a node to edit".to_string();
            return;
        };
        let buffer = inspector_edit_value(node, &target);
        self.inspector_edit = Some(InspectorTextEdit { target, buffer });
        self.status = "Editing bounded inspector value · Enter apply · Esc cancel".to_string();
    }

    fn handle_inspector_text_input(&mut self, key: KeyCode) -> bool {
        let Some(edit) = self.inspector_edit.as_mut() else {
            return false;
        };
        match key {
            KeyCode::Escape => {
                self.inspector_edit = None;
                self.status = "Inspector edit cancelled".to_string();
            }
            KeyCode::Enter => {
                let mutation = match &edit.target {
                    InspectorEditTarget::NodeName => PendingMutation::SetNodeName,
                    InspectorEditTarget::AgentModel => PendingMutation::SetAgentModel,
                    InspectorEditTarget::AgentSkills => PendingMutation::SetAgentSkills,
                    InspectorEditTarget::RepeatBound => PendingMutation::SetRepeatBound,
                    InspectorEditTarget::PredicatePath => PendingMutation::SetPredicatePath,
                    InspectorEditTarget::SchemaField { .. } => PendingMutation::SetSchemaField,
                };
                self.request_mutation(mutation);
            }
            KeyCode::Backspace => {
                edit.buffer.pop();
            }
            KeyCode::Char(character) if edit.buffer.len() < 4_096 => edit.buffer.push(character),
            _ => {}
        }
        true
    }

    fn resolve_conflict(&mut self) {
        let Some(conflict) = self.pending_conflict.take() else {
            self.status = "No draft conflict is awaiting explicit resolution".to_string();
            return;
        };
        let Some(current_generation) = self.generation else {
            self.pending_conflict = Some(conflict);
            self.status = "Reload the current draft before resolving its conflict".to_string();
            return;
        };
        if current_generation != conflict.current_generation {
            let expected_generation = conflict.expected_generation;
            let conflict_generation = conflict.current_generation;
            self.pending_conflict = Some(conflict);
            self.status = format!(
                "Conflict expected {expected_generation} → {conflict_generation}, but draft is now generation {current_generation}; reload before resolution"
            );
            return;
        }
        self.pending_edit_batch = Some(WorkflowAuthoringEditBatch {
            version: WORKFLOW_AUTHORING_EDIT_VERSION,
            expected_generation: current_generation,
            edits: conflict.edits,
        });
        self.status =
            format!("Explicitly reapplying reviewed edits to generation {current_generation}");
    }

    fn request_mutation(&mut self, mutation: PendingMutation) {
        if self.operations.contains(&AuthoringOperation::Edit) {
            self.status = "A semantic edit is already in progress".to_string();
            return;
        }
        if self.generation.is_none() {
            self.status =
                "This is a read-only template preview; open an instantiated draft".to_string();
            return;
        }
        self.pending_mutation = Some(mutation);
    }

    fn take_inspector_edit(&mut self) -> Result<InspectorTextEdit, String> {
        self.inspector_edit
            .take()
            .ok_or_else(|| "inspector text edit is unavailable".to_string())
    }

    fn take_inspector_buffer(&mut self, expected: &InspectorEditTarget) -> Result<String, String> {
        let edit = self.take_inspector_edit()?;
        if &edit.target != expected {
            return Err("inspector edit target changed before submission".to_string());
        }
        Ok(edit.buffer.trim().to_string())
    }

    #[allow(clippy::too_many_lines)]
    fn mutation_batch(
        &mut self,
        mutation: PendingMutation,
    ) -> Result<WorkflowAuthoringEditBatch, String> {
        let generation = self
            .generation
            .ok_or_else(|| "mutable draft generation is unavailable".to_string())?;
        let document = self
            .document
            .clone()
            .ok_or_else(|| "authoring document is unavailable".to_string())?;
        let document = &document;
        let edits = match mutation {
            PendingMutation::Add => add_node_edits(
                document,
                self.selected_palette_entry()
                    .ok_or_else(|| "select a palette item to add".to_string())?,
                self.catalog
                    .as_ref()
                    .ok_or_else(|| "portable catalog is unavailable".to_string())?,
                self.selected_node().map(|(id, _)| id),
            )?,
            PendingMutation::Duplicate => {
                let (node_id, node) = self
                    .selected_node()
                    .ok_or_else(|| "select a node to duplicate".to_string())?;
                let duplicate = duplicate_node(document, node);
                let mut edits = vec![WorkflowAuthoringEdit::AddNode {
                    node: duplicate.clone(),
                }];
                if let Some(defaults) = document.plugin_input_defaults.get(node_id) {
                    edits.push(WorkflowAuthoringEdit::UpdatePluginInputDefaults {
                        node_id: duplicate.id,
                        defaults: Some(defaults.clone()),
                    });
                }
                edits
            }
            PendingMutation::Remove => remove_node_edits(
                document,
                self.selected_node()
                    .map(|(id, _)| id)
                    .ok_or_else(|| "select a node to remove".to_string())?,
            )?,
            PendingMutation::Connect => {
                let target = self
                    .selected_node()
                    .map(|(target, _)| target.to_string())
                    .ok_or_else(|| "select a target node".to_string())?;
                let source = self
                    .connect_source
                    .take()
                    .ok_or_else(|| "select a source node before connecting".to_string())?;
                let source_node = document
                    .definition
                    .nodes
                    .get(&source)
                    .ok_or_else(|| "selected source node no longer exists".to_string())?;
                let target_node = document
                    .definition
                    .nodes
                    .get(&target)
                    .ok_or_else(|| "selected target node no longer exists".to_string())?;
                if source == target {
                    return Err(
                        "self edges require an explicit bounded repeat operation".to_string()
                    );
                }
                if source_node.output != target_node.input {
                    return Err(format!(
                        "typed ports are incompatible: {} → {}",
                        source_node.output.type_name, target_node.input.type_name
                    ));
                }
                vec![WorkflowAuthoringEdit::AddEdge {
                    edge: EdgeDefinition {
                        from: source,
                        to: target,
                        kind: EdgeKind::Direct,
                        transform: None,
                    },
                }]
            }
            PendingMutation::Reposition => {
                vec![WorkflowAuthoringEdit::UpdatePresentationNamespace {
                    namespace: GRAPH_PRESENTATION_NAMESPACE.to_string(),
                    value: Some(repositioned_layout(
                        document,
                        self.selected_node().map(|(id, _)| id),
                    )?),
                }]
            }
            PendingMutation::SetEntry => vec![WorkflowAuthoringEdit::UpdateEntries {
                entries: vec![
                    self.selected_node()
                        .map(|(id, _)| id.to_string())
                        .ok_or_else(|| "select an entry node".to_string())?,
                ],
            }],
            PendingMutation::SetExit => vec![WorkflowAuthoringEdit::UpdateExits {
                exits: vec![
                    self.selected_node()
                        .map(|(id, _)| id.to_string())
                        .ok_or_else(|| "select an exit node".to_string())?,
                ],
            }],
            PendingMutation::RemoveEdge => vec![WorkflowAuthoringEdit::RemoveEdge {
                selector: selected_edge_selector(document, self.selected_edge)?,
            }],
            PendingMutation::ToggleGroup => {
                vec![WorkflowAuthoringEdit::UpdatePresentationNamespace {
                    namespace: GRAPH_PRESENTATION_NAMESPACE.to_string(),
                    value: Some(toggled_group_layout(
                        document,
                        self.selected_node().map(|(id, _)| id),
                    )?),
                }]
            }
            PendingMutation::MoveLeft => vec![layout_move_edit(
                document,
                self.selected_node().map(|(id, _)| id),
                -4,
                0,
            )?],
            PendingMutation::MoveRight => vec![layout_move_edit(
                document,
                self.selected_node().map(|(id, _)| id),
                4,
                0,
            )?],
            PendingMutation::MoveUp => vec![layout_move_edit(
                document,
                self.selected_node().map(|(id, _)| id),
                0,
                -2,
            )?],
            PendingMutation::MoveDown => vec![layout_move_edit(
                document,
                self.selected_node().map(|(id, _)| id),
                0,
                2,
            )?],
            PendingMutation::CycleAgentProfile => vec![WorkflowAuthoringEdit::UpdateNode {
                node: updated_selected_node(document, self.selected_node, |node| {
                    let catalog = self
                        .catalog
                        .as_ref()
                        .ok_or_else(|| "portable catalog is unavailable".to_string())?;
                    cycle_agent_profile(node, catalog)
                })?,
            }],
            PendingMutation::ToggleAgentReadOnly => vec![WorkflowAuthoringEdit::UpdateNode {
                node: updated_selected_node(document, self.selected_node, toggle_agent_read_only)?,
            }],
            PendingMutation::IncreaseRepeatBound => vec![WorkflowAuthoringEdit::UpdateNode {
                node: updated_selected_node(document, self.selected_node, increase_repeat_bound)?,
            }],
            PendingMutation::CyclePredicate => vec![WorkflowAuthoringEdit::UpdateNode {
                node: updated_selected_node(document, self.selected_node, cycle_predicate)?,
            }],
            PendingMutation::SetNodeName => vec![WorkflowAuthoringEdit::UpdateNode {
                node: updated_selected_node(document, self.selected_node, |node| {
                    node.name = self.take_inspector_buffer(&InspectorEditTarget::NodeName)?;
                    if node.name.trim().is_empty() {
                        return Err("node name cannot be empty".to_string());
                    }
                    Ok(())
                })?,
            }],
            PendingMutation::SetAgentModel => vec![WorkflowAuthoringEdit::UpdateNode {
                node: updated_selected_node(document, self.selected_node, |node| {
                    set_agent_model(
                        node,
                        self.take_inspector_buffer(&InspectorEditTarget::AgentModel)?,
                    )
                })?,
            }],
            PendingMutation::SetAgentSkills => vec![WorkflowAuthoringEdit::UpdateNode {
                node: updated_selected_node(document, self.selected_node, |node| {
                    set_agent_skills(
                        node,
                        &self.take_inspector_buffer(&InspectorEditTarget::AgentSkills)?,
                        self.catalog
                            .as_ref()
                            .ok_or_else(|| "portable catalog is unavailable".to_string())?,
                    )
                })?,
            }],
            PendingMutation::SetRepeatBound => vec![WorkflowAuthoringEdit::UpdateNode {
                node: updated_selected_node(document, self.selected_node, |node| {
                    set_repeat_bound(
                        node,
                        &self.take_inspector_buffer(&InspectorEditTarget::RepeatBound)?,
                    )
                })?,
            }],
            PendingMutation::SetPredicatePath => vec![WorkflowAuthoringEdit::UpdateNode {
                node: updated_selected_node(document, self.selected_node, |node| {
                    set_predicate_path(
                        node,
                        &self.take_inspector_buffer(&InspectorEditTarget::PredicatePath)?,
                    )
                })?,
            }],
            PendingMutation::SetSchemaField => {
                let edit = self.take_inspector_edit()?;
                let InspectorEditTarget::SchemaField {
                    path,
                    control,
                    source,
                } = edit.target
                else {
                    return Err("schema-field edit target changed before submission".to_string());
                };
                let node_id = self
                    .selected_node()
                    .map(|(id, _)| id.to_string())
                    .ok_or_else(|| "select a node to edit".to_string())?;
                match source {
                    InspectorSchemaSource::NodeConfiguration => {
                        vec![WorkflowAuthoringEdit::UpdateNode {
                            node: updated_selected_node(document, self.selected_node, |node| {
                                set_schema_field(node, &path, control, &edit.buffer)
                            })?,
                        }]
                    }
                    InspectorSchemaSource::PluginInputDefaults => {
                        let mut defaults = document
                            .plugin_input_defaults
                            .get(&node_id)
                            .cloned()
                            .unwrap_or_else(|| serde_json::json!({}));
                        let value = parse_schema_control_value(control, &edit.buffer)?;
                        set_json_path_value(&mut defaults, &path, value)?;
                        vec![WorkflowAuthoringEdit::UpdatePluginInputDefaults {
                            node_id,
                            defaults: Some(defaults),
                        }]
                    }
                }
            }
        };
        Ok(WorkflowAuthoringEditBatch {
            version: WORKFLOW_AUTHORING_EDIT_VERSION,
            expected_generation: generation,
            edits,
        })
    }

    fn publish(&mut self) {
        if self.operations.contains(&AuthoringOperation::Publish)
            || self.operations.contains(&AuthoringOperation::Publishing)
            || self.operations.contains(&AuthoringOperation::Edit)
        {
            self.status = "Finish the current authoring operation before publication".to_string();
            return;
        }
        if self
            .preview
            .as_ref()
            .is_none_or(|preview| !preview.is_compiled())
        {
            self.status = "Resolve all diagnostics before publication".to_string();
            return;
        }
        self.operations.insert(AuthoringOperation::Publish);
        self.status = "Publishing exact reviewed draft generation…".to_string();
    }

    fn start_published(&mut self) {
        if self.operations.contains(&AuthoringOperation::Start)
            || self.operations.contains(&AuthoringOperation::Starting)
        {
            self.status = "Workflow start is already in progress".to_string();
            return;
        }
        if self.published_revision.is_none() {
            self.status = "Publish first; start is a separate explicit action".to_string();
            return;
        }
        if self.parent_session_id.is_none() {
            self.status = "An active parent session is required to start the revision".to_string();
            return;
        }
        self.operations.insert(AuthoringOperation::Start);
        self.status = "Starting exact immutable authored revision…".to_string();
    }

    const fn pane_rects(&self) -> (Rect, Rect, Rect, Rect) {
        editor_rects(self.last_area)
    }

    fn handle_mouse(&mut self, event: &bmux_tui::event::MouseEvent) -> bool {
        let (palette, canvas, inspector, _) = self.pane_rects();
        let position = event.position;
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if contains(palette, position.x, position.y) {
                    self.focus = EditorPane::Palette;
                    if position.y >= palette.y.saturating_add(PALETTE_HEADER_ROWS) {
                        let row = usize::from(
                            position
                                .y
                                .saturating_sub(palette.y.saturating_add(PALETTE_HEADER_ROWS)),
                        );
                        self.selected_palette =
                            (self.palette_scroll + row).min(self.palette.len().saturating_sub(1));
                    }
                } else if contains(canvas, position.x, position.y) {
                    self.focus = EditorPane::Canvas;
                    if position.y >= canvas.y.saturating_add(CANVAS_HEADER_ROWS) {
                        let row = usize::from(
                            position
                                .y
                                .saturating_sub(canvas.y.saturating_add(CANVAS_HEADER_ROWS)),
                        );
                        self.selected_node = (self.canvas_scroll + row)
                            .min(self.node_entries().len().saturating_sub(1));
                    }
                } else if contains(inspector, position.x, position.y) {
                    self.focus = EditorPane::Inspector;
                } else {
                    return false;
                }
                true
            }
            MouseEventKind::ScrollUp => {
                self.move_selection(-1);
                true
            }
            MouseEventKind::ScrollDown => {
                self.move_selection(1);
                true
            }
            _ => false,
        }
    }

    fn render_palette(&mut self, area: Rect, frame: &mut Frame<'_>) {
        draw_pane(frame, area, "Palette", self.focus == EditorPane::Palette);
        if area.height <= PALETTE_HEADER_ROWS {
            return;
        }
        let visible = usize::from(area.height.saturating_sub(PALETTE_HEADER_ROWS));
        keep_visible(&mut self.palette_scroll, self.selected_palette, visible);
        for (row, entry) in self
            .palette
            .iter()
            .skip(self.palette_scroll)
            .take(visible)
            .enumerate()
        {
            let marker = if self.palette_scroll + row == self.selected_palette {
                "▶"
            } else {
                " "
            };
            let kind = match entry.kind {
                PaletteEntryKind::NodeKind => "node",
                PaletteEntryKind::PluginBlock => "block",
                PaletteEntryKind::WorkflowCall => "call",
            };
            write_row(
                frame,
                area,
                u16::try_from(row).unwrap_or(u16::MAX) + PALETTE_HEADER_ROWS,
                &format!("{marker} {} [{kind}]", entry.label),
            );
        }
    }

    fn render_canvas(&mut self, area: Rect, frame: &mut Frame<'_>) {
        draw_pane(
            frame,
            area,
            "Graph canvas",
            self.focus == EditorPane::Canvas,
        );
        if area.height <= CANVAS_HEADER_ROWS {
            return;
        }
        let visible = usize::from(area.height.saturating_sub(CANVAS_HEADER_ROWS));
        keep_visible(&mut self.canvas_scroll, self.selected_node, visible);
        let nodes = self.node_entries();
        let edges = self
            .document
            .as_ref()
            .map_or(0, |document| document.definition.edges.len());
        write_row(
            frame,
            area,
            1,
            &format!("{} nodes · {edges} edges", nodes.len()),
        );
        let node_visible = visible.saturating_sub(edges.min(4));
        for (row, (id, node)) in nodes
            .iter()
            .skip(self.canvas_scroll)
            .take(node_visible)
            .enumerate()
        {
            let marker = if self.canvas_scroll + row == self.selected_node {
                "▶"
            } else {
                " "
            };
            let incoming = edge_sources(self.document.as_ref(), id);
            let line = format!(
                "{marker} {id}  {}  {} → {}{}",
                node_kind_label(node.kind),
                node.input.type_name,
                node.output.type_name,
                incoming
            );
            write_row(
                frame,
                area,
                u16::try_from(row).unwrap_or(u16::MAX) + CANVAS_HEADER_ROWS,
                &line,
            );
        }
        if let Some(document) = &self.document {
            for (offset, edge) in document.definition.edges.iter().take(4).enumerate() {
                let marker = if offset == self.selected_edge {
                    "◆"
                } else {
                    "·"
                };
                let row = CANVAS_HEADER_ROWS
                    .saturating_add(u16::try_from(node_visible).unwrap_or(u16::MAX))
                    .saturating_add(u16::try_from(offset).unwrap_or(u16::MAX));
                write_row(
                    frame,
                    area,
                    row,
                    &format!(
                        "{marker} edge {} → {} [{:?}]",
                        edge.from, edge.to, edge.kind
                    ),
                );
            }
        }
    }

    fn render_inspector(&self, area: Rect, frame: &mut Frame<'_>) {
        draw_pane(
            frame,
            area,
            "Inspector",
            self.focus == EditorPane::Inspector,
        );
        let selected_field = self
            .schema_fields()
            .get(self.selected_schema_field)
            .map(|field| field.path.clone());
        let mut lines = inspector_lines(
            self.selected_node(),
            &InspectorContext {
                diagnostics: &self.diagnostics,
                preview: self.preview.as_ref(),
                catalog: self.catalog.as_ref(),
                semantic_diff: self.semantic_diff().as_ref(),
                selected_schema_field: self.selected_schema_field,
                selected_edge: self.selected_edge,
                selected_field_path: selected_field.as_deref(),
            },
        );
        if let Some(edit) = &self.inspector_edit {
            lines.insert(0, format!("Editing {:?}: {}▏", edit.target, edit.buffer));
        }
        for (row, line) in lines
            .iter()
            .skip(self.inspector_scroll)
            .take(usize::from(area.height.saturating_sub(1)))
            .enumerate()
        {
            write_row(
                frame,
                area,
                u16::try_from(row).unwrap_or(u16::MAX) + 1,
                line,
            );
        }
    }
}

impl PluginTuiSurface for WorkflowAuthorSurface {
    fn id(&self) -> &'static str {
        "bcode.workflow-author"
    }

    fn title(&self) -> &'static str {
        "Workflow Graph Editor"
    }

    fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        self.last_area = area;
        frame.fill(area, " ", Style::new().fg(Color::White).bg(Color::Black));
        let (palette, canvas, inspector, footer) = editor_rects(area);
        self.render_palette(palette, frame);
        self.render_canvas(canvas, frame);
        self.render_inspector(inspector, frame);
        if footer.height > 0 {
            frame.write_line(
                footer,
                &Line::from(format!(
                    "{} · Tab panes · node d/x/c · edge e/E · group g · move Shift+HJKL · inspector n/a/t/+/v · R resolve conflict · p publish · s start",
                    self.status
                )),
            );
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle_event(&mut self, event: &Event, _host: &dyn PluginTuiHost) -> PluginTuiAction {
        if let Event::Key(key) = event
            && self.inspector_edit.is_some()
        {
            return if self.handle_inspector_text_input(key.key) {
                PluginTuiAction::Redraw
            } else {
                PluginTuiAction::None
            };
        }
        let changed = match event {
            Event::Key(key) if matches!(key.key, KeyCode::Escape | KeyCode::Char('q')) => {
                return PluginTuiAction::Close { outcome: None };
            }
            Event::Key(key) if key.key == KeyCode::Tab && key.modifiers.shift => {
                self.focus = self.focus.previous();
                true
            }
            Event::Key(key) if key.key == KeyCode::Tab => {
                self.focus = self.focus.next();
                true
            }
            Event::Key(key) if matches!(key.key, KeyCode::Up | KeyCode::Char('k')) => {
                self.move_selection(-1);
                true
            }
            Event::Key(key) if matches!(key.key, KeyCode::Down | KeyCode::Char('j')) => {
                self.move_selection(1);
                true
            }
            Event::Key(key) if matches!(key.key, KeyCode::Left | KeyCode::Char('h')) => {
                self.focus = self.focus.previous();
                true
            }
            Event::Key(key) if matches!(key.key, KeyCode::Right | KeyCode::Char('l')) => {
                self.focus = self.focus.next();
                true
            }
            Event::Key(key) if key.key == KeyCode::Enter && self.focus == EditorPane::Palette => {
                self.request_mutation(PendingMutation::Add);
                true
            }
            Event::Key(key)
                if key.key == KeyCode::Char('d') && self.focus == EditorPane::Canvas =>
            {
                self.request_mutation(PendingMutation::Duplicate);
                true
            }
            Event::Key(key)
                if key.key == KeyCode::Char('x') && self.focus == EditorPane::Canvas =>
            {
                self.request_mutation(PendingMutation::Remove);
                true
            }
            Event::Key(key)
                if key.key == KeyCode::Char('c') && self.focus == EditorPane::Canvas =>
            {
                if let Some(node_id) = self.selected_node().map(|(node_id, _)| node_id.to_string())
                {
                    if self.connect_source.is_some() {
                        self.request_mutation(PendingMutation::Connect);
                    } else {
                        self.connect_source = Some(node_id.clone());
                        self.status = format!(
                            "Connection source selected: {node_id}; choose target and press c"
                        );
                    }
                }
                true
            }
            Event::Key(key)
                if key.key == KeyCode::Char('m') && self.focus == EditorPane::Canvas =>
            {
                self.request_mutation(PendingMutation::Reposition);
                true
            }
            Event::Key(key)
                if key.key == KeyCode::Char('g') && self.focus == EditorPane::Canvas =>
            {
                self.request_mutation(PendingMutation::ToggleGroup);
                true
            }
            Event::Key(key)
                if key.key == KeyCode::Char('e') && self.focus == EditorPane::Canvas =>
            {
                let edge_count = self
                    .document
                    .as_ref()
                    .map_or(0, |document| document.definition.edges.len());
                if edge_count > 0 {
                    self.selected_edge = self.selected_edge.saturating_add(1) % edge_count;
                    self.status =
                        format!("Selected edge {} of {edge_count}", self.selected_edge + 1);
                }
                true
            }
            Event::Key(key)
                if key.key == KeyCode::Char('E') && self.focus == EditorPane::Canvas =>
            {
                self.request_mutation(PendingMutation::RemoveEdge);
                true
            }
            Event::Key(key)
                if key.key == KeyCode::Char('H') && self.focus == EditorPane::Canvas =>
            {
                self.request_mutation(PendingMutation::MoveLeft);
                true
            }
            Event::Key(key)
                if key.key == KeyCode::Char('L') && self.focus == EditorPane::Canvas =>
            {
                self.request_mutation(PendingMutation::MoveRight);
                true
            }
            Event::Key(key)
                if key.key == KeyCode::Char('K') && self.focus == EditorPane::Canvas =>
            {
                self.request_mutation(PendingMutation::MoveUp);
                true
            }
            Event::Key(key)
                if key.key == KeyCode::Char('J') && self.focus == EditorPane::Canvas =>
            {
                self.request_mutation(PendingMutation::MoveDown);
                true
            }
            Event::Key(key)
                if key.key == KeyCode::Char('i') && self.focus == EditorPane::Canvas =>
            {
                self.request_mutation(PendingMutation::SetEntry);
                true
            }
            Event::Key(key)
                if key.key == KeyCode::Char('o') && self.focus == EditorPane::Canvas =>
            {
                self.request_mutation(PendingMutation::SetExit);
                true
            }
            Event::Key(key)
                if key.key == KeyCode::Char('f') && self.focus == EditorPane::Inspector =>
            {
                self.cycle_schema_field();
                true
            }
            Event::Key(key)
                if key.key == KeyCode::Char('F') && self.focus == EditorPane::Inspector =>
            {
                self.begin_selected_schema_edit();
                true
            }
            Event::Key(key)
                if key.key == KeyCode::Char('n') && self.focus == EditorPane::Inspector =>
            {
                self.begin_inspector_edit(InspectorEditTarget::NodeName);
                true
            }
            Event::Key(key)
                if key.key == KeyCode::Char('M') && self.focus == EditorPane::Inspector =>
            {
                self.begin_inspector_edit(InspectorEditTarget::AgentModel);
                true
            }
            Event::Key(key)
                if key.key == KeyCode::Char('S') && self.focus == EditorPane::Inspector =>
            {
                self.begin_inspector_edit(InspectorEditTarget::AgentSkills);
                true
            }
            Event::Key(key)
                if key.key == KeyCode::Char('B') && self.focus == EditorPane::Inspector =>
            {
                self.begin_inspector_edit(InspectorEditTarget::RepeatBound);
                true
            }
            Event::Key(key)
                if key.key == KeyCode::Char('P') && self.focus == EditorPane::Inspector =>
            {
                self.begin_inspector_edit(InspectorEditTarget::PredicatePath);
                true
            }
            Event::Key(key)
                if key.key == KeyCode::Char('a') && self.focus == EditorPane::Inspector =>
            {
                self.request_mutation(PendingMutation::CycleAgentProfile);
                true
            }
            Event::Key(key)
                if key.key == KeyCode::Char('t') && self.focus == EditorPane::Inspector =>
            {
                self.request_mutation(PendingMutation::ToggleAgentReadOnly);
                true
            }
            Event::Key(key)
                if key.key == KeyCode::Char('+') && self.focus == EditorPane::Inspector =>
            {
                self.request_mutation(PendingMutation::IncreaseRepeatBound);
                true
            }
            Event::Key(key)
                if key.key == KeyCode::Char('v') && self.focus == EditorPane::Inspector =>
            {
                self.request_mutation(PendingMutation::CyclePredicate);
                true
            }
            Event::Key(key) if key.key == KeyCode::Char('R') => {
                self.resolve_conflict();
                true
            }
            Event::Key(key) if key.key == KeyCode::Char('p') => {
                self.publish();
                true
            }
            Event::Key(key) if key.key == KeyCode::Char('s') => {
                self.start_published();
                true
            }
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            _ => false,
        };
        if changed {
            PluginTuiAction::Redraw
        } else {
            PluginTuiAction::None
        }
    }

    #[allow(clippy::too_many_lines)]
    fn poll(&mut self, host: &dyn PluginTuiHost) -> PluginTuiAction {
        if self.operations.insert(AuthoringOperation::CatalogLoad) {
            let future = host.workflow_authoring_catalog();
            let sender = self.catalog_sender.clone();
            host.spawn(Box::pin(async move {
                let result = future.await.map_err(|error| error.to_string());
                let _ = sender.send(result);
            }));
        }
        if !self.operations.contains(&AuthoringOperation::DraftLoad)
            && let (Some(workflow_id), Some(draft_id)) =
                (self.workflow_id.clone(), self.draft_id.clone())
        {
            self.operations.insert(AuthoringOperation::DraftLoad);
            let future = host.workflow_authoring_draft(workflow_id, draft_id);
            let sender = self.authoring_sender.clone();
            host.spawn(Box::pin(async move {
                let result = future
                    .await
                    .map(|draft| draft.map(Box::new))
                    .map_err(|error| error.to_string());
                let _ = sender.send(AuthoringAsyncResult::Draft(result));
            }));
        }
        if !self.operations.contains(&AuthoringOperation::BaseLoad)
            && self.base_document.is_none()
            && let (Some(workflow_id), Some(revision)) =
                (self.workflow_id.clone(), self.base_revision)
        {
            self.operations.insert(AuthoringOperation::BaseLoad);
            let future = host.workflow_authoring_revision(workflow_id, revision);
            let sender = self.authoring_sender.clone();
            host.spawn(Box::pin(async move {
                let result = future
                    .await
                    .map(|revision| revision.map(Box::new))
                    .map_err(|error| error.to_string());
                let _ = sender.send(AuthoringAsyncResult::BaseRevision(result));
            }));
        }
        if self.operations.insert(AuthoringOperation::Edit) {
            self.operations.remove(&AuthoringOperation::Edit);
            let batch = if let Some(batch) = self.pending_edit_batch.take() {
                Some(Ok(batch))
            } else {
                self.pending_mutation
                    .take()
                    .map(|mutation| self.mutation_batch(mutation))
            };
            if let Some(batch) = batch {
                match batch {
                    Ok(batch) => {
                        let Some(workflow_id) = self.workflow_id.clone() else {
                            self.status = "mutable workflow identity is unavailable".to_string();
                            return PluginTuiAction::Redraw;
                        };
                        let Some(draft_id) = self.draft_id.clone() else {
                            self.status = "mutable draft identity is unavailable".to_string();
                            return PluginTuiAction::Redraw;
                        };
                        self.operations.insert(AuthoringOperation::Edit);
                        self.pending_edit_batch = Some(batch.clone());
                        self.status = "Applying atomic semantic edit…".to_string();
                        let future = host.apply_workflow_authoring_edits(
                            workflow_id,
                            draft_id,
                            batch,
                            self.producer.clone(),
                        );
                        let sender = self.authoring_sender.clone();
                        host.spawn(Box::pin(async move {
                            let result = future.await.map_err(|error| error.to_string());
                            let _ = sender.send(AuthoringAsyncResult::Edit(result));
                        }));
                    }
                    Err(error) => self.status = error,
                }
            }
        }
        if self.operations.remove(&AuthoringOperation::Publish) {
            self.operations.insert(AuthoringOperation::Publishing);
            let (Some(workflow_id), Some(draft_id), Some(expected_generation)) = (
                self.workflow_id.clone(),
                self.draft_id.clone(),
                self.generation,
            ) else {
                self.status = "Mutable draft identity is unavailable".to_string();
                return PluginTuiAction::Redraw;
            };
            let future = host.publish_workflow_authoring_draft(
                workflow_id,
                draft_id,
                expected_generation,
                false,
            );
            let sender = self.authoring_sender.clone();
            host.spawn(Box::pin(async move {
                let result = future.await.map_err(|error| error.to_string());
                let _ = sender.send(AuthoringAsyncResult::Publish(result));
            }));
        }
        if self.operations.remove(&AuthoringOperation::Start) {
            self.operations.insert(AuthoringOperation::Starting);
            let (Some(workflow_id), Some(revision), Some(parent_session_id)) = (
                self.workflow_id.clone(),
                self.published_revision,
                self.parent_session_id,
            ) else {
                self.status = "Published revision or parent session is unavailable".to_string();
                return PluginTuiAction::Redraw;
            };
            let configuration = self
                .document
                .as_ref()
                .and_then(|document| document.configuration_defaults.clone());
            let future = host.start_authored_workflow_revision(
                workflow_id,
                revision,
                parent_session_id,
                self.workspace_snapshot.clone(),
                configuration,
            );
            let sender = self.authoring_sender.clone();
            host.spawn(Box::pin(async move {
                let result = future.await.map_err(|error| error.to_string());
                let _ = sender.send(AuthoringAsyncResult::Start(result));
            }));
        }
        let mut changed = false;
        while let Ok(result) = self.catalog_receiver.try_recv() {
            changed = true;
            match result {
                Ok(catalog) => self.install_catalog(catalog),
                Err(error) => self.status = format!("Failed to load authoring catalog: {error}"),
            }
        }
        while let Ok(result) = self.authoring_receiver.try_recv() {
            changed = true;
            match result {
                AuthoringAsyncResult::Draft(Ok(Some(draft))) => self.install_draft(*draft),
                AuthoringAsyncResult::Draft(Ok(None)) => {
                    self.status = "Mutable draft no longer exists".to_string();
                }
                AuthoringAsyncResult::Draft(Err(error)) => {
                    self.status = format!("Failed to load mutable draft: {error}");
                }
                AuthoringAsyncResult::BaseRevision(Ok(Some(revision))) => {
                    self.install_base_revision(*revision);
                }
                AuthoringAsyncResult::BaseRevision(Ok(None)) => {
                    self.status = "Base revision no longer exists".to_string();
                }
                AuthoringAsyncResult::BaseRevision(Err(error)) => {
                    self.status = format!("Failed to load base revision: {error}");
                }
                AuthoringAsyncResult::Edit(Ok(PluginWorkflowAuthoringEditResult::Updated(
                    draft,
                ))) => {
                    self.operations.remove(&AuthoringOperation::Edit);
                    self.pending_edit_batch = None;
                    self.pending_conflict = None;
                    self.install_draft(*draft);
                }
                AuthoringAsyncResult::Edit(Ok(PluginWorkflowAuthoringEditResult::Conflict {
                    expected_generation,
                    current_generation,
                })) => {
                    self.operations.remove(&AuthoringOperation::Edit);
                    let edits = self
                        .pending_edit_batch
                        .take()
                        .map(|batch| batch.edits)
                        .unwrap_or_default();
                    self.pending_conflict = Some(PendingConflict {
                        expected_generation,
                        current_generation,
                        edits,
                    });
                    self.operations.remove(&AuthoringOperation::DraftLoad);
                    self.status = format!(
                        "Draft conflict: expected generation {expected_generation}, current {current_generation}; reloading exact draft, then press R to reapply reviewed edits"
                    );
                }
                AuthoringAsyncResult::Edit(Ok(PluginWorkflowAuthoringEditResult::Rejected {
                    diagnostics,
                })) => {
                    self.operations.remove(&AuthoringOperation::Edit);
                    self.pending_edit_batch = None;
                    self.diagnostics = diagnostics;
                    self.status =
                        "Semantic edit rejected with source-addressed diagnostics".to_string();
                }
                AuthoringAsyncResult::Edit(Err(error)) => {
                    self.operations.remove(&AuthoringOperation::Edit);
                    self.pending_edit_batch = None;
                    self.status = format!("Semantic edit failed: {error}");
                }
                AuthoringAsyncResult::Publish(Ok(
                    PluginWorkflowAuthoringPublishResult::Published { revision, .. },
                )) => {
                    self.operations.remove(&AuthoringOperation::Publishing);
                    self.published_revision = Some(revision);
                    self.status = format!(
                        "Published immutable revision {revision}; press s to start explicitly"
                    );
                }
                AuthoringAsyncResult::Publish(Ok(
                    PluginWorkflowAuthoringPublishResult::Conflict {
                        expected_generation,
                        current_generation,
                    },
                )) => {
                    self.operations.remove(&AuthoringOperation::Publishing);
                    self.operations.remove(&AuthoringOperation::DraftLoad);
                    self.status = format!(
                        "Publication conflict: expected generation {expected_generation}, current {current_generation}; reloading"
                    );
                }
                AuthoringAsyncResult::Publish(Err(error)) => {
                    self.operations.remove(&AuthoringOperation::Publishing);
                    self.status = format!("Publication failed: {error}");
                }
                AuthoringAsyncResult::Start(Ok(started)) => {
                    self.operations.remove(&AuthoringOperation::Starting);
                    self.status = format!("Started exact revision as run {}", started.run_id);
                }
                AuthoringAsyncResult::Start(Err(error)) => {
                    self.operations.remove(&AuthoringOperation::Starting);
                    self.status = format!("Workflow start failed: {error}");
                }
            }
        }
        if changed {
            PluginTuiAction::Redraw
        } else {
            PluginTuiAction::None
        }
    }
}

fn authoring_document(options: &serde_json::Value) -> Option<WorkflowAuthoringDocument> {
    options
        .get("draft")
        .and_then(|draft| draft.get("document"))
        .or_else(|| {
            options
                .get("template")
                .and_then(|template| template.get("authoring_document"))
        })
        .and_then(|document| serde_json::from_value(document.clone()).ok())
}

fn editor_producer() -> WorkflowProducerProvenance {
    WorkflowProducerProvenance {
        kind: WorkflowProducerKind::Plugin,
        producer_id: Some("bcode.workflow".to_string()),
        source_revision: None,
    }
}

fn add_node_edits(
    document: &WorkflowAuthoringDocument,
    entry: &PaletteEntry,
    catalog: &WorkflowAuthoringCatalogSnapshot,
    selected_node: Option<&str>,
) -> Result<Vec<WorkflowAuthoringEdit>, String> {
    let node = add_node(document, entry, catalog)?;
    let mut edits = vec![WorkflowAuthoringEdit::AddNode { node: node.clone() }];
    if let Some(source) = selected_node
        && let Some(source_node) = document.definition.nodes.get(source)
        && source_node.output == node.input
    {
        edits.push(WorkflowAuthoringEdit::AddEdge {
            edge: EdgeDefinition {
                from: source.to_string(),
                to: node.id.clone(),
                kind: EdgeKind::Direct,
                transform: None,
            },
        });
    }
    let layout = layout_with_new_node(document, &node.id);
    edits.push(WorkflowAuthoringEdit::UpdatePresentationNamespace {
        namespace: GRAPH_PRESENTATION_NAMESPACE.to_string(),
        value: Some(layout),
    });
    Ok(edits)
}

fn add_node(
    document: &WorkflowAuthoringDocument,
    entry: &PaletteEntry,
    catalog: &WorkflowAuthoringCatalogSnapshot,
) -> Result<NodeDefinition, String> {
    let id = unique_node_id(document, &node_id_base(entry));
    let node = match entry.kind {
        PaletteEntryKind::PluginBlock => {
            let block = catalog
                .blocks
                .get(&entry.identity)
                .ok_or_else(|| format!("catalog block '{}' is unavailable", entry.identity))?;
            NodeDefinition {
                id,
                name: entry.label.clone(),
                kind: NodeKind::PluginBlock,
                dataflow: WorkflowNodeDataflowPolicy::Direct,
                input: block.input.clone(),
                output: block.output.clone(),
                resources: block.resources.clone(),
                configuration: serde_json::to_value(block).map_err(|error| error.to_string())?,
            }
        }
        PaletteEntryKind::WorkflowCall => {
            let definition = catalog
                .workflow_definitions
                .get(&entry.identity)
                .ok_or_else(|| format!("catalog workflow '{}' is unavailable", entry.identity))?;
            let identity = bcode_workflow::WorkflowDefinitionIdentity {
                kind: definition.name.clone(),
                definition_id: entry.identity.clone(),
                definition_version: definition.schema_version,
            };
            NodeDefinition {
                id,
                name: entry.label.clone(),
                kind: NodeKind::WorkflowCall,
                dataflow: WorkflowNodeDataflowPolicy::Direct,
                input: definition.input.clone(),
                output: definition.output.clone(),
                resources: Vec::new(),
                configuration: serde_json::to_value(WorkflowCallConfiguration {
                    version: WORKFLOW_CALL_VERSION,
                    target: WorkflowCallTarget::Definition { identity },
                })
                .map_err(|error| error.to_string())?,
            }
        }
        PaletteEntryKind::NodeKind => generic_node(document, entry, catalog, id)?,
    };
    Ok(node)
}

fn layout_with_new_node(document: &WorkflowAuthoringDocument, node_id: &str) -> serde_json::Value {
    let existing = document
        .presentation
        .as_ref()
        .and_then(|presentation| presentation.namespaces.get(GRAPH_PRESENTATION_NAMESPACE));
    let mut layout = existing
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();
    layout.insert(
        "version".to_string(),
        serde_json::json!(GRAPH_PRESENTATION_VERSION),
    );
    let mut nodes = layout
        .remove("nodes")
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    let position = i64::try_from(nodes.len()).unwrap_or(i64::MAX);
    nodes.insert(
        node_id.to_string(),
        serde_json::json!({"x": position.saturating_mul(16), "y": position.saturating_mul(6)}),
    );
    layout.insert("nodes".to_string(), serde_json::Value::Object(nodes));
    serde_json::Value::Object(layout)
}

fn generic_node(
    document: &WorkflowAuthoringDocument,
    entry: &PaletteEntry,
    catalog: &WorkflowAuthoringCatalogSnapshot,
    id: String,
) -> Result<NodeDefinition, String> {
    let kind = node_kind_from_catalog_identity(&entry.identity)?;
    if matches!(kind, NodeKind::Task | NodeKind::Retry | NodeKind::FanOut) {
        return Err(format!(
            "{} is not supported by the durable production host",
            entry.label
        ));
    }
    let schema = selected_port_schema(document);
    let configuration = match kind {
        NodeKind::Agent => {
            let agent_profile = catalog
                .agent_profiles
                .iter()
                .next()
                .cloned()
                .ok_or_else(|| "no configured agent profile is available".to_string())?;
            serde_json::to_value(bcode_workflow::WorkflowAgentConfiguration {
                version: bcode_workflow::WORKFLOW_AGENT_CONFIGURATION_VERSION,
                execution_target: bcode_workflow::AgentExecutionTarget::FreshIsolated,
                agent_profile,
                provider: None,
                model: None,
                structured_output: bcode_workflow::AgentStructuredOutputPolicy {
                    schema: schema.clone(),
                    strict: true,
                },
                read_only: true,
                tool_capability: bcode_workflow::WorkflowToolCapability::ReadOnly,
                tool_allowlist: Vec::new(),
                timeout_ms: 300_000,
                skills: Vec::new(),
                prompt_mode: "json_input".to_string(),
                system_prompt:
                    "Process the typed workflow input and return the required structured output."
                        .to_string(),
            })
            .map_err(|error| error.to_string())?
        }
        NodeKind::Branch => serde_json::json!({
            "predicate_version": bcode_workflow::WORKFLOW_PREDICATE_VERSION,
            "predicate": {"operation": "equals", "version": 1, "path": "", "value": true},
            "true_entries": [], "false_entries": [], "true_nodes": [], "false_nodes": []
        }),
        NodeKind::Repeat => serde_json::json!({
            "predicate_version": bcode_workflow::WORKFLOW_PREDICATE_VERSION,
            "predicate": {"operation": "equals", "version": 1, "path": "", "value": true},
            "max_iterations": 20,
            "iteration_state": "explicit_back_edge_transform"
        }),
        NodeKind::Parallel => serde_json::json!({
            "join_policy": "wait_all",
            "branch_entries": [],
            "branch_nodes": []
        }),
        NodeKind::Input | NodeKind::Approval => serde_json::json!({"gate_version": 1}),
        NodeKind::WorkflowCall
        | NodeKind::PluginBlock
        | NodeKind::Task
        | NodeKind::Retry
        | NodeKind::FanOut => {
            return Err(format!("{} requires a catalog-owned contract", entry.label));
        }
    };
    Ok(NodeDefinition {
        id,
        name: entry.label.clone(),
        kind,
        dataflow: WorkflowNodeDataflowPolicy::Direct,
        input: schema.clone(),
        output: schema,
        resources: Vec::new(),
        configuration,
    })
}

fn selected_port_schema(document: &WorkflowAuthoringDocument) -> bcode_workflow::ValueSchema {
    document.definition.nodes.values().next().map_or_else(
        || document.definition.input.clone(),
        |node| node.output.clone(),
    )
}

const fn node_kind_identity(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Task => "task",
        NodeKind::Agent => "agent",
        NodeKind::Branch => "branch",
        NodeKind::Repeat => "repeat",
        NodeKind::Retry => "retry",
        NodeKind::Parallel => "parallel",
        NodeKind::FanOut => "fan_out",
        NodeKind::PluginBlock => "plugin_block",
        NodeKind::Input => "input",
        NodeKind::Approval => "approval",
        NodeKind::WorkflowCall => "workflow_call",
    }
}

fn node_kind_from_catalog_identity(identity: &str) -> Result<NodeKind, String> {
    match identity {
        "task" => Ok(NodeKind::Task),
        "agent" => Ok(NodeKind::Agent),
        "branch" => Ok(NodeKind::Branch),
        "repeat" => Ok(NodeKind::Repeat),
        "retry" => Ok(NodeKind::Retry),
        "parallel" => Ok(NodeKind::Parallel),
        "fan_out" => Ok(NodeKind::FanOut),
        "plugin_block" => Ok(NodeKind::PluginBlock),
        "input" => Ok(NodeKind::Input),
        "approval" => Ok(NodeKind::Approval),
        "workflow_call" => Ok(NodeKind::WorkflowCall),
        other => Err(format!("unknown portable node kind '{other}'")),
    }
}

fn inspector_edit_value(node: &NodeDefinition, target: &InspectorEditTarget) -> String {
    match target {
        InspectorEditTarget::NodeName => node.name.clone(),
        InspectorEditTarget::AgentModel => serde_json::from_value::<
            bcode_workflow::WorkflowAgentConfiguration,
        >(node.configuration.clone())
        .ok()
        .and_then(|agent| agent.model)
        .unwrap_or_default(),
        InspectorEditTarget::AgentSkills => serde_json::from_value::<
            bcode_workflow::WorkflowAgentConfiguration,
        >(node.configuration.clone())
        .map(|agent| {
            agent
                .skills
                .into_iter()
                .filter(|skill| skill.mode != bcode_workflow::AgentSkillActivationMode::Disabled)
                .map(|skill| skill.skill_id)
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default(),
        InspectorEditTarget::RepeatBound => node.configuration["max_iterations"]
            .as_u64()
            .map_or_else(String::new, |value| value.to_string()),
        InspectorEditTarget::PredicatePath => node.configuration["predicate"]["path"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        InspectorEditTarget::SchemaField {
            path,
            source: InspectorSchemaSource::NodeConfiguration,
            ..
        } => json_path_value(&node.configuration, path).map_or_else(String::new, schema_edit_value),
        InspectorEditTarget::SchemaField {
            source: InspectorSchemaSource::PluginInputDefaults,
            ..
        } => String::new(),
    }
}

fn schema_edit_value(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_string)
}

fn json_path_value<'a>(value: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    path.split('.')
        .filter(|segment| !segment.is_empty())
        .try_fold(value, |current, segment| current.get(segment))
}

fn set_schema_field(
    node: &mut NodeDefinition,
    path: &str,
    control: WorkflowSchemaFormControl,
    buffer: &str,
) -> Result<(), String> {
    let value = parse_schema_control_value(control, buffer)?;
    set_json_path_value(&mut node.configuration, path, value)
}

fn parse_schema_control_value(
    control: WorkflowSchemaFormControl,
    buffer: &str,
) -> Result<serde_json::Value, String> {
    match control {
        WorkflowSchemaFormControl::Text | WorkflowSchemaFormControl::Choice => {
            Ok(serde_json::json!(buffer))
        }
        WorkflowSchemaFormControl::Boolean => buffer
            .parse::<bool>()
            .map(serde_json::Value::Bool)
            .map_err(|_| "boolean field must be true or false".to_string()),
        WorkflowSchemaFormControl::Integer => buffer
            .parse::<i64>()
            .map(serde_json::Value::from)
            .map_err(|_| "integer field is invalid".to_string()),
        WorkflowSchemaFormControl::Number => buffer
            .parse::<f64>()
            .map(serde_json::Value::from)
            .map_err(|_| "number field is invalid".to_string()),
        WorkflowSchemaFormControl::Object
        | WorkflowSchemaFormControl::Array
        | WorkflowSchemaFormControl::Json => {
            serde_json::from_str(buffer).map_err(|error| format!("JSON field is invalid: {error}"))
        }
    }
}

fn set_json_path_value(
    root: &mut serde_json::Value,
    path: &str,
    value: serde_json::Value,
) -> Result<(), String> {
    let parts = path
        .split('.')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let (last, parents) = parts
        .split_last()
        .ok_or_else(|| "schema field path cannot be empty".to_string())?;
    let mut current = root;
    for part in parents {
        current = current
            .as_object_mut()
            .ok_or_else(|| format!("schema field parent '{part}' is not an object"))?
            .entry((*part).to_string())
            .or_insert_with(|| serde_json::json!({}));
    }
    current
        .as_object_mut()
        .ok_or_else(|| "schema field parent is not an object".to_string())?
        .insert((*last).to_string(), value);
    Ok(())
}

fn set_agent_model(node: &mut NodeDefinition, value: String) -> Result<(), String> {
    if node.kind != NodeKind::Agent {
        return Err("model controls apply only to agent nodes".to_string());
    }
    let mut configuration: bcode_workflow::WorkflowAgentConfiguration =
        serde_json::from_value(node.configuration.clone()).map_err(|error| error.to_string())?;
    configuration.model = (!value.is_empty()).then_some(value);
    node.configuration = serde_json::to_value(configuration).map_err(|error| error.to_string())?;
    Ok(())
}

fn set_agent_skills(
    node: &mut NodeDefinition,
    value: &str,
    catalog: &WorkflowAuthoringCatalogSnapshot,
) -> Result<(), String> {
    if node.kind != NodeKind::Agent {
        return Err("skill controls apply only to agent nodes".to_string());
    }
    let skill_ids = value
        .split(',')
        .map(str::trim)
        .filter(|skill| !skill.is_empty())
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    if let Some(unavailable) = skill_ids
        .iter()
        .find(|skill| !catalog.skills.contains(*skill))
    {
        return Err(format!(
            "skill '{unavailable}' is unavailable in the portable catalog"
        ));
    }
    let mut configuration: bcode_workflow::WorkflowAgentConfiguration =
        serde_json::from_value(node.configuration.clone()).map_err(|error| error.to_string())?;
    configuration.skills = skill_ids
        .into_iter()
        .map(|skill_id| bcode_workflow::AgentSkillSelection {
            skill_id,
            mode: bcode_workflow::AgentSkillActivationMode::Required,
        })
        .collect();
    node.configuration = serde_json::to_value(configuration).map_err(|error| error.to_string())?;
    Ok(())
}

fn set_repeat_bound(node: &mut NodeDefinition, value: &str) -> Result<(), String> {
    if node.kind != NodeKind::Repeat {
        return Err("repeat-bound controls apply only to repeat nodes".to_string());
    }
    let bound = value
        .parse::<u32>()
        .map_err(|_| "repeat bound must be a positive integer".to_string())?;
    if !(1..=1_000).contains(&bound) {
        return Err("repeat bound must be between 1 and 1000".to_string());
    }
    node.configuration["max_iterations"] = serde_json::json!(bound);
    Ok(())
}

fn set_predicate_path(node: &mut NodeDefinition, value: &str) -> Result<(), String> {
    if !matches!(node.kind, NodeKind::Branch | NodeKind::Repeat) {
        return Err("predicate controls apply only to branch or repeat nodes".to_string());
    }
    if value.len() > 4_096 {
        return Err("predicate path exceeds 4096 bytes".to_string());
    }
    node.configuration["predicate"]["path"] = serde_json::json!(value);
    Ok(())
}

fn updated_selected_node(
    document: &WorkflowAuthoringDocument,
    selected_node: usize,
    update: impl FnOnce(&mut NodeDefinition) -> Result<(), String>,
) -> Result<NodeDefinition, String> {
    let mut node = document
        .definition
        .nodes
        .values()
        .nth(selected_node)
        .cloned()
        .ok_or_else(|| "select a node to edit".to_string())?;
    update(&mut node)?;
    Ok(node)
}

fn cycle_agent_profile(
    node: &mut NodeDefinition,
    catalog: &WorkflowAuthoringCatalogSnapshot,
) -> Result<(), String> {
    if node.kind != NodeKind::Agent {
        return Err("agent profile controls apply only to agent nodes".to_string());
    }
    let mut configuration: bcode_workflow::WorkflowAgentConfiguration =
        serde_json::from_value(node.configuration.clone()).map_err(|error| error.to_string())?;
    let profiles = catalog.agent_profiles.iter().collect::<Vec<_>>();
    if profiles.is_empty() {
        return Err("no configured agent profile is available".to_string());
    }
    let next = profiles
        .iter()
        .position(|profile| profile.as_str() == configuration.agent_profile)
        .map_or(0, |index| index.saturating_add(1) % profiles.len());
    configuration.agent_profile.clone_from(profiles[next]);
    node.configuration = serde_json::to_value(configuration).map_err(|error| error.to_string())?;
    Ok(())
}

fn toggle_agent_read_only(node: &mut NodeDefinition) -> Result<(), String> {
    if node.kind != NodeKind::Agent {
        return Err("agent policy controls apply only to agent nodes".to_string());
    }
    let mut configuration: bcode_workflow::WorkflowAgentConfiguration =
        serde_json::from_value(node.configuration.clone()).map_err(|error| error.to_string())?;
    configuration.read_only = !configuration.read_only;
    configuration.tool_capability = if configuration.read_only {
        bcode_workflow::WorkflowToolCapability::ReadOnly
    } else {
        bcode_workflow::WorkflowToolCapability::Mutating
    };
    node.configuration = serde_json::to_value(configuration).map_err(|error| error.to_string())?;
    Ok(())
}

fn increase_repeat_bound(node: &mut NodeDefinition) -> Result<(), String> {
    if node.kind != NodeKind::Repeat {
        return Err("repeat-bound controls apply only to repeat nodes".to_string());
    }
    let current = node
        .configuration
        .get("max_iterations")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1);
    node.configuration["max_iterations"] = serde_json::json!(current.saturating_add(1).min(1_000));
    Ok(())
}

fn cycle_predicate(node: &mut NodeDefinition) -> Result<(), String> {
    if !matches!(node.kind, NodeKind::Branch | NodeKind::Repeat) {
        return Err("predicate controls apply only to branch or repeat nodes".to_string());
    }
    let expected = node
        .configuration
        .get("predicate")
        .and_then(|predicate| predicate.get("value"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    node.configuration["predicate"] = serde_json::json!({
        "operation": "equals",
        "version": bcode_workflow::WORKFLOW_PREDICATE_VERSION,
        "path": "",
        "value": !expected
    });
    Ok(())
}

fn selected_edge_selector(
    document: &WorkflowAuthoringDocument,
    selected_edge: usize,
) -> Result<WorkflowAuthoringEdgeSelector, String> {
    let edge = document
        .definition
        .edges
        .get(selected_edge)
        .ok_or_else(|| "select an edge to remove".to_string())?;
    let occurrence = document.definition.edges[..selected_edge]
        .iter()
        .filter(|candidate| candidate.from == edge.from && candidate.to == edge.to)
        .count();
    Ok(WorkflowAuthoringEdgeSelector {
        from: edge.from.clone(),
        to: edge.to.clone(),
        occurrence,
    })
}

fn layout_move_edit(
    document: &WorkflowAuthoringDocument,
    selected_node: Option<&str>,
    delta_x: i64,
    delta_y: i64,
) -> Result<WorkflowAuthoringEdit, String> {
    Ok(WorkflowAuthoringEdit::UpdatePresentationNamespace {
        namespace: GRAPH_PRESENTATION_NAMESPACE.to_string(),
        value: Some(moved_layout(document, selected_node, delta_x, delta_y)?),
    })
}

fn moved_layout(
    document: &WorkflowAuthoringDocument,
    selected_node: Option<&str>,
    delta_x: i64,
    delta_y: i64,
) -> Result<serde_json::Value, String> {
    let selected_node = selected_node.ok_or_else(|| "select a node to reposition".to_string())?;
    let mut layout = graph_layout(document);
    let mut nodes = layout
        .remove("nodes")
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    let current = nodes
        .get(selected_node)
        .and_then(serde_json::Value::as_object);
    let x = current
        .and_then(|position| position.get("x"))
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0)
        .saturating_add(delta_x);
    let y = current
        .and_then(|position| position.get("y"))
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0)
        .saturating_add(delta_y);
    nodes.insert(
        selected_node.to_string(),
        serde_json::json!({"x": x, "y": y}),
    );
    layout.insert("nodes".to_string(), serde_json::Value::Object(nodes));
    Ok(serde_json::Value::Object(layout))
}

fn toggled_group_layout(
    document: &WorkflowAuthoringDocument,
    selected_node: Option<&str>,
) -> Result<serde_json::Value, String> {
    let selected_node = selected_node.ok_or_else(|| "select a node to group".to_string())?;
    let mut layout = graph_layout(document);
    let mut groups = layout
        .remove("groups")
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    let group = groups
        .entry("group-1".to_string())
        .or_insert_with(|| serde_json::json!({"title": "Group 1", "nodes": []}));
    let nodes = group
        .as_object_mut()
        .and_then(|group| group.get_mut("nodes"))
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| "graph group metadata is malformed".to_string())?;
    if let Some(index) = nodes.iter().position(|node| node == selected_node) {
        nodes.remove(index);
    } else {
        nodes.push(serde_json::json!(selected_node));
        nodes.sort_by(|left, right| left.as_str().cmp(&right.as_str()));
    }
    layout.insert("groups".to_string(), serde_json::Value::Object(groups));
    Ok(serde_json::Value::Object(layout))
}

fn graph_layout(
    document: &WorkflowAuthoringDocument,
) -> serde_json::Map<String, serde_json::Value> {
    let mut layout = document
        .presentation
        .as_ref()
        .and_then(|presentation| presentation.namespaces.get(GRAPH_PRESENTATION_NAMESPACE))
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();
    layout.insert(
        "version".to_string(),
        serde_json::json!(GRAPH_PRESENTATION_VERSION),
    );
    layout
}

fn duplicate_node(document: &WorkflowAuthoringDocument, node: &NodeDefinition) -> NodeDefinition {
    let mut duplicate = node.clone();
    duplicate.id = unique_node_id(document, &format!("{}-copy", node.id));
    duplicate.name = format!("{} copy", node.name);
    duplicate
}

fn remove_node_edits(
    document: &WorkflowAuthoringDocument,
    node_id: &str,
) -> Result<Vec<WorkflowAuthoringEdit>, String> {
    if document.definition.nodes.len() <= 1 {
        return Err("a workflow must retain at least one node".to_string());
    }
    let mut edits = vec![WorkflowAuthoringEdit::RemoveNode {
        node_id: node_id.to_string(),
    }];
    if document
        .definition
        .entries
        .iter()
        .any(|entry| entry == node_id)
    {
        let entries = document
            .definition
            .entries
            .iter()
            .filter(|entry| entry.as_str() != node_id)
            .cloned()
            .collect::<Vec<_>>();
        if entries.is_empty() {
            return Err("choose another entry before removing the only entry node".to_string());
        }
        edits.push(WorkflowAuthoringEdit::UpdateEntries { entries });
    }
    if document.definition.exits.iter().any(|item| item == node_id) {
        let remaining_exits = document
            .definition
            .exits
            .iter()
            .filter(|item| item.as_str() != node_id)
            .cloned()
            .collect::<Vec<_>>();
        if remaining_exits.is_empty() {
            return Err("choose another exit before removing the only exit node".to_string());
        }
        edits.push(WorkflowAuthoringEdit::UpdateExits {
            exits: remaining_exits,
        });
    }
    Ok(edits)
}

fn repositioned_layout(
    document: &WorkflowAuthoringDocument,
    selected_node: Option<&str>,
) -> Result<serde_json::Value, String> {
    moved_layout(document, selected_node, 4, 2)
}

fn node_id_base(entry: &PaletteEntry) -> String {
    let source = match entry.kind {
        PaletteEntryKind::NodeKind => entry.identity.as_str(),
        PaletteEntryKind::PluginBlock => entry
            .identity
            .split('/')
            .next_back()
            .and_then(|value| value.split('@').next())
            .unwrap_or(entry.identity.as_str()),
        PaletteEntryKind::WorkflowCall => {
            entry.identity.split('@').next().unwrap_or("workflow-call")
        }
    };
    let base = source
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    base.trim_matches('-').to_string()
}

fn unique_node_id(document: &WorkflowAuthoringDocument, base: &str) -> String {
    let base = if base.is_empty() { "node" } else { base };
    if !document.definition.nodes.contains_key(base) {
        return base.to_string();
    }
    let mut candidate = format!("{base}-2");
    for suffix in 3_u32..=u32::MAX {
        if !document.definition.nodes.contains_key(&candidate) {
            return candidate;
        }
        candidate = format!("{base}-{suffix}");
    }
    format!("{base}-new")
}

fn palette_entries(catalog: &WorkflowAuthoringCatalogSnapshot) -> Vec<PaletteEntry> {
    let mut entries = catalog
        .capabilities
        .node_kinds
        .keys()
        .map(|kind| PaletteEntry {
            identity: kind.clone(),
            label: title_case(kind),
            kind: PaletteEntryKind::NodeKind,
        })
        .collect::<Vec<_>>();
    entries.extend(catalog.blocks.iter().map(|(identity, block)| PaletteEntry {
        identity: identity.clone(),
        label: format!("{} / {}", block.plugin_id, block.block_id),
        kind: PaletteEntryKind::PluginBlock,
    }));
    entries.extend(
        catalog
            .workflow_definitions
            .keys()
            .map(|identity| PaletteEntry {
                identity: identity.clone(),
                label: identity.clone(),
                kind: PaletteEntryKind::WorkflowCall,
            }),
    );
    entries.sort_by(|left, right| {
        left.label
            .cmp(&right.label)
            .then_with(|| left.identity.cmp(&right.identity))
    });
    entries
}

struct InspectorContext<'a> {
    diagnostics: &'a [WorkflowValidationDiagnostic],
    preview: Option<&'a WorkflowCompilationPreview>,
    catalog: Option<&'a WorkflowAuthoringCatalogSnapshot>,
    semantic_diff: Option<&'a bcode_workflow::WorkflowAuthoringSemanticDiff>,
    selected_schema_field: usize,
    selected_edge: usize,
    selected_field_path: Option<&'a str>,
}

#[allow(clippy::too_many_lines)]
fn inspector_lines(
    selected: Option<(&str, &NodeDefinition)>,
    context: &InspectorContext<'_>,
) -> Vec<String> {
    let InspectorContext {
        diagnostics,
        preview,
        catalog,
        semantic_diff,
        selected_schema_field,
        selected_edge,
        selected_field_path,
    } = context;
    let Some((id, node)) = selected else {
        return vec!["No node selected".to_string()];
    };
    let mut lines = vec![
        format!("{id} · {}", node.name),
        format!("Kind: {}", node_kind_label(node.kind)),
        format!("Dataflow: {:?}", node.dataflow),
        format!("Input: {}", node.input.type_name),
    ];
    append_form(&mut lines, "Input contract", &node.input, None);
    lines.push(format!("Output: {}", node.output.type_name));
    append_form(&mut lines, "Output contract", &node.output, None);
    if node.kind == NodeKind::PluginBlock {
        if let Ok(block) =
            serde_json::from_value::<WorkflowBlockDefinition>(node.configuration.clone())
        {
            append_form(
                &mut lines,
                "Plugin operation defaults [f select · F edit]",
                &block.input,
                Some(*selected_schema_field),
            );
        }
    } else if let Some(schema) = catalog.and_then(|catalog| {
        catalog
            .node_configuration_schemas
            .get(node_kind_identity(node.kind))
    }) {
        append_form(
            &mut lines,
            "Configuration [f select · F edit]",
            schema,
            Some(*selected_schema_field),
        );
    }
    if !node.resources.is_empty() {
        lines.push("Resources".to_string());
        lines.extend(
            node.resources
                .iter()
                .map(|resource| format!("  {} · {:?}", resource.resource, resource.access)),
        );
    }
    if node.kind == NodeKind::Agent
        && let Ok(agent) = serde_json::from_value::<bcode_workflow::WorkflowAgentConfiguration>(
            node.configuration.clone(),
        )
    {
        lines.push("Agent controls [a profile · t read-only · M model · S skills]".to_string());
        lines.push(format!(
            "  profile={} · provider={} · model={} · read_only={} · capability={:?}",
            agent.agent_profile,
            agent.provider.as_deref().unwrap_or("catalog default"),
            agent.model.as_deref().unwrap_or("catalog default"),
            agent.read_only,
            agent.tool_capability
        ));
        lines.push(format!(
            "  skills={} · tools={}",
            agent
                .skills
                .iter()
                .map(|skill| skill.skill_id.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            agent.tool_allowlist.join(", ")
        ));
    }
    if matches!(node.kind, NodeKind::Branch | NodeKind::Repeat) {
        lines.push("Predicate control [v toggle · P path]".to_string());
        lines.push(format!("  {}", node.configuration["predicate"]));
    }
    if node.kind == NodeKind::Repeat {
        lines.push(format!(
            "Repeat bound [+ increase · B set]: {}",
            node.configuration["max_iterations"]
        ));
    }
    lines.push("Node control [n edit name]".to_string());
    if node.kind == NodeKind::PluginBlock
        && let Ok(block) =
            serde_json::from_value::<WorkflowBlockDefinition>(node.configuration.clone())
    {
        lines.push("Owner contract".to_string());
        lines.push(format!(
            "  {} / {}@{}",
            block.plugin_id, block.block_id, block.block_version
        ));
        lines.push(format!(
            "  effect={:?} · reconciliation={:?}",
            block.effect, block.reconciliation
        ));
        if let Some(catalog) = catalog {
            let key = bcode_workflow::workflow_block_catalog_key(&block);
            lines.push(if catalog.blocks.contains_key(&key) {
                "  exact catalog contract available".to_string()
            } else {
                "  exact catalog contract unavailable".to_string()
            });
        }
    }
    let prefix = format!("definition.nodes.{id}");
    let edge_prefix = format!("definition.edges.{selected_edge}");
    let field_prefix = selected_field_path.map(|path| format!("{prefix}.configuration.{path}"));
    let node_diagnostics = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.document_path.starts_with(&prefix)
                || diagnostic.document_path.starts_with(&edge_prefix)
                || field_prefix
                    .as_ref()
                    .is_some_and(|field| diagnostic.document_path.starts_with(field))
        })
        .collect::<Vec<_>>();
    if !node_diagnostics.is_empty() {
        lines.push("Diagnostics".to_string());
        lines.extend(node_diagnostics.into_iter().map(|diagnostic| {
            format!(
                "  {:?} {}: {}",
                diagnostic.severity, diagnostic.document_path, diagnostic.message
            )
        }));
    }
    if let Some(compiled) = preview.and_then(|preview| preview.compiled.as_ref()) {
        lines.push("Workflow preview".to_string());
        lines.push(format!(
            "  capability={:?} · effects={:?}",
            compiled.effects.maximum_capability, compiled.effects.block_effects
        ));
        lines.push(format!(
            "  resources={} · grants={} · approvals={}",
            compiled.effects.resources.len(),
            compiled.permissions.explicit_grant_nodes.len(),
            compiled.permissions.mutation_approval_nodes.len()
        ));
        if !compiled.requirements.agents.is_empty() || !compiled.requirements.skills.is_empty() {
            lines.push(format!(
                "  agents={} · skills={}",
                compiled
                    .requirements
                    .agents
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
                compiled
                    .requirements
                    .skills
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let commands = compiled
            .definition
            .nodes
            .values()
            .filter_map(|node| {
                (node.kind == NodeKind::PluginBlock)
                    .then(|| {
                        serde_json::from_value::<WorkflowBlockDefinition>(
                            node.configuration.clone(),
                        )
                        .ok()
                    })
                    .flatten()
            })
            .filter(|block| {
                block.operation.contains("command") || block.operation.contains("shell")
            })
            .map(|block| block.operation)
            .collect::<Vec<_>>();
        if !commands.is_empty() {
            lines.push(format!("  commands={}", commands.join(", ")));
        }
        lines.push(format!(
            "  bounds: nodes={} · concurrency={} · cycles={} · retries={}",
            compiled.run_limits.node_execution_cap,
            compiled.run_limits.concurrency_cap,
            compiled.run_limits.cycle_cap,
            compiled.run_limits.retry_cap
        ));
    }
    if let Some(diff) = semantic_diff {
        lines.push("Revision diff before publication".to_string());
        lines.push(format!(
            "  changes={:?} · capability_increased={}",
            diff.changes, diff.capability_increased
        ));
        lines.push(format!(
            "  nodes +{} -{} ~{} · resources +{} -{}",
            diff.added_nodes.len(),
            diff.removed_nodes.len(),
            diff.changed_nodes.len(),
            diff.added_resources.len(),
            diff.removed_resources.len()
        ));
        lines.push(format!(
            "  effects +{:?} -{:?}",
            diff.added_effect_classes, diff.removed_effect_classes
        ));
    }
    lines
}

fn append_form(
    lines: &mut Vec<String>,
    title: &str,
    schema: &bcode_workflow::ValueSchema,
    selected: Option<usize>,
) {
    let Ok(form) = WorkflowSchemaFormDescription::from_schema(schema) else {
        lines.push(format!("{title}: unsupported schema"));
        return;
    };
    lines.push(title.to_string());
    lines.extend(
        form.fields
            .iter()
            .take(24)
            .enumerate()
            .map(|(index, field)| {
                let required = if field.required {
                    "required"
                } else {
                    "optional"
                };
                let marker = if selected == Some(index) { "▶" } else { " " };
                format!(
                    "{marker} {} · {} · {required}",
                    field.path,
                    form_control_label(field.control)
                )
            }),
    );
    if form.fields.len() > 24 {
        lines.push(format!("  … {} more fields", form.fields.len() - 24));
    }
}

const fn form_control_label(control: WorkflowSchemaFormControl) -> &'static str {
    match control {
        WorkflowSchemaFormControl::Object => "object",
        WorkflowSchemaFormControl::Array => "array",
        WorkflowSchemaFormControl::Text => "text",
        WorkflowSchemaFormControl::Number => "number",
        WorkflowSchemaFormControl::Integer => "integer",
        WorkflowSchemaFormControl::Boolean => "boolean",
        WorkflowSchemaFormControl::Choice => "choice",
        WorkflowSchemaFormControl::Json => "json",
    }
}

fn edge_sources(document: Option<&WorkflowAuthoringDocument>, node_id: &str) -> String {
    let Some(document) = document else {
        return String::new();
    };
    let incoming = document
        .definition
        .edges
        .iter()
        .filter(|edge| edge.to == node_id)
        .map(|edge| edge.from.as_str())
        .collect::<Vec<_>>();
    if incoming.is_empty() {
        String::new()
    } else {
        format!("  ← {}", incoming.join(","))
    }
}

const fn node_kind_label(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Task => "task",
        NodeKind::Agent => "agent",
        NodeKind::Branch => "branch",
        NodeKind::Repeat => "repeat",
        NodeKind::Retry => "retry",
        NodeKind::Parallel => "parallel",
        NodeKind::FanOut => "fan-out",
        NodeKind::PluginBlock => "plugin block",
        NodeKind::Input => "input",
        NodeKind::Approval => "approval",
        NodeKind::WorkflowCall => "workflow call",
    }
}

fn title_case(value: &str) -> String {
    value
        .split('_')
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

const fn editor_rects(area: Rect) -> (Rect, Rect, Rect, Rect) {
    let content_height = area.height.saturating_sub(1);
    let palette_width = area.width.saturating_mul(24) / 100;
    let inspector_width = area.width.saturating_mul(32) / 100;
    let canvas_width = area
        .width
        .saturating_sub(palette_width)
        .saturating_sub(inspector_width);
    let palette = Rect::new(area.x, area.y, palette_width, content_height);
    let canvas = Rect::new(
        area.x.saturating_add(palette_width),
        area.y,
        canvas_width,
        content_height,
    );
    let inspector = Rect::new(
        canvas.x.saturating_add(canvas_width),
        area.y,
        inspector_width,
        content_height,
    );
    let footer = Rect::new(
        area.x,
        area.y.saturating_add(content_height),
        area.width,
        area.height.saturating_sub(content_height),
    );
    (palette, canvas, inspector, footer)
}

fn draw_pane(frame: &mut Frame<'_>, area: Rect, title: &str, focused: bool) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let prefix = if focused { "▶ " } else { "  " };
    frame.write_line(
        Rect::new(area.x, area.y, area.width, 1),
        &Line::from(format!("{prefix}{title}")),
    );
}

fn write_row(frame: &mut Frame<'_>, area: Rect, row: u16, text: &str) {
    if row >= area.height || area.width == 0 {
        return;
    }
    frame.write_line(
        Rect::new(area.x, area.y.saturating_add(row), area.width, 1),
        &Line::from(text.to_string()),
    );
}

const fn keep_visible(scroll: &mut usize, selected: usize, visible: usize) {
    if visible == 0 {
        return;
    }
    if selected < *scroll {
        *scroll = selected;
    } else if selected >= scroll.saturating_add(visible) {
        *scroll = selected.saturating_add(1).saturating_sub(visible);
    }
}

const fn contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x && x < area.right() && y >= area.y && y < area.bottom()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_workflow::{
        ValueSchema, WORKFLOW_AUTHORING_DOCUMENT_VERSION, WorkflowAuthoringMetadata,
        WorkflowAuthoringPresentation, WorkflowDefinition, WorkflowNodeDataflowPolicy,
        WorkflowProducerKind, WorkflowProducerProvenance, WorkflowRequirementSummary,
        WorkflowRunLimitPolicy,
    };
    use bmux_tui::event::{MouseEvent, MouseEventKind};
    use bmux_tui::geometry::Point;
    use std::collections::{BTreeMap, BTreeSet};

    fn document() -> WorkflowAuthoringDocument {
        let schema = ValueSchema {
            type_name: "test/value-v1".to_string(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {"message": {"type": "string"}},
                "required": ["message"]
            }),
        };
        WorkflowAuthoringDocument {
            schema_version: WORKFLOW_AUTHORING_DOCUMENT_VERSION,
            workflow_id: "editor-test".to_string(),
            metadata: WorkflowAuthoringMetadata {
                title: "Editor test".to_string(),
                description: None,
                labels: BTreeMap::new(),
            },
            configuration_schema: schema.clone(),
            configuration_defaults: Some(serde_json::json!({"message": "hello"})),
            plugin_input_defaults: std::collections::BTreeMap::new(),
            definition: WorkflowDefinition {
                schema_version: 1,
                name: "editor-test".to_string(),
                input: schema.clone(),
                output: schema.clone(),
                nodes: BTreeMap::from([
                    (
                        "first".to_string(),
                        NodeDefinition {
                            id: "first".to_string(),
                            name: "First".to_string(),
                            kind: NodeKind::Input,
                            dataflow: WorkflowNodeDataflowPolicy::Direct,
                            input: schema.clone(),
                            output: schema.clone(),
                            resources: Vec::new(),
                            configuration: serde_json::json!({"gate_version": 1}),
                        },
                    ),
                    (
                        "second".to_string(),
                        NodeDefinition {
                            id: "second".to_string(),
                            name: "Second".to_string(),
                            kind: NodeKind::Approval,
                            dataflow: WorkflowNodeDataflowPolicy::Direct,
                            input: schema.clone(),
                            output: schema,
                            resources: Vec::new(),
                            configuration: serde_json::json!({"gate_version": 1}),
                        },
                    ),
                ]),
                entries: vec!["first".to_string()],
                exits: vec!["second".to_string()],
                edges: vec![bcode_workflow::EdgeDefinition {
                    from: "first".to_string(),
                    to: "second".to_string(),
                    kind: bcode_workflow::EdgeKind::Direct,
                    transform: None,
                }],
            },
            bindings: Vec::new(),
            requirements: WorkflowRequirementSummary::default(),
            run_limits: WorkflowRunLimitPolicy::default(),
            producer: WorkflowProducerProvenance {
                kind: WorkflowProducerKind::Plugin,
                producer_id: Some("bcode.workflow".to_string()),
                source_revision: None,
            },
            presentation: Some(WorkflowAuthoringPresentation {
                version: 1,
                namespaces: BTreeMap::new(),
            }),
        }
    }

    #[test]
    fn graph_editor_projects_canvas_and_schema_inspector() {
        let options = serde_json::json!({"template": {"authoring_document": document()}});
        let surface = WorkflowAuthorSurface::new(&options);
        assert_eq!(surface.node_entries().len(), 2);
        let lines = inspector_lines(
            surface.selected_node(),
            &InspectorContext {
                diagnostics: &[],
                preview: None,
                catalog: None,
                semantic_diff: None,
                selected_schema_field: 0,
                selected_edge: 0,
                selected_field_path: None,
            },
        )
        .join("\n");
        assert!(lines.contains("First"));
        assert!(lines.contains("message · text · required"));
        assert!(edge_sources(surface.document.as_ref(), "second").contains("first"));
    }

    #[test]
    fn catalog_palette_is_deterministic_and_catalog_driven() {
        let catalog = WorkflowAuthoringCatalogSnapshot {
            version: bcode_workflow::WORKFLOW_AUTHORING_CATALOG_VERSION,
            capabilities: bcode_workflow::WorkflowAuthoringCapabilitySummary::from(
                &bcode_workflow::WorkflowProductionCapabilities::current(),
            ),
            plugins: BTreeSet::new(),
            blocks: BTreeMap::new(),
            node_configuration_schemas: bcode_workflow::workflow_node_configuration_schemas(),
            workflow_definitions: BTreeMap::new(),
            agent_profiles: BTreeSet::new(),
            skills: BTreeSet::new(),
        };
        let entries = palette_entries(&catalog);
        assert!(entries.iter().any(|entry| entry.identity == "agent"));
        assert!(catalog.node_configuration_schemas.contains_key("agent"));
        let agent_form = WorkflowSchemaFormDescription::from_schema(
            catalog
                .node_configuration_schemas
                .get("agent")
                .expect("agent configuration schema"),
        )
        .expect("agent form");
        assert!(agent_form.fields.iter().any(|field| {
            field.path == "read_only" && field.control == WorkflowSchemaFormControl::Boolean
        }));
        assert!(
            entries
                .windows(2)
                .all(|pair| pair[0].label <= pair[1].label)
        );
    }

    #[test]
    fn typed_plugin_blocks_can_be_added_duplicated_removed_and_repositioned() {
        let mut document = document();
        let block = WorkflowBlockDefinition {
            block_id: "test.echo".to_string(),
            block_version: 1,
            plugin_id: "bcode.test".to_string(),
            operation: "test.echo".to_string(),
            input: document.definition.input.clone(),
            output: document.definition.output.clone(),
            effect: bcode_workflow::WorkflowBlockEffect::ReadOnly,
            resources: Vec::new(),
            authorization: bcode_workflow::WorkflowBlockAuthorization {
                capability: bcode_workflow::WorkflowToolCapability::ReadOnly,
                explicit_grant_required: false,
            },
            timeout_ms: 1_000,
            cancellation_supported: true,
            reconciliation: bcode_workflow::WorkflowBlockReconciliation::IdempotentReplay,
        };
        let key = bcode_workflow::workflow_block_catalog_key(&block);
        let catalog = WorkflowAuthoringCatalogSnapshot {
            version: bcode_workflow::WORKFLOW_AUTHORING_CATALOG_VERSION,
            capabilities: bcode_workflow::WorkflowAuthoringCapabilitySummary::from(
                &bcode_workflow::WorkflowProductionCapabilities::current(),
            ),
            plugins: BTreeSet::from(["bcode.test".to_string()]),
            blocks: BTreeMap::from([(key.clone(), block)]),
            node_configuration_schemas: bcode_workflow::workflow_node_configuration_schemas(),
            workflow_definitions: BTreeMap::new(),
            agent_profiles: BTreeSet::new(),
            skills: BTreeSet::new(),
        };
        let entry = palette_entries(&catalog)
            .into_iter()
            .find(|entry| entry.identity == key)
            .expect("plugin block palette entry");
        let node = add_node(&document, &entry, &catalog).expect("add node");
        assert_eq!(node.kind, NodeKind::PluginBlock);
        document
            .definition
            .nodes
            .insert(node.id.clone(), node.clone());
        let options = serde_json::json!({
            "draft": {
                "identity": {"workflow_id": "editor-test", "draft_id": "draft-1"},
                "generation": 1,
                "document": document.clone(),
                "producer": editor_producer()
            }
        });
        let mut surface = WorkflowAuthorSurface::new(&options);
        surface.catalog = Some(catalog);
        surface.selected_node = surface
            .node_entries()
            .iter()
            .position(|(id, _)| id == &node.id.as_str())
            .expect("plugin node selection");
        let fields = surface.schema_fields();
        let message_field = fields
            .iter()
            .position(|field| field.path == "message")
            .expect("plugin operation input form");
        surface.selected_schema_field = message_field;
        surface.begin_selected_schema_edit();
        let edit = surface
            .inspector_edit
            .as_mut()
            .expect("plugin defaults edit");
        edit.buffer = "configured".to_string();
        let batch = surface
            .mutation_batch(PendingMutation::SetSchemaField)
            .expect("plugin defaults mutation");
        assert!(matches!(
            &batch.edits[0],
            WorkflowAuthoringEdit::UpdatePluginInputDefaults { node_id, defaults: Some(defaults) }
                if node_id == &node.id && defaults["message"] == "configured"
        ));
        assert_eq!(
            surface
                .document
                .as_ref()
                .expect("surface document")
                .definition
                .nodes[&node.id]
                .configuration,
            node.configuration
        );
        let duplicate = duplicate_node(&document, &node);
        assert_ne!(duplicate.id, node.id);
        let layout = repositioned_layout(&document, Some(&node.id)).expect("layout");
        assert_eq!(layout["version"], GRAPH_PRESENTATION_VERSION);
        assert!(layout["nodes"][&node.id]["x"].is_number());
        assert_eq!(
            remove_node_edits(&document, &node.id)
                .expect("remove")
                .first(),
            Some(&WorkflowAuthoringEdit::RemoveNode {
                node_id: node.id.clone()
            })
        );
    }

    #[test]
    fn draft_conflict_requires_explicit_reapply_after_exact_reload() {
        let options = serde_json::json!({
            "draft": {
                "identity": {"workflow_id": "editor-test", "draft_id": "draft-1"},
                "generation": 1,
                "document": document(),
                "producer": editor_producer()
            }
        });
        let mut surface = WorkflowAuthorSurface::new(&options);
        let edits = vec![WorkflowAuthoringEdit::UpdateMetadata {
            metadata: bcode_workflow::WorkflowAuthoringMetadata {
                title: "Reviewed update".to_string(),
                description: None,
                labels: std::collections::BTreeMap::new(),
            },
        }];
        surface.pending_conflict = Some(PendingConflict {
            expected_generation: 1,
            current_generation: 2,
            edits: edits.clone(),
        });
        surface.generation = Some(1);
        surface.resolve_conflict();
        assert!(surface.pending_edit_batch.is_none());
        assert!(surface.pending_conflict.is_some());

        let mut reloaded = document();
        reloaded.metadata.title = "Concurrent update".to_string();
        surface.install_draft(PluginWorkflowAuthoringDraft {
            workflow_id: "editor-test".to_string(),
            draft_id: "draft-1".to_string(),
            base_revision: None,
            generation: 2,
            document: reloaded,
            producer: editor_producer(),
        });
        assert!(surface.status.contains("awaiting explicit R resolution"));
        surface.resolve_conflict();
        let batch = surface.pending_edit_batch.expect("explicit reapply batch");
        assert_eq!(batch.expected_generation, 2);
        assert_eq!(batch.edits, edits);
        assert!(surface.pending_conflict.is_none());
    }

    #[test]
    fn typed_connection_rejects_incompatible_ports() {
        let mut editor_document = document();
        editor_document
            .definition
            .nodes
            .get_mut("second")
            .expect("second")
            .input = ValueSchema {
            type_name: "test/incompatible-v1".to_string(),
            schema: serde_json::json!({"type": "boolean"}),
        };
        let options = serde_json::json!({
            "draft": {
                "identity": {"workflow_id": "editor-test", "draft_id": "draft-1"},
                "generation": 1,
                "document": editor_document,
                "producer": editor_producer()
            }
        });
        let mut surface = WorkflowAuthorSurface::new(&options);
        surface.connect_source = Some("first".to_string());
        surface.selected_node = 1;
        assert!(surface.mutation_batch(PendingMutation::Connect).is_err());
    }

    #[test]
    fn generalized_schema_controls_parse_and_apply_supported_values() {
        assert_eq!(
            parse_schema_control_value(WorkflowSchemaFormControl::Boolean, "true")
                .expect("boolean"),
            serde_json::json!(true)
        );
        assert_eq!(
            parse_schema_control_value(WorkflowSchemaFormControl::Integer, "42").expect("integer"),
            serde_json::json!(42)
        );
        assert_eq!(
            parse_schema_control_value(WorkflowSchemaFormControl::Number, "4.5").expect("number"),
            serde_json::json!(4.5)
        );
        assert_eq!(
            parse_schema_control_value(WorkflowSchemaFormControl::Array, "[1,2]").expect("array"),
            serde_json::json!([1, 2])
        );
        assert!(parse_schema_control_value(WorkflowSchemaFormControl::Boolean, "yes").is_err());

        let mut node = document()
            .definition
            .nodes
            .get("first")
            .cloned()
            .expect("node");
        node.configuration = serde_json::json!({"gate_version": 1, "enabled": false});
        set_schema_field(
            &mut node,
            "enabled",
            WorkflowSchemaFormControl::Boolean,
            "true",
        )
        .expect("field");
        set_schema_field(
            &mut node,
            "nested.limit",
            WorkflowSchemaFormControl::Integer,
            "7",
        )
        .expect("nested field");
        assert_eq!(node.configuration["enabled"], true);
        assert_eq!(node.configuration["nested"]["limit"], 7);
    }

    #[test]
    fn bounded_text_inspector_edits_model_skills_bounds_and_predicate_paths() {
        let editor_document = document();
        let catalog = WorkflowAuthoringCatalogSnapshot {
            version: bcode_workflow::WORKFLOW_AUTHORING_CATALOG_VERSION,
            capabilities: bcode_workflow::WorkflowAuthoringCapabilitySummary::from(
                &bcode_workflow::WorkflowProductionCapabilities::current(),
            ),
            plugins: BTreeSet::new(),
            blocks: BTreeMap::new(),
            node_configuration_schemas: bcode_workflow::workflow_node_configuration_schemas(),
            workflow_definitions: BTreeMap::new(),
            agent_profiles: BTreeSet::from(["build".to_string()]),
            skills: BTreeSet::from(["code-review".to_string(), "commit-message".to_string()]),
        };
        let agent_entry = PaletteEntry {
            identity: "agent".to_string(),
            label: "Agent".to_string(),
            kind: PaletteEntryKind::NodeKind,
        };
        let mut agent = add_node(&editor_document, &agent_entry, &catalog).expect("agent");
        set_agent_model(&mut agent, "model-x".to_string()).expect("model");
        set_agent_skills(&mut agent, "commit-message,code-review", &catalog).expect("skills");
        let configuration: bcode_workflow::WorkflowAgentConfiguration =
            serde_json::from_value(agent.configuration.clone()).expect("configuration");
        assert_eq!(configuration.model.as_deref(), Some("model-x"));
        assert_eq!(configuration.skills.len(), 2);

        let repeat_entry = PaletteEntry {
            identity: "repeat".to_string(),
            label: "Repeat".to_string(),
            kind: PaletteEntryKind::NodeKind,
        };
        let mut repeat = add_node(&editor_document, &repeat_entry, &catalog).expect("repeat");
        set_repeat_bound(&mut repeat, "37").expect("bound");
        set_predicate_path(&mut repeat, "condition_met").expect("path");
        assert_eq!(repeat.configuration["max_iterations"], 37);
        assert_eq!(repeat.configuration["predicate"]["path"], "condition_met");
        assert!(set_repeat_bound(&mut repeat, "0").is_err());
        assert!(set_agent_skills(&mut agent, "missing", &catalog).is_err());
    }

    #[test]
    fn inspector_updates_nodes_through_typed_update_operations() {
        let mut editor_document = document();
        let catalog = WorkflowAuthoringCatalogSnapshot {
            version: bcode_workflow::WORKFLOW_AUTHORING_CATALOG_VERSION,
            capabilities: bcode_workflow::WorkflowAuthoringCapabilitySummary::from(
                &bcode_workflow::WorkflowProductionCapabilities::current(),
            ),
            plugins: BTreeSet::new(),
            blocks: BTreeMap::new(),
            node_configuration_schemas: bcode_workflow::workflow_node_configuration_schemas(),
            workflow_definitions: BTreeMap::new(),
            agent_profiles: BTreeSet::from(["build".to_string(), "review".to_string()]),
            skills: BTreeSet::new(),
        };
        let agent_entry = PaletteEntry {
            identity: "agent".to_string(),
            label: "Agent".to_string(),
            kind: PaletteEntryKind::NodeKind,
        };
        let mut agent = add_node(&editor_document, &agent_entry, &catalog).expect("agent");
        cycle_agent_profile(&mut agent, &catalog).expect("profile");
        toggle_agent_read_only(&mut agent).expect("policy");
        let configuration: bcode_workflow::WorkflowAgentConfiguration =
            serde_json::from_value(agent.configuration.clone()).expect("configuration");
        assert_eq!(configuration.agent_profile, "review");
        assert!(!configuration.read_only);
        assert_eq!(
            configuration.tool_capability,
            bcode_workflow::WorkflowToolCapability::Mutating
        );

        editor_document
            .definition
            .nodes
            .insert(agent.id.clone(), agent);

        let repeat_entry = PaletteEntry {
            identity: "repeat".to_string(),
            label: "Repeat".to_string(),
            kind: PaletteEntryKind::NodeKind,
        };
        let mut repeat = add_node(&editor_document, &repeat_entry, &catalog).expect("repeat");
        increase_repeat_bound(&mut repeat).expect("bound");
        cycle_predicate(&mut repeat).expect("predicate");
        assert_eq!(repeat.configuration["max_iterations"], 21);
        assert_eq!(repeat.configuration["predicate"]["value"], false);
    }

    #[test]
    fn edge_removal_grouping_and_absolute_layout_remain_semantic_edits() {
        let editor_document = document();
        let selector = selected_edge_selector(&editor_document, 0).expect("edge selector");
        assert_eq!(selector.from, "first");
        assert_eq!(selector.to, "second");
        assert_eq!(selector.occurrence, 0);

        let grouped = toggled_group_layout(&editor_document, Some("first")).expect("group");
        assert_eq!(grouped["version"], GRAPH_PRESENTATION_VERSION);
        assert_eq!(
            grouped["groups"]["group-1"]["nodes"],
            serde_json::json!(["first"])
        );
        let moved = moved_layout(&editor_document, Some("first"), -8, 6).expect("move");
        assert_eq!(
            moved["nodes"]["first"],
            serde_json::json!({"x": -8, "y": 6})
        );

        let ungrouped = {
            let mut with_group = editor_document;
            with_group.presentation = Some(WorkflowAuthoringPresentation {
                version: 1,
                namespaces: BTreeMap::from([(GRAPH_PRESENTATION_NAMESPACE.to_string(), grouped)]),
            });
            toggled_group_layout(&with_group, Some("first")).expect("ungroup")
        };
        assert_eq!(
            ungrouped["groups"]["group-1"]["nodes"],
            serde_json::json!([])
        );
    }

    #[test]
    fn supported_generic_palette_nodes_are_constructed_without_json() {
        let editor_document = document();
        let mut catalog = WorkflowAuthoringCatalogSnapshot {
            version: bcode_workflow::WORKFLOW_AUTHORING_CATALOG_VERSION,
            capabilities: bcode_workflow::WorkflowAuthoringCapabilitySummary::from(
                &bcode_workflow::WorkflowProductionCapabilities::current(),
            ),
            plugins: BTreeSet::new(),
            blocks: BTreeMap::new(),
            node_configuration_schemas: bcode_workflow::workflow_node_configuration_schemas(),
            workflow_definitions: BTreeMap::new(),
            agent_profiles: BTreeSet::from(["build".to_string()]),
            skills: BTreeSet::new(),
        };
        for (identity, expected) in [
            ("agent", NodeKind::Agent),
            ("branch", NodeKind::Branch),
            ("repeat", NodeKind::Repeat),
            ("parallel", NodeKind::Parallel),
            ("input", NodeKind::Input),
            ("approval", NodeKind::Approval),
        ] {
            let entry = PaletteEntry {
                identity: identity.to_string(),
                label: title_case(identity),
                kind: PaletteEntryKind::NodeKind,
            };
            let node = add_node(&editor_document, &entry, &catalog).expect("generic node");
            assert_eq!(node.kind, expected);
            assert_eq!(node.input, node.output);
        }
        catalog.agent_profiles.clear();
        let agent = PaletteEntry {
            identity: "agent".to_string(),
            label: "Agent".to_string(),
            kind: PaletteEntryKind::NodeKind,
        };
        assert!(add_node(&editor_document, &agent, &catalog).is_err());
    }

    #[test]
    fn keyboard_and_mouse_change_terminal_selection_only() {
        let options = serde_json::json!({"template": {"authoring_document": document()}});
        let mut surface = WorkflowAuthorSurface::new(&options);
        surface.last_area = Rect::new(0, 0, 120, 30);
        surface.focus = EditorPane::Canvas;
        surface.move_selection(1);
        assert_eq!(surface.selected_node, 1);
        assert!(surface.handle_mouse(&MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(1, 3),
        )));
        assert_eq!(surface.focus, EditorPane::Palette);
        assert_eq!(surface.document.as_ref().expect("document"), &document());
    }
}
