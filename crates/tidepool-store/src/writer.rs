//! The writer: the one thread that owns the [`Store`].
//!
//! Everything that changes data goes through a bounded queue to this thread,
//! in order, which keeps the store free of locks and its behaviour
//! deterministic (what the write-ahead log will need in M5). The thread also
//! publishes epochs on its own clock: every `epoch_interval`, if anything
//! changed, it swaps in a new [`Snapshot`] and announces the epoch.
//!
//! Model changes go through the same queue ([`WriterHandle::change_model`]).
//! One that rewrites data is built on a thread of its own from the latest
//! snapshot while this thread carries on with the old model; when the build
//! is done, this thread catches up and swaps the new model in at an epoch.
//!
//! A full queue is backpressure: [`WriterHandle::apply`] fails fast with
//! [`Busy`] rather than queueing without bound, and the HTTP layer turns that
//! into `503 Retry-After`.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use tidepool_model::Model;
use tidepool_model::ir::TableId;
use tokio::sync::{oneshot, watch};

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use tidepool_config::server::Durability;

use crate::batch::Batch;
use crate::migrate::{Migration, ModelVersion, Rebuilt, build};
use crate::persist::{Checkpoints, Durable};
use crate::store::{Applied, Op, Snapshot, Store};

/// Batches the queue holds before writers are told to back off.
pub const QUEUE: usize = 64;

enum Command {
    Apply {
        table: TableId,
        op: Op,
        batch: Batch,
        reply: oneshot::Sender<Receipt>,
    },
    Publish {
        reply: oneshot::Sender<u64>,
    },
    ChangeModel {
        target: ModelVersion,
        reply: oneshot::Sender<Result<Change, String>>,
    },
}

/// How a model change went in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// Nothing to rewrite: the model is live from this epoch.
    Done { epoch: u64 },
    /// Rewriting `rows` rows in the background; [`WriterHandle::rebuild`]
    /// follows it, and the snapshot's version says when it's live.
    Rebuilding { rows: u64 },
}

/// A model change being built in the background.
#[derive(Debug)]
pub struct Rebuild {
    /// The config version it moves to.
    pub version: u64,
    pub rows: u64,
    pub done: Arc<AtomicU64>,
    pub started: Instant,
}

/// What applying a batch did, and the epoch from which it is visible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub applied: Applied,
    pub epoch: u64,
}

/// The queue is full; try again shortly.
#[derive(Debug, PartialEq, Eq)]
pub struct Busy;

/// The writer thread has stopped (the server is shutting down).
#[derive(Debug, PartialEq, Eq)]
pub struct Stopped;

/// How long something takes, kept as a count, a total and the last one:
/// enough for an average, the Prometheus `_sum`/`_count` pair, and a
/// status page. Lock-free; any thread records.
#[derive(Debug, Default)]
pub struct Timing {
    count: AtomicU64,
    total_micros: AtomicU64,
    last_micros: AtomicU64,
}

impl Timing {
    pub fn record(&self, took: Duration) {
        let micros = u64::try_from(took.as_micros()).unwrap_or(u64::MAX);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.total_micros.fetch_add(micros, Ordering::Relaxed);
        self.last_micros.store(micros, Ordering::Relaxed);
    }

    #[must_use]
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Seconds in total.
    #[must_use]
    pub fn total_seconds(&self) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        let total = self.total_micros.load(Ordering::Relaxed) as f64;
        total / 1e6
    }

    /// The last one, in milliseconds.
    #[must_use]
    pub fn last_ms(&self) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        let last = self.last_micros.load(Ordering::Relaxed) as f64;
        last / 1e3
    }
}

/// The way in: cheap to clone, shared by every request handler.
#[derive(Clone)]
pub struct WriterHandle {
    queue: SyncSender<Command>,
    snapshot: Arc<ArcSwap<Snapshot>>,
    epochs: watch::Receiver<u64>,
    log: Option<Arc<LogStatus>>,
    rebuild: Arc<std::sync::Mutex<Option<Rebuild>>>,
    /// How long publishing an epoch takes (freezing, sealing, snapshots).
    publishing: Arc<Timing>,
}

/// How the log and checkpoints stand, for status pages. Updated by the
/// writer and checkpoint threads.
#[derive(Debug)]
pub struct LogStatus {
    pub data_dir: PathBuf,
    pub durability: Durability,
    /// The last write logged and committed.
    pub last_lsn: AtomicU64,
    /// The last write the newest checkpoint holds.
    pub checkpoint_lsn: AtomicU64,
    /// When that checkpoint was written (`None`: not since the start).
    pub checkpoint_at: std::sync::Mutex<Option<Instant>>,
}

/// Configuration of the writer thread.
#[derive(Clone, Debug)]
pub struct WriterConfig {
    pub epoch_interval: Duration,
    pub seal_rows: usize,
    /// Where data is kept (plan §4.7); `None` keeps it in memory only.
    pub data: Option<PathBuf>,
    pub durability: Durability,
    /// How often a checkpoint is written (with a data directory).
    pub checkpoint_interval: Duration,
}

impl Default for WriterConfig {
    fn default() -> Self {
        Self {
            epoch_interval: Duration::from_millis(200),
            seal_rows: crate::store::DEFAULT_SEAL_ROWS,
            data: None,
            durability: Durability::Fsync,
            checkpoint_interval: Duration::from_mins(1),
        }
    }
}

/// The store, in memory or with its log.
enum Backend {
    Memory(Store),
    Disk(Durable),
}

impl Backend {
    fn store(&mut self) -> &mut Store {
        match self {
            Self::Memory(store) => store,
            Self::Disk(durable) => durable.store_mut(),
        }
    }

    fn apply(&mut self, table: TableId, op: Op, batch: &Batch) -> Applied {
        match self {
            Self::Memory(store) => store.apply(table, op, batch),
            Self::Disk(durable) => durable.apply(table, op, batch),
        }
    }

    fn migrate(&mut self, migration: Migration, rebuilt: Rebuilt) {
        match self {
            Self::Memory(store) => store.migrate(migration, rebuilt),
            Self::Disk(durable) => durable.migrate(migration, rebuilt),
        }
    }

    fn commit(&mut self) -> std::io::Result<()> {
        match self {
            Self::Memory(_) => Ok(()),
            Self::Disk(durable) => durable.commit(),
        }
    }

    /// Publish, with the LSN the snapshot covers (0 in memory).
    fn publish(&mut self) -> Option<(Snapshot, u64)> {
        match self {
            Self::Memory(store) => store.publish().map(|s| (s, 0)),
            Self::Disk(durable) => durable.publish(),
        }
    }
}

/// Start the writer thread for `model`. The thread runs until every handle
/// is dropped. With a data directory, the data there is recovered first.
///
/// # Errors
/// The operating system refuses to start a thread, or the data directory
/// can't be recovered.
pub fn spawn(
    model: ModelVersion,
    config: WriterConfig,
) -> std::io::Result<(WriterHandle, JoinHandle<()>)> {
    let log = config.data.as_ref().map(|dir| {
        Arc::new(LogStatus {
            data_dir: dir.clone(),
            durability: config.durability,
            last_lsn: AtomicU64::new(0),
            checkpoint_lsn: AtomicU64::new(0),
            checkpoint_at: std::sync::Mutex::new(None),
        })
    });
    let (mut backend, first, checkpoints) = match &config.data {
        None => {
            let mut store = Store::with_version(model, config.seal_rows);
            let first = store
                .publish()
                .unwrap_or_else(|| Snapshot::empty(Arc::clone(store.model())));
            (Backend::Memory(store), first, None)
        }
        Some(dir) => {
            let (durable, first, checkpoints) =
                Durable::open(model, dir, config.seal_rows, config.durability)?;
            (Backend::Disk(durable), first, Some(checkpoints))
        }
    };
    if let (Some(log), Backend::Disk(durable)) = (&log, &backend) {
        log.last_lsn.store(durable.last_lsn(), Ordering::Relaxed);
        log.checkpoint_lsn
            .store(durable.recovered_lsn(), Ordering::Relaxed);
    }
    let _ = backend.store().epoch();
    let (epochs_tx, epochs) = watch::channel(first.epoch);
    let snapshot = Arc::new(ArcSwap::from_pointee(first));
    let (queue, commands) = sync_channel(QUEUE);
    let published = Arc::clone(&snapshot);
    let checkpointer = match (checkpoints, &log) {
        (Some(checkpoints), Some(log)) => Some(Checkpointer::start(checkpoints, Arc::clone(log))?),
        _ => None,
    };
    let status = log.clone();
    let rebuild = Arc::new(std::sync::Mutex::new(None));
    let jobs = Arc::clone(&rebuild);
    let publishing = Arc::new(Timing::default());
    let timing = Arc::clone(&publishing);
    let thread = std::thread::Builder::new()
        .name("tidepool-writer".into())
        .spawn(move || {
            run(
                backend,
                &commands,
                &published,
                &epochs_tx,
                &config,
                checkpointer.as_ref(),
                status.as_deref(),
                &jobs,
                &timing,
            );
        })?;
    Ok((
        WriterHandle {
            queue,
            snapshot,
            epochs,
            log,
            rebuild,
            publishing,
        },
        thread,
    ))
}

/// The thread that writes checkpoints, so the writer never waits for one.
struct Checkpointer {
    jobs: SyncSender<(Arc<Snapshot>, u64)>,
    done: Receiver<u64>,
}

impl Checkpointer {
    fn start(mut checkpoints: Checkpoints, log: Arc<LogStatus>) -> std::io::Result<Self> {
        let (jobs, todo) = sync_channel::<(Arc<Snapshot>, u64)>(1);
        let (finished, done) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("tidepool-checkpoint".into())
            .spawn(move || {
                for (snapshot, lsn) in todo {
                    match checkpoints.write(&snapshot, lsn) {
                        Ok(()) => {
                            log.checkpoint_lsn.store(lsn, Ordering::Relaxed);
                            if let Ok(mut at) = log.checkpoint_at.lock() {
                                *at = Some(Instant::now());
                            }
                            let _ = finished.send(lsn);
                        }
                        Err(error) => tracing::error!("checkpoint at {lsn} failed: {error}"),
                    }
                }
            })?;
        Ok(Self { jobs, done })
    }
}

/// Retention runs this often.
const RETENTION_EVERY: Duration = Duration::from_secs(10);

/// The epoch interval after `migration`: the new model's, if the change
/// sets one other than the model had; otherwise as it was.
fn next_interval(store: &Store, migration: &Migration, now: Duration) -> Duration {
    let before = store.model().server.epoch_interval;
    let after = migration.target().model.server.epoch_interval;
    if before == after { now } else { after.0 }
}

/// A build running on its own thread, and what it will hand back.
struct Building {
    migration: Arc<Migration>,
    result: Receiver<Rebuilt>,
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn run(
    mut backend: Backend,
    commands: &Receiver<Command>,
    snapshot: &ArcSwap<Snapshot>,
    epochs: &watch::Sender<u64>,
    config: &WriterConfig,
    checkpointer: Option<&Checkpointer>,
    status: Option<&LogStatus>,
    jobs: &std::sync::Mutex<Option<Rebuild>>,
    timing: &Timing,
) {
    let mut building: Option<Building> = None;
    // the config's, until a model change sets another (plan §8.2: safe)
    let mut interval = config.epoch_interval;
    let mut next_tick = Instant::now() + interval;
    let mut next_checkpoint = Instant::now() + config.checkpoint_interval;
    let mut next_retention = Instant::now() + RETENTION_EVERY;
    let mut publish = |backend: &mut Backend| -> std::io::Result<u64> {
        // what's published is durable first (with a log)
        backend.commit()?;
        let started = Instant::now();
        let published = backend.publish();
        if published.is_some() {
            timing.record(started.elapsed());
        }
        if let Some((published, lsn)) = published {
            let epoch = published.epoch;
            let published = Arc::new(published);
            snapshot.store(Arc::clone(&published));
            let _ = epochs.send(epoch);
            if let Some(checkpointer) = checkpointer
                && Instant::now() >= next_checkpoint
                && checkpointer.jobs.try_send((published, lsn)).is_ok()
            {
                next_checkpoint = Instant::now() + config.checkpoint_interval;
            }
        }
        Ok(backend.store().epoch())
    };
    loop {
        // a finished build: catch up and swap
        if let Some(job) = &building
            && let Ok(rebuilt) = job.result.try_recv()
        {
            let job = building.take();
            if let Some(migration) = job.and_then(|j| Arc::try_unwrap(j.migration).ok()) {
                let next = next_interval(backend.store(), &migration, interval);
                if next != interval {
                    // the new clock starts now: a tick due on the old one doesn't fire
                    interval = next;
                    next_tick = Instant::now() + interval;
                }
                backend.migrate(migration, rebuilt);
                if let Err(error) = publish(&mut backend) {
                    tracing::error!("the write-ahead log failed, stopping writes: {error}");
                    return;
                }
            }
            if let Ok(mut job) = jobs.lock() {
                *job = None;
            }
        }
        if let (Some(checkpointer), Backend::Disk(durable)) = (checkpointer, &mut backend) {
            while let Ok(lsn) = checkpointer.done.try_recv() {
                if let Err(error) = durable.forget(lsn) {
                    tracing::warn!("removing old log files: {error}");
                }
            }
        }
        let wait = next_tick.saturating_duration_since(Instant::now());
        match commands.recv_timeout(wait) {
            Ok(first) => {
                // group commit: a burst of batches, one sync, then the acks
                let mut burst = vec![first];
                while burst.len() < QUEUE {
                    match commands.try_recv() {
                        Ok(command) => burst.push(command),
                        Err(_) => break,
                    }
                }
                let mut acks = Vec::new();
                for command in burst {
                    match command {
                        Command::Apply {
                            table,
                            op,
                            batch,
                            reply,
                        } => {
                            let applied = backend.apply(table, op, &batch);
                            acks.push((reply, applied));
                        }
                        Command::ChangeModel { target, reply } => {
                            if building.is_some() {
                                let _ = reply
                                    .send(Err("a model change is still being rebuilt".to_string()));
                                continue;
                            }
                            let version = target.version;
                            let migration = backend.store().plan_migration(target);
                            let rows = migration.rows();
                            if rows == 0 {
                                let next = next_interval(backend.store(), &migration, interval);
                                if next != interval {
                                    // the new clock starts now: a tick due on the old one doesn't fire
                                    interval = next;
                                    next_tick = Instant::now() + interval;
                                }
                                backend.migrate(migration, Rebuilt::default());
                                match publish(&mut backend) {
                                    Ok(epoch) => {
                                        let _ = reply.send(Ok(Change::Done { epoch }));
                                    }
                                    Err(error) => {
                                        tracing::error!(
                                            "the write-ahead log failed, stopping writes: {error}"
                                        );
                                        return;
                                    }
                                }
                                continue;
                            }
                            let migration = Arc::new(migration);
                            let done = Arc::new(AtomicU64::new(0));
                            let (finished, result) = sync_channel(1);
                            let from = snapshot.load_full();
                            let (job, counter) = (Arc::clone(&migration), Arc::clone(&done));
                            let started = std::thread::Builder::new()
                                .name("tidepool-rebuild".into())
                                .spawn(move || {
                                    let _ = finished.send(build(&job, &from, &counter));
                                });
                            if let Err(error) = started {
                                let _ = reply.send(Err(format!("can't start a rebuild: {error}")));
                                continue;
                            }
                            if let Ok(mut job) = jobs.lock() {
                                *job = Some(Rebuild {
                                    version,
                                    rows,
                                    done,
                                    started: Instant::now(),
                                });
                            }
                            building = Some(Building { migration, result });
                            let _ = reply.send(Ok(Change::Rebuilding { rows }));
                        }
                        Command::Publish { reply } => match publish(&mut backend) {
                            Ok(epoch) => {
                                let _ = reply.send(epoch);
                            }
                            Err(error) => {
                                tracing::error!(
                                    "the write-ahead log failed, stopping writes: {error}"
                                );
                                return;
                            }
                        },
                    }
                }
                if let Err(error) = backend.commit() {
                    // nothing since the last commit is safe: no acks, and
                    // no more writes (callers see the writer stopped)
                    tracing::error!("the write-ahead log failed, stopping writes: {error}");
                    return;
                }
                if let (Some(status), Backend::Disk(durable)) = (status, &backend) {
                    status.last_lsn.store(durable.last_lsn(), Ordering::Relaxed);
                }
                let epoch = backend.store().epoch() + 1;
                for (reply, applied) in acks {
                    // visible from the next epoch on
                    let _ = reply.send(Receipt { applied, epoch });
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if Instant::now() >= next_retention {
                    backend.store().expire(std::time::SystemTime::now());
                    next_retention = Instant::now() + RETENTION_EVERY;
                }
                if let Err(error) = publish(&mut backend) {
                    tracing::error!("the write-ahead log failed, stopping writes: {error}");
                    return;
                }
                if config.durability == Durability::Async
                    && let Backend::Disk(durable) = &mut backend
                    && let Err(error) = durable.flush()
                {
                    tracing::error!("the write-ahead log failed, stopping writes: {error}");
                    return;
                }
                next_tick += interval;
                if next_tick < Instant::now() {
                    // fell behind (a long seal): don't publish in a burst to catch up
                    next_tick = Instant::now() + interval;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

impl WriterHandle {
    /// Queue a batch. Resolves once it is applied (not yet visible: see
    /// [`Receipt::epoch`] and [`WriterHandle::visible`]).
    ///
    /// # Errors
    /// [`Busy`] when the queue is full.
    pub fn apply(
        &self,
        table: TableId,
        op: Op,
        batch: Batch,
    ) -> Result<oneshot::Receiver<Receipt>, Busy> {
        let (reply, receipt) = oneshot::channel();
        match self.queue.try_send(Command::Apply {
            table,
            op,
            batch,
            reply,
        }) {
            Ok(()) => Ok(receipt),
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => Err(Busy),
        }
    }

    /// Move to a new model (plan §8.1, apply): at once when nothing needs
    /// rewriting, otherwise after a background rebuild.
    ///
    /// # Errors
    /// The writer has stopped, or a rebuild is already running.
    pub async fn change_model(&self, target: ModelVersion) -> Result<Change, String> {
        let (reply, change) = oneshot::channel();
        self.queue
            .send(Command::ChangeModel { target, reply })
            .map_err(|_| "the writer has stopped".to_string())?;
        change
            .await
            .map_err(|_| "the writer has stopped".to_string())?
    }

    /// How long publishing an epoch takes.
    #[must_use]
    pub fn publishing(&self) -> &Timing {
        &self.publishing
    }

    /// The rebuild in progress: (version, rows done, rows), if any.
    #[must_use]
    pub fn rebuild(&self) -> Option<(u64, u64, u64)> {
        let job = self.rebuild.lock().ok()?;
        job.as_ref()
            .map(|j| (j.version, j.done.load(Ordering::Relaxed), j.rows))
    }

    /// Publish now instead of waiting for the next tick; resolves to the
    /// current epoch. For tests and for `?wait=visible` latency.
    ///
    /// # Errors
    /// The writer has stopped.
    pub async fn publish(&self) -> Result<u64, Stopped> {
        let (reply, epoch) = oneshot::channel();
        self.queue
            .send(Command::Publish { reply })
            .map_err(|_| Stopped)?;
        epoch.await.map_err(|_| Stopped)
    }

    /// Resolves once `epoch` has been published.
    ///
    /// # Errors
    /// The writer has stopped.
    pub async fn visible(&self, epoch: u64) -> Result<(), Stopped> {
        let mut epochs = self.epochs.clone();
        epochs
            .wait_for(|published| *published >= epoch)
            .await
            .map_err(|_| Stopped)?;
        Ok(())
    }

    /// A handle whose queue nobody drains, holding `capacity` batches: for
    /// testing what callers do when the writer is busy.
    #[doc(hidden)]
    #[must_use]
    pub fn unconsumed(model: Arc<Model>, capacity: usize) -> (Self, impl Drop) {
        let (queue, commands) = sync_channel(capacity);
        let (_, epochs) = watch::channel(0);
        let handle = Self {
            queue,
            snapshot: Arc::new(ArcSwap::from_pointee(Snapshot::empty(model))),
            epochs,
            log: None,
            rebuild: Arc::new(std::sync::Mutex::new(None)),
            publishing: Arc::new(Timing::default()),
        };
        // the receiver, kept alive so the queue fills instead of disconnecting
        (handle, DropGuard(commands))
    }

    /// The log's state, when data is kept on disk.
    #[must_use]
    pub fn log(&self) -> Option<&LogStatus> {
        self.log.as_deref()
    }

    /// Notified with every newly published epoch.
    #[must_use]
    pub fn epochs(&self) -> watch::Receiver<u64> {
        self.epochs.clone()
    }

    /// The latest published snapshot.
    #[must_use]
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.load_full()
    }
}

/// Keeps an undrained queue's receiver alive.
#[doc(hidden)]
pub struct DropGuard(#[allow(dead_code)] Receiver<Command>);

impl Drop for DropGuard {
    fn drop(&mut self) {}
}
