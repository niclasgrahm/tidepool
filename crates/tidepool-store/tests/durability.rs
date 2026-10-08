//! Crash injection (plan §11.4): a durable store is written to, checkpointed
//! and "crashed" at random points, over and over, and must always recover
//! exactly what it acknowledged.
//!
//! A crash here is a power loss: everything not synced is gone. Work since
//! the last commit is dropped, the open log file is cut back to its last
//! fsync and given a random torn tail, and some crashes happen halfway
//! through writing a checkpoint. After recovery, the store's contents must
//! equal the acknowledged writes replayed into a fresh in-memory store:
//! nothing acknowledged lost, nothing unacknowledged invented.
//!
//! `TIDEPOOL_CRASH_SEEDS` sets how many seeds run, ten crashes each:
//! default 20 for every `just ci`; `just crash` runs the M5 exit
//! criterion's 1,000 crash points.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};
use tidepool_config::ConfigSet;
use tidepool_config::server::Durability;
use tidepool_model::ir::Model;
use tidepool_model::{Catalog, compile};
use tidepool_store::batch::{Shape, json_array};
use tidepool_store::persist::Durable;
use tidepool_store::{Batch, Op, Snapshot, Store};

fn model() -> Arc<Model> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/retail");
    let (set, _) = match ConfigSet::load_dir(&dir) {
        Ok(loaded) => loaded,
        Err(err) => panic!("reading the example: {err}"),
    };
    match compile(&set, &Catalog::default()) {
        (Some(compiled), _) => Arc::new(compiled.model),
        (None, d) => panic!("the example should compile: {d:?}"),
    }
}

/// `SplitMix64`.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// One write: table, op, rows as JSON.
#[derive(Clone)]
struct Write_(String, Op, Value);

fn random_write(rng: &mut Rng, serial: &mut u64) -> Write_ {
    *serial += 1;
    let regions = ["north", "south", "east", "west"];
    match rng.below(10) {
        0..=2 => {
            let rows: Vec<Value> = (0..=rng.below(5))
                .map(|_| {
                    let region = regions[usize::try_from(rng.below(4)).unwrap_or(0)];
                    json!({
                        "customer_id": format!("c{}", rng.below(30)),
                        "name": format!("Name {}", rng.below(1000)),
                        "region": region,
                    })
                })
                .collect();
            Write_("customers".into(), Op::Write, Value::Array(rows))
        }
        3 => {
            let keys: Vec<Value> = (0..=rng.below(2))
                .map(|_| json!({"customer_id": format!("c{}", rng.below(30))}))
                .collect();
            Write_("customers".into(), Op::Delete, Value::Array(keys))
        }
        _ => {
            let rows: Vec<Value> = (0..=rng.below(20))
                .map(|_| {
                    *serial += 1;
                    let status = ["paid", "shipped"][usize::try_from(rng.below(2)).unwrap_or(0)];
                    json!({
                        "order_id": format!("o{serial}"),
                        "customer_id": format!("c{}", rng.below(30)),
                        "product_id": format!("p{}", rng.below(5)),
                        "status": status,
                        "ordered_at": "2026-10-01T12:00:00Z",
                        "quantity": rng.below(9),
                        "amount": format!("{}.{:02}", rng.below(500), rng.below(100)),
                    })
                })
                .collect();
            Write_("orders".into(), Op::Write, Value::Array(rows))
        }
    }
}

fn decode(model: &Model, write: &Write_) -> (tidepool_model::ir::TableId, Batch) {
    let Some(table) = model.table_named(&write.0) else {
        panic!("no table {}", write.0);
    };
    let shape = if write.1 == Op::Delete {
        Shape::Keys
    } else {
        Shape::Rows
    };
    match json_array(table, shape, write.2.to_string().as_bytes()) {
        Ok(batch) => (table.id, batch),
        Err(rejected) => panic!("{rejected:?}"),
    }
}

/// Every table's live rows, decoded and sorted: what a store holds, with
/// row ids and dictionary codes out of the picture.
fn contents(snapshot: &Snapshot) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    for table in &snapshot.model.tables {
        let mut rows: Vec<String> = snapshot
            .newest_rows(table.id, usize::MAX)
            .iter()
            .map(|row| format!("{row:?}"))
            .collect();
        rows.sort();
        out.insert(table.name.clone(), rows);
    }
    out
}

fn expected(model: &Arc<Model>, acked: &[Write_]) -> BTreeMap<String, Vec<String>> {
    let mut store = Store::with_seal_rows(Arc::clone(model), 1 << 20);
    for write in acked {
        let (table, batch) = decode(model, write);
        store.apply(table, write.1, &batch);
    }
    let snapshot = store
        .publish()
        .unwrap_or_else(|| Snapshot::empty(Arc::clone(model)));
    contents(&snapshot)
}

fn open(
    model: &Arc<Model>,
    dir: &Path,
    seal: usize,
) -> (Durable, Snapshot, tidepool_store::persist::Checkpoints) {
    match Durable::open(Arc::clone(model), dir, seal, Durability::Fsync) {
        Ok(mut opened) => {
            // small log files, so checkpoints get to drop some
            opened.0.roll_log_at(4096);
            opened
        }
        Err(err) => panic!("recovering {}: {err}", dir.display()),
    }
}

#[test]
fn nothing_acknowledged_is_lost_across_random_crashes() {
    let seeds: u64 = std::env::var("TIDEPOOL_CRASH_SEEDS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(20);
    let model = model();
    let mut crash_points = 0;
    for seed in 0..seeds {
        let mut rng = Rng(seed);
        let dir = match tempfile::tempdir() {
            Ok(dir) => dir,
            Err(err) => panic!("{err}"),
        };
        let seal = 8 + usize::try_from(rng.below(64)).unwrap_or(8);
        let (mut durable, _, mut checkpoints) = open(&model, dir.path(), seal);
        let mut acked: Vec<Write_> = Vec::new();
        let mut pending: Vec<Write_> = Vec::new();
        let mut serial = seed * 1_000_000;
        // a checkpoint's LSN whose log isn't forgotten yet
        let mut late: Option<u64> = None;
        for crash in 0..10 {
            // a stretch of work: writes, commits, publishes, checkpoints
            let mut died_in_checkpoint = false;
            for _ in 0..rng.below(40) {
                if let Some(lsn) = late
                    && rng.chance(20)
                {
                    if let Err(err) = durable.forget(lsn) {
                        panic!("seed {seed}: forgetting the log: {err}");
                    }
                    late = None;
                }
                let write = random_write(&mut rng, &mut serial);
                let (table, batch) = decode(&model, &write);
                durable.apply(table, write.1, &batch);
                pending.push(write);
                if rng.chance(30) {
                    if let Err(err) = durable.commit() {
                        panic!("seed {seed}: committing: {err}");
                    }
                    acked.append(&mut pending);
                }
                if rng.chance(10) {
                    if let Err(err) = durable.commit() {
                        panic!("seed {seed}: committing: {err}");
                    }
                    acked.append(&mut pending);
                    if let Some((snapshot, lsn)) = durable.publish()
                        && rng.chance(50)
                    {
                        let dies = rng.chance(30);
                        if dies {
                            checkpoints.crash_after(usize::try_from(rng.below(6)).unwrap_or(0));
                        }
                        match checkpoints.write(&snapshot, lsn) {
                            // forgotten later, as when the checkpoint
                            // thread reports back while writes go on
                            Ok(()) => late = Some(lsn),
                            Err(_) if dies => {
                                died_in_checkpoint = true;
                                break;
                            }
                            Err(err) => panic!("seed {seed}: checkpoint: {err}"),
                        }
                    }
                }
            }

            // power loss: unsynced work gone, a torn tail on the log
            let (log, synced) = durable.durable_log();
            drop(durable);
            pending.clear();
            if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&log) {
                let _ = file.set_len(synced);
            }
            let torn: Vec<u8> = (0..rng.below(24))
                .map(|_| u8::try_from(rng.below(256)).unwrap_or(0))
                .collect();
            if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(&log) {
                let _ = file.write_all(&torn);
            }

            let (recovered, snapshot, fresh) = open(&model, dir.path(), seal);
            assert_eq!(
                contents(&snapshot),
                expected(&model, &acked),
                "seed {seed}, crash {crash} (in a checkpoint: {died_in_checkpoint}): \
                 the recovered store isn't the acknowledged writes"
            );
            durable = recovered;
            checkpoints = fresh;
            late = None;
            crash_points += 1;
        }
    }
    println!("{crash_points} crash points recovered exactly");
}
