//! Loaded-runtime tool discovery snapshots. Discovery owns freshness; dispatch and
//! streamed presentation consume immutable snapshots and never refresh warm entries.

use std::{collections::BTreeMap, sync::Arc};

use bcode_tool::{ListToolsRequest, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID, ToolList};
use futures::StreamExt as _;
use tokio::sync::Mutex;

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    runtime: Option<bcode_plugin::PluginRuntimeGeneration>,
    pub(super) lists: BTreeMap<String, ToolList>,
    pub(super) failures: BTreeMap<String, String>,
}

impl Snapshot {
    pub(super) fn catalog_for_runtime(
        &self,
        runtime: &bcode_plugin::PluginRuntimeHost,
    ) -> Result<super::UnifiedToolCatalog, String> {
        if self.runtime.as_ref() != Some(&runtime.runtime_generation()) {
            return Err("tool catalog runtime generation changed".to_owned());
        }
        let mut catalog = super::UnifiedToolCatalog::new();
        let mut names = std::collections::BTreeSet::new();
        for (plugin_id, list) in &self.lists {
            for definition in &list.tools {
                if names.insert(definition.name.clone()) {
                    catalog.insert(super::RegisteredTool::plugin(
                        definition.clone(),
                        plugin_id.clone(),
                    ));
                }
            }
        }
        Ok(catalog)
    }

    pub(super) fn producer_for_runtime(
        &self,
        runtime: &bcode_plugin::PluginRuntimeHost,
        name: &str,
    ) -> Option<String> {
        if self.runtime.as_ref() != Some(&runtime.runtime_generation()) {
            return None;
        }
        self.lists.iter().find_map(|(plugin_id, list)| {
            list.tools
                .iter()
                .any(|tool| tool.name == name)
                .then(|| plugin_id.clone())
        })
    }

    pub(super) fn discovery(
        &self,
    ) -> Vec<(
        bcode_tool::ToolDefinition,
        Option<bcode_tool::ToolDiscoveryPolicy>,
    )> {
        let mut names = std::collections::BTreeSet::new();
        self.lists
            .values()
            .flat_map(|list| {
                list.tools
                    .iter()
                    .filter(|tool| names.insert(tool.name.clone()))
                    .map(|tool| (tool.clone(), list.discovery.get(&tool.name).cloned()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

#[derive(Debug)]
struct Generation {
    runtime: bcode_plugin::PluginRuntimeGeneration,
    revision: u64,
    snapshot: Arc<Snapshot>,
}

#[derive(Debug, Default)]
pub struct ToolCatalogCache {
    published: std::sync::Mutex<Option<Generation>>,
    refresh: Mutex<()>,
}

impl ToolCatalogCache {
    fn current(&self, runtime: &bcode_plugin::PluginRuntimeHost) -> Option<(u64, Arc<Snapshot>)> {
        self.published
            .lock()
            .expect("tool catalog lock poisoned")
            .as_ref()
            .filter(|generation| generation.runtime == runtime.runtime_generation())
            .map(|generation| (generation.revision, Arc::clone(&generation.snapshot)))
    }

    /// Presentation must never start plugin work or wait behind discovery.
    #[cfg(test)]
    pub(super) fn peek(&self, runtime: &bcode_plugin::PluginRuntimeHost) -> Option<Arc<Snapshot>> {
        self.current(runtime).map(|(_, snapshot)| snapshot)
    }

    pub(super) async fn get(
        &self,
        runtime: &bcode_plugin::PluginRuntimeHost,
        discover: bool,
    ) -> Arc<Snapshot> {
        self.get_with(runtime, discover, |plugin_id| async move {
            runtime
                .invoke_service_json::<_, ToolList>(
                    &plugin_id,
                    TOOL_SERVICE_INTERFACE_ID,
                    OP_LIST_TOOLS,
                    &ListToolsRequest::default(),
                )
                .await
                .map_err(|_| "tool catalog discovery failed".to_owned())
        })
        .await
    }

    async fn get_with<F, Fut>(
        &self,
        runtime: &bcode_plugin::PluginRuntimeHost,
        discover: bool,
        load: F,
    ) -> Arc<Snapshot>
    where
        F: Fn(String) -> Fut + Sync,
        Fut: Future<Output = Result<ToolList, String>>,
    {
        let observed = self.current(runtime);
        if let Some((_, snapshot)) = &observed
            && snapshot.failures.is_empty()
            && (!discover || snapshot.lists.values().all(ToolList::is_runtime_stable))
        {
            return Arc::clone(snapshot);
        }
        let refresh = self.refresh.lock().await;
        let current = self.current(runtime);
        // A concurrent discovery already refreshed this generation, including its
        // failures. Share that result instead of creating a retry storm.
        if let Some((revision, snapshot)) = &current
            && observed.as_ref().map(|(revision, _)| revision) != Some(revision)
        {
            return Arc::clone(snapshot);
        }
        let mut snapshot = Snapshot {
            runtime: Some(runtime.runtime_generation()),
            ..Snapshot::default()
        };
        let plugin_ids = runtime
            .registry()
            .manifests()
            .values()
            .filter(|manifest| {
                manifest
                    .services
                    .iter()
                    .any(|service| service.interface_id == TOOL_SERVICE_INTERFACE_ID)
            })
            .map(|manifest| manifest.id.clone())
            .collect::<Vec<_>>();
        let mut pending = Vec::new();
        for plugin_id in plugin_ids {
            let stable = current
                .as_ref()
                .and_then(|(_, previous)| previous.lists.get(&plugin_id))
                .filter(|list| list.is_runtime_stable());
            if let Some(list) = stable {
                snapshot.lists.insert(plugin_id, list.clone());
            } else {
                snapshot
                    .failures
                    .insert(plugin_id.clone(), "tool discovery pending".to_owned());
                pending.push(plugin_id);
            }
        }
        let revision = current.map_or(1, |(revision, _)| revision.saturating_add(1));
        self.publish(runtime, revision, &snapshot);

        let mut results = futures::stream::iter(pending)
            .map(|plugin_id| {
                let load = &load;
                async move {
                    let result = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        load(plugin_id.clone()),
                    )
                    .await
                    .unwrap_or_else(|_| Err("tool catalog provider timed out".to_owned()));
                    (plugin_id, result)
                }
            })
            .buffer_unordered(8);
        while let Some((plugin_id, result)) = results.next().await {
            match result {
                Ok(list) => {
                    snapshot.failures.remove(&plugin_id);
                    snapshot.lists.insert(plugin_id, list);
                }
                Err(error) => {
                    snapshot.failures.insert(plugin_id, error);
                }
            }
            // Retain completed providers even if the initiating caller cancels.
            self.publish(runtime, revision, &snapshot);
        }
        drop(results);
        let snapshot = Arc::new(snapshot);
        *self.published.lock().expect("tool catalog lock poisoned") = Some(Generation {
            runtime: runtime.runtime_generation(),
            revision: revision.saturating_add(1),
            snapshot: Arc::clone(&snapshot),
        });
        drop(refresh);
        snapshot
    }

    fn publish(
        &self,
        runtime: &bcode_plugin::PluginRuntimeHost,
        revision: u64,
        snapshot: &Snapshot,
    ) {
        *self.published.lock().expect("tool catalog lock poisoned") = Some(Generation {
            runtime: runtime.runtime_generation(),
            revision,
            snapshot: Arc::new(snapshot.clone()),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn runtime() -> bcode_plugin::PluginRuntimeHost {
        bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
            &bcode_plugin::PluginSelection {
                mode: bcode_plugin::PluginSelectionMode::Explicit,
                enabled: std::collections::BTreeSet::from(["bcode.filesystem".to_owned()]),
                disabled: std::collections::BTreeSet::new(),
            },
            &[bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../plugins/filesystem-plugin/bcode-plugin.toml"),
                bcode_filesystem_plugin::static_plugin(),
            )],
        )
        .expect("load filesystem runtime")
    }

    fn list(stable: bool) -> ToolList {
        if stable {
            ToolList::with_runtime_stable_discovery(Vec::new(), |_| unreachable!())
        } else {
            ToolList::with_discovery(Vec::new(), |_| unreachable!())
        }
    }

    #[tokio::test]
    async fn reuses_stable_catalog_and_replaces_runtime_generation() {
        let cache = ToolCatalogCache::default();
        let runtime = runtime();
        let calls = AtomicUsize::new(0);
        let load = |_| async {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(list(true))
        };
        let first = cache.get_with(&runtime, true, &load).await;
        let second = cache.get_with(&runtime.clone(), true, &load).await;
        drop(runtime);
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let replacement = self::runtime();
        assert!(cache.peek(&replacement).is_none());
        cache.get_with(&replacement, false, &load).await;
        drop(replacement);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn volatile_refresh_is_coalesced_and_snapshots_are_immutable() {
        let cache = ToolCatalogCache::default();
        let runtime = runtime();
        let calls = AtomicUsize::new(0);
        let load = |_| async {
            calls.fetch_add(1, Ordering::Relaxed);
            tokio::task::yield_now().await;
            Ok(list(false))
        };
        let (first, second) = tokio::join!(
            cache.get_with(&runtime, true, &load),
            cache.get_with(&runtime, true, &load),
        );
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(Arc::ptr_eq(
            &first,
            &cache.get_with(&runtime, false, &load).await
        ));
        let third = cache.get_with(&runtime, true, &load).await;
        drop(runtime);
        assert!(!Arc::ptr_eq(&first, &third));
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn legacy_and_unknown_policy_are_volatile() {
        let legacy: ToolList = serde_json::from_value(serde_json::json!({"tools": []})).unwrap();
        assert!(!legacy.is_runtime_stable());
        let future: ToolList = serde_json::from_value(serde_json::json!({
            "tools": [], "cache_policy": {"version": 2, "runtime_stable": true}
        }))
        .unwrap();
        assert!(!future.is_runtime_stable());
        assert!(list(true).is_runtime_stable());
    }

    #[tokio::test]
    async fn partial_refresh_excludes_failed_provider_and_preserves_pinned_mapping() {
        let cache = ToolCatalogCache::default();
        let runtime = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
            &bcode_plugin::PluginSelection {
                mode: bcode_plugin::PluginSelectionMode::Explicit,
                enabled: std::collections::BTreeSet::from([
                    "bcode.filesystem".to_owned(),
                    "bcode.shell".to_owned(),
                ]),
                disabled: std::collections::BTreeSet::new(),
            },
            &[
                bcode_plugin::StaticBundledPlugin::new(
                    include_str!("../../../plugins/filesystem-plugin/bcode-plugin.toml"),
                    bcode_filesystem_plugin::static_plugin(),
                ),
                bcode_plugin::StaticBundledPlugin::new(
                    include_str!("../../../plugins/shell-plugin/bcode-plugin.toml"),
                    bcode_shell_plugin::static_plugin(),
                ),
            ],
        )
        .unwrap();
        let mut cancelled = Box::pin(cache.get_with(&runtime, true, |plugin_id| async move {
            if plugin_id == "bcode.shell" {
                std::future::pending().await
            } else {
                let mut tools = list(true);
                tools.tools.push(bcode_tool::ToolDefinition {
                    name: plugin_id,
                    description: String::new(),
                    input_schema: serde_json::json!({}),
                });
                Ok(tools)
            }
        }));
        tokio::select! {
            _ = &mut cancelled => panic!("hung provider completed"),
            () = tokio::task::yield_now() => {}
        }
        drop(cancelled);
        let interrupted = cache.peek(&runtime).unwrap();
        assert_eq!(interrupted.lists.len(), 1);
        assert_eq!(interrupted.failures.len(), 1);
        let calls = AtomicUsize::new(0);
        let first = cache
            .get_with(&runtime, true, |plugin_id| async move {
                let mut tools = list(plugin_id == "bcode.filesystem");
                tools.tools.push(bcode_tool::ToolDefinition {
                    name: plugin_id,
                    description: String::new(),
                    input_schema: serde_json::json!({}),
                });
                Ok(tools)
            })
            .await;
        let partial = cache
            .get_with(&runtime, true, |_| async {
                calls.fetch_add(1, Ordering::Relaxed);
                Err("unavailable".to_owned())
            })
            .await;
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(partial.lists.len(), 1);
        assert_eq!(partial.failures.len(), 1);
        assert_eq!(
            first.producer_for_runtime(&runtime, "bcode.shell"),
            Some("bcode.shell".to_owned())
        );
        assert_eq!(partial.producer_for_runtime(&runtime, "bcode.shell"), None);
        assert_eq!(
            partial.producer_for_runtime(&runtime, "bcode.filesystem"),
            Some("bcode.filesystem".to_owned())
        );
        assert!(first.catalog_for_runtime(&self::runtime()).is_err());
        drop(runtime);
    }

    #[tokio::test]
    async fn hung_provider_is_bounded_and_initiator_cancellation_retains_progress() {
        let cache = ToolCatalogCache::default();
        let runtime = runtime();
        let mut hung = Box::pin(cache.get_with(&runtime, true, |_| std::future::pending()));
        tokio::select! {
            _ = &mut hung => panic!("pending provider unexpectedly completed"),
            () = tokio::task::yield_now() => {}
        }
        // Dropping an initiator leaves an explicitly incomplete, retryable snapshot.
        drop(hung);
        let partial = cache.peek(&runtime).unwrap();
        assert_eq!(partial.failures.len(), 1);
        assert!(partial.lists.is_empty());
        let recovered = cache
            .get_with(&runtime, true, |_| async { Ok(list(true)) })
            .await;
        assert!(recovered.failures.is_empty());
        // A separate cache lets the real per-provider timeout run without relying on
        // caller cancellation or whole-catalog deadlines.
        let bounded = ToolCatalogCache::default();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(6),
            bounded.get_with(&runtime, true, |_| std::future::pending()),
        )
        .await
        .unwrap();
        drop(runtime);
        assert_eq!(result.failures.len(), 1);
    }

    #[tokio::test]
    async fn cold_failure_is_shared_but_not_cached_as_complete() {
        let cache = ToolCatalogCache::default();
        let runtime = runtime();
        let calls = AtomicUsize::new(0);
        let fail = |_| async {
            calls.fetch_add(1, Ordering::Relaxed);
            tokio::task::yield_now().await;
            Err("unavailable".to_owned())
        };
        let (first, second) = tokio::join!(
            cache.get_with(&runtime, false, &fail),
            cache.get_with(&runtime, false, &fail),
        );
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.failures.len(), 1);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let recovered = cache
            .get_with(&runtime, false, |_| async { Ok(list(true)) })
            .await;
        drop(runtime);
        assert!(recovered.failures.is_empty());
        assert_eq!(first.failures.len(), 1);
    }
}
