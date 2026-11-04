//! Golden tests for plans: the retail example as the base, one change on top,
//! and a snapshot of what `tidepool plan` prints. The snapshots are the spec
//! for change classes (plan §8.2).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tidepool_config::ConfigSet;
use tidepool_config::diag::render_all;
use tidepool_model::{Catalog, Compiled, compile, plan};

fn retail() -> BTreeMap<PathBuf, String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/retail");
    let mut files = BTreeMap::new();
    for sub in ["", "tables", "dashboards"] {
        let Ok(entries) = std::fs::read_dir(dir.join(sub)) else {
            panic!("reading {}", dir.join(sub).display());
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "yaml") {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    panic!("reading {}", path.display());
                };
                files.insert(Path::new(sub).join(entry.file_name()), text);
            }
        }
    }
    files
}

fn build(files: BTreeMap<PathBuf, String>, previous: &Catalog) -> Compiled {
    let (set, mut diagnostics) = ConfigSet::from_files(files);
    let (compiled, more) = compile(&set, previous);
    diagnostics.extend(more);
    match compiled {
        Some(compiled) if diagnostics.is_empty() => compiled,
        _ => panic!(
            "config should compile cleanly:\n{}",
            render_all(&diagnostics, &set.files)
        ),
    }
}

enum Edit {
    Replace(&'static str, &'static str, &'static str),
    Add(&'static str, &'static str),
    Remove(&'static str),
}
use Edit::{Add, Remove, Replace};

fn edited<'a>(edits: impl IntoIterator<Item = &'a Edit>) -> BTreeMap<PathBuf, String> {
    let mut files = retail();
    for edit in edits {
        match edit {
            Replace(file, from, to) => {
                let Some(text) = files.get_mut(Path::new(file)) else {
                    panic!("no file {file}");
                };
                assert_eq!(
                    text.matches(from).count(),
                    1,
                    "`{from}` should occur exactly once in {file}"
                );
                *text = text.replacen(from, to, 1);
            }
            Add(file, text) => {
                files.insert(PathBuf::from(file), (*text).to_string());
            }
            Remove(file) => {
                assert!(files.remove(Path::new(file)).is_some(), "no file {file}");
            }
        }
    }
    files
}

/// The plan from the retail example with `base` applied to the retail example
/// with `base` and then `edits` applied.
fn plan_between(base: &[Edit], edits: &[Edit]) -> String {
    let before = build(edited(base), &Catalog::default());
    let after = build(edited(base.iter().chain(edits)), &before.catalog);
    plan::diff(&before.model, &after.model).render()
}

fn plan_for(edits: &[Edit]) -> String {
    plan_between(&[], edits)
}

macro_rules! scenario {
    ($name:ident, [$($edit:expr),* $(,)?]) => {
        #[test]
        fn $name() {
            insta::assert_snapshot!(stringify!($name), plan_for(&[$($edit),*]));
        }
    };
    ($name:ident, base: [$($base:expr),* $(,)?], [$($edit:expr),* $(,)?]) => {
        #[test]
        fn $name() {
            insta::assert_snapshot!(stringify!($name), plan_between(&[$($base),*], &[$($edit),*]));
        }
    };
}

#[test]
fn an_unchanged_config_plans_nothing() {
    assert_eq!(plan_for(&[]), "no changes\n");
}

#[test]
fn compiling_from_scratch_plans_everything_as_added_and_safe() {
    let empty = build(BTreeMap::new(), &Catalog::default());
    let retail = build(retail(), &empty.catalog);
    let plan = plan::diff(&empty.model, &retail.model);
    assert!(!plan.is_empty());
    // everything is new, except settings, which always exist (with defaults)
    assert!(
        plan.changes
            .iter()
            .all(|c| c.op == plan::Op::Add || c.kind == "setting"),
        "{}",
        plan.render()
    );
    assert_eq!(plan.class(), Some(plan::ChangeClass::Safe));
}

const RETURNS: &str = "name: returns\nmode: append\ncolumns:\n  order_id:   { type: string }\n  returned_at: { type: timestamptz }\n  reason:     { type: string }\n";

scenario!(add_a_table, [Add("tables/returns.yaml", RETURNS)]);
scenario!(
    add_a_nullable_column,
    [Replace(
        "tables/orders.yaml",
        "  note:",
        "  channel:     { type: string, nullable: true }\n  note:"
    )]
);
scenario!(
    add_a_required_column,
    [Replace(
        "tables/orders.yaml",
        "  note:",
        "  channel:     { type: string }\n  note:"
    )]
);
scenario!(
    widen_a_column,
    [Replace(
        "tables/orders.yaml",
        "quantity:    { type: int32 }",
        "quantity:    { type: int64 }"
    )]
);
scenario!(
    narrow_a_column,
    [Replace(
        "tables/orders.yaml",
        "\"decimal(14,2)\"",
        "\"decimal(10,2)\""
    )]
);
scenario!(
    make_a_column_required,
    [Replace(
        "tables/customers.yaml",
        "segment:     { type: string, nullable: true }",
        "segment:     { type: string }"
    )]
);
scenario!(
    change_an_encoding,
    [Replace(
        "tables/orders.yaml",
        "status:      { type: string }",
        "status:      { type: string, encoding: plain }"
    )]
);
// the notes dashboard goes with the column it searches
scenario!(
    drop_a_column,
    [
        Replace(
            "tables/orders.yaml",
            "  note:        { type: string, nullable: true, encoding: plain }\n",
            ""
        ),
        Remove("dashboards/shipping_notes.yaml"),
    ]
);
scenario!(
    rename_a_column,
    [
        Replace(
            "tables/orders.yaml",
            "  note:        { type: string, nullable: true, encoding: plain }",
            "  comment:     { type: string, nullable: true, encoding: plain, renamed_from: note }"
        ),
        Replace(
            "dashboards/shipping_notes.yaml",
            "field: orders.note",
            "field: orders.comment"
        ),
        Replace(
            "dashboards/shipping_notes.yaml",
            ", orders.note]",
            ", orders.comment]"
        ),
    ]
);
scenario!(
    rename_a_column_without_saying_so,
    [
        Replace(
            "tables/orders.yaml",
            "  note:        { type: string, nullable: true, encoding: plain }",
            "  comment:     { type: string, nullable: true, encoding: plain }"
        ),
        Replace(
            "dashboards/shipping_notes.yaml",
            "field: orders.note",
            "field: orders.comment"
        ),
        Replace(
            "dashboards/shipping_notes.yaml",
            ", orders.note]",
            ", orders.comment]"
        ),
    ]
);
const STORES: &str = "name: stores\nmode: upsert\nprimary_key: [store_id]\ncolumns:\n  store_id: { type: string }\n  region:   { type: string }\n";
scenario!(
    change_a_primary_key,
    base: [Add("tables/stores.yaml", STORES)],
    [Replace("tables/stores.yaml", "primary_key: [store_id]", "primary_key: [store_id, region]")]
);
scenario!(
    change_retention,
    [Replace(
        "tables/orders.yaml",
        "retention: 730d",
        "retention: 365d"
    )]
);
scenario!(
    change_a_metric,
    [Replace(
        "model.ossie.yaml",
        "\"COUNT(DISTINCT orders.customer_id)\"",
        "\"COUNT(DISTINCT orders.order_id)\""
    )]
);
scenario!(
    change_server_settings,
    [
        Replace(
            "tidepool.yaml",
            "epoch_interval: 200ms",
            "epoch_interval: 500ms\nlisten: 0.0.0.0:7070"
        ),
        Replace("tidepool.yaml", "listen: 127.0.0.1:7070\n", "")
    ]
);
scenario!(remove_a_dashboard, [Remove("dashboards/sales.yaml")]);
scenario!(
    join_two_tables_that_hold_data,
    base: [
        Replace("tables/products.yaml", "  category:   { type: string }", "  category:   { type: string }\n  supplier:   { type: string, nullable: true }"),
        Add("tables/suppliers.yaml", "name: suppliers\nmode: upsert\nprimary_key: [supplier_id]\ncolumns:\n  supplier_id: { type: string }\n"),
        Replace("model.ossie.yaml", "\nrelationships:\n", "  - name: suppliers\n    source: suppliers\n\nrelationships:\n"),
    ],
    [Replace("model.ossie.yaml", "\nmetrics:\n", "  - name: product_supplier\n    from: products\n    to: suppliers\n    from_columns: [supplier]\n    to_columns: [supplier_id]\n\nmetrics:\n")]
);
scenario!(
    join_a_new_column,
    [
        Replace(
            "tables/products.yaml",
            "  category:   { type: string }",
            "  category:   { type: string }\n  supplier:   { type: string, nullable: true }"
        ),
        Add(
            "tables/suppliers.yaml",
            "name: suppliers\nmode: upsert\nprimary_key: [supplier_id]\ncolumns:\n  supplier_id: { type: string }\n"
        ),
        Replace(
            "model.ossie.yaml",
            "\nrelationships:\n",
            "  - name: suppliers\n    source: suppliers\n\nrelationships:\n"
        ),
        Replace(
            "model.ossie.yaml",
            "\nmetrics:\n",
            "  - name: product_supplier\n    from: products\n    to: suppliers\n    from_columns: [supplier]\n    to_columns: [supplier_id]\n\nmetrics:\n"
        ),
    ]
);
scenario!(
    selections_flow_both_ways,
    [Replace(
        "model.ossie.yaml",
        r#"data: '{"filter_direction": "one_to_many"}'"#,
        r#"data: '{"filter_direction": "both"}'"#
    )]
);

const TREE_EXTENSION: &str = "    primary_key: [category]\n    custom_extensions:\n      - vendor_name: TIDEPOOL                # a parent–child hierarchy (plan §5.6): a tree\n        data: '{\"hierarchy\": {\"name\": \"tree\", \"parent\": \"parent\", \"kind\": \"level\"}}'\n";
const TREE_FILTER: &str =
    "  - field: categories.tree               # a hierarchy: shown as a tree\n";

scenario!(
    add_a_hierarchy_over_a_column_with_data,
    base: [
        Replace("model.ossie.yaml", TREE_EXTENSION, "    primary_key: [category]\n"),
        Replace("dashboards/sales.yaml", TREE_FILTER, ""),
    ],
    [
        Replace("model.ossie.yaml", "    primary_key: [category]\n", TREE_EXTENSION),
    ]
);
scenario!(
    add_a_hierarchy_with_a_new_parent_column,
    base: [
        Replace("model.ossie.yaml", TREE_EXTENSION, "    primary_key: [category]\n"),
        Replace("dashboards/sales.yaml", TREE_FILTER, ""),
        Replace("tables/categories.yaml", "  parent:   { type: string, nullable: true }   # the parent category; NULL at the top\n", ""),
    ],
    [
        Replace("tables/categories.yaml", "  level:", "  parent:   { type: string, nullable: true }\n  level:"),
        Replace("model.ossie.yaml", "    primary_key: [category]\n", TREE_EXTENSION),
    ]
);
scenario!(
    remove_a_hierarchy,
    [
        Replace(
            "model.ossie.yaml",
            TREE_EXTENSION,
            "    primary_key: [category]\n"
        ),
        Replace("dashboards/sales.yaml", TREE_FILTER, ""),
    ]
);
scenario!(
    measure_a_filter,
    [Replace(
        "dashboards/sales.yaml",
        "  - field: products.category\n",
        "  - field: products.category\n    measure: order_count\n"
    )]
);

// --- who can reach what (plan §9.2): class security --------------------------

const KAYAK: &str = "auth:\n  ingest:\n    - name: kayak\n      token_env: KAYAK_TOKEN\n";

scenario!(
    move_from_one_ingest_token_to_credentials,
    [Replace(
        "tidepool.yaml",
        "durability: fsync\n",
        "durability: fsync\nauth:\n  ingest:\n    - name: kayak\n      token_env: KAYAK_TOKEN\n    - name: sensors\n      token_env: SENSOR_TOKEN\n      tables: [orders]\n      operations: [append]\n"
    )]
);
scenario!(
    make_a_table_writable_from_the_internet,
    base: [Replace("tidepool.yaml", "durability: fsync\n", KAYAK)],
    [Replace(
        "tidepool.yaml",
        "      token_env: KAYAK_TOKEN\n",
        "      token_env: KAYAK_TOKEN\n    - name: web\n      token: none\n      public: true\n      tables: [orders]\n      limits: { rate: 20/s }\n      origins: [\"https://shop.example.com\"]\n"
    )]
);
scenario!(
    remove_a_credential,
    base: [Replace(
        "tidepool.yaml",
        "durability: fsync\n",
        "auth:\n  ingest:\n    - name: kayak\n      token_env: KAYAK_TOKEN\n    - name: old\n      token_env: OLD_TOKEN\n"
    )],
    [Replace("tidepool.yaml", "    - name: old\n      token_env: OLD_TOKEN\n", "")]
);
scenario!(
    let_agents_apply_destructive_changes,
    [Replace(
        "tidepool.yaml",
        "durability: fsync\n",
        "durability: fsync\nagents:\n  auto_apply: [safe, rebuild, destructive]\n"
    )]
);
scenario!(
    change_the_admin_token_variable,
    [Replace(
        "tidepool.yaml",
        "durability: fsync\n",
        "durability: fsync\nauth:\n  admin_token_env: OPS_TOKEN\n"
    )]
);
scenario!(
    fill_a_column_with_the_credential,
    [Replace(
        "tables/orders.yaml",
        "  note:        { type: string, nullable: true, encoding: plain }\n",
        "  note:        { type: string, nullable: true, encoding: plain }\n  written_by:  { type: string, nullable: true, filled: credential }\n"
    )]
);
scenario!(
    start_filling_a_column,
    [Replace(
        "tables/orders.yaml",
        "  note:        { type: string, nullable: true, encoding: plain }\n",
        "  note:        { type: string, nullable: true, encoding: plain, filled: credential }\n"
    )]
);
