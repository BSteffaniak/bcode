//! The on-disk cache: what a review computed or learned that is costly to get again, kept across
//! processes so a review opens from disk and is confirmed afterwards.
//!
//! It is separate from the review store and disposable: never needed for correctness, bounded by
//! age (each kind's entries are dropped [`CacheKind::kept_for`] after they were written) and by
//! size (past [`CacheKind::max_entries`], the oldest are dropped), rebuilt from scratch when it is
//! unreadable, and safe to delete at any time. Every failure is a miss
//! (reads) or a skipped write; none reaches a caller.
//!
//! Entries are text under a kind and a key:
//!
//! * content-addressed answers that never change ([`CacheKind::MergeBase`],
//!   [`CacheKind::Index`]): keyed by commit ids, so they need no invalidation;
//! * provider facts ([`CacheKind::Pull`], [`CacheKind::Stack`]): suggestions shown at once and
//!   confirmed with the provider afterwards, with when they were written.
//!
//! One worker thread owns the database; calls send it requests. Reads wait for their answer and
//! come first: writes are held until no read has arrived for [`WRITE_DELAY`] and then written in
//! small transactions, so a burst of reads (opening a review) never waits behind writes. A read
//! also sees the writes still held, so it sees every earlier write.
//! See `docs/review-loading-architecture.md`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use switchy::database::query::{
    FilterableQuery as _, SortDirection, count_all, select, where_eq, where_gte, where_in, where_lt,
};
use switchy::database::schema::{Column, DataType, create_table};
use switchy::database::{Database, DatabaseError, DatabaseValue, Row};
use switchy::schema::discovery::code::{CodeMigration, CodeMigrationSource};
use switchy::schema::runner::MigrationRunner;

/// Environment variable naming the cache file, or `off` to disable it.
pub const CODE_REVIEW_CACHE_ENV: &str = "BCODE_CODE_REVIEW_CACHE";

const TABLE: &str = "review_cache_entries";
const MIGRATIONS_TABLE: &str = "__bcode_review_cache_migrations";
/// Keys per `IN (…)` query.
const KEYS_PER_QUERY: usize = 400;
const BUSY_TIMEOUT: Duration = Duration::from_millis(250);
/// How long without a read before held writes start.
const WRITE_DELAY: Duration = Duration::from_millis(200);
/// Entries per write transaction; reads are answered between them.
const WRITE_CHUNK: usize = 64;

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
    /// The hunks of one file between two commits.
    Hunks,
    /// A discovery contributor's answer, with the refs it read.
    Discovery,
    /// A pull request's conversation as a provider reported it.
    Activity,
}

impl CacheKind {
    const ALL: [Self; 7] = [
        Self::MergeBase,
        Self::Index,
        Self::Pull,
        Self::Stack,
        Self::Hunks,
        Self::Discovery,
        Self::Activity,
    ];

    const fn as_str(self) -> &'static str {
        match self {
            Self::MergeBase => "merge_base",
            Self::Index => "index",
            Self::Pull => "pull",
            Self::Stack => "stack",
            Self::Hunks => "hunks",
            Self::Discovery => "discovery",
            Self::Activity => "activity",
        }
    }

    /// Most entries kept; past it, the oldest are dropped down to nine tenths of it.
    #[must_use]
    pub const fn max_entries(self) -> usize {
        match self {
            Self::MergeBase => 200_000,
            Self::Index => 4_000,
            Self::Pull | Self::Hunks => 20_000,
            Self::Stack => 1_000,
            Self::Discovery | Self::Activity => 2_000,
        }
    }

    /// How long entries are kept after they were written (rewriting one renews it).
    #[must_use]
    pub const fn kept_for(self) -> Duration {
        match self {
            Self::MergeBase => Duration::from_hours(90 * 24),
            Self::Index | Self::Pull | Self::Hunks | Self::Discovery => {
                Duration::from_hours(30 * 24)
            }
            Self::Stack => crate::provider::STACK_KEPT,
            // A conversation is shown at once and refreshed; older than this is not worth it.
            Self::Activity => Duration::from_hours(7 * 24),
        }
    }
}

/// An entry read back: its text and when it was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    /// The stored text.
    pub value: String,
    /// When it was written.
    pub written: SystemTime,
}

type Entries = BTreeMap<String, CacheEntry>;

/// A request to the worker that owns the database.
enum Request {
    Get {
        kind: CacheKind,
        keys: Vec<String>,
        reply: mpsc::Sender<Entries>,
    },
    Prefixed {
        kind: CacheKind,
        prefix: String,
        reply: mpsc::Sender<Entries>,
    },
    Put {
        kind: CacheKind,
        entries: Vec<(String, String)>,
    },
}

/// The worker thread and the way to reach it.
#[derive(Debug)]
struct Worker {
    requests: mpsc::Sender<Request>,
    thread: std::thread::JoinHandle<()>,
    /// Signalled when the worker has written what it held and closed the database.
    finished: mpsc::Receiver<()>,
}

/// Longest a dropped cache waits for its last writes. The entries are disposable and the database
/// survives being left mid-write (an unreadable file is rebuilt on open), so a slow disk never holds
/// the program open past this.
const CLOSE_BUDGET: Duration = Duration::from_millis(250);

/// The on-disk cache (see the module docs). Share it behind an `Arc`; reads block briefly.
#[derive(Debug, Default)]
pub struct ReviewCache {
    /// `None`: disabled, or the file could not be opened.
    worker: Mutex<Option<Worker>>,
}

impl ReviewCache {
    /// A cache that keeps nothing.
    #[must_use]
    pub fn disabled() -> Self {
        Self::default()
    }

    /// The cache at `path` (its directory is created), rebuilt when unreadable; disabled when it
    /// cannot be opened at all (another process holding it, say).
    #[must_use]
    pub fn open(path: &Path) -> Self {
        Self {
            worker: Mutex::new(start(path)),
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
    pub fn enabled(&self) -> bool {
        self.lock().is_some()
    }

    /// The entries of `kind` stored under `keys` (missing ones are absent).
    #[must_use]
    pub fn get_many(&self, kind: CacheKind, keys: &[String]) -> Entries {
        if keys.is_empty() {
            return Entries::new();
        }
        self.ask(|reply| Request::Get {
            kind,
            keys: keys.to_vec(),
            reply,
        })
    }

    /// The entries of `kind` whose keys start with `prefix`.
    #[must_use]
    pub fn get_prefixed(&self, kind: CacheKind, prefix: &str) -> Entries {
        self.ask(|reply| Request::Prefixed {
            kind,
            prefix: prefix.to_owned(),
            reply,
        })
    }

    /// Store `entries` of `kind`, replacing what their keys held. Does not wait for the write.
    pub fn put_many(&self, kind: CacheKind, entries: &[(String, String)]) {
        if entries.is_empty() {
            return;
        }
        if let Some(worker) = self.lock().as_ref() {
            let _ = worker.requests.send(Request::Put {
                kind,
                entries: entries.to_vec(),
            });
        }
    }

    fn ask(&self, request: impl FnOnce(mpsc::Sender<Entries>) -> Request) -> Entries {
        let (reply, answer) = mpsc::channel();
        let sent = self
            .lock()
            .as_ref()
            .is_some_and(|worker| worker.requests.send(request(reply)).is_ok());
        if !sent {
            return Entries::new();
        }
        answer.recv().unwrap_or_default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Worker>> {
        self.worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl ReviewCache {
    /// Write what is held and close, waiting as long as that takes.
    pub fn close(self) {
        self.finish(None);
    }

    /// Let the worker write what it holds and close the database, waiting at most `budget`
    /// (as long as it takes when `None`); a worker still busy after that is left to finish on
    /// its own, or be stopped by the process exiting.
    fn finish(&self, budget: Option<Duration>) {
        let worker = self.lock().take();
        if let Some(Worker {
            requests,
            thread,
            finished,
        }) = worker
        {
            drop(requests);
            let done = budget.map_or_else(
                || finished.recv().is_ok(),
                |budget| finished.recv_timeout(budget).is_ok(),
            );
            if done {
                let _ = thread.join();
            }
        }
    }
}

impl Drop for ReviewCache {
    /// Close, waiting at most [`CLOSE_BUDGET`] for the last writes.
    fn drop(&mut self) {
        self.finish(Some(CLOSE_BUDGET));
    }
}

/// Start the worker for the database at `path`: open it (rebuilding a file that is not a
/// readable cache), migrate it, drop expired entries, then answer requests until every sender is
/// gone. `None` when it cannot be opened.
fn start(path: &Path) -> Option<Worker> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let path = path.to_path_buf();
    let (ready, opened) = mpsc::channel();
    let (requests, incoming) = mpsc::channel::<Request>();
    let (finish, finished) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("review-cache".to_owned())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                let _ = ready.send(false);
                return;
            };
            let Some(database) = runtime.block_on(open_or_rebuild(&path)) else {
                let _ = ready.send(false);
                return;
            };
            let _ = ready.send(true);
            runtime.block_on(serve(database.as_ref(), incoming));
            drop(database);
            let _ = finish.send(());
        })
        .ok()?;
    if opened.recv().unwrap_or(false) {
        Some(Worker {
            requests,
            thread,
            finished,
        })
    } else {
        let _ = thread.join();
        None
    }
}

async fn open_or_rebuild(path: &Path) -> Option<Box<dyn Database>> {
    match open_database(path).await {
        Ok(database) => Some(database),
        // Held by another process: this one does without.
        Err(error) if is_lock_error(&error) => None,
        // Unreadable or from an incompatible version: start over.
        Err(_) => {
            for suffix in ["", "-wal", "-shm"] {
                let mut file = path.as_os_str().to_owned();
                file.push(suffix);
                let _ = std::fs::remove_file(PathBuf::from(file));
            }
            open_database(path).await.ok()
        }
    }
}

async fn open_database(path: &Path) -> Result<Box<dyn Database>, String> {
    let database = switchy::database_connection::builder()
        .turso()
        .with_path(path)
        .with_busy_timeout(BUSY_TIMEOUT)
        .with_multiprocess_wal(false)
        .build()
        .await
        .map_err(|error| error.to_string())?;
    let mut source = CodeMigrationSource::new();
    source.add_migration(CodeMigration::new(
        "001_review_cache_entries".to_owned(),
        Box::new(
            create_table(TABLE)
                .if_not_exists(true)
                .column(column("id", DataType::Text))
                .column(column("kind", DataType::Text))
                .column(column("key", DataType::Text))
                .column(column("value", DataType::Text))
                .column(column("written_ms", DataType::BigInt))
                .primary_key("id"),
        ),
        None,
    ));
    MigrationRunner::new(Box::new(source))
        .with_table_name(MIGRATIONS_TABLE.to_owned())
        .run(database.as_ref())
        .await
        .map_err(|error| error.to_string())?;
    let now = SystemTime::now();
    for kind in CacheKind::ALL {
        let expired = millis_of(now.checked_sub(kind.kept_for()).unwrap_or(UNIX_EPOCH));
        database
            .delete(TABLE)
            .filter(Box::new(where_eq("kind", kind.as_str())))
            .filter(Box::new(where_lt("written_ms", expired)))
            .execute(database.as_ref())
            .await
            .map_err(|error| error.to_string())?;
        // Kept to size here, off the way out (closing writes only what is held).
        trim(database.as_ref(), kind, kind.max_entries())
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(database)
}

fn is_lock_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "locking error",
        "failed locking file",
        "database is locked",
        "busy",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

fn column(name: &str, data_type: DataType) -> Column {
    Column {
        name: name.to_owned(),
        nullable: false,
        auto_increment: false,
        data_type,
        default: None,
    }
}

/// A row's id: its kind and key.
fn id(kind: CacheKind, key: &str) -> String {
    format!("{}\u{1}{key}", kind.as_str())
}

/// Writes held until a quiet moment, oldest first.
type Held = std::collections::VecDeque<(CacheKind, String, String)>;

/// Answer `incoming` until every sender is gone, then write what is held. Once every held write
/// is written in a quiet moment, the kinds written to are trimmed to their size; closing only
/// writes (the next open trims).
async fn serve(database: &dyn Database, incoming: mpsc::Receiver<Request>) {
    let mut held = Held::new();
    let mut grown = std::collections::BTreeSet::new();
    loop {
        let request = if held.is_empty() {
            incoming
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        } else {
            incoming.recv_timeout(WRITE_DELAY)
        };
        match request {
            Ok(Request::Put { kind, entries }) => {
                grown.insert(kind);
                held.extend(entries.into_iter().map(|(key, value)| (kind, key, value)));
            }
            Ok(read) => answer(database, read, &held).await,
            // Quiet: write a chunk, then look for reads again.
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let chunk: Vec<_> = held.drain(..held.len().min(WRITE_CHUNK)).collect();
                let _ = put(database, chunk).await;
                if held.is_empty() {
                    for kind in std::mem::take(&mut grown) {
                        let _ = trim(database, kind, kind.max_entries()).await;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = put(database, held.into_iter().collect()).await;
                return;
            }
        }
    }
}

/// Answer a read from the database and the writes still `held` (which are newer).
async fn answer(database: &dyn Database, request: Request, held: &Held) {
    let now = SystemTime::now();
    let newer = |kind: CacheKind, wanted: &dyn Fn(&str) -> bool| -> Entries {
        held.iter()
            .filter(|(held_kind, key, _)| *held_kind == kind && wanted(key))
            .map(|(_, key, value)| {
                (
                    key.clone(),
                    CacheEntry {
                        value: value.clone(),
                        written: now,
                    },
                )
            })
            .collect()
    };
    match request {
        Request::Get { kind, keys, reply } => {
            let mut found = Entries::new();
            for chunk in keys.chunks(KEYS_PER_QUERY) {
                let ids: Vec<DatabaseValue> = chunk
                    .iter()
                    .map(|key| DatabaseValue::String(id(kind, key)))
                    .collect();
                if let Ok(rows) = database
                    .select(TABLE)
                    .columns(&["key", "value", "written_ms"])
                    .filter(Box::new(where_in("id", ids)))
                    .execute(database)
                    .await
                {
                    found.extend(rows.iter().filter_map(entry_of));
                }
            }
            let wanted: std::collections::BTreeSet<&str> =
                keys.iter().map(String::as_str).collect();
            found.extend(newer(kind, &|key| wanted.contains(key)));
            let _ = reply.send(found);
        }
        Request::Prefixed {
            kind,
            prefix,
            reply,
        } => {
            // Keys in [prefix, prefix + U+10FFFF).
            let end = format!("{prefix}\u{10ffff}");
            let rows = database
                .select(TABLE)
                .columns(&["key", "value", "written_ms"])
                .filter(Box::new(where_eq("kind", kind.as_str())))
                .filter(Box::new(where_gte("key", prefix.clone())))
                .filter(Box::new(where_lt("key", end)))
                .execute(database)
                .await
                .unwrap_or_default();
            let mut found: Entries = rows.iter().filter_map(entry_of).collect();
            found.extend(newer(kind, &|key| key.starts_with(prefix.as_str())));
            let _ = reply.send(found);
        }
        Request::Put { .. } => {}
    }
}

async fn put(
    database: &dyn Database,
    entries: Vec<(CacheKind, String, String)>,
) -> Result<(), DatabaseError> {
    if entries.is_empty() {
        return Ok(());
    }
    let now = millis_of(SystemTime::now());
    let transaction = database.begin_transaction().await?;
    for (kind, key, value) in entries {
        transaction
            .upsert(TABLE)
            .unique(&["id"])
            .value("id", id(kind, &key))
            .value("kind", kind.as_str())
            .value("key", key)
            .value("value", value)
            .value("written_ms", now)
            .execute(&*transaction)
            .await?;
    }
    transaction.commit().await
}

/// Drop the oldest entries of `kind` (by when they were written, then by key) down to nine
/// tenths of `max` when it holds more than `max`.
async fn trim(database: &dyn Database, kind: CacheKind, max: usize) -> Result<(), DatabaseError> {
    let count = database
        .select(TABLE)
        .project_as(count_all(), "entries")
        .filter(Box::new(where_eq("kind", kind.as_str())))
        .execute_first(database)
        .await?
        .and_then(|row| row.get("entries")?.as_i64())
        .and_then(|count| usize::try_from(count).ok())
        .unwrap_or(0);
    if count <= max {
        return Ok(());
    }
    let oldest = select(TABLE)
        .columns(&["id"])
        .filter(Box::new(where_eq("kind", kind.as_str())))
        .sort("written_ms", SortDirection::Asc)
        .sort("key", SortDirection::Asc)
        .limit(count - max + max / 10);
    database
        .delete(TABLE)
        .filter(Box::new(where_in("id", oldest)))
        .execute(database)
        .await?;
    Ok(())
}

fn entry_of(row: &Row) -> Option<(String, CacheEntry)> {
    let text = |column: &str| {
        row.get(column)
            .and_then(|value| value.as_str().map(str::to_owned))
    };
    Some((
        text("key")?,
        CacheEntry {
            value: text("value")?,
            written: time_of(row.get("written_ms")?.as_i64()?),
        },
    ))
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

    #[test]
    fn entries_round_trip_by_key_and_prefix_and_survive_reopening() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("cache.db");
        let cache = ReviewCache::open(&path);
        assert!(cache.enabled());
        cache.put_many(
            CacheKind::Pull,
            &[
                ("gh|o/r|1".into(), "one".into()),
                ("gh|o/r|2".into(), "two".into()),
                ("gh|x/y|1".into(), "other".into()),
            ],
        );
        cache.put_many(CacheKind::Stack, &[("gh|o/r|1".into(), "stack".into())]);
        // Later writes replace earlier ones.
        cache.put_many(CacheKind::Pull, &[("gh|o/r|2".into(), "two again".into())]);
        let found = cache.get_many(CacheKind::Pull, &["gh|o/r|1".into(), "gh|o/r|9".into()]);
        assert_eq!(found.len(), 1);
        assert_eq!(found["gh|o/r|1"].value, "one");
        assert!(found["gh|o/r|1"].written <= SystemTime::now());
        let scoped = cache.get_prefixed(CacheKind::Pull, "gh|o/r|");
        assert_eq!(
            scoped
                .values()
                .map(|entry| entry.value.as_str())
                .collect::<Vec<_>>(),
            ["one", "two again"]
        );
        drop(cache);
        // A later run (or another process) reads the same entries.
        let again = ReviewCache::open(&path);
        assert_eq!(
            again.get_many(CacheKind::Stack, &["gh|o/r|1".into()])["gh|o/r|1"].value,
            "stack"
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
        cache.put_many(CacheKind::MergeBase, &[("a|b".into(), "c".into())]);
        assert_eq!(
            cache.get_many(CacheKind::MergeBase, &["a|b".into()]).len(),
            1
        );
        let disabled = ReviewCache::disabled();
        assert!(!disabled.enabled());
        disabled.put_many(CacheKind::MergeBase, &[("a|b".into(), "c".into())]);
        assert_eq!(
            disabled
                .get_many(CacheKind::MergeBase, &["a|b".into()])
                .len(),
            0
        );
    }

    #[test]
    fn past_its_size_a_kind_drops_its_oldest_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cache.db");
        let max = CacheKind::Stack.max_entries();
        let entries: Vec<(String, String)> = (0..=max + 50)
            .map(|index| (format!("{index:06}"), "s".to_owned()))
            .collect();
        {
            let cache = ReviewCache::open(&path);
            // The first 100 are written first, so they are the oldest.
            cache.put_many(CacheKind::Stack, &entries[..100]);
            std::thread::sleep(WRITE_DELAY * 3);
            cache.put_many(CacheKind::Stack, &entries[100..]);
            // Another kind is not trimmed by this one's size.
            cache.put_many(CacheKind::Pull, &[("p".into(), "p".into())]);
            // Closing writes what is held; opening trims what grew past its size.
            cache.close();
        }
        let cache = ReviewCache::open(&path);
        let kept = cache.get_prefixed(CacheKind::Stack, "");
        assert_eq!(kept.len(), max * 9 / 10);
        assert!(!kept.contains_key("000000") && !kept.contains_key("000099"));
        assert!(kept.contains_key(&format!("{:06}", max + 50)));
        assert_eq!(cache.get_prefixed(CacheKind::Pull, "").len(), 1);
    }

    #[test]
    fn many_keys_are_read_in_bounded_queries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ReviewCache::open(&dir.path().join("cache.db"));
        let entries: Vec<(String, String)> = (0..1_000)
            .map(|index| (format!("{index:04}"), index.to_string()))
            .collect();
        cache.put_many(CacheKind::Index, &entries);
        let keys: Vec<String> = entries.iter().map(|(key, _)| key.clone()).collect();
        assert_eq!(cache.get_many(CacheKind::Index, &keys).len(), 1_000);
        // Kinds do not see each other's entries.
        assert_eq!(cache.get_many(CacheKind::Pull, &keys).len(), 0);
    }
}

#[cfg(test)]
mod close_tests {
    use super::*;

    #[test]
    fn dropping_a_busy_cache_does_not_wait_for_its_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ReviewCache::open(&dir.path().join("cache.db"));
        // Enough held writes that writing them takes well past the close budget.
        let entries: Vec<(String, String)> = (0..20_000)
            .map(|index| (format!("{index:06}"), "x".repeat(200)))
            .collect();
        cache.put_many(CacheKind::Hunks, &entries);
        let started = std::time::Instant::now();
        drop(cache);
        assert!(
            started.elapsed() < CLOSE_BUDGET + Duration::from_millis(250),
            "dropping waited {:?}",
            started.elapsed()
        );
    }
}
