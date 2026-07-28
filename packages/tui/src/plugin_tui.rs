//! Native plugin TUI presentation and surface helpers.

use bcode_plugin::{PluginHost, PluginLoadError, PluginRuntimeHost, StaticBundledPlugin};
use bcode_plugin_sdk::tui::{
    BoxedPluginTuiSurface, PluginTuiArtifactChunk, PluginTuiRegistry, PluginTuiSurfaceOpenRequest,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Bounded route metadata and duration for one generic plugin visual operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginVisualTiming {
    /// Stable operation label.
    pub operation: &'static str,
    /// Routed plugin identifier.
    pub plugin_id: String,
    /// Routed schema identifier.
    pub schema: String,
    /// Duration in microseconds.
    pub duration_micros: u64,
}

/// One bounded adapter diagnostic after generic host routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginVisualDiagnostic {
    /// Routed plugin identifier.
    pub plugin_id: String,
    /// Adapter-owned bounded diagnostic name.
    pub name: String,
    /// Non-negative observation value.
    pub value: u64,
}

/// Process-local presentation state for one TUI instance.
///
/// Registries are retained because visual adapters may accumulate incremental artifact state that
/// later render passes consume.
#[derive(Debug)]
pub struct PluginTuiPresentation {
    host: Arc<PluginHost>,
    registries: Mutex<BTreeMap<String, Arc<PluginTuiRegistry>>>,
    visual_revisions: Mutex<BTreeMap<String, u64>>,
    dirty_visuals: Mutex<BTreeSet<String>>,
    visual_generation: AtomicU64,
    full_generation: AtomicU64,
    timings: Mutex<Vec<PluginVisualTiming>>,
}

impl PluginTuiPresentation {
    /// Create presentation state around a loaded plugin host.
    #[must_use]
    pub fn new(host: PluginHost) -> Self {
        Self::from_shared(Arc::new(host))
    }

    /// Create presentation state around a shared loaded plugin host.
    #[must_use]
    pub const fn from_shared(host: Arc<PluginHost>) -> Self {
        Self {
            host,
            registries: Mutex::new(BTreeMap::new()),
            visual_revisions: Mutex::new(BTreeMap::new()),
            dirty_visuals: Mutex::new(BTreeSet::new()),
            visual_generation: AtomicU64::new(0),
            full_generation: AtomicU64::new(0),
            timings: Mutex::new(Vec::new()),
        }
    }

    /// Return the routing host.
    #[must_use]
    pub fn host(&self) -> &PluginHost {
        &self.host
    }

    /// Return the full presentation generation for registry/adapter replacement.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.full_generation.load(Ordering::Relaxed)
    }

    /// Return the aggregate generation for isolated visual-state updates.
    #[must_use]
    pub fn visual_generation(&self) -> u64 {
        self.visual_generation.load(Ordering::Relaxed)
    }

    /// Return the generic adapter-state revision for one invocation.
    #[must_use]
    pub fn visual_revision(&self, invocation_id: &str) -> u64 {
        self.visual_revisions
            .lock()
            .ok()
            .and_then(|revisions| revisions.get(invocation_id).copied())
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub fn bump_visual_revision_for_test(&self, invocation_id: &str) {
        if let Ok(mut revisions) = self.visual_revisions.lock() {
            let revision = revisions.entry(invocation_id.to_owned()).or_default();
            *revision = revision.wrapping_add(1);
        }
        self.mark_visual_dirty(invocation_id);
    }

    fn mark_visual_dirty(&self, invocation_id: &str) {
        self.visual_generation.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut dirty) = self.dirty_visuals.lock() {
            dirty.insert(invocation_id.to_owned());
        }
    }

    /// Drain at most `limit` invocation identifiers whose retained adapter state changed.
    ///
    /// Identifiers beyond the limit remain dirty for a later render tick.
    pub fn drain_dirty_visuals_bounded(&self, limit: usize) -> BTreeSet<String> {
        self.dirty_visuals.lock().map_or_else(
            |_| BTreeSet::new(),
            |mut dirty| {
                let mut drained = BTreeSet::new();
                for _ in 0..limit {
                    let Some(invocation_id) = dirty.pop_first() else {
                        break;
                    };
                    drained.insert(invocation_id);
                }
                drained
            },
        )
    }

    /// Drain all invocation identifiers whose retained adapter state changed.
    pub fn drain_dirty_visuals(&self) -> BTreeSet<String> {
        self.dirty_visuals
            .lock()
            .map_or_else(|_| BTreeSet::new(), |mut dirty| std::mem::take(&mut *dirty))
    }

    #[cfg(test)]
    pub fn install_registry_for_test(&self, plugin_id: &str, registry: PluginTuiRegistry) {
        if let Ok(mut registries) = self.registries.lock() {
            registries.insert(plugin_id.to_owned(), Arc::new(registry));
            self.full_generation.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Return one retained native TUI registry.
    #[must_use]
    pub fn registry(&self, plugin_id: &str) -> Option<Arc<PluginTuiRegistry>> {
        let mut registries = self.registries.lock().ok()?;
        if let Some(registry) = registries.get(plugin_id).cloned() {
            return Some(registry);
        }
        let registry = Arc::new(tui_registry(plugin_id)?);
        registries.insert(plugin_id.to_owned(), Arc::clone(&registry));
        drop(registries);
        Some(registry)
    }

    /// Return whether the host can route one visual to a native TUI adapter.
    #[must_use]
    pub fn accepts_visual(
        &self,
        producer_plugin_id: &str,
        schema: &str,
        schema_version: u32,
    ) -> bool {
        let producer = Some(producer_plugin_id);
        self.host
            .visual_adapter(schema, schema_version, "tui", producer)
            .and_then(|route| self.registry(&route.plugin_id))
            .is_some()
    }

    /// Return whether the routed adapter consumes bytes from one artifact reference.
    #[must_use]
    pub fn accepts_artifact_reference(
        &self,
        producer_plugin_id: &str,
        schema: &str,
        schema_version: u32,
        reference_key: &str,
        content_type: Option<&str>,
    ) -> bool {
        let producer = Some(producer_plugin_id);
        let Some(route) = self
            .host
            .visual_adapter(schema, schema_version, "tui", producer)
        else {
            return false;
        };
        self.registry(&route.plugin_id).is_some_and(|registry| {
            registry.visual_accepts_artifact_reference(&route.schema, reference_key, content_type)
        })
    }

    /// Drain bounded diagnostics from retained plugin visual registries.
    pub fn drain_diagnostics(&self) -> Vec<PluginVisualDiagnostic> {
        const MAX_DIAGNOSTICS: usize = 64;
        let Ok(registries) = self.registries.lock() else {
            return Vec::new();
        };
        registries
            .iter()
            .flat_map(|(plugin_id, registry)| {
                registry
                    .drain_visual_diagnostics()
                    .into_iter()
                    .filter(|diagnostic| valid_diagnostic_name(&diagnostic.name))
                    .map(|diagnostic| PluginVisualDiagnostic {
                        plugin_id: plugin_id.clone(),
                        name: diagnostic.name,
                        value: diagnostic.value,
                    })
            })
            .take(MAX_DIAGNOSTICS)
            .collect()
    }

    /// Drain bounded generic visual-operation timings.
    pub fn drain_timings(&self) -> Vec<PluginVisualTiming> {
        self.timings
            .lock()
            .map_or_else(|_| Vec::new(), |mut timings| std::mem::take(&mut *timings))
    }

    /// Record one bounded routed visual-operation timing.
    pub fn record_visual_timing(
        &self,
        operation: &'static str,
        plugin_id: &str,
        schema: &str,
        started: Instant,
    ) {
        self.record_timing(PluginVisualTiming {
            operation,
            plugin_id: plugin_id.to_owned(),
            schema: schema.to_owned(),
            duration_micros: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
        });
    }

    fn record_timing(&self, timing: PluginVisualTiming) {
        if let Ok(mut timings) = self.timings.lock() {
            const MAX_PENDING_TIMINGS: usize = 256;
            if timings.len() < MAX_PENDING_TIMINGS {
                timings.push(timing);
            }
        }
    }

    /// Deliver opaque artifact bytes to the retained adapter selected by generic routing metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when the owning adapter rejects the chunk.
    pub fn deliver_artifact_chunk(&self, chunk: &PluginTuiArtifactChunk) -> Result<bool, String> {
        let producer = Some(chunk.producer_plugin_id.as_str());
        let Some(route) =
            self.host
                .visual_adapter(&chunk.schema, chunk.schema_version, "tui", producer)
        else {
            return Ok(false);
        };
        let Some(registry) = self.registry(&route.plugin_id) else {
            return Ok(false);
        };
        let started = Instant::now();
        let delivered = registry.visual_artifact_chunk(chunk)?;
        self.record_timing(PluginVisualTiming {
            operation: "artifact_delivery",
            plugin_id: route.plugin_id.clone(),
            schema: route.schema,
            duration_micros: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
        });
        if delivered && let Ok(mut revisions) = self.visual_revisions.lock() {
            let revision = revisions.entry(chunk.tool_call_id.clone()).or_default();
            *revision = revision.wrapping_add(1);
        }
        if delivered {
            self.mark_visual_dirty(&chunk.tool_call_id);
        }
        Ok(delivered)
    }
}

fn valid_diagnostic_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_')
        })
}

/// Return a newly constructed platform-owned TUI registry for an enabled bundled plugin.
///
/// Long-lived visual rendering must acquire registries through [`PluginTuiPresentation`]. Fresh
/// registries remain appropriate for opening independent interactive surface instances.
#[must_use]
pub fn tui_registry(plugin_id: &str) -> Option<PluginTuiRegistry> {
    let registry = bcode_bundled_plugins::tui_registry(plugin_id);
    #[cfg(test)]
    let registry = registry.or_else(|| match plugin_id {
        "bcode.filesystem" => Some(bcode_filesystem_plugin::filesystem_tui_registry()),
        "bcode.git" => Some(bcode_git_plugin::git_tui_registry()),
        "bcode.question" => Some(bcode_question_plugin::question_tui_registry()),
        "bcode.shell" => Some(bcode_shell_plugin::shell_tui_registry()),
        "bcode.vim-edit" => Some(bcode_vim_edit_plugin::vim_edit_tui_registry()),
        _ => None,
    });
    registry
}

/// Load the default local plugin host for TUI client-side services.
///
/// # Errors
///
/// Returns plugin loading errors from discovery, loading, or activation.
pub fn load_default_host_with_static_bundled(
    static_plugins: &[StaticBundledPlugin],
) -> Result<PluginHost, PluginLoadError> {
    let selection = bcode_plugin::PluginSelection::all_enabled();
    if let Ok(host) = PluginHost::load_defaults_with_static_bundled(&selection, static_plugins) {
        Ok(host)
    } else {
        let selected = bcode_plugin::filter_selected_static_plugins(static_plugins, &selection)?;
        let visual_plugins = selected
            .into_iter()
            .filter(|(manifest, _)| {
                tui_registry(&manifest.id).is_some_and(|registry| {
                    manifest.visual_adapters.iter().any(|adapter| {
                        (adapter.surfaces.is_empty()
                            || adapter.surfaces.iter().any(|surface| surface == "tui"))
                            && registry.supports_visual(&adapter.schema)
                    })
                })
            })
            .collect::<Vec<_>>();
        Ok(PluginHost::load_static_plugins_best_effort(&visual_plugins))
    }
}

/// Load persistent presentation state for TUI client-side visual adapters.
///
/// # Errors
///
/// Returns plugin loading errors from discovery, loading, or activation.
pub fn load_default_presentation_with_static_bundled(
    static_plugins: &[StaticBundledPlugin],
) -> Result<PluginTuiPresentation, PluginLoadError> {
    load_default_host_with_static_bundled(static_plugins).map(PluginTuiPresentation::new)
}

/// Load the default local plugin runtime for TUI client-side services.
///
/// # Errors
///
/// Returns plugin loading errors from discovery, loading, or activation.
pub fn load_default_runtime_with_static_bundled(
    static_plugins: &[StaticBundledPlugin],
) -> Result<PluginRuntimeHost, PluginLoadError> {
    PluginRuntimeHost::load_defaults_with_static_bundled(
        &bcode_plugin::PluginSelection::all_enabled(),
        static_plugins,
    )
}

/// Open a native TUI surface from a platform-owned registry.
///
/// # Errors
///
/// Returns an error when the plugin is not loaded, has no native TUI registry, or the surface
/// factory fails to open the surface.
pub async fn open_plugin_tui_surface(
    runtime: &PluginRuntimeHost,
    plugin_id: &str,
    surface_kind: &str,
    request: PluginTuiSurfaceOpenRequest,
) -> Result<BoxedPluginTuiSurface, PluginLoadError> {
    if !runtime
        .plugin_ids()
        .iter()
        .any(|loaded| loaded == plugin_id)
    {
        return Err(PluginLoadError::PluginNotLoaded(plugin_id.to_string()));
    }
    let registry = tui_registry(plugin_id)
        .ok_or_else(|| PluginLoadError::PluginNotLoaded(plugin_id.to_string()))?;
    if runtime
        .registry()
        .tui_surface(plugin_id, surface_kind)
        .is_none()
    {
        return Err(PluginLoadError::TuiSurfaceOpen {
            plugin_id: plugin_id.to_string(),
            message: format!("plugin does not declare TUI surface kind '{surface_kind}'"),
        });
    }
    registry
        .open(surface_kind, request)
        .await
        .map_err(|error| PluginLoadError::TuiSurfaceOpen {
            plugin_id: plugin_id.to_string(),
            message: error.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::prelude::Line;

    #[derive(Default)]
    struct StatefulTestAdapter {
        bytes: Mutex<Vec<u8>>,
        chunks: Mutex<Vec<(String, String, u64, u64)>>,
    }

    impl bcode_plugin_sdk::tui::PluginTuiVisualAdapter for StatefulTestAdapter {
        fn supports(&self, kind: &str) -> bool {
            kind == "bcode.shell.run"
        }

        fn accepts_artifact_reference(
            &self,
            kind: &str,
            reference_key: &str,
            content_type: Option<&str>,
        ) -> bool {
            kind == "bcode.shell.run"
                && reference_key == "shell_recording"
                && content_type == Some("application/x-bcode-shell-recording; version=3")
        }

        fn artifact_chunk(&self, chunk: &PluginTuiArtifactChunk) -> Result<(), String> {
            self.bytes
                .lock()
                .map_err(|_| "test adapter state poisoned".to_owned())?
                .extend_from_slice(&chunk.bytes);
            self.chunks
                .lock()
                .map_err(|_| "test adapter chunk state poisoned".to_owned())?
                .push((
                    chunk.artifact_id.clone(),
                    chunk.reference_key.clone(),
                    chunk.offset,
                    chunk.revision,
                ));
            Ok(())
        }

        fn rows(
            &self,
            _kind: &str,
            _payload: &serde_json::Value,
            _context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
        ) -> Vec<Line> {
            let text = self
                .bytes
                .lock()
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default();
            vec![Line::from(text)]
        }
    }

    fn test_presentation() -> PluginTuiPresentation {
        let bundled = [StaticBundledPlugin::new(
            include_str!("../../../plugins/shell-plugin/bcode-plugin.toml"),
            bcode_shell_plugin::static_plugin(),
        )];
        let selected = bcode_plugin::filter_selected_static_plugins(
            &bundled,
            &bcode_plugin::PluginSelection::all_enabled(),
        )
        .expect("select test plugin");
        let host = PluginHost::load_static_plugins(&selected).expect("load test plugin");
        PluginTuiPresentation::new(host)
    }

    fn deliver_shell_recording_commit(
        presentation: &PluginTuiPresentation,
        commit: &bcode_shell_plugin::recording::ShellRecordingCommit,
        previous_bytes: u64,
        revision: u64,
    ) -> u64 {
        let mut bytes = std::fs::read(&commit.path).expect("recording prefix");
        bytes.truncate(usize::try_from(commit.committed_bytes).expect("committed length"));
        bytes.drain(..usize::try_from(previous_bytes).expect("offset"));
        assert!(
            presentation
                .deliver_artifact_chunk(&PluginTuiArtifactChunk {
                    tool_call_id: "shell-call".to_owned(),
                    artifact_id: "shell-artifact".to_owned(),
                    reference_key: "shell_recording".to_owned(),
                    producer_plugin_id: "bcode.shell".to_owned(),
                    schema: "bcode.shell.run".to_owned(),
                    schema_version: 1,
                    content_type: Some(
                        "application/x-bcode-shell-recording; version=3".to_owned(),
                    ),
                    offset: previous_bytes,
                    total_bytes: commit.committed_bytes,
                    revision,
                    finalized: commit.finalized,
                    bytes,
                })
                .expect("deliver growing artifact chunk")
        );
        assert_eq!(presentation.visual_revision("shell-call"), revision);
        assert_eq!(
            presentation.drain_dirty_visuals(),
            BTreeSet::from(["shell-call".to_owned()])
        );
        commit.committed_bytes
    }

    #[test]
    fn dirty_visual_drain_is_bounded_and_retains_overflow_for_next_tick() {
        let presentation = test_presentation();
        for index in 0..100 {
            presentation.bump_visual_revision_for_test(&format!("call-{index:03}"));
        }
        let first = presentation.drain_dirty_visuals_bounded(64);
        assert_eq!(first.len(), 64);
        assert_eq!(first.first().map(String::as_str), Some("call-000"));
        assert_eq!(first.last().map(String::as_str), Some("call-063"));
        let second = presentation.drain_dirty_visuals_bounded(64);
        assert_eq!(second.len(), 36);
        assert_eq!(second.first().map(String::as_str), Some("call-064"));
        assert_eq!(second.last().map(String::as_str), Some("call-099"));
        assert!(presentation.drain_dirty_visuals_bounded(64).is_empty());
    }

    #[test]
    fn one_visual_revision_changes_only_its_transcript_signature() {
        let presentation = test_presentation();
        let first = crate::transcript::tool_request_item(
            "call-one",
            Some("bcode.shell"),
            "shell",
            "{}",
            None,
        );
        let second = crate::transcript::tool_request_item(
            "call-two",
            Some("bcode.shell"),
            "shell",
            "{}",
            None,
        );
        let first_before =
            crate::transcript_projection::test_layout_signature(&first, 80, Some(&presentation));
        let second_before =
            crate::transcript_projection::test_layout_signature(&second, 80, Some(&presentation));

        presentation.bump_visual_revision_for_test("call-one");

        let first_after =
            crate::transcript_projection::test_layout_signature(&first, 80, Some(&presentation));
        let second_after =
            crate::transcript_projection::test_layout_signature(&second, 80, Some(&presentation));
        assert_ne!(first_before, first_after);
        assert_eq!(second_before, second_after);
    }

    #[test]
    #[ignore = "manual deterministic performance baseline"]
    fn targeted_visual_update_transcript_baseline_report() {
        for transcript_len in [10_usize, 500, 2_000] {
            let presentation = test_presentation();
            let items = (0..transcript_len)
                .map(|index| {
                    crate::transcript::tool_request_item(
                        &format!("call-{index}"),
                        Some("bcode.shell"),
                        "shell",
                        "{}",
                        None,
                    )
                })
                .collect::<Vec<_>>();
            let mut cache = crate::transcript_layout::TranscriptLayoutCache::default();
            cache.sync(crate::transcript_layout::TranscriptLayoutSpec {
                width: 80,
                fingerprint: crate::transcript_layout::TranscriptLayoutFingerprint::new(
                    "baseline-initial".to_owned(),
                ),
                structural_fingerprint: crate::transcript_layout::TranscriptLayoutFingerprint::new(
                    "baseline-structure".to_owned(),
                ),
                transcript_len,
                pending_len: 0,
                transcript_signature: |index| {
                    crate::transcript_projection::test_layout_signature(
                        &items[index],
                        80,
                        Some(&presentation),
                    )
                },
                transcript_rows: |index| vec![Line::from(format!("row-{index}"))],
                transcript_invocation_id: |index| Some(format!("call-{index}")),
                pending_signature: |index| {
                    crate::transcript_layout::TranscriptLayoutSignature::new(format!(
                        "pending-{index}"
                    ))
                },
                pending_rows: |_| Vec::new(),
                history_banner_signature: || None,
                history_banner_rows: Vec::new,
                reset: || false,
            });
            presentation.bump_visual_revision_for_test("call-0");
            let started = Instant::now();
            let stats = cache.sync_visuals(
                crate::transcript_layout::TranscriptLayoutFingerprint::new(
                    "baseline-updated".to_owned(),
                ),
                &std::collections::BTreeSet::from(["call-0".to_owned()]),
                |index| {
                    crate::transcript_projection::test_layout_signature(
                        &items[index],
                        80,
                        Some(&presentation),
                    )
                },
                |index| vec![Line::from(format!("row-{index}"))],
            );
            println!(
                "BCODE_PERF_CASE {}",
                serde_json::json!({
                    "domain": "transcript_visual_update",
                    "transcript_entries": transcript_len,
                    "entries_scanned": stats.entries_scanned,
                    "signatures_changed": stats.signatures_changed,
                    "entries_rebuilt": stats.entries_rebuilt,
                    "rows_regenerated": stats.rows_regenerated,
                    "sync_us": stats.duration_micros,
                    "wall_us": u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
                })
            );
        }
    }

    #[test]
    fn one_visual_update_rebuilds_only_its_entry_across_transcript_sizes() {
        for transcript_len in [10_usize, 500, 2_000] {
            let presentation = test_presentation();
            let items = (0..transcript_len)
                .map(|index| {
                    crate::transcript::tool_request_item(
                        &format!("call-{index}"),
                        Some("bcode.shell"),
                        "shell",
                        "{}",
                        None,
                    )
                })
                .collect::<Vec<_>>();
            let mut cache = crate::transcript_layout::TranscriptLayoutCache::default();
            let initial = cache.sync(crate::transcript_layout::TranscriptLayoutSpec {
                width: 80,
                fingerprint: crate::transcript_layout::TranscriptLayoutFingerprint::new(
                    "initial".to_owned(),
                ),
                structural_fingerprint: crate::transcript_layout::TranscriptLayoutFingerprint::new(
                    "structure".to_owned(),
                ),
                transcript_len,
                pending_len: 0,
                transcript_signature: |index| {
                    crate::transcript_projection::test_layout_signature(
                        &items[index],
                        80,
                        Some(&presentation),
                    )
                },
                transcript_rows: |index| vec![Line::from(format!("row-{index}"))],
                transcript_invocation_id: |index| Some(format!("call-{index}")),
                pending_signature: |index| {
                    crate::transcript_layout::TranscriptLayoutSignature::new(format!(
                        "pending-{index}"
                    ))
                },
                pending_rows: |_| Vec::new(),
                history_banner_signature: || None,
                history_banner_rows: Vec::new,
                reset: || false,
            });
            assert_eq!(initial.entries_rebuilt, transcript_len);

            presentation.bump_visual_revision_for_test("call-0");
            let updated = cache.sync_visuals(
                crate::transcript_layout::TranscriptLayoutFingerprint::new("updated".to_owned()),
                &std::collections::BTreeSet::from(["call-0".to_owned()]),
                |index| {
                    crate::transcript_projection::test_layout_signature(
                        &items[index],
                        80,
                        Some(&presentation),
                    )
                },
                |index| vec![Line::from(format!("row-{index}"))],
            );
            assert_eq!(updated.entries_scanned, 1);
            assert_eq!(updated.signatures_changed, 1);
            assert_eq!(updated.entries_rebuilt, 1);
            assert_eq!(updated.rows_regenerated, 1);
        }
    }

    #[test]
    fn git_contribution_schema_routes_through_platform_registry() {
        let bundled = [StaticBundledPlugin::new(
            include_str!("../../../plugins/git-plugin/bcode-plugin.toml"),
            bcode_git_plugin::static_plugin(),
        )];
        let selected = bcode_plugin::filter_selected_static_plugins(
            &bundled,
            &bcode_plugin::PluginSelection::all_enabled(),
        )
        .expect("select Git plugin");
        let presentation = PluginTuiPresentation::new(
            PluginHost::load_static_plugins(&selected).expect("load Git plugin"),
        );
        let route = presentation
            .host()
            .visual_adapter("bcode.git.clone_request", 1, "tui", Some("bcode.git"))
            .expect("Git contribution route");
        assert_eq!(route.plugin_id, "bcode.git");
        let registry = presentation.registry("bcode.git").expect("Git registry");
        let rows = registry
            .visual_rows(
                "bcode.git.clone_request",
                &serde_json::json!({
                    "url": "https://github.com/bmorphism/bcode",
                    "ref": "main"
                }),
                &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                    80,
                    bcode_plugin_sdk::tui::PluginTuiDiffLayout::Unified,
                    None,
                ),
            )
            .expect("Git contribution rows");
        let rendered = rows
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_str())
            .collect::<String>();
        assert!(rendered.contains("github.com/bmorphism/bcode"));
        assert!(rendered.contains("main"));
    }

    #[test]
    fn growing_shell_artifact_advances_only_owner_revision() {
        let presentation = test_presentation();
        let producer = "bcode.shell";
        let schema = "bcode.shell.run";
        let reference_key = "shell_recording";
        let content_type = "application/x-bcode-shell-recording; version=3";
        assert!(presentation.accepts_artifact_reference(
            producer,
            schema,
            1,
            reference_key,
            Some(content_type),
        ));
        let directory = tempfile::tempdir().expect("recording temp dir");
        let path = directory.path().join("growing.bcsr");
        let (commit_sender, commit_receiver) = std::sync::mpsc::channel();
        let observer: bcode_shell_plugin::recording::ShellRecordingCommitObserver =
            Arc::new(move |commit| {
                let _ = commit_sender.send(commit);
            });
        let mut writer =
            bcode_shell_plugin::recording::AsyncShellRecordingWriter::create_with_observer(
                &path,
                80,
                24,
                Some(observer),
            )
            .expect("recording writer");
        let mut previous_bytes = deliver_shell_recording_commit(
            &presentation,
            &commit_receiver
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("initial recording commit"),
            0,
            1,
        );
        let mut delivered_revisions = 1_u64;
        for revision in 1..=32 {
            writer
                .write_output_with(
                    revision,
                    format!("raw-{revision}\n").as_bytes(),
                    Some(format!("rendered-{revision}\r\n").as_bytes()),
                    || {},
                )
                .expect("recording output");
            delivered_revisions = delivered_revisions.saturating_add(1);
            previous_bytes = deliver_shell_recording_commit(
                &presentation,
                &commit_receiver
                    .recv_timeout(std::time::Duration::from_secs(1))
                    .expect("growing recording commit"),
                previous_bytes,
                delivered_revisions,
            );
        }
        writer
            .finish(33, Some(0), None, false, false)
            .expect("finish recording");
        delivered_revisions = delivered_revisions.saturating_add(1);
        let final_commit = commit_receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("final recording commit");
        assert!(final_commit.finalized);
        let _final_bytes = deliver_shell_recording_commit(
            &presentation,
            &final_commit,
            previous_bytes,
            delivered_revisions,
        );
        assert_eq!(presentation.visual_revision("other-call"), 0);
        assert_eq!(
            presentation.drain_timings().len(),
            usize::try_from(delivered_revisions).expect("revisions")
        );
    }

    #[test]
    fn artifact_delivery_routes_only_supported_references_and_invalidates_the_owner() {
        let presentation = test_presentation();
        let mut registry = PluginTuiRegistry::default();
        registry.register_visual_adapter(Box::new(StatefulTestAdapter::default()));
        presentation
            .registries
            .lock()
            .expect("presentation registries")
            .insert("bcode.shell".to_owned(), Arc::new(registry));

        assert!(presentation.accepts_artifact_reference(
            "bcode.shell",
            "bcode.shell.run",
            1,
            "shell_recording",
            Some("application/x-bcode-shell-recording; version=3"),
        ));
        assert!(!presentation.accepts_artifact_reference(
            "bcode.shell",
            "bcode.shell.run",
            1,
            "clean_output",
            Some("text/plain"),
        ));
        assert!(!presentation.accepts_artifact_reference(
            "bcode.shell",
            "bcode.shell.run",
            1,
            "shell_recording",
            Some("text/plain"),
        ));

        assert!(
            presentation
                .deliver_artifact_chunk(&PluginTuiArtifactChunk {
                    tool_call_id: "call-owner".to_owned(),
                    artifact_id: "artifact-owner".to_owned(),
                    reference_key: "shell_recording".to_owned(),
                    producer_plugin_id: "bcode.shell".to_owned(),
                    schema: "bcode.shell.run".to_owned(),
                    schema_version: 1,
                    content_type: Some(
                        "application/x-bcode-shell-recording; version=3".to_owned(),
                    ),
                    offset: 0,
                    total_bytes: 3,
                    revision: 7,
                    finalized: false,
                    bytes: b"abc".to_vec(),
                })
                .expect("deliver owned artifact chunk")
        );
        assert_eq!(presentation.visual_revision("call-owner"), 1);
        assert_eq!(presentation.visual_revision("call-other"), 0);
        assert_eq!(
            presentation.drain_dirty_visuals(),
            BTreeSet::from(["call-owner".to_owned()])
        );
    }

    #[test]
    fn presentation_retains_one_registry_for_delivery_and_rendering() {
        let presentation = test_presentation();
        assert!(presentation.accepts_artifact_reference(
            "bcode.shell",
            "bcode.shell.run",
            1,
            "shell_recording",
            Some("application/x-bcode-shell-recording; version=3"),
        ));
        assert!(!presentation.accepts_artifact_reference(
            "bcode.shell",
            "bcode.shell.run",
            1,
            "clean_output",
            Some("text/plain; charset=utf-8"),
        ));
        let mut registry = PluginTuiRegistry::default();
        registry.register_visual_adapter(Box::new(StatefulTestAdapter::default()));
        presentation
            .registries
            .lock()
            .expect("presentation registries")
            .insert("bcode.shell".to_owned(), Arc::new(registry));

        let first = presentation.registry("bcode.shell").expect("registry");
        assert_eq!(presentation.revision(), 0);
        assert!(
            presentation
                .deliver_artifact_chunk(&PluginTuiArtifactChunk {
                    tool_call_id: "call".to_owned(),
                    artifact_id: "artifact".to_owned(),
                    reference_key: "reference".to_owned(),
                    producer_plugin_id: "bcode.shell".to_owned(),
                    schema: "bcode.shell.run".to_owned(),
                    schema_version: 1,
                    content_type: None,
                    offset: 0,
                    total_bytes: 3,
                    revision: 1,
                    finalized: false,
                    bytes: b"abc".to_vec(),
                })
                .expect("deliver artifact chunk")
        );
        assert_eq!(presentation.revision(), 0);
        assert_eq!(presentation.visual_revision("call"), 1);
        assert_eq!(presentation.visual_revision("other-call"), 0);
        assert_eq!(
            presentation.drain_dirty_visuals(),
            BTreeSet::from(["call".to_owned()])
        );

        let second = presentation.registry("bcode.shell").expect("registry");
        assert!(Arc::ptr_eq(&first, &second));
        let rows = second
            .visual_rows(
                "bcode.shell.run",
                &serde_json::Value::Null,
                &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                    80,
                    bcode_plugin_sdk::tui::PluginTuiDiffLayout::Unified,
                    None,
                ),
            )
            .expect("stateful adapter rows");
        assert_eq!(rows[0].spans[0].content, "abc");
    }
}
