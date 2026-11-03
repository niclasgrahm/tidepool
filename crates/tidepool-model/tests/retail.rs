//! The example project compiles cleanly: it is the reference config, so a
//! diagnostic here is either a bug in the example or in the compiler.

use std::path::Path;

use tidepool_config::ConfigSet;
use tidepool_config::diag::render_all;
use tidepool_model::{Catalog, compile};

#[test]
fn the_retail_example_compiles_without_diagnostics() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/retail");
    let (set, mut diagnostics) = match ConfigSet::load_dir(&dir) {
        Ok(loaded) => loaded,
        Err(err) => panic!("reading {}: {err}", dir.display()),
    };
    let (compiled, more) = compile(&set, &Catalog::default());
    diagnostics.extend(more);
    assert!(
        diagnostics.is_empty(),
        "\n{}",
        render_all(&diagnostics, &set.files)
    );
    let Some(compiled) = compiled else {
        panic!("no model despite no diagnostics");
    };
    let model = compiled.model;
    assert_eq!(model.tables.len(), 4);
    assert_eq!(model.relationships.len(), 3);
    // the category tree: a hierarchy whose parent shares the key's
    // dictionary (and the products' categories with it), shown as a tree
    let Some(categories) = model.table_named("categories") else {
        panic!("no categories table");
    };
    let Some(tree) = model
        .dataset(categories.id)
        .and_then(|d| d.field("tree"))
        .and_then(tidepool_model::ir::Field::hierarchy)
    else {
        panic!("categories.tree should be a hierarchy");
    };
    assert!(
        model
            .key_domains
            .iter()
            .any(|d| d.columns.contains(&(categories.id, tree.key))
                && d.columns.contains(&(categories.id, tree.parent))),
        "the parent column shares the key's dictionary"
    );
    let widgets: Vec<_> = model
        .dashboards
        .iter()
        .flat_map(|d| &d.filters)
        .filter(|f| f.field.field == "tree")
        .map(|f| f.widget)
        .collect();
    assert_eq!(
        widgets,
        vec![tidepool_config::dashboard::FilterWidget::Tree]
    );
    assert!(
        model
            .dashboards
            .iter()
            .flat_map(|d| &d.filters)
            .any(|f| f.measure.is_some()),
        "a filter shows a measure"
    );
    assert_eq!(model.dashboards.len(), 3);
    // the notes dashboard searches a plain text column and shows rows
    assert!(
        model
            .dashboards
            .iter()
            .flat_map(|d| &d.filters)
            .any(|f| f.widget == tidepool_config::dashboard::FilterWidget::Search),
        "a filter is a search box"
    );
    assert!(
        model
            .dashboards
            .iter()
            .flat_map(|d| &d.visuals)
            .any(|v| v.visual_type == tidepool_config::dashboard::VisualType::Rows),
        "a visual shows rows"
    );
    // the recent dashboard opens on a window of the last seven days
    let windows: Vec<_> = model
        .dashboards
        .iter()
        .flat_map(|d| &d.filters)
        .filter_map(|f| f.default)
        .collect();
    assert_eq!(
        windows,
        vec![tidepool_config::dashboard::RelativeWindow {
            last: 7,
            unit: tidepool_config::dashboard::WindowUnit::Day,
            ago: 0
        }]
    );
    // customer_id and product_id each join two tables into one key domain,
    // and products' category joins the category tree's key and parent
    assert_eq!(model.key_domains.len(), 3);
}
