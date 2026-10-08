//! The store against the retail example's model: writes, upserts, deletes,
//! sealing, and what each snapshot sees.

use std::path::Path;
use std::sync::Arc;

use tidepool_config::ConfigSet;
use tidepool_model::ir::{Model, TableId};
use tidepool_model::{Catalog, compile};
use tidepool_store::batch::{Shape, json_array};
use tidepool_store::store::{DictId, MAX_CHUNKS};
use tidepool_store::value::Value;
use tidepool_store::{Op, Snapshot, Store};

fn model() -> Arc<Model> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/retail");
    let (set, diagnostics) = match ConfigSet::load_dir(&dir) {
        Ok(loaded) => loaded,
        Err(err) => panic!("reading the example: {err}"),
    };
    assert!(diagnostics.is_empty());
    match compile(&set, &Catalog::default()) {
        (Some(compiled), _) => Arc::new(compiled.model),
        (None, d) => panic!("the example should compile: {d:?}"),
    }
}

fn table(model: &Model, name: &str) -> TableId {
    match model.table_named(name) {
        Some(table) => table.id,
        None => panic!("no table {name}"),
    }
}

/// Decode `json` for `name` and apply it.
fn write(store: &mut Store, name: &str, op: Op, json: &str) -> tidepool_store::Applied {
    let model = Arc::clone(store.model());
    let Some(table) = model.table_named(name) else {
        panic!("no table {name}");
    };
    let shape = if op == Op::Delete {
        Shape::Keys
    } else {
        Shape::Rows
    };
    let batch = match json_array(table, shape, json.as_bytes()) {
        Ok(batch) => batch,
        Err(rejected) => panic!("{rejected:?}"),
    };
    store.apply(table.id, op, &batch)
}

fn publish(store: &mut Store) -> Snapshot {
    match store.publish() {
        Some(snapshot) => snapshot,
        None => panic!("expected a new epoch"),
    }
}

/// The newest rows of a table, as JSON values, for easy comparison.
fn rows(snapshot: &Snapshot, name: &str, limit: usize) -> Vec<Vec<serde_json::Value>> {
    snapshot
        .newest_rows(table(&snapshot.model, name), limit)
        .into_iter()
        .map(|row| row.iter().map(Value::to_json).collect())
        .collect()
}

fn customer(id: &str, region: &str) -> String {
    format!(r#"{{"customer_id": "{id}", "name": "N{id}", "region": "{region}"}}"#)
}

fn order(id: u32, customer: &str) -> String {
    format!(
        r#"{{"order_id": "o{id}", "customer_id": "{customer}", "product_id": "p1", "status": "paid", "ordered_at": "2026-10-07T12:00:00Z", "quantity": {id}, "amount": "{id}.50"}}"#
    )
}

#[test]
fn written_rows_become_visible_at_the_next_epoch() {
    let mut store = Store::new(model());
    let before = publish(&mut store);
    write(
        &mut store,
        "orders",
        Op::Write,
        &format!("[{}]", order(1, "c1")),
    );
    assert!(
        rows(&before, "orders", 10).is_empty(),
        "an old snapshot never changes"
    );

    let after = publish(&mut store);
    assert_eq!(after.epoch, before.epoch + 1);
    assert_eq!(
        rows(&after, "orders", 10),
        [[
            serde_json::json!("o1"),
            serde_json::json!("c1"),
            serde_json::json!("p1"),
            serde_json::json!("paid"),
            serde_json::json!("2026-10-07T12:00:00Z"),
            serde_json::json!(1),
            serde_json::json!("1.50"),
            serde_json::Value::Null
        ]]
    );
}

#[test]
fn publishing_without_changes_makes_no_epoch() {
    let mut store = Store::new(model());
    publish(&mut store);
    assert!(store.publish().is_none());
}

#[test]
fn an_upsert_replaces_the_row_with_the_same_key() {
    let mut store = Store::new(model());
    write(
        &mut store,
        "customers",
        Op::Write,
        &format!("[{}, {}]", customer("c1", "north"), customer("c2", "south")),
    );
    publish(&mut store);

    let applied = write(
        &mut store,
        "customers",
        Op::Write,
        &format!("[{}]", customer("c1", "east")),
    );
    assert_eq!((applied.written, applied.replaced), (1, 1));
    let snapshot = publish(&mut store);
    let customers = table(&snapshot.model, "customers");
    assert_eq!(snapshot.table(customers).map(|t| t.live_rows), Some(2));
    let regions: Vec<serde_json::Value> = rows(&snapshot, "customers", 10)
        .into_iter()
        .map(|r| r[2].clone())
        .collect();
    assert_eq!(
        regions,
        [serde_json::json!("east"), serde_json::json!("south")]
    );
}

#[test]
fn an_upsert_reports_which_columns_changed() {
    let model = model();
    let customers = model.table_named("customers").cloned();
    let Some(customers) = customers else {
        panic!("no customers")
    };
    let region = customers.column_named("region").map(|c| c.id);
    let mut store = Store::new(Arc::clone(&model));
    write(
        &mut store,
        "customers",
        Op::Write,
        &format!("[{}]", customer("c1", "north")),
    );
    publish(&mut store);

    // same values: nothing changed, so nothing that depends on them is stale
    write(
        &mut store,
        "customers",
        Op::Write,
        &format!("[{}]", customer("c1", "north")),
    );
    let same = publish(&mut store);
    assert_eq!(
        same.table(customers.id).map(|t| t.changed.clone()),
        Some(vec![])
    );

    write(
        &mut store,
        "customers",
        Op::Write,
        &format!("[{}]", customer("c1", "west")),
    );
    let moved = publish(&mut store);
    assert_eq!(
        moved.table(customers.id).map(|t| t.changed.clone()),
        Some(region.into_iter().collect())
    );
}

#[test]
fn deletes_remove_rows_by_key_and_count_misses() {
    let mut store = Store::new(model());
    write(
        &mut store,
        "customers",
        Op::Write,
        &format!("[{}, {}]", customer("c1", "north"), customer("c2", "south")),
    );
    publish(&mut store);
    let applied = write(
        &mut store,
        "customers",
        Op::Delete,
        r#"[{"customer_id": "c1"}, {"customer_id": "nope"}]"#,
    );
    assert_eq!((applied.replaced, applied.missing), (1, 1));
    let snapshot = publish(&mut store);
    let ids: Vec<serde_json::Value> = rows(&snapshot, "customers", 10)
        .into_iter()
        .map(|r| r[0].clone())
        .collect();
    assert_eq!(ids, [serde_json::json!("c2")]);

    // a deleted key can be written again
    write(
        &mut store,
        "customers",
        Op::Write,
        &format!("[{}]", customer("c1", "east")),
    );
    let again = publish(&mut store);
    assert_eq!(rows(&again, "customers", 10).len(), 2);
}

#[test]
fn sealing_keeps_every_row_and_value() {
    let mut store = Store::with_seal_rows(model(), 10);
    for batch in 0..3 {
        let orders: Vec<String> = (0..9).map(|i| order(batch * 9 + i, "c1")).collect();
        write(
            &mut store,
            "orders",
            Op::Write,
            &format!("[{}]", orders.join(",")),
        );
        publish(&mut store);
    }
    let snapshot = publish_after(&mut store);
    let orders = table(&snapshot.model, "orders");
    let Some(table) = snapshot.table(orders) else {
        panic!("no orders")
    };
    assert!(
        table.segments.iter().any(|s| s.data.sealed),
        "two chunks of 9 rows reach the 10-row threshold"
    );
    let quantities: Vec<serde_json::Value> = rows(&snapshot, "orders", 100)
        .into_iter()
        .map(|r| r[5].clone())
        .collect();
    let expected: Vec<serde_json::Value> = (0..27).rev().map(|q| serde_json::json!(q)).collect();
    assert_eq!(quantities, expected);
}

/// Publish whatever is pending, or reuse the current state.
fn publish_after(store: &mut Store) -> Snapshot {
    write(
        store,
        "products",
        Op::Write,
        r#"[{"product_id": "p1", "name": "Bike", "category": "bikes", "list_price": "100"}]"#,
    );
    publish(store)
}

#[test]
fn a_delete_in_a_sealed_segment_leaves_older_snapshots_alone() {
    let mut store = Store::with_seal_rows(model(), 2);
    write(
        &mut store,
        "customers",
        Op::Write,
        &format!(
            "[{}, {}, {}]",
            customer("c1", "n"),
            customer("c2", "s"),
            customer("c3", "e")
        ),
    );
    let before = publish(&mut store);
    let customers = table(&before.model, "customers");
    let version = |s: &Snapshot| s.table(customers).map(|t| t.segments[0].version);
    assert!(
        before
            .table(customers)
            .is_some_and(|t| t.segments[0].data.sealed)
    );

    write(
        &mut store,
        "customers",
        Op::Delete,
        r#"[{"customer_id": "c2"}]"#,
    );
    let after = publish(&mut store);
    assert_eq!(rows(&before, "customers", 10).len(), 3);
    assert_eq!(rows(&after, "customers", 10).len(), 2);
    assert_ne!(
        version(&before),
        version(&after),
        "a changed delete bitmap is a new version"
    );
}

#[test]
fn many_small_chunks_are_sealed_together() {
    let mut store = Store::new(model());
    for i in 0..=u32::try_from(MAX_CHUNKS).unwrap_or(64) {
        write(
            &mut store,
            "orders",
            Op::Write,
            &format!("[{}]", order(i, "c1")),
        );
        publish(&mut store);
    }
    let snapshot = publish_after(&mut store);
    let orders = table(&snapshot.model, "orders");
    let segments = snapshot.table(orders).map_or(0, |t| t.segments.len());
    assert!(segments <= 2, "{segments} segments");
    assert_eq!(rows(&snapshot, "orders", 1000).len(), MAX_CHUNKS + 1);
}

#[test]
fn joined_columns_share_one_dictionary() {
    let mut store = Store::new(model());
    write(
        &mut store,
        "customers",
        Op::Write,
        &format!("[{}]", customer("c9", "north")),
    );
    write(
        &mut store,
        "orders",
        Op::Write,
        &format!("[{}]", order(1, "c9")),
    );
    let snapshot = publish(&mut store);
    let model = Arc::clone(&snapshot.model);
    let column = |table: &str, column: &str| {
        model
            .table_named(table)
            .and_then(|t| t.column_named(column))
            .and_then(|c| c.key_domain)
    };
    let domain = column("orders", "customer_id");
    assert!(domain.is_some());
    assert_eq!(domain, column("customers", "customer_id"));
    let Some(domain) = domain else { return };
    // one value, written through both tables, is one code
    assert_eq!(
        snapshot
            .dicts
            .get(&DictId::Domain(domain))
            .map(tidepool_store::dict::DictSnapshot::len),
        Some(1)
    );
}

fn order_at(id: u32, at: &str) -> String {
    format!(
        r#"{{"order_id":"o{id}","customer_id":"c1","product_id":"p1","status":"paid","ordered_at":"{at}","quantity":1,"amount":"1.00"}}"#
    )
}

#[test]
fn retention_drops_sealed_segments_older_than_the_window() {
    // orders keep 730 days of `ordered_at`
    let mut store = Store::with_seal_rows(model(), 4);
    let old: Vec<String> = (0..4)
        .map(|i| order_at(i, "2020-01-01T00:00:00Z"))
        .collect();
    write(
        &mut store,
        "orders",
        Op::Write,
        &format!("[{}]", old.join(",")),
    );
    let _ = publish(&mut store);
    let recent: Vec<String> = (4..8)
        .map(|i| order_at(i, "2026-09-01T00:00:00Z"))
        .collect();
    write(
        &mut store,
        "orders",
        Op::Write,
        &format!("[{}]", recent.join(",")),
    );
    let _ = publish(&mut store);

    let now = std::time::UNIX_EPOCH + std::time::Duration::from_hours(497_500); // 2026-10
    assert_eq!(store.expire(now), 4);
    let snapshot = publish(&mut store);
    let orders = table(&snapshot.model, "orders");
    assert_eq!(snapshot.table(orders).map(|t| t.live_rows), Some(4));
    // nothing more to do the second time
    assert_eq!(store.expire(now), 0);
}

#[test]
fn the_writer_recovers_its_data_directory() {
    use tidepool_store::{WriterConfig, spawn};
    let dir = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(err) => panic!("{err}"),
    };
    let config = WriterConfig {
        data: Some(dir.path().to_path_buf()),
        ..WriterConfig::default()
    };
    let runtime = match tokio::runtime::Builder::new_current_thread().build() {
        Ok(runtime) => runtime,
        Err(err) => panic!("{err}"),
    };
    let model = model();
    let orders = table(&model, "orders");
    {
        let (writer, thread) = match spawn(
            tidepool_store::ModelVersion::initial(Arc::clone(&model)),
            config.clone(),
        ) {
            Ok(started) => started,
            Err(err) => panic!("{err}"),
        };
        let spec = model
            .table_named("orders")
            .unwrap_or_else(|| panic!("no orders"));
        let body = format!(
            "[{},{}]",
            order_at(1, "2026-09-01T00:00:00Z"),
            order_at(2, "2026-09-01T00:00:00Z")
        );
        let batch =
            json_array(spec, Shape::Rows, body.as_bytes()).unwrap_or_else(|r| panic!("{r:?}"));
        let receipt = writer
            .apply(orders, Op::Write, batch)
            .unwrap_or_else(|_| panic!("busy"));
        // acknowledged means logged
        let receipt = runtime
            .block_on(receipt)
            .unwrap_or_else(|_| panic!("stopped"));
        assert_eq!(receipt.applied.written, 2);
        drop(writer);
        let _ = thread.join();
    }
    let (writer, _thread) = match spawn(
        tidepool_store::ModelVersion::initial(Arc::clone(&model)),
        config,
    ) {
        Ok(started) => started,
        Err(err) => panic!("{err}"),
    };
    assert_eq!(
        writer.snapshot().table(orders).map(|t| t.live_rows),
        Some(2)
    );
}
