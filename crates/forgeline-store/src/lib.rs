//! The append-only event log, on SQLite.
//!
//! * Every append runs in one transaction that also validates the new events
//!   against the run's current state ([`RunState::apply`]) and updates the
//!   `runs` summary table, so the log can never contain an event the
//!   projection would reject.
//! * `seq` is gapless and strictly increasing (single writer, SQLite
//!   `AUTOINCREMENT`), so a client that has seen `seq = n` resumes with
//!   [`EventStore::events_after`]`(n)`.
//! * Appends carrying an idempotency key are recorded; retrying with the same
//!   key returns the originally stored events instead of writing twice.
//! * Committed events are broadcast to subscribers. A subscriber that falls
//!   behind (`RecvError::Lagged`) re-reads from the log by `seq`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use forgeline_core::{ApplyError, Event, EventKind, RunId, RunState, RunStatus, Usage};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Bumped whenever the schema changes; see [`migrate`].
const SCHEMA_VERSION: i64 = 1;
/// Version of the JSON encoding of [`EventKind`] written into `payload`.
const PAYLOAD_VERSION: i64 = 1;
/// How many committed events a slow subscriber may fall behind before it
/// has to catch up from the log.
const BROADCAST_CAPACITY: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("corrupt event payload at seq {seq}: {source}")]
    Payload { seq: u64, source: serde_json::Error },
    #[error("event rejected: {0}")]
    Rejected(#[from] ApplyError),
    #[error("run {0} does not exist")]
    UnknownRun(RunId),
    #[error("database was created by a newer Forgeline (schema {found}, supported {supported})")]
    SchemaTooNew { found: i64, supported: i64 },
    #[error("store worker panicked")]
    Worker,
}

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// One row of the `runs` table: enough to list runs without replaying them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
pub struct RunSummary {
    pub run_id: RunId,
    pub title: String,
    pub status: RunStatus,
    pub stage: Option<String>,
    pub tasks_total: usize,
    pub tasks_done: usize,
    pub pending_approvals: usize,
    pub usage: Usage,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub last_seq: u64,
}

impl RunSummary {
    fn of(state: &RunState) -> Self {
        Self {
            run_id: state.id,
            title: state.spec.title.clone(),
            status: state.status,
            stage: state.stage.clone(),
            tasks_total: state.tasks.len(),
            tasks_done: state
                .tasks
                .values()
                .filter(|t| t.status == forgeline_core::TaskStatus::Done)
                .count(),
            pending_approvals: state.pending_approvals().count(),
            usage: state.total_usage(),
            created_at_ms: state.created_at_ms,
            updated_at_ms: state.updated_at_ms,
            last_seq: state.last_seq,
        }
    }
}

struct Inner {
    conn: Connection,
    /// Current state of runs touched since startup. Loaded lazily by replay.
    runs: HashMap<RunId, RunState>,
}

/// Handle to the event log. Cheap to clone.
#[derive(Clone)]
pub struct EventStore {
    inner: Arc<Mutex<Inner>>,
    events: broadcast::Sender<Arc<Event>>,
}

impl EventStore {
    /// Opens (creating if needed) a store at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::from_connection(conn)
    }

    /// An in-memory store, for tests and throwaway runs.
    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(mut conn: Connection) -> Result<Self> {
        // Other processes (the CLI, a second engine) may hold the write lock
        // briefly; wait for it instead of failing.
        conn.busy_timeout(std::time::Duration::from_secs(15))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&mut conn)?;
        let (events, _) = broadcast::channel(BROADCAST_CAPACITY);
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                conn,
                runs: HashMap::new(),
            })),
            events,
        })
    }

    /// Receives every event committed after this call.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.events.subscribe()
    }

    /// Appends events to a run atomically. The first event of a new run must
    /// be `run_created`. With an `idempotency_key`, repeating the call returns
    /// the events stored the first time and writes nothing.
    pub async fn append(
        &self,
        run_id: RunId,
        kinds: Vec<EventKind>,
        idempotency_key: Option<String>,
    ) -> Result<Vec<Event>> {
        let (stored, fresh) = self
            .with_inner(move |inner| inner.append(run_id, kinds, idempotency_key.as_deref()))
            .await?;
        if fresh {
            for event in &stored {
                // No receivers is fine; nobody is watching.
                let _ = self.events.send(Arc::new(event.clone()));
            }
        }
        Ok(stored)
    }

    /// Current state of a run, replaying its log if it is not cached.
    pub async fn run(&self, run_id: RunId) -> Result<Option<RunState>> {
        self.with_inner(move |inner| Ok(inner.state(run_id)?.cloned()))
            .await
    }

    /// A run's events with `seq > after`, oldest first.
    pub async fn run_events(&self, run_id: RunId, after: u64, limit: u32) -> Result<Vec<Event>> {
        self.with_inner(move |inner| {
            query_events(
                &inner.conn,
                "WHERE run_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
                params![run_id.to_string(), after as i64, limit],
            )
        })
        .await
    }

    /// Events across all runs with `seq > after`, oldest first.
    pub async fn events_after(&self, after: u64, limit: u32) -> Result<Vec<Event>> {
        self.with_inner(move |inner| {
            query_events(
                &inner.conn,
                "WHERE seq > ?1 ORDER BY seq LIMIT ?2",
                params![after as i64, limit],
            )
        })
        .await
    }

    /// Summaries of all runs, most recently updated first.
    pub async fn list_runs(&self) -> Result<Vec<RunSummary>> {
        self.with_inner(|inner| {
            let mut stmt = inner
                .conn
                .prepare("SELECT summary FROM runs ORDER BY updated_at_ms DESC, run_id DESC")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            let mut out = Vec::new();
            for row in rows {
                let json = row?;
                out.push(
                    serde_json::from_str(&json)
                        .map_err(|source| StoreError::Payload { seq: 0, source })?,
                );
            }
            Ok(out)
        })
        .await
    }

    /// The highest `seq` in the log (0 when empty).
    pub async fn last_seq(&self) -> Result<u64> {
        self.with_inner(|inner| {
            let seq: Option<i64> =
                inner
                    .conn
                    .query_row("SELECT MAX(seq) FROM events", [], |row| row.get(0))?;
            Ok(seq.unwrap_or(0) as u64)
        })
        .await
    }

    /// Runs a closure against the connection on the blocking thread pool.
    async fn with_inner<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Inner) -> Result<T> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut guard = inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            f(&mut guard)
        })
        .await
        .map_err(|_| StoreError::Worker)?
    }
}

impl Inner {
    /// Returns the state of a run, bringing the cached copy up to date with
    /// events other processes may have appended.
    fn state(&mut self, run_id: RunId) -> Result<Option<&RunState>> {
        let cached = self.runs.remove(&run_id);
        if let Some(state) = catch_up(&self.conn, run_id, cached)? {
            self.runs.insert(run_id, state);
        }
        Ok(self.runs.get(&run_id))
    }

    fn append(
        &mut self,
        run_id: RunId,
        kinds: Vec<EventKind>,
        idempotency_key: Option<&str>,
    ) -> Result<(Vec<Event>, bool)> {
        // Take the write lock up front so the idempotency check, the
        // catch-up read and the insert see one consistent log, even when
        // several processes share the database.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(key) = idempotency_key {
            let prior: Option<(i64, i64)> = tx
                .query_row(
                    "SELECT first_seq, last_seq FROM idempotency WHERE key = ?1",
                    [key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((first, last)) = prior {
                let events = query_events(
                    &tx,
                    "WHERE seq BETWEEN ?1 AND ?2 ORDER BY seq",
                    params![first, last],
                )?;
                return Ok((events, false));
            }
        }
        if kinds.is_empty() {
            return Ok((Vec::new(), false));
        }

        // Validate against a scratch copy of the state; only publish it to
        // the cache once the transaction commits.
        let mut state = catch_up(&tx, run_id, self.runs.get(&run_id).cloned())?;
        let at_ms = now_ms();
        let mut stored = Vec::with_capacity(kinds.len());
        for kind in kinds {
            let payload = serde_json::to_string(&kind).expect("events always serialize");
            tx.execute(
                "INSERT INTO events (run_id, at_ms, type, payload, payload_version)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    run_id.to_string(),
                    at_ms,
                    kind.name(),
                    payload,
                    PAYLOAD_VERSION
                ],
            )?;
            let event = Event {
                seq: tx.last_insert_rowid() as u64,
                run_id,
                at_ms,
                kind,
            };
            match &mut state {
                Some(state) => state.apply(&event)?,
                None => state = Some(RunState::new(&event)?),
            }
            stored.push(event);
        }
        let state = state.expect("at least one event was applied");
        upsert_summary(&tx, &state)?;
        if let Some(key) = idempotency_key {
            tx.execute(
                "INSERT INTO idempotency (key, first_seq, last_seq) VALUES (?1, ?2, ?3)",
                params![
                    key,
                    stored.first().map(|e| e.seq as i64),
                    stored.last().map(|e| e.seq as i64)
                ],
            )?;
        }
        tx.commit()?;
        self.runs.insert(run_id, state);
        Ok((stored, true))
    }
}

/// Applies any events newer than `cached` (appended by another process), or
/// replays the run from scratch when nothing is cached. `None` means the run
/// does not exist.
fn catch_up(
    conn: &Connection,
    run_id: RunId,
    cached: Option<RunState>,
) -> Result<Option<RunState>> {
    match cached {
        Some(mut state) => {
            let newer = query_events(
                conn,
                "WHERE run_id = ?1 AND seq > ?2 ORDER BY seq",
                params![run_id.to_string(), state.last_seq as i64],
            )?;
            for event in &newer {
                state.apply(event)?;
            }
            Ok(Some(state))
        }
        None => {
            let events = query_events(
                conn,
                "WHERE run_id = ?1 ORDER BY seq",
                params![run_id.to_string()],
            )?;
            if events.is_empty() {
                Ok(None)
            } else {
                Ok(Some(RunState::replay(&events)?))
            }
        }
    }
}

fn upsert_summary(tx: &Transaction<'_>, state: &RunState) -> Result<()> {
    let summary = RunSummary::of(state);
    tx.execute(
        "INSERT INTO runs (run_id, status, updated_at_ms, summary) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(run_id) DO UPDATE SET
           status = excluded.status,
           updated_at_ms = excluded.updated_at_ms,
           summary = excluded.summary",
        params![
            state.id.to_string(),
            serde_json::to_value(state.status)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned)),
            state.updated_at_ms,
            serde_json::to_string(&summary).expect("summaries always serialize"),
        ],
    )?;
    Ok(())
}

fn query_events(
    conn: &Connection,
    clause: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<Event>> {
    let sql = format!("SELECT seq, run_id, at_ms, payload FROM events {clause}");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params, |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (seq, run_id, at_ms, payload) = row?;
        let seq = seq as u64;
        let kind =
            serde_json::from_str(&payload).map_err(|source| StoreError::Payload { seq, source })?;
        let run_id = run_id.parse().map_err(|_| StoreError::Payload {
            seq,
            source: serde::de::Error::custom("invalid run id"),
        })?;
        out.push(Event {
            seq,
            run_id,
            at_ms,
            kind,
        });
    }
    Ok(out)
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(StoreError::SchemaTooNew {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    if version < 1 {
        let tx = conn.transaction()?;
        tx.execute_batch(
            "CREATE TABLE events (
                seq             INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id          TEXT    NOT NULL,
                at_ms           INTEGER NOT NULL,
                type            TEXT    NOT NULL,
                payload         TEXT    NOT NULL,
                payload_version INTEGER NOT NULL
            );
            CREATE INDEX events_by_run ON events (run_id, seq);
            CREATE TABLE runs (
                run_id        TEXT PRIMARY KEY,
                status        TEXT,
                updated_at_ms INTEGER NOT NULL,
                summary       TEXT NOT NULL
            );
            CREATE TABLE idempotency (
                key       TEXT PRIMARY KEY,
                first_seq INTEGER NOT NULL,
                last_seq  INTEGER NOT NULL
            );
            PRAGMA user_version = 1;",
        )?;
        tx.commit()?;
    }
    Ok(())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use forgeline_core::{RunSpec, TaskId, TaskSpec, TaskStatus, WorkSource};

    fn created(title: &str) -> EventKind {
        EventKind::RunCreated {
            spec: RunSpec {
                title: title.into(),
                request: "do the thing".into(),
                source: WorkSource::Manual,
                base_ref: "main".into(),
                pipeline: "default".into(),
                budget: Default::default(),
            },
        }
    }

    fn task(task_id: TaskId, key: &str) -> EventKind {
        EventKind::TaskCreated {
            task_id,
            spec: TaskSpec {
                key: key.into(),
                title: key.into(),
                description: String::new(),
                role: "implementer".into(),
                depends_on: Vec::new(),
                acceptance: Vec::new(),
            },
        }
    }

    #[tokio::test]
    async fn appends_are_sequenced_validated_and_projected() {
        let store = EventStore::open_in_memory().unwrap();
        let run = RunId::new();
        let t1 = TaskId::new();
        let stored = store
            .append(run, vec![created("first"), task(t1, "T1")], None)
            .await
            .unwrap();
        assert_eq!(stored.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2]);

        // An event the projection rejects rolls back the whole batch.
        let ghost = TaskId::new();
        let err = store
            .append(
                run,
                vec![
                    EventKind::TaskStatusChanged {
                        task_id: t1,
                        status: TaskStatus::Running,
                        reason: None,
                    },
                    EventKind::TaskStatusChanged {
                        task_id: ghost,
                        status: TaskStatus::Done,
                        reason: None,
                    },
                ],
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            StoreError::Rejected(ApplyError::UnknownTask(_))
        ));
        assert_eq!(store.last_seq().await.unwrap(), 2);
        let state = store.run(run).await.unwrap().unwrap();
        assert_eq!(state.tasks[&t1].status, TaskStatus::Pending);

        // The sequence stays gapless after the rollback.
        let next = store
            .append(
                run,
                vec![EventKind::TaskStatusChanged {
                    task_id: t1,
                    status: TaskStatus::Done,
                    reason: None,
                }],
                None,
            )
            .await
            .unwrap();
        assert_eq!(next[0].seq, 3);

        let summaries = store.list_runs().await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].title, "first");
        assert_eq!(summaries[0].tasks_total, 1);
        assert_eq!(summaries[0].tasks_done, 1);
        assert_eq!(summaries[0].last_seq, 3);
    }

    #[tokio::test]
    async fn first_event_must_create_the_run() {
        let store = EventStore::open_in_memory().unwrap();
        let err = store
            .append(RunId::new(), vec![task(TaskId::new(), "T1")], None)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            StoreError::Rejected(ApplyError::NotCreated(_))
        ));
        assert_eq!(store.last_seq().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn idempotency_keys_prevent_double_writes() {
        let store = EventStore::open_in_memory().unwrap();
        let run = RunId::new();
        let first = store
            .append(run, vec![created("x")], Some("create-x".into()))
            .await
            .unwrap();
        let mut rx = store.subscribe();
        let again = store
            .append(run, vec![created("x")], Some("create-x".into()))
            .await
            .unwrap();
        assert_eq!(first, again);
        assert_eq!(store.last_seq().await.unwrap(), 1);
        assert!(
            rx.try_recv().is_err(),
            "a replayed append is not re-broadcast"
        );
    }

    #[tokio::test]
    async fn subscribers_see_commits_and_can_resume_from_the_log() {
        let store = EventStore::open_in_memory().unwrap();
        let mut rx = store.subscribe();
        let a = RunId::new();
        let b = RunId::new();
        store.append(a, vec![created("a")], None).await.unwrap();
        store.append(b, vec![created("b")], None).await.unwrap();
        assert_eq!(rx.recv().await.unwrap().run_id, a);
        assert_eq!(rx.recv().await.unwrap().run_id, b);

        let after_first = store.events_after(1, 100).await.unwrap();
        assert_eq!(after_first.len(), 1);
        assert_eq!(after_first[0].run_id, b);
        assert_eq!(store.run_events(a, 0, 100).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn two_handles_on_one_file_stay_consistent() {
        // Two stores on the same file behave like two processes (for
        // example the engine and `forgeline approve`).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.sqlite");
        let engine = EventStore::open(&path).unwrap();
        let cli = EventStore::open(&path).unwrap();
        let run = RunId::new();
        let t1 = TaskId::new();
        engine
            .append(run, vec![created("shared"), task(t1, "T1")], None)
            .await
            .unwrap();
        // The engine has the run cached; the CLI appends behind its back.
        assert!(engine.run(run).await.unwrap().is_some());
        cli.append(
            run,
            vec![EventKind::TaskStatusChanged {
                task_id: t1,
                status: TaskStatus::Ready,
                reason: None,
            }],
            None,
        )
        .await
        .unwrap();
        // Reads catch up...
        let state = engine.run(run).await.unwrap().unwrap();
        assert_eq!(state.tasks[&t1].status, TaskStatus::Ready);
        assert_eq!(state.last_seq, 3);
        // ...and so do writes, which validate against the caught-up state.
        let next = engine
            .append(
                run,
                vec![EventKind::TaskStatusChanged {
                    task_id: t1,
                    status: TaskStatus::Running,
                    reason: None,
                }],
                None,
            )
            .await
            .unwrap();
        assert_eq!(next[0].seq, 4);
        let summary = &cli.list_runs().await.unwrap()[0];
        assert_eq!(summary.last_seq, 4);
    }

    #[tokio::test]
    async fn state_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("forgeline.sqlite");
        let run = RunId::new();
        let t1 = TaskId::new();
        {
            let store = EventStore::open(&path).unwrap();
            store
                .append(run, vec![created("persisted"), task(t1, "T1")], None)
                .await
                .unwrap();
        }
        let store = EventStore::open(&path).unwrap();
        let state = store.run(run).await.unwrap().unwrap();
        assert_eq!(state.spec.title, "persisted");
        assert!(state.tasks.contains_key(&t1));
        let next = store
            .append(
                run,
                vec![EventKind::TaskStatusChanged {
                    task_id: t1,
                    status: TaskStatus::Ready,
                    reason: None,
                }],
                None,
            )
            .await
            .unwrap();
        assert_eq!(next[0].seq, 3);
    }
}
