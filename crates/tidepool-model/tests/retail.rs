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
    assert_eq!(model.tables.len(), 3);
    assert_eq!(model.relationships.len(), 2);
    assert_eq!(model.dashboards.len(), 2);
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
    // customer_id and product_id each join two tables into one key domain
    assert_eq!(model.key_domains.len(), 2);
}
