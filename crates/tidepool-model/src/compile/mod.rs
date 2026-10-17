//! Config files → [`Model`]: every cross-file check, and id assignment.
//!
//! Compilation never stops at the first problem. Each stage reports into one
//! [`Diagnostics`] and carries on with what it could make sense of, so one run
//! lists everything wrong with a config. A stage that depends on a broken
//! object skips just that object (a relationship to an unknown dataset is
//! reported once, not again by every metric that would have used it).

mod dashboards;
mod metrics;
mod semantic;
mod server;
mod tables;

use tidepool_config::diag::{Diagnostic, Diagnostics};
use tidepool_config::project::ConfigSet;

use crate::catalog::{Assigner, Catalog};
use crate::ir::Model;

/// A model that compiled, and the catalog to compile its successor against.
#[derive(Clone, Debug)]
pub struct Compiled {
    pub model: Model,
    pub catalog: Catalog,
}

/// Compile a config set, assigning ids against `previous`.
///
/// Returns the model only if there were no errors and every file parsed;
/// the diagnostics may hold warnings either way.
#[must_use]
pub fn compile(set: &ConfigSet, previous: &Catalog) -> (Option<Compiled>, Diagnostics) {
    let mut cx = Cx {
        set,
        diagnostics: Diagnostics::default(),
        ids: Assigner::new(previous),
    };

    let server = set
        .server
        .as_ref()
        .map(|s| s.value.clone())
        .unwrap_or_default();
    let tables = tables::compile(&mut cx);
    server::check(&mut cx, &tables);
    let semantic = semantic::compile(&mut cx, &tables);
    let metrics = metrics::compile(&mut cx, &tables, &semantic);
    let dashboards = dashboards::compile(&mut cx, &tables, &semantic, &metrics);

    let model_unparsed =
        set.is_unparsed(std::path::Path::new(tidepool_config::project::MODEL_FILE));
    if set.model.is_none() && !model_unparsed && !set.dashboards.is_empty() {
        cx.diagnostics.push(
            Diagnostic::error("there are dashboards but no model.ossie.yaml")
                .help("dashboards show metrics, and metrics are defined in model.ossie.yaml"),
        );
    }

    let Cx {
        diagnostics, ids, ..
    } = cx;
    let mut diagnostics = diagnostics;
    diagnostics.sort(&set.files);
    // a file that didn't parse (reported by whoever parsed it) is missing
    // from everything above, which can look fine without it
    if diagnostics.has_errors() || !set.unparsed.is_empty() {
        return (None, diagnostics);
    }

    let mut tables = tables.all;
    // the key domains are decided by the relationships, but live on the columns
    for domain in &semantic.key_domains {
        for (table, column) in &domain.columns {
            if let Some(column) = tables
                .iter_mut()
                .find(|t| t.id == *table)
                .and_then(|t| t.columns.iter_mut().find(|c| c.id == *column))
            {
                column.key_domain = Some(domain.id);
            }
        }
    }

    let model = Model {
        server,
        direction: semantic.direction,
        name: set.model.as_ref().map(|m| m.value.name.clone()),
        tables,
        datasets: semantic.datasets,
        relationships: semantic.relationships,
        key_domains: semantic.key_domains,
        metrics: metrics.metrics,
        dashboards,
    };
    (
        Some(Compiled {
            model,
            catalog: ids.next,
        }),
        diagnostics,
    )
}

/// State shared by the compile stages.
struct Cx<'a> {
    set: &'a ConfigSet,
    diagnostics: Diagnostics,
    ids: Assigner<'a>,
}

impl Cx<'_> {
    fn push(&mut self, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic);
    }
}

/// Whether `name` can be used unquoted in an expression: a plain SQL
/// identifier. Names in Tidepool must be, so every name can be referred to.
fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && name.len() <= 128
}

const IDENTIFIER_HELP: &str =
    "names start with a letter or underscore and contain only letters, digits and underscores";
