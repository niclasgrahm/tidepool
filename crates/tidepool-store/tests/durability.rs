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
//! Some writes carry an idempotency key: after every recovery the store must
//! hold exactly the keys of the acknowledged ones, each with the credential
//! it was sent with and the response it was acknowledged with, whether it
//! came back from the log or from a checkpoint.
//!
//! Model changes are part of the stream (M7): every so often the model
//! moves to a version that drops or adds back a relationship over populated
//! tables (which rewrites one side's codes) and a column, sometimes built
//! from a snapshot the way the writer's background rebuild does. They are
//! acknowledged like writes, at the next commit.
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
use tidepool_model::compile;
use tidepool_model::ir::Model;
use tidepool_store::batch::{Shape, json_array};
use tidepool_store::keys::{Keyed, Scoped};
use tidepool_store::migrate::build;
use tidepool_store::persist::Durable;
use tidepool_store::{Applied, Batch, ModelVersion, Op, Rebuilt, Snapshot, Store};

/// The retail example (without dashboards), or the variant without the
/// orders → customers relationship and with an extra column, as the
/// version after `previous`.
fn version(previous: Option<&ModelVersion>, variant: bool) -> ModelVersion {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/retail");
    let read = |p: &str| std::fs::read_to_string(dir.join(p)).unwrap_or_default();
    let mut files = vec![
        ("tidepool.yaml".to_string(), read("tidepool.yaml")),
        ("model.ossie.yaml".to_string(), read("model.ossie.yaml")),
        ("tables/orders.yaml".to_string(), read("tables/orders.yaml")),
        (
            "tables/customers.yaml".to_string(),
            read("tables/customers.yaml"),
        ),
        (
            "tables/products.yaml".to_string(),
            read("tables/products.yaml"),
        ),
        (
            "tables/categories.yaml".to_string(),
            read("tables/categories.yaml"),
        ),
    ];
    if variant {
        files[1].1 = files[1].1.replace(
            "  - name: order_customer
    from: orders                         # many side
    to: customers                        # one side
    from_columns: [customer_id]
    to_columns: [customer_id]
",
            "",
        );
        files[2]
            .1
            .push_str("  channel:     { type: string, nullable: true }\n");
    }
    let (set, _) = ConfigSet::from_files(files);
    let catalog = previous.map(|p| (*p.catalog).clone()).unwrap_or_default();
    match compile(&set, &catalog) {
        (Some(compiled), _) => ModelVersion {
            version: previous.map_or(1, |p| p.version + 1),
            model: Arc::new(compiled.model),
            catalog: Arc::new(compiled.catalog),
        },
        (None, d) => panic!("the example should compile: {d:?}"),
    }
}

/// A write (with its idempotency key and receipt, if it had a key), or a
/// model change, in the order they were acknowledged.
#[derive(Clone)]
enum Event {
    Write(Write_, Option<Receipt>),
    Model(ModelVersion),
}

/// A keyed write as the store should remember it: credential, table, key,
/// what it did, the epoch it was acknowledged with.
type Receipt = (String, u32, String, Applied, u64);

fn receipt(keyed: &Keyed) -> Receipt {
    (
        keyed.credential.clone(),
        keyed.table.0,
        keyed.key.clone(),
        keyed.applied,
        keyed.epoch,
    )
}

/// The receipts of the acknowledged keyed writes, oldest first.
fn expected_keys(acked: &[Event]) -> Vec<Receipt> {
    acked
        .iter()
        .filter_map(|e| match e {
            Event::Write(_, key) => key.clone(),
            Event::Model(_) => None,
        })
        .collect()
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
    match rng.below(11) {
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
        // the customers' whole contents: the ones not listed go
        10 => {
            let mut rows: Vec<Value> = Vec::new();
            for c in 0..30 {
                if rng.chance(40) {
                    let region = regions[usize::try_from(rng.below(4)).unwrap_or(0)];
                    rows.push(json!({
                        "customer_id": format!("c{c}"),
                        "name": format!("Name {}", rng.below(1000)),
                        "region": region,
                    }));
                }
            }
            Write_("customers".into(), Op::Replace, Value::Array(rows))
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

fn expected(first: &ModelVersion, acked: &[Event]) -> BTreeMap<String, Vec<String>> {
    let mut store = Store::with_version(first.clone(), 1 << 20);
    for event in acked {
        match event {
            Event::Write(write, _) => {
                let model = Arc::clone(store.model());
                let (table, batch) = decode(&model, write);
                store.apply(table, write.1, &batch);
            }
            Event::Model(version) => store.change_model(version.clone()),
        }
    }
    let snapshot = store
        .publish()
        .unwrap_or_else(|| Snapshot::empty(Arc::clone(store.model())));
    contents(&snapshot)
}

fn open(
    first: &ModelVersion,
    dir: &Path,
    seal: usize,
) -> (Durable, Snapshot, tidepool_store::persist::Checkpoints) {
    match Durable::open(first.clone(), dir, seal, Durability::Fsync) {
        Ok(mut opened) => {
            // small log files, so checkpoints get to drop some
            opened.0.roll_log_at(4096);
            opened
        }
        Err(err) => panic!("recovering {}: {err}", dir.display()),
    }
}

#[test]
#[allow(clippy::too_many_lines)] // one scenario, start to end
fn nothing_acknowledged_is_lost_across_random_crashes() {
    let seeds: u64 = std::env::var("TIDEPOOL_CRASH_SEEDS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(20);
    let first = version(None, false);
    let mut crash_points = 0;
    let mut changes = 0;
    let mut keyed = 0;
    for seed in 0..seeds {
        let mut rng = Rng(seed);
        let dir = match tempfile::tempdir() {
            Ok(dir) => dir,
            Err(err) => panic!("{err}"),
        };
        let seal = 8 + usize::try_from(rng.below(64)).unwrap_or(8);
        let (mut durable, _, mut checkpoints) = open(&first, dir.path(), seal);
        let mut acked: Vec<Event> = Vec::new();
        let mut pending: Vec<Event> = Vec::new();
        // the newest version made, acknowledged or not: the next follows it
        let mut newest = first.clone();
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
                if rng.chance(4) {
                    // a model change, sometimes built from a snapshot first
                    let current = durable.store().model_version();
                    let variant = current.model.relationships.len() == 2;
                    let next = version(Some(&newest), variant);
                    newest = next.clone();
                    let migration = durable.store().plan_migration(next.clone());
                    let rebuilt = match durable.publish() {
                        Some((snapshot, _)) if rng.chance(50) => {
                            build(&migration, &snapshot, &std::sync::atomic::AtomicU64::new(0))
                        }
                        _ => Rebuilt::default(),
                    };
                    durable.migrate(migration, rebuilt);
                    pending.push(Event::Model(next));
                    changes += 1;
                }
                let write = random_write(&mut rng, &mut serial);
                let model = Arc::clone(durable.store().model());
                let (table, batch) = decode(&model, &write);
                // two producers, sometimes with the same key
                let key = rng.chance(30).then(|| Scoped {
                    credential: if rng.chance(50) { "kayak" } else { "sensors" }.to_string(),
                    key: format!("key-{seed}-{}", serial / 2),
                });
                let applied = durable.apply(table, write.1, &batch, key.as_ref());
                let epoch = durable.store().visible_epoch();
                let key = key.map(|key| (key.credential, table.0, key.key, applied, epoch));
                pending.push(Event::Write(write, key));
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
                        let keys = durable.keys();
                        match checkpoints.write(&snapshot, lsn, &keys) {
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

            let (mut recovered, snapshot, fresh) = open(&first, dir.path(), seal);
            // a model change that wasn't acknowledged is gone: the next
            // follows the recovered one
            newest = match acked.iter().rev().find_map(|e| match e {
                Event::Model(v) => Some(v.clone()),
                Event::Write(..) => None,
            }) {
                Some(v) if v.version >= newest.version => v,
                _ => newest,
            };
            assert_eq!(
                contents(&snapshot),
                expected(&first, &acked),
                "seed {seed}, crash {crash} (in a checkpoint: {died_in_checkpoint}): \
                 the recovered store isn't the acknowledged writes"
            );
            assert_eq!(
                recovered.keys().iter().map(receipt).collect::<Vec<_>>(),
                expected_keys(&acked),
                "seed {seed}, crash {crash} (in a checkpoint: {died_in_checkpoint}): \
                 the recovered idempotency keys aren't the acknowledged ones"
            );
            keyed += expected_keys(&acked).len();
            durable = recovered;
            checkpoints = fresh;
            late = None;
            crash_points += 1;
        }
    }
    assert!(
        seeds == 0 || changes > 0,
        "the stream should change the model"
    );
    assert!(seeds == 0 || keyed > 0, "some keys should be recovered");
    println!(
        "{crash_points} crash points recovered exactly, {changes} model changes made, \
         {keyed} recovered keys checked"
    );
}
