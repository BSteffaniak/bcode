//! Disposable compiled command policies, owned by the permission evaluator.

use super::{
    AgentConfig, CommandPolicy, CommandPolicyLayer, Rule, rule_specificity, word_patterns,
};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

pub struct CompiledCommandLayer {
    pub legacy: Vec<Rule>,
    pub words: word_patterns::WordRules,
    legacy_executables: BTreeMap<String, Vec<usize>>,
    legacy_fallback: Vec<usize>,
}

impl CompiledCommandLayer {
    /// Merge indexed and wildcard candidates in configuration order. Legacy
    /// globs remain text globs: only a completely literal first word is indexed.
    pub fn legacy_candidates(&self, command: &str) -> impl Iterator<Item = (usize, &Rule)> {
        let mut indexed = command
            .split_whitespace()
            .next()
            .and_then(|executable| self.legacy_executables.get(executable))
            .into_iter()
            .flatten()
            .copied()
            .peekable();
        let mut fallback = self.legacy_fallback.iter().copied().peekable();
        std::iter::from_fn(move || {
            let index = match (indexed.peek(), fallback.peek()) {
                (Some(left), Some(right)) if left < right => indexed.next(),
                (_, Some(_)) => fallback.next(),
                (Some(_), None) => indexed.next(),
                (None, None) => None,
            }?;
            Some((index, &self.legacy[index]))
        })
    }
}

pub struct CompiledCommandPolicy {
    pub layers: Vec<CompiledCommandLayer>,
    pub ordered: bool,
}

struct CacheEntry {
    base: CommandPolicyLayer,
    ordered: Option<CommandPolicy>,
    compiled: OnceLock<Result<Arc<CompiledCommandPolicy>, String>>,
}

impl CacheEntry {
    fn resolve(&self) -> Result<Arc<CompiledCommandPolicy>, String> {
        self.compiled
            .get_or_init(|| compile(&self.base, self.ordered.as_ref()).map(Arc::new))
            .clone()
    }
}

/// Eviction bounds retained snapshots, not the number of rules a policy accepts.
#[derive(Default)]
struct PolicyCache {
    entries: VecDeque<Arc<CacheEntry>>,
}

impl PolicyCache {
    fn entry(&mut self, config: &AgentConfig) -> Arc<CacheEntry> {
        let permission = &config.permission;
        if let Some(index) = self.entries.iter().position(|entry| {
            entry.base.command == permission.command
                && entry.base.command_patterns == permission.command_patterns
                && entry.ordered == permission.command_policy
        }) {
            let entry = self.entries.remove(index).expect("located cache entry");
            self.entries.push_front(Arc::clone(&entry));
            return entry;
        }
        let entry = Arc::new(CacheEntry {
            base: CommandPolicyLayer {
                command: permission.command.clone(),
                command_patterns: permission.command_patterns.clone(),
            },
            ordered: permission.command_policy.clone(),
            compiled: OnceLock::new(),
        });
        self.entries.push_front(Arc::clone(&entry));
        self.entries.truncate(16);
        entry
    }

    #[cfg(test)]
    fn get(&mut self, config: &AgentConfig) -> Result<Arc<CompiledCommandPolicy>, String> {
        self.entry(config).resolve()
    }
}

fn compile(
    base: &CommandPolicyLayer,
    ordered: Option<&CommandPolicy>,
) -> Result<CompiledCommandPolicy, String> {
    if ordered.is_some_and(|policy| policy.version != 1) {
        return Err("unsupported command policy version".to_owned());
    }
    let layers = std::iter::once(base)
        .chain(ordered.into_iter().flat_map(|policy| &policy.layers))
        .enumerate()
        .map(|(index, layer)| {
            let legacy: Vec<_> = layer
                .command
                .iter()
                .map(|(pattern, action)| Rule {
                    pattern: pattern.clone(),
                    action: *action,
                    specificity: rule_specificity(pattern),
                })
                .collect();
            let mut legacy_executables = BTreeMap::<String, Vec<usize>>::new();
            let mut legacy_fallback = Vec::new();
            for (index, rule) in legacy.iter().enumerate() {
                let executable = rule.pattern.split_whitespace().next().filter(|word| {
                    !word.contains('*') && !rule.pattern.starts_with(char::is_whitespace)
                });
                if let Some(executable) = executable {
                    legacy_executables
                        .entry(executable.to_owned())
                        .or_default()
                        .push(index);
                } else {
                    legacy_fallback.push(index);
                }
            }
            Ok(CompiledCommandLayer {
                legacy,
                legacy_executables,
                legacy_fallback,
                words: word_patterns::compile(&layer.command_patterns)
                    .map_err(|error| format!("command policy layer {index}: {error}"))?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(CompiledCommandPolicy {
        layers,
        ordered: ordered.is_some(),
    })
}

pub fn cached(config: &AgentConfig) -> Result<Arc<CompiledCommandPolicy>, String> {
    static CACHE: OnceLock<Mutex<PolicyCache>> = OnceLock::new();
    let entry = CACHE
        .get_or_init(|| Mutex::new(PolicyCache::default()))
        .lock()
        .map_err(|_| "compiled command policy cache is unavailable".to_owned())?
        .entry(config);
    // Compilation is single-flight per snapshot, outside the cache lock. A cold
    // policy does not block cache hits or compilation of unrelated policies.
    entry.resolve()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Action, glob_matches};

    #[test]
    fn reuses_exact_policy_and_invalidates_actions_layers_and_patterns() {
        let mut cache = PolicyCache::default();
        let mut config = AgentConfig::default();
        config
            .permission
            .command_patterns
            .insert("git status".to_owned(), Action::Allow);
        let first = cache.get(&config).unwrap();
        assert!(Arc::ptr_eq(&first, &cache.get(&config).unwrap()));
        config
            .permission
            .command_patterns
            .insert("git status".to_owned(), Action::Deny);
        let denied = cache.get(&config).unwrap();
        assert!(!Arc::ptr_eq(&first, &denied));
        config.permission.command_policy = Some(CommandPolicy::default());
        let ordered = cache.get(&config).unwrap();
        assert!(!Arc::ptr_eq(&denied, &ordered));
        config
            .permission
            .command_patterns
            .insert("cmd []".to_owned(), Action::Allow);
        assert!(cache.get(&config).is_err());
        assert!(cache.get(&config).is_err());
        config.permission.command_patterns.remove("cmd []");
        assert!(Arc::ptr_eq(&ordered, &cache.get(&config).unwrap()));
        config
            .permission
            .command
            .insert("*".to_owned(), Action::Deny);
        assert!(!Arc::ptr_eq(&ordered, &cache.get(&config).unwrap()));
        config
            .permission
            .command_policy
            .as_mut()
            .unwrap()
            .layers
            .push(CommandPolicyLayer::default());
        let layered = cache.get(&config).unwrap();
        config.permission.command_policy.as_mut().unwrap().version = 2;
        assert!(cache.get(&config).is_err());
        config.permission.command_policy.as_mut().unwrap().version = 1;
        assert!(Arc::ptr_eq(&layered, &cache.get(&config).unwrap()));
    }

    #[test]
    fn simultaneous_requests_share_one_compilation() {
        let cache = Arc::new(Mutex::new(PolicyCache::default()));
        let config = AgentConfig::default();
        let results = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(8);
            for _ in 0..8 {
                let cache = &cache;
                let config = &config;
                handles.push(scope.spawn(move || {
                    let entry = cache.lock().unwrap().entry(config);
                    entry.resolve().unwrap()
                }));
            }
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        for result in &results {
            assert!(Arc::ptr_eq(&results[0], result));
        }
    }

    #[test]
    fn eviction_does_not_invalidate_active_snapshots_or_reject_new_policies() {
        let mut cache = PolicyCache::default();
        let mut config = AgentConfig::default();
        let first = cache.get(&config).unwrap();
        for index in 0..40 {
            config
                .permission
                .command
                .insert(format!("cmd{index}"), Action::Deny);
            cache.get(&config).unwrap();
        }
        assert_eq!(cache.entries.len(), 16);
        assert_eq!(first.layers[0].legacy.len(), 0);
        assert!(!Arc::ptr_eq(
            &first,
            &cache.get(&AgentConfig::default()).unwrap()
        ));
    }

    #[test]
    fn legacy_index_preserves_full_scan_matches_and_order() {
        let patterns = [
            "*",
            "git*",
            "git *",
            "git status",
            "*status",
            " git *",
            "git\tstatus",
            "状态 ...",
            "",
            "echo a*",
        ];
        let base = CommandPolicyLayer {
            command: patterns
                .into_iter()
                .map(|pattern| (pattern.to_owned(), Action::Deny))
                .collect(),
            command_patterns: BTreeMap::new(),
        };
        let compiled = compile(&base, None).unwrap();
        let layer = &compiled.layers[0];
        for subject in [
            "git status",
            "git\tstatus",
            " git status",
            "git",
            "gitish",
            "echo abc",
            "状态 ...",
            "",
            "other",
        ] {
            let expected: Vec<_> = layer
                .legacy
                .iter()
                .enumerate()
                .filter(|(_, rule)| glob_matches(&rule.pattern, subject))
                .map(|(index, _)| index)
                .collect();
            let actual: Vec<_> = layer
                .legacy_candidates(subject)
                .filter(|(_, rule)| glob_matches(&rule.pattern, subject))
                .map(|(index, _)| index)
                .collect();
            assert_eq!(actual, expected, "{subject}");
        }
    }
}
