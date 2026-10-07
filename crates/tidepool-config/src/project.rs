//! A whole config directory: which file is which, parsed.
//!
//! ```text
//! tidepool.yaml          server settings            → ServerConfig
//! tables/<name>.yaml     one physical table each    → TableFile
//! model.ossie.yaml       the semantic layer         → OssieModel
//! dashboards/<id>.yaml   one dashboard each         → DashboardFile
//! ```
//!
//! [`ConfigSet::from_files`] is the core: it takes files as (relative path,
//! text) pairs, so the same code loads a directory, a test's inline files, or
//! (later) a change proposed over the API. Parsing never stops at the first
//! bad file; every problem becomes a diagnostic and the files that did parse
//! are kept, so validation can still report on them.

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;

use crate::dashboard::DashboardFile;
use crate::diag::{Diagnostic, Diagnostics};
use crate::ossie::{OssieModel, OssieWrapped};
use crate::server::ServerConfig;
use crate::source::{Location, SourceFile, YamlPath};
use crate::table::TableFile;

pub const SERVER_FILE: &str = "tidepool.yaml";
pub const MODEL_FILE: &str = "model.ossie.yaml";
pub const TABLES_DIR: &str = "tables";
pub const DASHBOARDS_DIR: &str = "dashboards";

/// One parsed file and where it came from.
#[derive(Clone, Debug)]
pub struct Parsed<T> {
    /// Relative to the config directory, e.g. `tables/orders.yaml`.
    pub file: PathBuf,
    pub value: T,
}

impl<T> Parsed<T> {
    /// The file name without its extension: a table file's expected name, a
    /// dashboard's id.
    #[must_use]
    pub fn stem(&self) -> String {
        self.file
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| {
                n.trim_end_matches(".yaml")
                    .trim_end_matches(".yml")
                    .to_string()
            })
            .unwrap_or_default()
    }
}

/// Everything in a config directory.
#[derive(Clone, Debug, Default)]
pub struct ConfigSet {
    /// Every file that was read, for rendering diagnostics.
    pub files: Vec<SourceFile>,
    pub server: Option<Parsed<ServerConfig>>,
    pub tables: Vec<Parsed<TableFile>>,
    pub model: Option<Parsed<OssieModel>>,
    pub dashboards: Vec<Parsed<DashboardFile>>,
    /// Config files that didn't parse (their errors are in the diagnostics).
    /// Validation uses this to stay quiet about references into them, rather
    /// than burying the parse error under "doesn't exist" errors.
    pub unparsed: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Server,
    Table,
    Model,
    Dashboard,
}

/// What a relative path is, by convention, or `None` for files that aren't
/// Tidepool config (READMEs, other YAML).
fn kind_of(path: &Path) -> Option<Kind> {
    let parts: Vec<&str> = path.iter().filter_map(|p| p.to_str()).collect();
    let is_yaml = |name: &str| {
        Path::new(name)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"))
    };
    match parts.as_slice() {
        [SERVER_FILE] => Some(Kind::Server),
        [MODEL_FILE] => Some(Kind::Model),
        [TABLES_DIR, name] if is_yaml(name) => Some(Kind::Table),
        [DASHBOARDS_DIR, name] if is_yaml(name) => Some(Kind::Dashboard),
        _ => None,
    }
}

impl ConfigSet {
    /// Parse a set of files. Paths are relative to the config directory and use
    /// the layout above; anything else is ignored (YAML that looks misplaced
    /// gets a warning).
    pub fn from_files<P, T>(files: impl IntoIterator<Item = (P, T)>) -> (Self, Diagnostics)
    where
        P: Into<PathBuf>,
        T: Into<String>,
    {
        let mut set = Self::default();
        let mut diagnostics = Diagnostics::default();
        let mut files: Vec<SourceFile> = files
            .into_iter()
            .map(|(path, text)| SourceFile::new(path, text))
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));

        for file in &files {
            let Some(kind) = kind_of(file.path()) else {
                let name = file.path().to_string_lossy();
                if name.ends_with(".yaml") || name.ends_with(".yml") {
                    diagnostics.push(
                        Diagnostic::warning(format!("`{name}` is not a Tidepool config file, so it is ignored"))
                            .at_location(file.path(), None)
                            .help(format!(
                                "config files are {SERVER_FILE}, {MODEL_FILE}, {TABLES_DIR}/<table>.yaml and {DASHBOARDS_DIR}/<id>.yaml"
                            )),
                    );
                }
                continue;
            };
            let parsed = match kind {
                Kind::Server => {
                    set.server = parse(file, &mut diagnostics);
                    set.server.is_some()
                }
                Kind::Table => {
                    let table = parse(file, &mut diagnostics);
                    let ok = table.is_some();
                    set.tables.extend(table);
                    ok
                }
                Kind::Dashboard => {
                    let dashboard = parse(file, &mut diagnostics);
                    let ok = dashboard.is_some();
                    set.dashboards.extend(dashboard);
                    ok
                }
                Kind::Model => {
                    set.model = parse_model(file, &mut diagnostics);
                    set.model.is_some()
                }
            };
            if !parsed {
                set.unparsed.push(file.path.clone());
            }
        }
        set.files = files;
        (set, diagnostics)
    }

    /// Read a config directory: the files at its root plus `tables/` and
    /// `dashboards/` (not recursively).
    ///
    /// # Errors
    /// The directory, or a file in it, can't be read.
    pub fn load_dir(dir: &Path) -> std::io::Result<(Self, Diagnostics)> {
        let mut files = Vec::new();
        for sub in [None, Some(TABLES_DIR), Some(DASHBOARDS_DIR)] {
            let path = sub.map_or_else(|| dir.to_path_buf(), |sub| dir.join(sub));
            if sub.is_some() && !path.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(&path)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || !entry.file_type()?.is_file() {
                    continue;
                }
                let relative =
                    sub.map_or_else(|| PathBuf::from(&name), |sub| Path::new(sub).join(&name));
                if relative
                    .extension()
                    .is_some_and(|e| e == "yaml" || e == "yml")
                {
                    files.push((relative, std::fs::read_to_string(entry.path())?));
                }
            }
        }
        Ok(Self::from_files(files))
    }

    /// The source of a file, by its relative path.
    /// Whether a file of this name was present but failed to parse.
    #[must_use]
    pub fn is_unparsed(&self, path: &Path) -> bool {
        self.unparsed.iter().any(|p| p == path)
    }

    #[must_use]
    pub fn file(&self, path: &Path) -> Option<&SourceFile> {
        self.files.iter().find(|f| f.path() == path)
    }
}

fn parse<T: DeserializeOwned>(
    file: &SourceFile,
    diagnostics: &mut Diagnostics,
) -> Option<Parsed<T>> {
    match serde_norway::from_str(&file.text) {
        Ok(value) => Some(Parsed {
            file: file.path.clone(),
            value,
        }),
        Err(err) => {
            diagnostics.push(serde_diagnostic(file, &err));
            None
        }
    }
}

/// The model file, in either Ossie shape: flat (0.2) or wrapped in a
/// `semantic_model:` list (0.1.1).
fn parse_model(file: &SourceFile, diagnostics: &mut Diagnostics) -> Option<Parsed<OssieModel>> {
    let wrapped = file
        .index
        .top_level_keys()
        .iter()
        .any(|k| k == "semantic_model");
    if !wrapped {
        return parse(file, diagnostics);
    }
    let parsed: Parsed<OssieWrapped> = parse(file, diagnostics)?;
    let OssieWrapped {
        version,
        semantic_model,
    } = parsed.value;
    let count = semantic_model.len();
    if count != 1 {
        diagnostics.push(
            Diagnostic::error(format!(
                "`semantic_model` holds {count} models; Tidepool serves exactly one"
            ))
            .at_key(file.path(), YamlPath::root().key("semantic_model")),
        );
    }
    let model = semantic_model.into_iter().next()?;
    Some(Parsed {
        file: parsed.file,
        value: model.into_model(version),
    })
}

/// A serde error as a diagnostic, with serde's own location.
fn serde_diagnostic(file: &SourceFile, err: &serde_norway::Error) -> Diagnostic {
    let location = err.location().map(|l| Location {
        line: l.line(),
        col: l.column(),
        len: 1,
    });
    // serde_norway appends " at line N column M"; the location says that already
    let message = err.to_string();
    let message = message
        .rsplit_once(" at line ")
        .map_or(message.as_str(), |(head, _)| head)
        .to_string();
    Diagnostic::error(message).at_location(file.path(), location)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_sorted_into_kinds_by_path() {
        let (set, diagnostics) = ConfigSet::from_files([
            ("tidepool.yaml", "epoch_interval: 100ms\n"),
            (
                "tables/orders.yaml",
                "name: orders\ncolumns:\n  id: { type: string }\n",
            ),
            ("dashboards/sales.yaml", "name: Sales\n"),
            (
                "model.ossie.yaml",
                "version: '0.2.0'\nname: m\ndatasets: []\n",
            ),
            ("README.md", "not config"),
        ]);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(set.server.is_some());
        assert_eq!(set.tables.len(), 1);
        assert_eq!(set.dashboards[0].stem(), "sales");
        assert_eq!(set.model.map(|m| m.value.name), Some("m".to_string()));
    }

    #[test]
    fn misplaced_yaml_is_a_warning() {
        let (_, diagnostics) = ConfigSet::from_files([("tables/nested/x.yaml", "name: x\n")]);
        assert_eq!(diagnostics.len(), 1);
        assert!(!diagnostics.has_errors());
    }

    #[test]
    fn the_wrapped_0_1_1_shape_is_unwrapped() {
        let (set, diagnostics) = ConfigSet::from_files([(
            "model.ossie.yaml",
            "version: 0.1.1\nsemantic_model:\n  - name: m\n    datasets: []\n",
        )]);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let model = set.model.map(|m| m.value);
        assert_eq!(model.as_ref().map(|m| m.name.as_str()), Some("m"));
        assert_eq!(model.map(|m| m.version), Some("0.1.1".to_string()));
    }

    #[test]
    fn a_parse_error_carries_serde_location_without_repeating_it() {
        let (set, diagnostics) = ConfigSet::from_files([(
            "tables/orders.yaml",
            "name: orders\ncolumns:\n  id: { type: varchar }\n",
        )]);
        assert!(set.tables.is_empty());
        assert!(set.is_unparsed(Path::new("tables/orders.yaml")));
        let Some(diagnostic) = diagnostics.iter().next() else {
            panic!("expected a diagnostic");
        };
        assert!(
            diagnostic.message.contains("unknown column type `varchar`"),
            "{}",
            diagnostic.message
        );
        assert!(!diagnostic.message.contains(" at line "));
        assert_eq!(diagnostic.location.map(|l| l.line), Some(3));
    }
}
