//! Model changes over stored data (plan §8.2): every kind of change on the
//! retail example, at once and as a background build that writes carry on
//! through, and through the log and checkpoints.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use tidepool_config::ConfigSet;
use tidepool_config::server::Durability;
use tidepool_model::compile;
use tidepool_model::ir::{Model, TableId};
use tidepool_store::batch::{Batch, Shape, json_array};
use tidepool_store::migrate::build;
use tidepool_store::persist::Durable;
use tidepool_store::value::Value;
use tidepool_store::{ModelVersion, Op, Rebuilt, Snapshot, Store};

/// The retail example's files (no dashboards: they aren't what changes
/// here), with `edits` applied as (file, from, to) replacements.
fn files(edits: &[(&str, &str, &str)]) -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/retail");
    let mut files = Vec::new();
    for sub in ["", "tables"] {
        let entries = match std::fs::read_dir(dir.join(sub)) {
            Ok(entries) => entries,
            Err(err) => panic!("reading the example: {err}"),
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = if sub.is_empty() {
                name
            } else {
                format!("{sub}/{name}")
            };
            if !Path::new(&path)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("yaml"))
            {
                continue;
            }
            let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
            files.push((path, text));
        }
    }
    for (file, from, to) in edits {
        let Some((_, text)) = files.iter_mut().find(|(p, _)| p == file) else {
            if from.is_empty() {
                files.push(((*file).to_string(), (*to).to_string()));
                continue;
            }
            panic!("no file {file}");
        };
        assert!(text.contains(from), "{file} has no `{from}`");
        *text = text.replacen(from, to, 1);
    }
    files
}

/// `files` compiled as the version after `previous`.
fn version(previous: Option<&ModelVersion>, files: Vec<(String, String)>) -> ModelVersion {
    let (set, _) = ConfigSet::from_files(files);
    let catalog = previous.map(|p| (*p.catalog).clone()).unwrap_or_default();
    match compile(&set, &catalog) {
        (Some(compiled), _) => ModelVersion {
            version: previous.map_or(1, |p| p.version + 1),
            model: Arc::new(compiled.model),
            catalog: Arc::new(compiled.catalog),
        },
        (None, d) => panic!("the edited example should compile: {d:?}"),
    }
}

const NO_ORDER_CUSTOMER: (&str, &str, &str) = (
    "model.ossie.yaml",
    "  - name: order_customer
    from: orders                         # many side
    to: customers                        # one side
    from_columns: [customer_id]
    to_columns: [customer_id]
",
    "",
);

fn table(model: &Model, name: &str) -> TableId {
    match model.table_named(name) {
        Some(table) => table.id,
        None => panic!("no table {name}"),
    }
}

fn decode(model: &Model, name: &str, op: Op, json: &str) -> Batch {
    let Some(table) = model.table_named(name) else {
        panic!("no table {name}");
    };
    let shape = if op == Op::Delete {
        Shape::Keys
    } else {
        Shape::Rows
    };
    match json_array(table, shape, json.as_bytes()) {
        Ok(batch) => batch,
        Err(rejected) => panic!("{rejected:?}"),
    }
}

fn write(store: &mut Store, name: &str, json: &str) {
    let model = Arc::clone(store.model());
    let batch = decode(&model, name, Op::Write, json);
    store.apply(table(&model, name), Op::Write, &batch);
}

fn publish(store: &mut Store) -> Snapshot {
    match store.publish() {
        Some(snapshot) => snapshot,
        None => panic!("expected a new epoch"),
    }
}

/// Every live row of a table as `column=value` pairs, sorted: what a
/// change must keep, whatever it does to the stored form.
fn rows(snapshot: &Snapshot, name: &str) -> Vec<String> {
    let id = table(&snapshot.model, name);
    let Some(t) = snapshot.table(id) else {
        panic!("no table {name} in the snapshot");
    };
    let mut out: Vec<String> = snapshot
        .newest_rows(id, usize::MAX)
        .into_iter()
        .map(|row| {
            t.layouts
                .iter()
                .zip(row)
                .map(|(l, v)| format!("{}={}", l.name, v.to_json()))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    out.sort();
    out
}

fn order(id: u32, customer: &str, amount: &str) -> String {
    format!(
        r#"{{"order_id":"o{id}","customer_id":"{customer}","product_id":"p1","status":"paid","ordered_at":"2026-10-01T00:00:00Z","quantity":{id},"amount":"{amount}","note":null}}"#
    )
}

fn orders(range: std::ops::Range<u32>) -> String {
    let rows: Vec<String> = range
        .map(|i| order(i, &format!("c{}", i % 7), &format!("{i}.50")))
        .collect();
    format!("[{}]", rows.join(","))
}

fn customers(range: std::ops::Range<u32>) -> String {
    let rows: Vec<String> = range
        .map(|i| {
            format!(
                r#"{{"customer_id":"c{i}","name":"n{i}","region":"r{}"}}"#,
                i % 3
            )
        })
        .collect();
    format!("[{}]", rows.join(","))
}

/// A store of version `v` with customers and orders in a few chunks and
/// a sealed segment.
fn filled(v: &ModelVersion) -> Store {
    let mut store = Store::with_version(v.clone(), 8);
    write(&mut store, "customers", &customers(0..5));
    write(&mut store, "orders", &orders(0..6));
    publish(&mut store);
    write(&mut store, "orders", &orders(6..14));
    publish(&mut store);
    write(&mut store, "orders", &orders(14..17));
    store
}

#[test]
fn a_new_nullable_column_reads_null_and_shares_the_old_columns() {
    let v1 = version(None, files(&[]));
    let mut store = filled(&v1);
    let before = publish(&mut store);
    let v2 = version(
        Some(&v1),
        files(&[(
            "tables/orders.yaml",
            "  note:        { type: string, nullable: true, encoding: plain }",
            "  note:        { type: string, nullable: true, encoding: plain }\n  channel:     { type: string, nullable: true }",
        )]),
    );
    let migration = store.plan_migration(v2.clone());
    assert_eq!(migration.rows(), 0, "adding a column rewrites nothing");
    store.migrate(migration, Rebuilt::default());
    write(
        &mut store,
        "orders",
        r#"[{"order_id":"o99","customer_id":"c1","product_id":"p1","status":"paid","ordered_at":"2026-10-01T00:00:00Z","quantity":1,"amount":"1.00","channel":"web"}]"#,
    );
    let after = publish(&mut store);
    assert_eq!(after.version, 2);
    let got = rows(&after, "orders");
    assert_eq!(got.len(), 18);
    assert_eq!(
        got.iter().filter(|r| r.ends_with("channel=null")).count(),
        17
    );
    assert!(got.iter().any(|r| r.ends_with(r#"channel="web""#)));

    // the old columns are the same memory in the new segments
    let id = table(&v1.model, "orders");
    let (Some(old), Some(new)) = (before.table(id), after.table(id)) else {
        panic!("orders in both snapshots");
    };
    assert!(Arc::ptr_eq(
        &old.segments[0].data.columns[0],
        &new.segments[0].data.columns[0]
    ));
    // nothing about customers changed: the same table snapshot
    let customers = table(&v1.model, "customers");
    let (Some(was), Some(now)) = (before.tables.get(&customers), after.tables.get(&customers))
    else {
        panic!("customers in both snapshots");
    };
    assert!(Arc::ptr_eq(was, now));
}

#[test]
fn a_removed_column_and_table_are_gone() {
    let v1 = version(None, files(&[]));
    let mut store = filled(&v1);
    let mut changed = files(&[(
        "tables/orders.yaml",
        "  note:        { type: string, nullable: true, encoding: plain }\n",
        "",
    )]);
    changed.retain(|(p, _)| p != "tables/products.yaml");
    // the model mentions products: drop it there too
    for (path, text) in &mut changed {
        if path == "model.ossie.yaml" {
            *text = text
                .replace(
                    "  - name: products
    source: products
    primary_key: [product_id]
    fields:
      - name: category
        expression: { dialects: [{ dialect: ANSI_SQL, expression: category }] }
",
                    "",
                )
                .replace(
                    "  - name: order_product
    from: orders
    to: products
    from_columns: [product_id]
    to_columns: [product_id]
",
                    "",
                )
                .replace(
                    "  - name: product_category
    from: products
    to: categories
    from_columns: [category]
    to_columns: [category]
",
                    "",
                );
        }
    }
    let v2 = version(Some(&v1), changed);
    store.change_model(v2);
    let snapshot = publish(&mut store);
    assert!(snapshot.model.table_named("products").is_none());
    assert_eq!(snapshot.tables.len(), 3);
    let got = rows(&snapshot, "orders");
    assert_eq!(got.len(), 17);
    assert!(got.iter().all(|r| !r.contains("note=")));
}

/// Joining two populated tables merges their key dictionaries: both
/// columns end up in one, every value kept, the key index working.
fn check_merged(snapshot: &Snapshot, count: usize) {
    let (orders, customers) = (
        table(&snapshot.model, "orders"),
        table(&snapshot.model, "customers"),
    );
    let dict = |t: TableId| {
        snapshot
            .table(t)
            .and_then(|t| t.layouts.iter().find(|l| l.name == "customer_id"))
            .and_then(|l| l.dict)
    };
    assert!(dict(orders).is_some());
    assert_eq!(dict(orders), dict(customers), "one dictionary for both");
    assert_eq!(rows(snapshot, "customers").len(), count);
}

#[test]
fn a_relationship_over_populated_tables_merges_their_dictionaries() {
    let v1 = version(None, files(&[NO_ORDER_CUSTOMER]));
    let mut store = filled(&v1);
    let before = publish(&mut store);
    let expected_orders = rows(&before, "orders");
    let v2 = version(Some(&v1), files(&[]));
    let migration = store.plan_migration(v2);
    assert!(migration.rows() > 0, "the codes of one side are rewritten");
    store.migrate(migration, Rebuilt::default());
    write(&mut store, "customers", &customers(3..9));
    let after = publish(&mut store);
    assert_eq!(rows(&after, "orders"), expected_orders);
    check_merged(&after, 9);
}

#[test]
fn a_background_build_catches_up_with_what_was_written_meanwhile() {
    let v1 = version(None, files(&[NO_ORDER_CUSTOMER]));
    let mut store = filled(&v1);
    // a customer no order has: the merged dictionary gets a code for it
    // after the base's, so the base's codes added during the build move
    write(
        &mut store,
        "customers",
        r#"[{"customer_id":"c-only","name":"x","region":"r0"}]"#,
    );
    let started = Arc::new(publish(&mut store));
    let v2 = version(Some(&v1), files(&[]));
    let migration = store.plan_migration(v2);
    let progress = AtomicU64::new(0);
    let rebuilt = build(&migration, &started, &progress);
    assert_eq!(
        progress.load(std::sync::atomic::Ordering::Relaxed),
        migration.rows()
    );

    // meanwhile: new customers and new customer ids on orders, a seal
    write(&mut store, "customers", &customers(5..9));
    write(&mut store, "orders", &orders(17..30));
    publish(&mut store);
    write(
        &mut store,
        "orders",
        &format!("[{}]", order(30, "c-new", "3.00")),
    );
    let expected = {
        let mut probe = Store::with_version(v1.clone(), 8);
        write(&mut probe, "customers", &customers(0..9));
        write(
            &mut probe,
            "customers",
            r#"[{"customer_id":"c-only","name":"x","region":"r0"}]"#,
        );
        write(&mut probe, "orders", &orders(0..30));
        write(
            &mut probe,
            "orders",
            &format!("[{}]", order(30, "c-new", "3.00")),
        );
        rows(&publish(&mut probe), "orders")
    };

    store.migrate(migration, rebuilt);
    let after = publish(&mut store);
    assert_eq!(rows(&after, "orders"), expected);
    assert_eq!(rows(&after, "customers").len(), 10);
    // upserts still find their rows through the rebuilt key index
    write(
        &mut store,
        "customers",
        r#"[{"customer_id":"c1","name":"renamed","region":"r9"}]"#,
    );
    let last = publish(&mut store);
    let customers = rows(&last, "customers");
    assert_eq!(customers.len(), 10);
    assert!(customers.iter().any(|r| r.contains(r#"name="renamed""#)));
}

#[test]
fn widened_and_converted_columns_keep_their_values() {
    let v1 = version(None, files(&[]));
    let mut store = filled(&v1);
    let v2 = version(
        Some(&v1),
        files(&[
            (
                "tables/orders.yaml",
                r#"amount:      { type: "decimal(14,2)" }"#,
                r#"amount:      { type: "decimal(24,4)" }"#,
            ),
            (
                "tables/orders.yaml",
                "quantity:    { type: int32 }",
                "quantity:    { type: int64 }",
            ),
            (
                "tables/orders.yaml",
                "order_id:    { type: string, encoding: plain }",
                "order_id:    { type: string }",
            ),
        ]),
    );
    store.change_model(v2);
    let after = publish(&mut store);
    let got = rows(&after, "orders");
    assert_eq!(got.len(), 17);
    assert!(
        got.iter()
            .any(|r| r.contains(r#"order_id="o3""#) && r.contains(r#"amount="3.5000""#)),
        "{got:?}"
    );
    let id = table(&after.model, "orders");
    let order_id = after
        .table(id)
        .and_then(|t| t.layouts.iter().find(|l| l.name == "order_id"))
        .and_then(|l| l.dict);
    assert!(order_id.is_some(), "order_id is dictionary-encoded now");
}

#[test]
fn an_append_table_made_upsert_keeps_the_latest_row_per_key() {
    let v1 = version(None, files(&[]));
    let mut store = Store::with_version(v1.clone(), 8);
    write(&mut store, "orders", &orders(0..4));
    write(
        &mut store,
        "orders",
        &format!("[{}]", order(2, "c9", "99.00")),
    );
    publish(&mut store);
    let v2 = version(
        Some(&v1),
        files(&[
            (
                "tables/orders.yaml",
                "mode: append",
                "mode: upsert\nprimary_key: [order_id]",
            ),
            (
                "tables/orders.yaml",
                "order_id:    { type: string, encoding: plain }",
                "order_id:    { type: string }",
            ),
        ]),
    );
    store.change_model(v2);
    let got = rows(&publish(&mut store), "orders");
    assert_eq!(got.len(), 4);
    assert!(got.iter().any(|r| r.contains(r#"amount="99.00""#)));
    write(
        &mut store,
        "orders",
        &format!("[{}]", order(1, "c9", "7.00")),
    );
    let got = rows(&publish(&mut store), "orders");
    assert_eq!(got.len(), 4, "an upsert replaces the row with its key");
}

#[test]
fn a_batch_decoded_before_a_change_is_fitted_to_the_new_columns() {
    let v1 = version(None, files(&[]));
    let mut store = Store::with_version(v1.clone(), 8);
    let old = decode(&v1.model, "orders", Op::Write, &orders(0..2));
    let v2 = version(
        Some(&v1),
        files(&[
            (
                "tables/orders.yaml",
                r#"amount:      { type: "decimal(14,2)" }"#,
                r#"amount:      { type: "decimal(24,4)" }"#,
            ),
            (
                "tables/orders.yaml",
                "  note:        { type: string, nullable: true, encoding: plain }\n",
                "  channel:     { type: string, nullable: true }\n",
            ),
        ]),
    );
    store.change_model(v2);
    store.apply(table(&v1.model, "orders"), Op::Write, &old);
    let got = rows(&publish(&mut store), "orders");
    assert_eq!(got.len(), 2);
    assert!(got[0].contains(r#"amount="0.5000""#), "{got:?}");
    assert!(got[0].ends_with("channel=null"));
}

#[test]
fn a_model_change_is_replayed_from_the_log_and_kept_by_checkpoints() {
    let dir = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(err) => panic!("{err}"),
    };
    let open = |v: &ModelVersion| match Durable::open(v.clone(), dir.path(), 8, Durability::Fsync) {
        Ok(opened) => opened,
        Err(err) => panic!("opening: {err}"),
    };
    let v1 = version(None, files(&[NO_ORDER_CUSTOMER]));
    let v2 = version(Some(&v1), files(&[]));

    let (mut durable, _, mut checkpoints) = open(&v1);
    let apply = |durable: &mut Durable, name: &str, json: &str| {
        let model = Arc::clone(durable.store().model());
        let batch = decode(&model, name, Op::Write, json);
        durable.apply(table(&model, name), Op::Write, &batch, None);
    };
    apply(&mut durable, "customers", &customers(0..5));
    apply(&mut durable, "orders", &orders(0..12));
    let Some((snapshot, lsn)) = durable.publish() else {
        panic!("a snapshot");
    };
    // a checkpoint before the change: the change itself is replayed
    if let Err(err) = checkpoints.write(&snapshot, lsn, &[]) {
        panic!("{err}");
    }
    let migration = durable.store().plan_migration(v2.clone());
    let rebuilt = build(&migration, &snapshot, &AtomicU64::new(0));
    durable.migrate(migration, rebuilt);
    apply(&mut durable, "customers", &customers(5..9));
    apply(&mut durable, "orders", &orders(12..20));
    if let Err(err) = durable.commit() {
        panic!("{err}");
    }
    let Some((live, lsn)) = durable.publish() else {
        panic!("a snapshot");
    };
    let expected = rows(&live, "orders");
    drop((durable, checkpoints));

    // reopened with the old config: the data says which model it is in
    let (durable, replayed, mut checkpoints) = open(&v1);
    assert_eq!(replayed.version, 2);
    assert_eq!(rows(&replayed, "orders"), expected);
    check_merged(&replayed, 9);

    // a checkpoint after the change holds the model and the rebuilt files
    if let Err(err) = checkpoints.write(&replayed, lsn, &[]) {
        panic!("{err}");
    }
    drop((durable, checkpoints, live));
    let (_durable, recovered, _) = open(&v1);
    assert_eq!(recovered.version, 2);
    assert_eq!(rows(&recovered, "orders"), expected);
    check_merged(&recovered, 9);
    let _ = Value::Null;
}

#[test]
fn the_writer_rebuilds_in_the_background_and_takes_the_new_epoch_interval() {
    use std::time::Duration;
    use tidepool_store::{Change, WriterConfig, spawn};
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => panic!("{err}"),
    };
    let v1 = version(None, files(&[NO_ORDER_CUSTOMER]));
    let config = WriterConfig {
        epoch_interval: Duration::from_millis(10),
        seal_rows: 8,
        ..WriterConfig::default()
    };
    let (writer, _thread) = match spawn(v1.clone(), config) {
        Ok(started) => started,
        Err(err) => panic!("{err}"),
    };
    let send = |name: &str, json: &str| {
        let model = writer.snapshot().model.clone();
        let batch = decode(&model, name, Op::Write, json);
        let receipt = writer
            .apply(table(&model, name), Op::Write, batch, None)
            .unwrap_or_else(|_| panic!("busy"));
        runtime
            .block_on(receipt)
            .unwrap_or_else(|_| panic!("stopped"))
    };
    send("customers", &customers(0..5));
    for chunk in 0..10 {
        send("orders", &orders(chunk * 10..chunk * 10 + 10));
    }
    // the relationship, and an epoch interval far longer than the test
    let v2 = version(
        Some(&v1),
        files(&[(
            "tidepool.yaml",
            "epoch_interval: 200ms",
            "epoch_interval: 1h",
        )]),
    );
    match runtime.block_on(writer.change_model(v2)) {
        Ok(Change::Rebuilding { rows }) => assert!(rows > 0),
        other => panic!("expected a background rebuild, got {other:?}"),
    }
    // ingest carries on meanwhile, decoded under whichever model is live
    send("orders", &orders(100..110));
    for _ in 0..500 {
        if writer.snapshot().version == 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(writer.snapshot().version, 2, "the rebuild went live");
    assert!(writer.rebuild().is_none());
    // the batch sent meanwhile may have landed after the swap, when epochs
    // are an hour apart: publish it now
    assert!(
        runtime.block_on(writer.publish()).is_ok(),
        "the writer stopped"
    );
    let snapshot = writer.snapshot();
    check_merged(&snapshot, 5);
    assert_eq!(rows(&snapshot, "orders").len(), 110);

    // an hour between epochs now: a write isn't visible soon
    let receipt = send("orders", &orders(110..111));
    let soon = runtime.block_on(async {
        tokio::time::timeout(Duration::from_millis(300), writer.visible(receipt.epoch)).await
    });
    assert!(
        soon.is_err(),
        "visible within 300 ms despite a 1 h epoch interval"
    );
    // and back: the waiting write shows up at the next (quick) epoch
    let v3 = version(Some(&writer.snapshot().model_version()), files(&[]));
    match runtime.block_on(writer.change_model(v3)) {
        Ok(Change::Done { .. }) => {}
        other => panic!("expected an immediate change, got {other:?}"),
    }
    let soon = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), writer.visible(receipt.epoch)).await
    });
    assert!(soon.is_ok(), "visible again once the interval is back");
}

/// The retail model without its category tree: the parent column held
/// apart from the key's dictionary.
const NO_TREE: (&str, &str, &str) = (
    "model.ossie.yaml",
    "    custom_extensions:
      - vendor_name: TIDEPOOL                # a parent–child hierarchy (plan §5.6): a tree
        data: '{\"hierarchy\": {\"name\": \"tree\", \"parent\": \"parent\", \"kind\": \"level\"}}'
",
    "",
);

#[test]
fn a_hierarchy_over_a_populated_table_puts_its_parents_in_the_key_dictionary() {
    let v1 = version(None, files(&[NO_TREE]));
    let mut store = Store::with_version(v1.clone(), 8);
    let tree = r#"[{"category":"outdoor","parent":null,"level":"department"},
        {"category":"camping","parent":"outdoor","level":"aisle"},
        {"category":"tents","parent":"camping","level":"category"},
        {"category":"lost","parent":"nowhere","level":"category"}]"#;
    write(&mut store, "categories", tree);
    let before = publish(&mut store);
    let expected = rows(&before, "categories");
    let dict = |snapshot: &Snapshot, column: &str| {
        snapshot
            .table(table(&snapshot.model, "categories"))
            .and_then(|t| t.layouts.iter().find(|l| l.name == column))
            .and_then(|l| l.dict)
    };
    assert_ne!(dict(&before, "parent"), dict(&before, "category"));

    let v2 = version(Some(&v1), files(&[]));
    let migration = store.plan_migration(v2);
    assert!(migration.rows() > 0, "the parents are re-encoded");
    store.migrate(migration, Rebuilt::default());
    write(
        &mut store,
        "categories",
        r#"[{"category":"stoves","parent":"camping","level":"category"}]"#,
    );
    let after = publish(&mut store);
    assert!(dict(&after, "parent").is_some());
    assert_eq!(
        dict(&after, "parent"),
        dict(&after, "category"),
        "one dictionary"
    );
    let mut expected = expected;
    expected.push(r#"category="stoves" parent="camping" level="category""#.to_string());
    expected.sort();
    assert_eq!(rows(&after, "categories"), expected);
}
