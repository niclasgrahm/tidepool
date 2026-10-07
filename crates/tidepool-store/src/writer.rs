//! The writer: the one thread that owns the [`Store`].
//!
//! Everything that changes data goes through a bounded queue to this thread,
//! in order, which keeps the store free of locks and its behaviour
//! deterministic (what the write-ahead log will need in M5). The thread also
//! publishes epochs on its own clock: every `epoch_interval`, if anything
//! changed, it swaps in a new [`Snapshot`] and announces the epoch.
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

use crate::batch::Batch;
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

/// The way in: cheap to clone, shared by every request handler.
#[derive(Clone)]
pub struct WriterHandle {
    queue: SyncSender<Command>,
    snapshot: Arc<ArcSwap<Snapshot>>,
    epochs: watch::Receiver<u64>,
}

/// Configuration of the writer thread.
#[derive(Clone, Copy, Debug)]
pub struct WriterConfig {
    pub epoch_interval: Duration,
    pub seal_rows: usize,
}

impl Default for WriterConfig {
    fn default() -> Self {
        Self {
            epoch_interval: Duration::from_millis(200),
            seal_rows: crate::store::DEFAULT_SEAL_ROWS,
        }
    }
}

/// Start the writer thread for `model`. The thread runs until every handle
/// is dropped.
///
/// # Errors
/// The operating system refuses to start a thread.
pub fn spawn(
    model: Arc<Model>,
    config: WriterConfig,
) -> std::io::Result<(WriterHandle, JoinHandle<()>)> {
    let mut store = Store::with_seal_rows(model, config.seal_rows);
    let first = store
        .publish()
        .unwrap_or_else(|| Snapshot::empty(Arc::clone(store.model())));
    let (epochs_tx, epochs) = watch::channel(first.epoch);
    let snapshot = Arc::new(ArcSwap::from_pointee(first));
    let (queue, commands) = sync_channel(QUEUE);
    let published = Arc::clone(&snapshot);
    let thread = std::thread::Builder::new()
        .name("tidepool-writer".into())
        .spawn(move || {
            run(
                store,
                &commands,
                &published,
                &epochs_tx,
                config.epoch_interval,
            );
        })?;
    Ok((
        WriterHandle {
            queue,
            snapshot,
            epochs,
        },
        thread,
    ))
}

fn run(
    mut store: Store,
    commands: &Receiver<Command>,
    snapshot: &ArcSwap<Snapshot>,
    epochs: &watch::Sender<u64>,
    interval: Duration,
) {
    let mut next_tick = Instant::now() + interval;
    let publish = |store: &mut Store| {
        if let Some(published) = store.publish() {
            let epoch = published.epoch;
            snapshot.store(Arc::new(published));
            let _ = epochs.send(epoch);
        }
        store.epoch()
    };
    loop {
        let wait = next_tick.saturating_duration_since(Instant::now());
        match commands.recv_timeout(wait) {
            Ok(Command::Apply {
                table,
                op,
                batch,
                reply,
            }) => {
                let applied = store.apply(table, op, &batch);
                // visible from the next epoch on
                let _ = reply.send(Receipt {
                    applied,
                    epoch: store.epoch() + 1,
                });
            }
            Ok(Command::Publish { reply }) => {
                let epoch = publish(&mut store);
                let _ = reply.send(epoch);
            }
            Err(RecvTimeoutError::Timeout) => {
                publish(&mut store);
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
        };
        // the receiver, kept alive so the queue fills instead of disconnecting
        (handle, DropGuard(commands))
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
