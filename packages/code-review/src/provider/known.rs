//! What providers reported recently, kept in memory and in the on-disk cache: stacks (with when
//! each was resolved or last confirmed) and pull requests. Both are suggestions: a remembered
//! stack is shown at once and confirmed afterwards, a remembered pull request only says where to
//! start looking (its head branch, say). Nothing here is what a review pins.

use crate::cache::{CacheKind, ReviewCache};
use bcode_code_review_models::{PullRequestSummary, ReviewStack};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

/// How long a remembered stack is used as it is when it must be current; an older one is
/// confirmed with its provider first.
pub const STACK_FRESH: Duration = Duration::from_secs(30);

/// How old a remembered stack may be and still be confirmed rather than resolved again.
///
/// Confirming is one batched request but does not see pull requests opened on top of a stack
/// since, so past this, resolving is worth its cost.
pub const STACK_RECONFIRM: Duration = Duration::from_hours(1);

/// How long a stack is remembered at all: shown at once when a review opens, then confirmed.
pub const STACK_KEPT: Duration = Duration::from_hours(7 * 24);

/// Most pull requests kept in memory; past it, the oldest half is forgotten.
const KNOWN_PULLS_MAX: usize = 10_000;

/// The provider and repository stacks and pull requests are remembered under.
type Scope = (String, String);

/// The on-disk key prefix of `scope`.
fn prefix((provider, repository): &Scope) -> String {
    format!("{provider}\0{repository}\0")
}

fn scope(provider: &str, repository: &str) -> Scope {
    (provider.to_owned(), repository.to_owned())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How long ago `time` was (zero for a time in the future).
fn age(time: SystemTime) -> Duration {
    SystemTime::now().duration_since(time).unwrap_or_default()
}

/// A stack and when it was resolved or last confirmed.
#[derive(Debug)]
struct KnownStack {
    confirmed: SystemTime,
    stack: ReviewStack,
}

/// Stacks by provider and repository, so a review opened after its stack was resolved (from the
/// wizard, the home screen, another open, or an earlier run) does not resolve it again. A scope's
/// stacks are read from disk the first time it is asked about.
#[derive(Debug, Default)]
pub(super) struct KnownStacks {
    memory: Mutex<BTreeMap<Scope, Vec<KnownStack>>>,
    loaded: Mutex<BTreeSet<Scope>>,
    disk: Arc<ReviewCache>,
}

impl KnownStacks {
    /// Stacks also kept in `disk`.
    pub(super) fn with_disk(disk: Arc<ReviewCache>) -> Self {
        Self {
            disk,
            ..Self::default()
        }
    }

    /// Remember `stack`, resolved or confirmed just now: it replaces every remembered stack that
    /// shares a layer with it.
    pub(super) fn remember(&self, provider: &str, repository: &str, stack: &ReviewStack) {
        let scope = scope(provider, repository);
        self.load(&scope);
        let now = SystemTime::now();
        insert(
            lock(&self.memory).entry(scope.clone()).or_default(),
            KnownStack {
                confirmed: now,
                stack: stack.clone(),
            },
        );
        if let (Some(bottom), Ok(bytes)) = (stack.layers.first(), serde_json::to_string(stack)) {
            self.disk.put_many(
                CacheKind::Stack,
                &[(format!("{}{}", prefix(&scope), bottom.pull.number), bytes)],
            );
        }
    }

    /// The remembered stack with `number` as a layer, and how long ago it was confirmed.
    pub(super) fn containing(
        &self,
        provider: &str,
        repository: &str,
        number: u64,
    ) -> Option<(ReviewStack, Duration)> {
        let scope = scope(provider, repository);
        self.load(&scope);
        let mut memory = lock(&self.memory);
        let stacks = memory.get_mut(&scope)?;
        stacks.retain(|known| age(known.confirmed) < STACK_KEPT);
        let found = stacks
            .iter()
            .rev()
            .find(|known| known.stack.layer(number).is_some())
            .map(|known| (known.stack.clone(), age(known.confirmed)));
        drop(memory);
        found
    }

    /// Read `scope`'s stacks from disk, once, oldest first so newer ones replace what they
    /// overlap.
    fn load(&self, scope: &Scope) {
        if !self.disk.enabled() || !lock(&self.loaded).insert(scope.clone()) {
            return;
        }
        let mut stored: Vec<KnownStack> = self
            .disk
            .get_prefixed(CacheKind::Stack, &prefix(scope))
            .into_values()
            .filter(|entry| age(entry.written) < STACK_KEPT)
            .filter_map(|entry| {
                Some(KnownStack {
                    confirmed: entry.written,
                    stack: serde_json::from_str(&entry.value).ok()?,
                })
            })
            .collect();
        stored.sort_by_key(|known| known.confirmed);
        let mut memory = lock(&self.memory);
        let stacks = memory.entry(scope.clone()).or_default();
        // What this process remembered before reading is newer than anything on disk.
        let current = std::mem::take(stacks);
        for known in stored.into_iter().chain(current) {
            insert(stacks, known);
        }
        drop(memory);
    }
}

/// Add `known` to `stacks`, dropping every stack it shares a layer with.
fn insert(stacks: &mut Vec<KnownStack>, known: KnownStack) {
    let numbers: BTreeSet<u64> = known
        .stack
        .layers
        .iter()
        .map(|layer| layer.pull.number)
        .collect();
    stacks.retain(|old| {
        !old.stack
            .layers
            .iter()
            .any(|layer| numbers.contains(&layer.pull.number))
    });
    stacks.push(known);
}

/// A pull request's place among [`KnownPulls`]: provider, repository, number.
type PullKey = (String, String, u64);

/// Pull requests the providers reported (in lists, lookups, and stacks), by provider,
/// repository, and number, so what one screen or run learned another does not ask again. Each
/// carries the serial of when it was noted, so the oldest are forgotten first.
#[derive(Debug, Default)]
pub(super) struct KnownPulls {
    /// Each with the serial of when it was noted.
    memory: Mutex<BTreeMap<PullKey, (u64, PullRequestSummary)>>,
    disk: Arc<ReviewCache>,
}

impl KnownPulls {
    /// Pull requests also kept in `disk`.
    pub(super) fn with_disk(disk: Arc<ReviewCache>) -> Self {
        Self {
            disk,
            ..Self::default()
        }
    }

    pub(super) fn note<'a>(
        &self,
        provider: &str,
        repository: &str,
        pulls: impl IntoIterator<Item = &'a PullRequestSummary>,
    ) {
        let scope = scope(provider, repository);
        let mut stored = Vec::new();
        {
            let mut memory = lock(&self.memory);
            let mut serial = memory
                .values()
                .map(|(serial, _)| *serial)
                .max()
                .unwrap_or(0);
            for pull in pulls {
                serial += 1;
                if self.disk.enabled()
                    && let Ok(bytes) = serde_json::to_string(pull)
                {
                    stored.push((format!("{}{}", prefix(&scope), pull.number), bytes));
                }
                memory.insert(
                    (scope.0.clone(), scope.1.clone(), pull.number),
                    (serial, pull.clone()),
                );
            }
            if memory.len() > KNOWN_PULLS_MAX {
                let mut serials: Vec<u64> = memory.values().map(|(serial, _)| *serial).collect();
                serials.sort_unstable();
                let keep_from = serials[serials.len() / 2];
                memory.retain(|_, (serial, _)| *serial >= keep_from);
            }
        }
        self.disk.put_many(CacheKind::Pull, &stored);
    }

    pub(super) fn get(
        &self,
        provider: &str,
        repository: &str,
        number: u64,
    ) -> Option<PullRequestSummary> {
        let key = (provider.to_owned(), repository.to_owned(), number);
        if let Some((_, pull)) = lock(&self.memory).get(&key) {
            return Some(pull.clone());
        }
        let stored = format!("{}{number}", prefix(&scope(provider, repository)));
        let entry = self
            .disk
            .get_many(CacheKind::Pull, std::slice::from_ref(&stored))
            .remove(&stored)?;
        serde_json::from_str(&entry.value).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_code_review_models::ReviewStackLayer;

    fn pull(number: u64) -> PullRequestSummary {
        PullRequestSummary {
            number,
            title: format!("#{number}"),
            author: None,
            base_branch: format!("b{}", number - 1),
            head_branch: format!("b{number}"),
            head_oid: format!("{number:040}"),
            base_oid: None,
            review_requested_from_me: false,
            draft: false,
            updated: None,
            url: None,
        }
    }

    fn stack(numbers: &[u64]) -> ReviewStack {
        ReviewStack {
            trunk: "main".into(),
            rooted_at_default_branch: true,
            layers: numbers
                .iter()
                .map(|number| ReviewStackLayer {
                    pull: pull(*number),
                    parent: None,
                })
                .collect(),
            requested: numbers[0],
            siblings: Vec::new(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn stacks_and_pulls_remembered_by_one_run_are_known_to_the_next() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cache.db");
        {
            let disk = Arc::new(ReviewCache::open(&path));
            let stacks = KnownStacks::with_disk(Arc::clone(&disk));
            stacks.remember("gh", "o/r", &stack(&[1, 2, 3]));
            // A newer resolution sharing a layer replaces it.
            stacks.remember("gh", "o/r", &stack(&[3, 4]));
            stacks.remember("gh", "x/y", &stack(&[1]));
            KnownPulls::with_disk(disk).note("gh", "o/r", &[pull(7)]);
        }
        let disk = Arc::new(ReviewCache::open(&path));
        let stacks = KnownStacks::with_disk(Arc::clone(&disk));
        let (found, age) = stacks.containing("gh", "o/r", 4).expect("remembered");
        assert_eq!(found, stack(&[3, 4]));
        assert!(age < STACK_FRESH);
        assert!(stacks.containing("gh", "o/r", 1).is_none());
        assert!(stacks.containing("gh", "x/y", 1).is_some());
        assert!(stacks.containing("other", "o/r", 4).is_none());
        let pulls = KnownPulls::with_disk(disk);
        assert_eq!(
            pulls.get("gh", "o/r", 7).map(|pull| pull.head_branch),
            Some("b7".to_owned())
        );
        assert_eq!(pulls.get("gh", "o/r", 8), None);
    }

    #[test]
    fn without_a_disk_only_this_process_remembers() {
        let stacks = KnownStacks::default();
        stacks.remember("gh", "o/r", &stack(&[1, 2]));
        assert!(stacks.containing("gh", "o/r", 2).is_some());
        assert!(KnownStacks::default().containing("gh", "o/r", 2).is_none());
    }
}
