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
//!   [`CacheKind::Index`], and the comparison kinds [`CacheKind::Replay`], [`CacheKind::Delta`],
//!   [`CacheKind::Marks`]): keyed by commit ids, so they need no invalidation;
//! * provider facts ([`CacheKind::Pull`], [`CacheKind::Stack`]): suggestions shown at once and
//!   confirmed with the provider afterwards, with when they were written.
//!
//! One worker thread per cache owns the connection; calls send it requests. Each review app and
//! the daemon has its own worker, and the workers take turns at the file (see `crate::access`):
//! a worker opens the database only while it has work and closes it when idle. Reads wait for
//! their answer and come first: writes are held until no read has arrived for [`WRITE_DELAY`] and
//! then written in small transactions, so a burst of reads (opening a review) never waits behind
//! writes. A read also sees the writes still held, so it sees every earlier write.
//! See `docs/review-loading-architecture.md`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use switchy::database::query::{
    FilterableQuery as _, SortDirection, count_all, select, where_eq, where_gte, where_in, where_lt,
};
use switchy::database::schema::{Column, DataType, create_table};
use switchy::database::{Database, DatabaseError, DatabaseValue, Row};
use switchy::schema::discovery::code::{CodeMigration, CodeMigrationSource};
use switchy::schema::runner::MigrationRunner;

use crate::access::AccessTurn;

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
/// Longest the worker keeps the database open while other processes may be waiting for it.
const MAX_HOLD: Duration = Duration::from_millis(500);
/// Pause after giving the turn back at [`MAX_HOLD`], so a waiting process takes it first.
const YIELD_AFTER_HOLD: Duration = Duration::from_millis(5);
/// Longest a read waits for its turn; past it the read is a miss.
const READ_TURN_WAIT: Duration = Duration::from_millis(500);
/// Longest held writes wait for their turn before waiting for the next quiet moment.
const WRITE_TURN_WAIT: Duration = Duration::from_millis(50);
/// Longest a starting worker waits for its first turn; it opens the cache later otherwise.
const START_TURN_WAIT: Duration = Duration::from_millis(250);
/// Longest a closing worker waits for its turn to write what it holds.
const CLOSE_TURN_WAIT: Duration = Duration::from_secs(2);
/// Expired entries dropped per quiet moment.
const EXPIRE_CHUNK: usize = 500;
/// Most writes held while the database is unavailable; past it, the oldest are dropped.
const MAX_HELD: usize = 20_000;

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
    /// An old version's change replayed on a new fork point: the replay commit, or that it
    /// conflicted.
    Replay,
    /// A layer's delta between a replay commit and a head.
    Delta,
    /// One file's line marks between a fork point, a replay commit, and a head.
    Marks,
}

impl CacheKind {
    const ALL: [Self; 10] = [
        Self::MergeBase,
        Self::Index,
        Self::Pull,
        Self::Stack,
        Self::Hunks,
        Self::Discovery,
        Self::Activity,
        Self::Replay,
        Self::Delta,
        Self::Marks,
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
            Self::Replay => "replay",
            Self::Delta => "delta",
            Self::Marks => "marks",
        }
    }

    /// Most entries kept; past it, the oldest are dropped down to nine tenths of it.
    #[must_use]
    pub const fn max_entries(self) -> usize {
        match self {
            Self::MergeBase => 200_000,
            Self::Index => 4_000,
            Self::Pull | Self::Hunks | Self::Marks => 20_000,
            Self::Replay | Self::Delta => 10_000,
            Self::Stack => 1_000,
            Self::Discovery | Self::Activity => 2_000,
        }
    }

    /// How long entries are kept after they were written (rewriting one renews it).
    #[must_use]
    pub const fn kept_for(self) -> Duration {
        match self {
            Self::MergeBase => Duration::from_hours(90 * 24),
            Self::Index
            | Self::Pull
            | Self::Hunks
            | Self::Discovery
            | Self::Replay
            | Self::Delta
            | Self::Marks => Duration::from_hours(30 * 24),
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
    /// can be neither opened nor rebuilt. Other processes using it at the same time share it.
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
/// readable cache) and migrate it, then answer requests until every sender is gone, dropping
/// expired entries in quiet moments. `None` when it cannot be opened at all; a cache other
/// processes are using at the moment is opened when its turn comes.
fn start(path: &Path) -> Option<Worker> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let mut connection = Connection::new(path);
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
            let usable = runtime.block_on(async {
                connection.database(START_TURN_WAIT).await.is_some() || !connection.broken
            });
            let _ = ready.send(usable);
            if !usable {
                return;
            }
            runtime.block_on(serve(&mut connection, incoming));
            drop(connection);
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

/// The cache database while the worker holds its turn at it.
struct Open {
    database: Box<dyn Database>,
    /// Released after `database` is closed.
    _turn: AccessTurn,
    since: Instant,
}

/// The worker's way to the database. Several processes share the file (each review app keeps
/// its own cache worker), so the worker opens it only while it has work, during a turn (see
/// `crate::access`), and closes it when idle or after holding it for [`MAX_HOLD`].
struct Connection {
    path: PathBuf,
    turn: PathBuf,
    open: Option<Open>,
    /// Whether expired entries may remain from before this worker started.
    expiring: bool,
    /// The file could be neither opened nor rebuilt.
    broken: bool,
}

impl Connection {
    fn new(path: &Path) -> Self {
        let mut turn = path.as_os_str().to_owned();
        turn.push(".access.lock");
        Self {
            path: path.to_path_buf(),
            turn: PathBuf::from(turn),
            open: None,
            expiring: true,
            broken: false,
        }
    }

    /// The database, taking the turn (waiting at most `wait`) and opening it when closed.
    /// `None` when the turn did not come, or the file cannot be opened.
    async fn database(&mut self, wait: Duration) -> Option<&dyn Database> {
        if self.open.is_none() {
            let turn = AccessTurn::take(&self.turn, wait).ok()??;
            match open_or_rebuild(&self.path).await {
                Opened::Ready(database) => {
                    self.open = Some(Open {
                        database,
                        _turn: turn,
                        since: Instant::now(),
                    });
                }
                // Held by a process that does not take turns: try again later.
                Opened::Locked => return None,
                Opened::Broken => {
                    self.broken = true;
                    return None;
                }
            }
        }
        self.open.as_ref().map(|open| open.database.as_ref())
    }

    /// Whether the database has been open long enough that another waiter should go first.
    fn held_too_long(&self) -> bool {
        self.open
            .as_ref()
            .is_some_and(|open| open.since.elapsed() >= MAX_HOLD)
    }

    const fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Close the database and give up the turn.
    fn close(&mut self) {
        self.open = None;
    }
}

/// How an open went.
enum Opened {
    Ready(Box<dyn Database>),
    /// Another process holds the file without taking turns.
    Locked,
    /// The file is not a cache and could not be rebuilt.
    Broken,
}

/// Open the cache at `path`, rebuilding a file that is not a readable cache.
async fn open_or_rebuild(path: &Path) -> Opened {
    match open_database(path).await {
        Ok(database) => Opened::Ready(database),
        Err(error) if is_lock_error(&error) => Opened::Locked,
        // Unreadable or from an incompatible version: start over.
        Err(_) => {
            for suffix in ["", "-wal", "-shm"] {
                let mut file = path.as_os_str().to_owned();
                file.push(suffix);
                let _ = std::fs::remove_file(PathBuf::from(file));
            }
            open_database(path)
                .await
                .map_or(Opened::Broken, Opened::Ready)
        }
    }
}

async fn open_database(path: &Path) -> Result<Box<dyn Database>, String> {
    let database = switchy::database_connection::builder()
        .turso()
        .with_path(path)
        .with_busy_timeout(BUSY_TIMEOUT)
        // Turso's multi-process WAL mode is experimental; processes take turns at the file
        // instead, so one connection is open at a time.
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
    Ok(database)
}

/// Drop up to [`EXPIRE_CHUNK`] expired entries, then keep each kind to its size. `Ok(true)`
/// once no expired entry is left.
async fn expire_step(database: &dyn Database) -> Result<bool, DatabaseError> {
    let now = SystemTime::now();
    for kind in CacheKind::ALL {
        let expired = millis_of(now.checked_sub(kind.kept_for()).unwrap_or(UNIX_EPOCH));
        let ids: Vec<DatabaseValue> = database
            .select(TABLE)
            .columns(&["id"])
            .filter(Box::new(where_eq("kind", kind.as_str())))
            .filter(Box::new(where_lt("written_ms", expired)))
            .limit(EXPIRE_CHUNK)
            .execute(database)
            .await?
            .iter()
            .filter_map(|row| row.get("id"))
            .collect();
        if !ids.is_empty() {
            database
                .delete(TABLE)
                .filter(Box::new(where_in("id", ids)))
                .execute(database)
                .await?;
            return Ok(false);
        }
    }
    for kind in CacheKind::ALL {
        trim(database, kind, kind.max_entries()).await?;
    }
    Ok(true)
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
///
/// Expired entries are dropped a chunk at a time in quiet moments after the worker starts, so a
/// large cache never holds up the reads that open a review. The database is opened for a request
/// and closed once nothing has arrived and nothing is held or left to expire for [`WRITE_DELAY`],
/// or after [`MAX_HOLD`], so other processes get their turns. A read whose
/// turn does not come within [`READ_TURN_WAIT`] is answered from the held writes alone; held
/// writes whose turn does not come wait for a later quiet moment.
async fn serve(connection: &mut Connection, incoming: mpsc::Receiver<Request>) {
    let mut held = Held::new();
    let mut grown = std::collections::BTreeSet::new();
    loop {
        let request = if held.is_empty() && !connection.is_open() && !connection.expiring {
            incoming
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        } else {
            incoming.recv_timeout(WRITE_DELAY)
        };
        if connection.held_too_long() {
            connection.close();
            // Let a waiter woken by the release take the turn before this worker asks again.
            std::thread::sleep(YIELD_AFTER_HOLD);
        }
        match request {
            Ok(Request::Put { kind, entries }) => {
                grown.insert(kind);
                held.extend(entries.into_iter().map(|(key, value)| (kind, key, value)));
                while held.len() > MAX_HELD {
                    held.pop_front();
                }
            }
            Ok(read) => {
                let database = connection.database(READ_TURN_WAIT).await;
                answer(database, read, &held).await;
            }
            // Quiet with nothing held: drop expired entries, then give the turn back.
            Err(mpsc::RecvTimeoutError::Timeout) if held.is_empty() => {
                if !connection.expiring {
                    connection.close();
                } else if let Some(database) = connection.database(WRITE_TURN_WAIT).await {
                    // A failure leaves the rest for the next start.
                    connection.expiring = !expire_step(database).await.unwrap_or(true);
                } else {
                    // Busy elsewhere: the next start drops them.
                    connection.expiring = false;
                }
            }
            // Quiet: write a chunk, then look for reads again.
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let Some(database) = connection.database(WRITE_TURN_WAIT).await else {
                    continue;
                };
                let chunk: Vec<_> = held.drain(..held.len().min(WRITE_CHUNK)).collect();
                let _ = put(database, chunk).await;
                if held.is_empty() {
                    for kind in std::mem::take(&mut grown) {
                        let _ = trim(database, kind, kind.max_entries()).await;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if !held.is_empty()
                    && let Some(database) = connection.database(CLOSE_TURN_WAIT).await
                {
                    let _ = put(database, held.into_iter().collect()).await;
                }
                connection.close();
                return;
            }
        }
    }
}

/// Answer a read from the database (when this worker has its turn) and the writes still `held`
/// (which are newer).
async fn answer(database: Option<&dyn Database>, request: Request, held: &Held) {
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
            // Without the database (its turn did not come), only held writes are found.
            for chunk in keys.chunks(KEYS_PER_QUERY) {
                let Some(database) = database else { break };
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
            let rows = match database {
                Some(database) => database
                    .select(TABLE)
                    .columns(&["key", "value", "written_ms"])
                    .filter(Box::new(where_eq("kind", kind.as_str())))
                    .filter(Box::new(where_gte("key", prefix.clone())))
                    .filter(Box::new(where_lt("key", end)))
                    .execute(database)
                    .await
                    .unwrap_or_default(),
                None => Vec::new(),
            };
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
            // Closing writes what is held; the next start trims what grew past its size in its
            // first quiet moment.
            cache.close();
        }
        let cache = ReviewCache::open(&path);
        std::thread::sleep(WRITE_DELAY * 3);
        let kept = cache.get_prefixed(CacheKind::Stack, "");
        assert_eq!(kept.len(), max * 9 / 10);
        assert!(!kept.contains_key("000000") && !kept.contains_key("000099"));
        assert!(kept.contains_key(&format!("{:06}", max + 50)));
        assert_eq!(cache.get_prefixed(CacheKind::Pull, "").len(), 1);
    }

    #[test]
    fn expired_entries_are_dropped_a_chunk_at_a_time_after_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cache.db");
        let expired = EXPIRE_CHUNK * 2 + 10;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let database = open_database(&path).await.expect("open");
                for (index, written_ms) in (0..expired)
                    .map(|index| (index, 0_i64))
                    .chain([(expired, millis_of(SystemTime::now()))])
                {
                    let key = format!("{index:06}");
                    database
                        .insert(TABLE)
                        .value("id", id(CacheKind::Pull, &key))
                        .value("kind", CacheKind::Pull.as_str())
                        .value("key", key)
                        .value("value", "v")
                        .value("written_ms", written_ms)
                        .execute(database.as_ref())
                        .await
                        .expect("insert");
                }
            });
        let cache = ReviewCache::open(&path);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while cache.get_prefixed(CacheKind::Pull, "").len() > 1 {
            assert!(
                std::time::Instant::now() < deadline,
                "expired entries stayed"
            );
            std::thread::sleep(WRITE_DELAY);
        }
        let kept = cache.get_prefixed(CacheKind::Pull, "");
        assert!(kept.contains_key(&format!("{expired:06}")));
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
