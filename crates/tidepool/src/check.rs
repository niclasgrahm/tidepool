//! `tidepool validate` and `tidepool plan`: the config pipeline from the
//! command line, without a server.

use std::path::Path;
use std::process::ExitCode;

use tidepool_config::ConfigSet;
use tidepool_config::diag::render_all;
use tidepool_model::{Catalog, Compiled, Model, compile, plan};

/// Load and compile a config directory, printing diagnostics to stderr.
/// `None` if it has errors (or can't be read).
pub fn load(dir: &Path, previous: &Catalog) -> Option<Compiled> {
    let (set, mut diagnostics) = match ConfigSet::load_dir(dir) {
        Ok(loaded) => loaded,
        Err(err) => {
            eprintln!(
                "error: can't read config directory {}: {err}",
                dir.display()
            );
            return None;
        }
    };
    let (compiled, more) = compile(&set, previous);
    diagnostics.extend(more);
    diagnostics.sort(&set.files);
    if !diagnostics.is_empty() {
        eprint!("{}", render_all(&diagnostics, &set.files));
    }
    compiled
}

/// One line describing a model, for a successful `validate`.
fn summary(model: &Model) -> String {
    let count = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    format!(
        "{}, {}, {}, {}",
        count(model.tables.len(), "table"),
        count(model.relationships.len(), "relationship"),
        count(model.metrics.len(), "metric"),
        count(model.dashboards.len(), "dashboard"),
    )
}

/// `tidepool validate`: exit 0 if the config compiles (warnings allowed).
pub fn validate(dir: &Path) -> ExitCode {
    match load(dir, &Catalog::default()) {
        Some(compiled) => {
            println!("ok: {}", summary(&compiled.model));
            ExitCode::SUCCESS
        }
        None => ExitCode::FAILURE,
    }
}

/// `tidepool plan`: what changes from `base` (or from nothing) to `dir`.
pub fn plan(dir: &Path, base: Option<&Path>) -> ExitCode {
    let before = if let Some(base) = base {
        let Some(compiled) = load(base, &Catalog::default()) else {
            eprintln!(
                "error: the base config ({}) doesn't compile",
                base.display()
            );
            return ExitCode::FAILURE;
        };
        compiled
    } else {
        // no base: plan against an empty project, which always compiles
        let (Some(compiled), _) = compile(&ConfigSet::default(), &Catalog::default()) else {
            return ExitCode::FAILURE;
        };
        compiled
    };
    let Some(after) = load(dir, &before.catalog) else {
        return ExitCode::FAILURE;
    };
    print!("{}", plan::diff(&before.model, &after.model).render());
    ExitCode::SUCCESS
}
