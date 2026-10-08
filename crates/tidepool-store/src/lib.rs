//! The column store: values, dictionaries, segments, the primary-key index,
//! epochs and snapshots, and the decoders that turn ingest bodies into
//! batches. In memory in M2; durable (WAL, segment files) in M5.
//!
//! The shape of it: one writer thread owns a [`store::Store`] and applies
//! batches in order; every epoch it publishes an immutable
//! [`store::Snapshot`], which is all readers ever see. There is no shared
//! mutable state between them, and no `unsafe`.

mod arrow;
pub mod batch;
mod codec;
pub mod column;
pub mod dict;
pub mod migrate;
pub mod persist;
pub mod store;
pub mod value;
pub mod wal;
pub mod writer;

pub use batch::{Batch, Rejected, RowError, Shape};
pub use migrate::{Migration, ModelVersion, Rebuilt};
pub use store::{Applied, Op, Snapshot, Store, TableSnapshot};
pub use writer::{Busy, Change, LogStatus, Receipt, WriterConfig, WriterHandle, spawn};
