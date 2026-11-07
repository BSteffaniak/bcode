//! What one client is told: its own worktree in detail, the others as totals.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::engine::{Applied, Engine, EngineConfig, LiveUpdate, WorktreeSnapshot, WorktreeTotalAt};

/// One client of an [`Engine`], remembering what it was last told so each update carries
/// only changes.
pub struct Subscriber {
    /// Root of the client's own worktree, as change events name it.
    root: PathBuf,
    config: EngineConfig,
    last_primary: Option<WorktreeSnapshot>,
    last_totals: BTreeMap<PathBuf, WorktreeTotalAt>,
}

impl Subscriber {
    /// A client whose own worktree has `root`, wanting `config`.
    #[must_use]
    pub const fn new(root: PathBuf, config: EngineConfig) -> Self {
        Self {
            root,
            config,
            last_primary: None,
            last_totals: BTreeMap::new(),
        }
    }

    /// What the client wants kept.
    #[must_use]
    pub const fn config(&self) -> EngineConfig {
        self.config
    }

    /// Want `config` from now on.
    pub const fn set_config(&mut self, config: EngineConfig) {
        self.config = config;
    }

    /// What changed for this client in the round that produced `applied`. A new client
    /// gets everything already known.
    pub fn report(&mut self, engine: &Engine, applied: &Applied) -> LiveUpdate {
        let mut update = LiveUpdate {
            metadata: applied.metadata,
            files: applied.files.contains(&self.root),
            errors: applied
                .errors
                .iter()
                .filter(|(root, _)| root.as_ref().is_none_or(|root| *root == self.root))
                .map(|(_, message)| message.clone())
                .collect(),
            stats: engine.stats(),
            ..LiveUpdate::default()
        };
        if let Some(snapshot) = engine.snapshot(&self.root, self.config.counts)
            && self.last_primary.as_ref() != Some(&snapshot)
        {
            self.last_primary = Some(snapshot.clone());
            update.primary = Some(snapshot);
        }
        if self.config.other_worktrees {
            for (listed, root, measured) in engine.totals() {
                if root == self.root {
                    continue;
                }
                if self.last_totals.get(listed) != Some(&measured) {
                    self.last_totals.insert(listed.to_owned(), measured.clone());
                    update.totals.insert(listed.to_owned(), measured);
                }
            }
        }
        for listed in &applied.removed {
            if self.last_totals.remove(listed).is_some() {
                update.removed.push(listed.clone());
            }
        }
        update
    }
}
