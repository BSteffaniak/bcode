//! The on-disk cache: what a review computed or learned that is costly to get again, kept across
//! processes so a review opens from disk and is confirmed afterwards.
//!
//! It is separate from the review store and disposable: never needed for correctness, bounded in
//! size, rebuilt from scratch when it is unreadable or from another schema version, and safe to
//! delete at any time. Every failure is a miss (reads) or a skipped write; none reaches a caller.
//!
//! Entries are bytes under a kind and a key:
//!
//! * content-addressed answers that never change ([`CacheKind::MergeBase`],
//!   [`CacheKind::Index`]): keyed by commit ids, so they need no invalidation;
//! * provider facts ([`CacheKind::Pull`], [`CacheKind::Stack`]): suggestions shown at once and
//!   confirmed with the provider afterwards, with when they were written.
//!
//! See `docs/review-loading-architecture.md`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Environment variable naming the cache file, or `off` to disable it.
pub const CODE_REVIEW_CACHE_ENV: &str = "BCODE_CODE_REVIEW_CACHE";

/// Schema version; any other version found on disk is dropped and rebuilt.
const SCHEMA_VERSION: i64 = 1;

/// How long a write waits for another process holding the database.
const BUSY_TIMEOUT: Duration = Duration::from_millis(250);

/// What an entry holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CacheKind {
    /// The merge base of two commits.
    MergeBase,
    /// The file listing of a diff between two commits.
    Index,
    /// A pull request as a provider reported it.
    Pull,
    /// A stack as a provider resolved or confirmed it.
    Stack,
}

impl CacheKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::MergeBase => "merge_base",
            Self::Index => "index",
            Self::Pull => "pull",
            Self::Stack => "stack",
        }
    }

    /// Most entries kept; past it, the oldest tenth is dropped.
    const fn max_entries(self) -> i64 {
        match self {
            Self::MergeBase => 200_000,
            Self::Index => 4_000,
            Self::Pull => 20_000,
            Self::Stack => 1_000,
        }
    }
}

/// An entry read back: its bytes and when it was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    /// The stored bytes.
    pub value: Vec<u8>,
    /// When it was written.
    pub written: SystemTime,
}

/// The on-disk cache (see the module docs). Cheap to share behind an `Arc`; calls block briefly.
#[derive(Debug, Default)]
pub struct ReviewCache {
    /// `None`: disabled, or the file could not be opened.
    connection: Option<Mutex<rusqlite::Connection>>,
}

impl ReviewCache {
    /// A cache that keeps nothing.
    #[must_use]
    pub fn disabled() -> Self {
        Self::default()
    }

    /// The cache at `path` (its directory is created), rebuilt when unreadable; disabled when it
    /// cannot be opened at all.
    #[must_use]
    pub fn open(path: &Path) -> Self {
        let connection = open_connection(path).or_else(|| {
            // Unreadable (corrupt, or not a database): start over.
            for suffix in ["", "-wal", "-shm"] {
                let mut file = path.as_os_str().to_owned();
                file.push(suffix);
                let _ = std::fs::remove_file(PathBuf::from(file));
            }
            open_connection(path)
        });
        Self {
            connection: connection.map(Mutex::new),
        }
    }

    /// The cache configured for this user: [`CODE_REVIEW_CACHE_ENV`] when set (`off` disables
    /// it), else `review-cache.db` in Bcode's state directory.
    #[must_use]
    pub fn for_user() -> Self {
        match std::env::var_os(CODE_REVIEW_CACHE_ENV) {
            Some(value) if value == "off" => Self::disabled(),
            Some(value) => Self::open(Path::new(&value)),
            None => Self::open(
                &bcode_config::default_state_dir()
                    .join("code-review")
                    .join("review-cache.db"),
            ),
        }
    }

    /// Whether entries are kept.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.connection.is_some()
    }

    /// The entries of `kind` stored under `keys` (missing ones are absent).
    #[must_use]
    pub fn get_many(&self, kind: CacheKind, keys: &[String]) -> BTreeMap<String, CacheEntry> {
        if keys.is_empty() {
            return BTreeMap::new();
        }
        self.with(|connection| {
            let mut found = BTreeMap::new();
            let mut statement = connection.prepare_cached(
                "SELECT value, written_ms FROM entries WHERE kind = ?1 AND key = ?2",
            )?;
            for key in keys {
                let row = statement.query_row(rusqlite::params![kind.as_str(), key], |row| {
                    Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?))
                });
                match row {
                    Ok((value, written_ms)) => {
                        found.insert(
                            key.clone(),
                            CacheEntry {
                                value,
                                written: time_of(written_ms),
                            },
                        );
                    }
                    Err(rusqlite::Error::QueryReturnedNoRows) => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(found)
        })
        .unwrap_or_default()
    }

    /// The entries of `kind` whose keys start with `prefix`.
    #[must_use]
    pub fn get_prefixed(&self, kind: CacheKind, prefix: &str) -> BTreeMap<String, CacheEntry> {
        self.with(|connection| {
            // Keys in [prefix, prefix + U+10FFFF): a range scan of the primary key.
            let end = format!("{prefix}\u{10ffff}");
            let mut statement = connection.prepare_cached(
                "SELECT key, value, written_ms FROM entries \
                 WHERE kind = ?1 AND key >= ?2 AND key < ?3",
            )?;
            let rows =
                statement.query_map(rusqlite::params![kind.as_str(), prefix, end], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        CacheEntry {
                            value: row.get(1)?,
                            written: time_of(row.get(2)?),
                        },
                    ))
                })?;
            rows.collect::<Result<BTreeMap<_, _>, _>>()
        })
        .unwrap_or_default()
    }

    /// Store `entries` of `kind` (replacing what their keys held), then drop the oldest entries
    /// of `kind` past its bound.
    pub fn put_many(&self, kind: CacheKind, entries: &[(String, Vec<u8>)]) {
        if entries.is_empty() {
            return;
        }
        let now = millis_of(SystemTime::now());
        let _ = self.with(|connection| {
            let transaction = connection.transaction()?;
            {
                let mut statement = transaction.prepare_cached(
                    "INSERT OR REPLACE INTO entries (kind, key, value, written_ms) \
                     VALUES (?1, ?2, ?3, ?4)",
                )?;
                for (key, value) in entries {
                    statement.execute(rusqlite::params![kind.as_str(), key, value, now])?;
                }
            }
            let count: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM entries WHERE kind = ?1",
                [kind.as_str()],
                |row| row.get(0),
            )?;
            let max = kind.max_entries();
            if count > max {
                // Drop down to 90% of the bound, oldest first.
                let excess = count - max + max / 10;
                transaction.execute(
                    "DELETE FROM entries WHERE kind = ?1 AND key IN \
                     (SELECT key FROM entries WHERE kind = ?1 ORDER BY written_ms, key LIMIT ?2)",
                    rusqlite::params![kind.as_str(), excess],
                )?;
            }
            transaction.commit()
        });
    }

    /// Run `operation` on the connection; `None` when disabled or it fails.
    fn with<T>(
        &self,
        operation: impl FnOnce(&mut rusqlite::Connection) -> rusqlite::Result<T>,
    ) -> Option<T> {
        let mut connection = self
            .connection
            .as_ref()?
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        operation(&mut connection).ok()
    }
}

/// Open (creating) the database at `path` with the current schema; `None` when it cannot be.
fn open_connection(path: &Path) -> Option<rusqlite::Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let connection = rusqlite::Connection::open(path).ok()?;
    connection.busy_timeout(BUSY_TIMEOUT).ok()?;
    // Shared by the daemon plugin and standalone apps: readers never wait for a writer.
    connection
        .query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))
        .ok()?;
    connection
        .execute_batch("PRAGMA synchronous = NORMAL;")
        .ok()?;
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .ok()?;
    if version != SCHEMA_VERSION {
        connection
            .execute_batch(&format!(
                "DROP TABLE IF EXISTS entries;
                 CREATE TABLE entries (
                     kind TEXT NOT NULL,
                     key TEXT NOT NULL,
                     value BLOB NOT NULL,
                     written_ms INTEGER NOT NULL,
                     PRIMARY KEY (kind, key)
                 ) WITHOUT ROWID;
                 CREATE INDEX entries_age ON entries (kind, written_ms);
                 PRAGMA user_version = {SCHEMA_VERSION};"
            ))
            .ok()?;
    }
    Some(connection)
}

fn millis_of(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| i64::try_from(since.as_millis()).ok())
        .unwrap_or(0)
}

fn time_of(millis: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(u64::try_from(millis).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(text: &str) -> Vec<u8> {
        text.as_bytes().to_vec()
    }

    #[test]
    fn entries_round_trip_by_key_and_prefix_and_survive_reopening() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("cache.db");
        let cache = ReviewCache::open(&path);
        assert!(cache.enabled());
        cache.put_many(
            CacheKind::Pull,
            &[
                ("gh|o/r|1".into(), bytes("one")),
                ("gh|o/r|2".into(), bytes("two")),
                ("gh|x/y|1".into(), bytes("other")),
            ],
        );
        cache.put_many(CacheKind::Stack, &[("gh|o/r|1".into(), bytes("stack"))]);
        let found = cache.get_many(CacheKind::Pull, &["gh|o/r|1".into(), "gh|o/r|9".into()]);
        assert_eq!(found.len(), 1);
        assert_eq!(found["gh|o/r|1"].value, bytes("one"));
        assert!(found["gh|o/r|1"].written <= SystemTime::now());
        let scoped: Vec<String> = cache
            .get_prefixed(CacheKind::Pull, "gh|o/r|")
            .into_keys()
            .collect();
        assert_eq!(scoped, ["gh|o/r|1", "gh|o/r|2"]);
        drop(cache);
        // Another process (or a later run) reads the same entries.
        let again = ReviewCache::open(&path);
        assert_eq!(
            again.get_many(CacheKind::Stack, &["gh|o/r|1".into()])["gh|o/r|1"].value,
            bytes("stack")
        );
    }

    #[test]
    fn an_unreadable_file_is_rebuilt_and_a_disabled_cache_keeps_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cache.db");
        std::fs::write(
            &path,
            "this is not a database, it is a cache that went wrong",
        )
        .unwrap();
        let cache = ReviewCache::open(&path);
        assert!(cache.enabled());
        cache.put_many(CacheKind::MergeBase, &[("a|b".into(), bytes("c"))]);
        assert_eq!(
            cache.get_many(CacheKind::MergeBase, &["a|b".into()]).len(),
            1
        );
        let disabled = ReviewCache::disabled();
        disabled.put_many(CacheKind::MergeBase, &[("a|b".into(), bytes("c"))]);
        assert!(
            disabled
                .get_many(CacheKind::MergeBase, &["a|b".into()])
                .is_empty()
        );
    }

    #[test]
    fn the_oldest_entries_go_first_past_a_kinds_bound() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ReviewCache::open(&dir.path().join("cache.db"));
        let max = usize::try_from(CacheKind::Stack.max_entries()).unwrap();
        let entries: Vec<(String, Vec<u8>)> = (0..=max)
            .map(|index| (format!("{index:06}"), bytes("s")))
            .collect();
        // Written in two rounds so the first is older.
        cache.put_many(CacheKind::Stack, &entries[..10]);
        std::thread::sleep(Duration::from_millis(5));
        cache.put_many(CacheKind::Stack, &entries[10..]);
        let kept = cache.get_prefixed(CacheKind::Stack, "");
        assert!(kept.len() <= max, "{}", kept.len());
        assert!(!kept.contains_key("000000"));
        assert!(kept.contains_key(&format!("{max:06}")));
    }
}
