//! Golden tests for config mistakes: each case takes the retail example,
//! breaks it in one way, and snapshots what `tidepool validate` would print.
//!
//! The snapshots (tests/snapshots/) are the spec for error quality: the
//! message, the `file:line:col` it points at, the underlined token, and the
//! help. Review changes to them like changes to UI copy. `cargo insta review`
//! after an intended change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tidepool_config::ConfigSet;
use tidepool_config::diag::render_all;
use tidepool_model::{Catalog, compile};

enum Edit {
    /// Replace exactly one occurrence of `from` with `to` in `file`.
    Replace(&'static str, &'static str, &'static str),
    Remove(&'static str),
    Add(&'static str, &'static str),
}
use Edit::{Add, Remove, Replace};

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

/// Apply the edits to the retail example and render what validation says.
fn validate(edits: &[Edit]) -> String {
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
            Remove(file) => {
                assert!(files.remove(Path::new(file)).is_some(), "no file {file}");
            }
            Add(file, text) => {
                files.insert(PathBuf::from(file), (*text).to_string());
            }
        }
    }
    let (set, mut diagnostics) = ConfigSet::from_files(files);
    let (_, more) = compile(&set, &Catalog::default());
    diagnostics.extend(more);
    diagnostics.sort(&set.files);
    render_all(&diagnostics, &set.files)
}

macro_rules! case {
    ($name:ident, [$($edit:expr),* $(,)?]) => {
        #[test]
        fn $name() {
            let rendered = validate(&[$($edit),*]);
            assert!(!rendered.starts_with("0 errors, 0 warnings"), "the edit broke nothing");
            insta::assert_snapshot!(stringify!($name), rendered);
        }
    };
}

#[test]
fn the_unbroken_example_is_clean() {
    assert_eq!(validate(&[]), "0 errors, 0 warnings\n");
}

// --- YAML and per-file shape -------------------------------------------------

case!(
    yaml_syntax_error,
    [Replace(
        "tables/orders.yaml",
        "name: orders",
        "name: orders: broken"
    )]
);
case!(
    unquoted_decimal_in_flow_mapping,
    [Replace(
        "tables/orders.yaml",
        "\"decimal(14,2)\"",
        "decimal(14,2)"
    )]
);
case!(
    unknown_column_type,
    [Replace(
        "tables/orders.yaml",
        "status:      { type: string }",
        "status:      { type: varchar }"
    )]
);
case!(
    misspelled_column_option,
    [Replace(
        "tables/customers.yaml",
        "segment:     { type: string, nullable: true }",
        "segment:     { type: string, nulable: true }"
    )]
);
case!(
    misplaced_yaml_file,
    [Add("tables/archive/old.yaml", "name: old\n")]
);

// --- tables ------------------------------------------------------------------

case!(
    table_name_differs_from_file,
    [Replace("tables/orders.yaml", "name: orders", "name: order")]
);
case!(
    upsert_without_primary_key,
    [Replace(
        "tables/customers.yaml",
        "primary_key: [customer_id]\n",
        ""
    )]
);
case!(
    append_with_primary_key,
    [Replace(
        "tables/orders.yaml",
        "mode: append\n",
        "mode: append\nprimary_key: [order_id]\n"
    )]
);
case!(
    primary_key_names_unknown_column,
    [Replace(
        "tables/products.yaml",
        "primary_key: [product_id]",
        "primary_key: [produkt_id]"
    )]
);
case!(
    primary_key_on_plain_column,
    [Replace(
        "tables/customers.yaml",
        "customer_id: { type: string }",
        "customer_id: { type: string, encoding: plain }"
    )]
);
case!(
    event_time_is_not_temporal,
    [Replace(
        "tables/orders.yaml",
        "event_time: ordered_at",
        "event_time: status"
    )]
);
case!(
    retention_without_event_time,
    [Replace(
        "tables/orders.yaml",
        "event_time: ordered_at\n",
        ""
    )]
);

// --- datasets and fields -------------------------------------------------------

case!(
    dataset_reads_unknown_table,
    [Replace(
        "model.ossie.yaml",
        "source: products",
        "source: product"
    )]
);
case!(
    field_uses_an_aggregate,
    [Replace(
        "model.ossie.yaml",
        "\"EXTRACT(YEAR FROM ordered_at)\"",
        "\"SUM(quantity)\""
    )]
);
case!(
    field_uses_unknown_column,
    [Replace(
        "model.ossie.yaml",
        "\"EXTRACT(YEAR FROM ordered_at)\"",
        "\"EXTRACT(YEAR FROM orderd_at)\""
    )]
);

// --- relationships -------------------------------------------------------------

case!(
    relationship_to_unknown_dataset,
    [Replace(
        "model.ossie.yaml",
        "to: customers                        # one side",
        "to: customer"
    )]
);
case!(
    relationship_not_to_primary_key,
    [Replace(
        "model.ossie.yaml",
        "from_columns: [customer_id]\n    to_columns: [customer_id]",
        "from_columns: [customer_id]\n    to_columns: [region]"
    )]
);
case!(
    relationship_type_mismatch,
    [Replace(
        "tables/orders.yaml",
        "customer_id: { type: string }",
        "customer_id: { type: int64, encoding: dict }"
    )]
);
case!(
    relationship_on_plain_column,
    [Replace(
        "tables/orders.yaml",
        "product_id:  { type: string }",
        "product_id:  { type: string, encoding: plain }"
    )]
);
case!(
    relationships_form_a_cycle,
    [
        Replace(
            "tables/customers.yaml",
            "  region:      { type: string }",
            "  region:      { type: string }\n  favourite:   { type: string, nullable: true }"
        ),
        Replace(
            "tables/products.yaml",
            "  category:   { type: string }",
            "  category:   { type: string }\n  owner:      { type: string, nullable: true }"
        ),
        Replace(
            "model.ossie.yaml",
            "\nmetrics:\n",
            "  - name: customer_favourite\n    from: customers\n    to: products\n    from_columns: [favourite]\n    to_columns: [product_id]\n  - name: product_owner\n    from: products\n    to: customers\n    from_columns: [owner]\n    to_columns: [customer_id]\n\nmetrics:\n"
        ),
    ]
);
case!(
    two_paths_with_no_roles,
    [
        Replace(
            "tables/products.yaml",
            "  category:   { type: string }",
            "  category:   { type: string }\n  supplier:   { type: string }"
        ),
        Add(
            "tables/suppliers.yaml",
            "name: suppliers\nmode: upsert\nprimary_key: [supplier_id]\ncolumns:\n  supplier_id: { type: string }\n"
        ),
        Replace(
            "tables/orders.yaml",
            "  status:      { type: string }",
            "  status:      { type: string }\n  supplier:    { type: string }"
        ),
        Replace(
            "model.ossie.yaml",
            "\nrelationships:\n",
            "  - name: suppliers\n    source: suppliers\n\nrelationships:\n"
        ),
        Replace(
            "model.ossie.yaml",
            "\nmetrics:\n",
            "  - name: product_supplier\n    from: products\n    to: suppliers\n    from_columns: [supplier]\n    to_columns: [supplier_id]\n  - name: order_supplier\n    from: orders\n    to: suppliers\n    from_columns: [supplier]\n    to_columns: [supplier_id]\n\nmetrics:\n"
        ),
    ]
);

// --- metrics -------------------------------------------------------------------

case!(
    metric_without_aggregate,
    [Replace(
        "model.ossie.yaml",
        "expression: \"SUM(orders.amount)\" }] }\n  - name: paid_revenue",
        "expression: \"orders.amount\" }] }\n  - name: paid_revenue"
    )]
);
case!(
    metric_with_bare_column,
    [Replace(
        "model.ossie.yaml",
        "\"COUNT(DISTINCT orders.customer_id)\"",
        "\"COUNT(DISTINCT orders.customer_id) + orders.quantity\""
    )]
);
case!(
    metric_field_typo,
    [Replace(
        "model.ossie.yaml",
        "\"COUNT(DISTINCT orders.customer_id)\"",
        "\"COUNT(DISTINCT orders.customr_id)\""
    )]
);
case!(
    metric_ambiguous_field,
    [Replace(
        "model.ossie.yaml",
        "\"COUNT(DISTINCT orders.customer_id)\"",
        "\"COUNT(DISTINCT customer_id)\""
    )]
);
case!(
    metric_spans_unrelated_tables,
    [Replace(
        "model.ossie.yaml",
        "\"COUNT(DISTINCT orders.customer_id)\"",
        "\"COUNT(DISTINCT customers.customer_id) + COUNT(products.product_id)\""
    )]
);
case!(
    count_star_without_anchor,
    [Replace(
        "model.ossie.yaml",
        "        data: '{\"table\": \"orders\"}'      # anchor table for COUNT(*)\n",
        "        data: '{}'\n"
    )]
);
case!(
    metric_syntax_error,
    [Replace(
        "model.ossie.yaml",
        "\"COUNT(DISTINCT orders.customer_id)\"",
        "\"COUNT(DISTINCT orders.customer_id\""
    )]
);
case!(
    metric_in_unreadable_dialect,
    [Replace(
        "model.ossie.yaml",
        "[{ dialect: ANSI_SQL, expression: \"COUNT(DISTINCT orders.customer_id)\" }]",
        "[{ dialect: SNOWFLAKE, expression: \"COUNT(DISTINCT orders.customer_id)\" }]"
    )]
);
case!(
    metric_uses_metric_without_derived,
    [Replace(
        "model.ossie.yaml",
        "        data: '{\"kind\": \"derived\"}'      # expression over other metrics, computed after aggregation\n",
        "        data: '{}'\n"
    )]
);
case!(
    derived_metric_typo,
    [Replace(
        "model.ossie.yaml",
        "\"revenue / order_count\"",
        "\"revenue / order_cnt\""
    )]
);
case!(
    derived_metrics_in_a_loop,
    [
        Replace(
            "model.ossie.yaml",
            "\"revenue / order_count\"",
            "\"revenue / margin\""
        ),
        Replace(
            "model.ossie.yaml",
            "\ncustom_extensions:\n  - vendor_name: TIDEPOOL\n    data: '{\"filter_direction\"",
            "  - name: margin\n    expression: { dialects: [{ dialect: ANSI_SQL, expression: \"avg_order_value * 2\" }] }\n    custom_extensions:\n      - vendor_name: TIDEPOOL\n        data: '{\"kind\": \"derived\"}'\n\ncustom_extensions:\n  - vendor_name: TIDEPOOL\n    data: '{\"filter_direction\""
        ),
    ]
);
case!(
    extension_with_unknown_key,
    [Replace(
        "model.ossie.yaml",
        "'{\"ignore_selection\": [\"customers.region\"]}'",
        "'{\"ignore_selecton\": [\"customers.region\"]}'"
    )]
);
case!(
    extension_with_invalid_json,
    [Replace(
        "model.ossie.yaml",
        "'{\"ignore_selection\": [\"customers.region\"]}'",
        "'{ignore_selection: [customers.region]}'"
    )]
);
case!(
    time_offset_on_non_time_field,
    [Replace(
        "model.ossie.yaml",
        "\"time_field\": \"orders.ordered_at\"",
        "\"time_field\": \"orders.status\""
    )]
);

// --- dashboards ------------------------------------------------------------------

case!(
    dashboard_unknown_metric,
    [Replace(
        "dashboards/sales.yaml",
        "metrics: [revenue]\n    sort",
        "metrics: [revnue]\n    sort"
    )]
);
case!(
    dashboard_unknown_field,
    [Replace(
        "dashboards/sales.yaml",
        "  - field: products.category\n",
        "  - field: products.categry\n"
    )]
);
case!(
    dashboard_dimension_unreachable,
    [
        Replace(
            "model.ossie.yaml",
            "\ncustom_extensions:\n  - vendor_name: TIDEPOOL\n    data: '{\"filter_direction\"",
            "  - name: product_count\n    expression: { dialects: [{ dialect: ANSI_SQL, expression: \"COUNT(products.product_id)\" }] }\n\ncustom_extensions:\n  - vendor_name: TIDEPOOL\n    data: '{\"filter_direction\""
        ),
        Replace(
            "dashboards/sales.yaml",
            "metrics: [revenue]\n    sort: { by: revenue",
            "metrics: [product_count]\n    sort: { by: product_count"
        ),
    ]
);
case!(
    dashboard_bar_with_two_dimensions,
    [Replace(
        "dashboards/sales.yaml",
        "dimensions: [customers.region]",
        "dimensions: [customers.region, customers.segment]"
    )]
);
case!(
    dashboard_brush_on_non_time_axis,
    [Replace(
        "dashboards/sales.yaml",
        "dimensions: [{ field: orders.ordered_at, grain: day }]",
        "dimensions: [orders.status]"
    )]
);
case!(
    dashboard_sort_by_unshown_metric,
    [Replace(
        "dashboards/sales.yaml",
        "sort: { by: revenue, desc: true }\n    on_click",
        "sort: { by: order_count, desc: true }\n    on_click"
    )]
);
case!(
    dashboard_visuals_overlap,
    [Replace(
        "dashboards/sales.yaml",
        "at: { x: 3, y: 0, w: 5, h: 4 }",
        "at: { x: 2, y: 0, w: 5, h: 4 }"
    )]
);
case!(
    dashboard_past_the_grid,
    [Replace(
        "dashboards/sales.yaml",
        "at: { x: 8, y: 0, w: 4, h: 4 }",
        "at: { x: 9, y: 0, w: 4, h: 4 }"
    )]
);
case!(
    dashboard_grain_on_non_time_field,
    [Replace(
        "dashboards/sales.yaml",
        "dimensions: [customers.region]",
        "dimensions: [{ field: customers.region, grain: month }]"
    )]
);
case!(dashboards_without_a_model, [Remove("model.ossie.yaml")]);
