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
    metric_format_unknown_word,
    [Replace(
        "model.ossie.yaml",
        "data: '{\"table\": \"orders\"}'",
        "data: '{\"table\": \"orders\", \"format\": \"int compakt\"}'"
    )]
);
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
        "\"time_offset\": \"-1y\", \"time_field\": \"orders.ordered_at\"",
        "\"time_offset\": \"-1y\", \"time_field\": \"orders.status\""
    )]
);
case!(
    median_of_a_string,
    [Replace(
        "model.ossie.yaml",
        "\"MEDIAN(orders.amount)\"",
        "\"MEDIAN(orders.status)\""
    )]
);
case!(
    within_group_on_a_sum,
    [Replace(
        "model.ossie.yaml",
        "\"PERCENTILE_CONT(0.9) WITHIN GROUP (ORDER BY orders.amount)\"",
        "\"SUM(orders.amount) WITHIN GROUP (ORDER BY orders.amount)\""
    )]
);
case!(
    time_window_in_minutes_or_months,
    [Replace(
        "model.ossie.yaml",
        "\"time_window\": \"last:7d\"",
        "\"time_window\": \"last:7m\""
    )]
);
case!(
    time_window_without_a_field,
    [Replace(
        "model.ossie.yaml",
        "'{\"time_window\": \"ytd\", \"time_field\": \"orders.ordered_at\"}'",
        "'{\"time_window\": \"ytd\"}'"
    )]
);
case!(
    pie_of_two_dimensions,
    [Replace(
        "dashboards/sales.yaml",
        "    dimensions: [products.category]\n    metrics: [revenue]\n    on_click: select\n",
        "    dimensions: [products.category, customers.region]\n    metrics: [revenue]\n    on_click: select\n"
    )]
);
case!(
    pie_of_too_many_slices,
    [Replace(
        "dashboards/sales.yaml",
        "    dimensions: [products.category]\n    metrics: [revenue]\n    on_click: select\n",
        "    dimensions: [products.category]\n    metrics: [revenue]\n    on_click: select\n    limit: 20\n"
    )]
);
case!(
    scatter_of_one_metric,
    [Replace(
        "dashboards/sales.yaml",
        "    dimensions: [products.name]\n    metrics: [revenue, order_count]\n",
        "    dimensions: [products.name]\n    metrics: [revenue]\n"
    )]
);
case!(
    heatmap_of_one_dimension,
    [Replace(
        "dashboards/sales.yaml",
        "    dimensions: [customers.region, products.category]\n    metrics: [revenue]\n    on_click: select\n    at: { x: 0, y: 12",
        "    dimensions: [customers.region]\n    metrics: [revenue]\n    on_click: select\n    at: { x: 0, y: 12"
    )]
);
case!(
    total_of_an_unknown_field,
    [Replace(
        "model.ossie.yaml",
        "'{\"total\": [\"*\"]}'",
        "'{\"total\": [\"customers.regoin\"]}'"
    )]
);
case!(
    total_on_a_derived_metric,
    [Replace(
        "model.ossie.yaml",
        "'{\"kind\": \"derived\", \"format\": \"percent decimals:1\"}'",
        "'{\"kind\": \"derived\", \"total\": [\"*\"]}'"
    )]
);

case!(
    metric_sums_a_string,
    [Replace(
        "model.ossie.yaml",
        "\"COUNT(DISTINCT orders.customer_id)\"",
        "\"SUM(customers.region)\""
    )]
);
case!(
    field_adds_a_number_to_a_string,
    [Replace(
        "model.ossie.yaml",
        "\"EXTRACT(YEAR FROM ordered_at)\"",
        "\"status + 1\""
    )]
);
case!(
    derived_metric_mixes_a_string_metric,
    [
        Replace(
            "model.ossie.yaml",
            "\"COUNT(DISTINCT orders.customer_id)\"",
            "\"MAX(orders.status)\""
        ),
        Replace(
            "model.ossie.yaml",
            "\"revenue / order_count\"",
            "\"revenue / customer_count\""
        ),
    ]
);

// --- dashboards ------------------------------------------------------------------

case!(
    dashboard_format_for_a_metric_not_shown,
    [Replace(
        "dashboards/sales.yaml",
        "format: { revenue: compact }",
        "format: { revenu: compact }"
    )]
);
case!(
    dashboard_format_unknown_word,
    [Replace(
        "dashboards/sales.yaml",
        "format: { revenue: compact }",
        "format: { revenue: compact millions }"
    )]
);
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
case!(
    dashboard_hour_grain_on_a_date,
    [Replace(
        "tables/orders.yaml",
        "ordered_at:  { type: timestamptz }",
        "ordered_at:  { type: date }"
    )]
);
case!(
    field_hour_grain_on_a_date,
    [
        Replace(
            "tables/orders.yaml",
            "ordered_at:  { type: timestamptz }",
            "ordered_at:  { type: date }"
        ),
        Replace(
            "model.ossie.yaml",
            "datatype: DateTimeTz\n        dimension: { is_time: true }\n",
            "dimension: { is_time: true }\n        custom_extensions:\n          - vendor_name: TIDEPOOL\n            data: '{\"grain\": [\"hour\", \"day\"]}'\n"
        ),
        Replace("dashboards/sales_recent.yaml", "grain: hour", "grain: day"),
    ]
);
case!(dashboards_without_a_model, [Remove("model.ossie.yaml")]);
case!(
    dashboard_default_on_a_list,
    [Replace(
        "dashboards/sales_recent.yaml",
        "  - field: customers.region\n",
        "  - field: customers.region\n    default: { last: 7, unit: day }\n"
    )]
);
case!(
    dashboard_default_of_no_periods,
    [Replace(
        "dashboards/sales_recent.yaml",
        "default: { last: 7, unit: day }",
        "default: { last: 0, unit: day }"
    )]
);
case!(
    dashboard_default_in_hours_on_a_date,
    [
        Replace(
            "model.ossie.yaml",
            "      - name: status\n",
            "      - name: order_day\n        expression: { dialects: [{ dialect: ANSI_SQL, expression: \"DATE_TRUNC('day', ordered_at)\" }] }\n        dimension: { is_time: true }\n      - name: status\n"
        ),
        Replace(
            "dashboards/sales_recent.yaml",
            "  - field: orders.ordered_at\n    widget: date_range\n    default: { last: 7, unit: day }",
            "  - field: orders.order_day\n    widget: date_range\n    default: { last: 24, unit: hour }"
        )
    ]
);
case!(
    dashboard_default_on_a_number_marked_as_time,
    [
        Replace(
            "model.ossie.yaml",
            "        datatype: Integer\n",
            "        datatype: Integer\n        dimension: { is_time: true }\n"
        ),
        Replace(
            "dashboards/sales_recent.yaml",
            "  - field: orders.ordered_at\n    widget: date_range\n    default:",
            "  - field: orders.order_year\n    widget: date_range\n    default:"
        )
    ]
);
case!(
    both_ways_with_a_loop,
    [
        Replace(
            "model.ossie.yaml",
            r#"data: '{"filter_direction": "one_to_many"}'"#,
            r#"data: '{"filter_direction": "both"}'"#
        ),
        Replace(
            "model.ossie.yaml",
            "  - name: order_product\n",
            "  - name: billed_to\n    from: orders\n    to: customers\n    from_columns: [customer_id]\n    to_columns: [customer_id]\n    custom_extensions:\n      - vendor_name: TIDEPOOL\n        data: '{\"role\": \"billing\"}'\n  - name: order_product\n"
        ),
    ]
);

// --- hierarchies and filter measures ---------------------------------------------

const TREE: &str =
    r#"data: '{"hierarchy": {"name": "tree", "parent": "parent", "kind": "level"}}'"#;

case!(
    hierarchy_named_like_a_column,
    [
        // the dashboard names the hierarchy by its old name
        Replace(
            "dashboards/sales.yaml",
            "  - field: categories.tree               # a hierarchy: shown as a tree\n",
            ""
        ),
        Replace(
            "model.ossie.yaml",
            TREE,
            r#"data: '{"hierarchy": {"name": "parent", "parent": "parent", "kind": "level"}}'"#
        ),
    ]
);
case!(
    hierarchy_name_not_an_identifier,
    [
        // the dashboard names the hierarchy by its old name
        Replace(
            "dashboards/sales.yaml",
            "  - field: categories.tree               # a hierarchy: shown as a tree\n",
            ""
        ),
        Replace(
            "model.ossie.yaml",
            TREE,
            r#"data: '{"hierarchy": {"name": "the tree", "parent": "parent", "kind": "level"}}'"#
        ),
    ]
);
case!(
    hierarchy_unknown_parent,
    [Replace(
        "model.ossie.yaml",
        TREE,
        r#"data: '{"hierarchy": {"name": "tree", "parent": "parnet", "kind": "level"}}'"#
    )]
);
case!(
    hierarchy_unknown_kind,
    [Replace(
        "model.ossie.yaml",
        TREE,
        r#"data: '{"hierarchy": {"name": "tree", "parent": "parent", "kind": "levle"}}'"#
    )]
);
case!(
    hierarchy_parent_is_the_key,
    [Replace(
        "model.ossie.yaml",
        TREE,
        r#"data: '{"hierarchy": {"name": "tree", "parent": "category", "kind": "level"}}'"#
    )]
);
case!(
    hierarchy_parent_of_another_type,
    [Replace(
        "tables/categories.yaml",
        "  parent:   { type: string, nullable: true }",
        "  parent:   { type: int64, nullable: true }"
    )]
);
case!(
    hierarchy_parent_plain,
    [Replace(
        "tables/categories.yaml",
        "  parent:   { type: string, nullable: true }",
        "  parent:   { type: string, nullable: true, encoding: plain }"
    )]
);
case!(
    hierarchy_on_an_append_table,
    [Replace(
        "model.ossie.yaml",
        "      - name: status\n        expression: { dialects: [{ dialect: ANSI_SQL, expression: status }] }\n",
        "      - name: status\n        expression: { dialects: [{ dialect: ANSI_SQL, expression: status }] }\n    custom_extensions:\n      - vendor_name: TIDEPOOL\n        data: '{\"hierarchy\": {\"name\": \"tree\", \"parent\": \"status\"}}'\n"
    )]
);
case!(
    hierarchy_over_a_two_column_key,
    [
        Add(
            "tables/shelves.yaml",
            "name: shelves\nmode: upsert\nprimary_key: [aisle, shelf]\ncolumns:\n  aisle: { type: string }\n  shelf: { type: string }\n  above: { type: string, nullable: true }\n"
        ),
        Replace(
            "model.ossie.yaml",
            "\nrelationships:\n",
            "  - name: shelves\n    source: shelves\n    custom_extensions:\n      - vendor_name: TIDEPOOL\n        data: '{\"hierarchy\": {\"name\": \"stack\", \"parent\": \"above\"}}'\n\nrelationships:\n"
        ),
    ]
);
case!(
    self_relationship,
    [Replace(
        "model.ossie.yaml",
        "  - name: product_category\n",
        "  - name: category_parent\n    from: categories\n    to: categories\n    from_columns: [parent]\n    to_columns: [category]\n  - name: product_category\n"
    )]
);
case!(
    tree_on_a_field_that_is_no_hierarchy,
    [Replace(
        "dashboards/sales.yaml",
        "  - field: products.category\n",
        "  - field: products.category\n    widget: tree\n"
    )]
);
case!(
    filter_measures_unknown_metric,
    [Replace(
        "dashboards/sales.yaml",
        "    measure: revenue ",
        "    measure: revenu "
    )]
);
case!(
    filter_measure_on_a_date_range,
    [Replace(
        "dashboards/sales.yaml",
        "  - field: orders.ordered_at\n    widget: date_range\n",
        "  - field: orders.ordered_at\n    widget: date_range\n    measure: revenue\n"
    )]
);
case!(
    filter_measure_unreachable,
    [
        Replace(
            "model.ossie.yaml",
            "\ncustom_extensions:\n  - vendor_name: TIDEPOOL\n    data: '{\"filter_direction\"",
            "  - name: product_count\n    expression: { dialects: [{ dialect: ANSI_SQL, expression: \"COUNT(products.product_id)\" }] }\n\ncustom_extensions:\n  - vendor_name: TIDEPOOL\n    data: '{\"filter_direction\""
        ),
        Replace(
            "dashboards/sales.yaml",
            "    measure: revenue ",
            "    measure: product_count "
        ),
    ]
);
case!(
    dashboard_kpi_without_metrics,
    [Replace(
        "dashboards/sales_recent.yaml",
        "    metrics: [order_count]\n",
        ""
    )]
);
case!(
    dashboard_table_shows_nothing,
    [Replace(
        "dashboards/sales.yaml",
        "    dimensions: [products.name, products.category]\n    metrics: [revenue, revenue_share, order_count, avg_order_value]\n    sort: { by: revenue, desc: true }\n",
        ""
    )]
);
case!(
    dashboard_filter_of_two_dimensions,
    [Replace(
        "dashboards/sales.yaml",
        "    type: table\n    dimensions: [products.name, products.category]\n    metrics: [revenue, revenue_share, order_count, avg_order_value]\n    sort: { by: revenue, desc: true }\n",
        "    type: filter\n    dimensions: [products.name, products.category]\n"
    )]
);
case!(
    dashboard_filter_with_metrics,
    [Replace(
        "dashboards/sales.yaml",
        "    type: table\n    dimensions: [products.name, products.category]\n    metrics: [revenue, revenue_share, order_count, avg_order_value]\n    sort: { by: revenue, desc: true }\n",
        "    type: filter\n    dimensions: [products.category]\n    metrics: [revenue]\n"
    )]
);
case!(
    dashboard_filter_with_a_grain,
    [Replace(
        "dashboards/sales.yaml",
        "    type: line\n    dimensions: [{ field: orders.ordered_at, grain: day }]\n    metrics: [revenue, order_count]\n    on_brush: select_range\n",
        "    type: filter\n    dimensions: [{ field: orders.ordered_at, grain: day }]\n"
    )]
);
case!(
    dashboard_table_of_unrelated_dimensions,
    [
        Add(
            "tables/stores.yaml",
            "name: stores\nmode: upsert\nprimary_key: [store_id]\ncolumns:\n  store_id: { type: string }\n  city: { type: string }\n"
        ),
        Replace(
            "model.ossie.yaml",
            "\nrelationships:\n",
            "  - name: stores\n    source: stores\n    primary_key: [store_id]\n    fields:\n      - name: city\n        expression: { dialects: [{ dialect: ANSI_SQL, expression: city }] }\n\nrelationships:\n"
        ),
        Replace(
            "dashboards/sales.yaml",
            "    dimensions: [products.name, products.category]\n    metrics: [revenue, revenue_share, order_count, avg_order_value]\n    sort: { by: revenue, desc: true }\n",
            "    dimensions: [products.category, stores.city]\n"
        ),
    ]
);

// --- tidepool.yaml: listeners and ingest credentials (plan §9.2) -------------

/// `tidepool.yaml` with these lines appended under `auth:`.
fn auth(lines: &'static str) -> Edit {
    Replace("tidepool.yaml", "durability: fsync\n", lines)
}

case!(
    listen_without_an_address,
    [Replace(
        "tidepool.yaml",
        "listen: 127.0.0.1:7070",
        "listen: {}"
    )]
);
case!(
    ingest_token_env_and_credentials,
    [auth(
        "durability: fsync\nauth:\n  ingest_token_env: FEED_TOKEN\n  ingest:\n    - name: kayak\n      token_env: KAYAK_TOKEN\n"
    )]
);
case!(
    credential_defined_twice,
    [auth(
        "durability: fsync\nauth:\n  ingest:\n    - name: kayak\n      token_env: KAYAK_TOKEN\n    - name: kayak\n      token_env: OTHER_TOKEN\n"
    )]
);
case!(
    credential_name_with_a_space,
    [auth(
        "durability: fsync\nauth:\n  ingest:\n    - name: field sensors\n      token_env: SENSOR_TOKEN\n"
    )]
);
case!(
    credential_without_a_token,
    [auth(
        "durability: fsync\nauth:\n  ingest:\n    - name: kayak\n"
    )]
);
case!(
    credential_with_a_token_and_none,
    [auth(
        "durability: fsync\nauth:\n  ingest:\n    - name: beacon\n      token_env: BEACON_TOKEN\n      token: none\n      public: true\n"
    )]
);
case!(
    credential_token_env_names_nothing,
    [auth(
        "durability: fsync\nauth:\n  ingest:\n    - name: kayak\n      token_env: []\n"
    )]
);
case!(
    credential_without_a_token_not_public,
    [auth(
        "durability: fsync\nauth:\n  ingest:\n    - name: beacon\n      token: none\n      tables: [orders]\n"
    )]
);
case!(
    credential_without_a_token_deletes,
    [auth(
        "durability: fsync\nauth:\n  ingest:\n    - name: beacon\n      token: none\n      public: true\n      tables: [customers]\n      operations: [append, delete]\n"
    )]
);
case!(
    public_credential_upserts,
    [auth(
        "durability: fsync\nauth:\n  ingest:\n    - name: crm\n      token_env: CRM_TOKEN\n      public: true\n      tables: [customers]\n      operations: [upsert]\n"
    )]
);
case!(
    credential_may_do_nothing,
    [auth(
        "durability: fsync\nauth:\n  ingest:\n    - name: kayak\n      token_env: KAYAK_TOKEN\n      operations: []\n"
    )]
);
case!(
    credential_grants_no_tables,
    [auth(
        "durability: fsync\nauth:\n  ingest:\n    - name: kayak\n      token_env: KAYAK_TOKEN\n      tables: []\n"
    )]
);
case!(
    credential_grants_an_unknown_table,
    [auth(
        "durability: fsync\nauth:\n  ingest:\n    - name: shop\n      token_env: SHOP_TOKEN\n      tables: [order]\n"
    )]
);
case!(
    two_credentials_without_a_token_on_one_table,
    [auth(
        "durability: fsync\nauth:\n  ingest:\n    - name: web\n      token: none\n      public: true\n      tables: [orders]\n    - name: app\n      token: none\n      public: true\n      tables: [orders, customers]\n"
    )]
);
case!(
    credential_origin_with_a_path,
    [auth(
        "durability: fsync\nauth:\n  ingest:\n    - name: web\n      token_env: WEB_TOKEN\n      public: true\n      tables: [orders]\n      origins: [\"https://shop.example.com/checkout\"]\n"
    )]
);
case!(
    credential_batch_rows_zero,
    [auth(
        "durability: fsync\nauth:\n  ingest:\n    - name: web\n      token_env: WEB_TOKEN\n      tables: [orders]\n      limits: { batch_rows: 0 }\n"
    )]
);
case!(
    filled_column_of_the_wrong_type,
    [Replace(
        "tables/orders.yaml",
        "  note:        { type: string, nullable: true, encoding: plain }\n",
        "  note:        { type: string, nullable: true, encoding: plain }\n  received_at: { type: string, filled: received }\n"
    )]
);
case!(
    filled_column_in_the_primary_key,
    [Replace(
        "tables/customers.yaml",
        "  customer_id: { type: string }",
        "  customer_id: { type: string, filled: credential }"
    )]
);

// --- text search and rows (plan §5.7) ----------------------------------------

case!(
    search_on_a_number,
    [Replace(
        "dashboards/shipping_notes.yaml",
        "  - field: orders.status\n",
        "  - field: orders.quantity\n    widget: search\n"
    )]
);
case!(
    rows_with_metrics,
    [Replace(
        "dashboards/shipping_notes.yaml",
        "    limit: 50\n",
        "    metrics: [order_count]\n    limit: 50\n"
    )]
);
case!(
    rows_without_columns,
    [Replace(
        "dashboards/shipping_notes.yaml",
        "    dimensions: [orders.ordered_at, customers.name, orders.status, orders.amount, orders.note]\n",
        ""
    )]
);
case!(
    rows_over_the_limit,
    [Replace(
        "dashboards/shipping_notes.yaml",
        "    limit: 50\n",
        "    limit: 5000\n"
    )]
);
case!(
    rows_with_a_grain,
    [Replace(
        "dashboards/shipping_notes.yaml",
        "[orders.ordered_at, customers.name,",
        "[{ field: orders.ordered_at, grain: day }, customers.name,"
    )]
);
case!(
    rows_with_a_sort,
    [Replace(
        "dashboards/shipping_notes.yaml",
        "    limit: 50\n",
        "    sort: { by: orders.amount, desc: true }\n    limit: 50\n"
    )]
);
