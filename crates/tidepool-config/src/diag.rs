//! Problems found in config files, and how they are shown.
//!
//! Every check in the config and model crates reports into a [`Diagnostics`]
//! rather than returning on the first problem: a config with five mistakes
//! should say so once, not over five runs. A diagnostic names a file and a
//! [`YamlPath`]; [`render`] turns that into `file:line:col` and the offending
//! line with the token underlined, rustc-style.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::source::{Location, SourceFile, YamlPath};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
}

/// Whether a diagnostic points at a value or at the key that introduces it.
/// "Unknown field `colour`" is about the key; "unknown dataset `cust`" is
/// about the value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Value,
    Key,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    /// The file, relative to the config directory. `None` for problems with
    /// the project as a whole ("no model.ossie.yaml").
    pub file: Option<PathBuf>,
    pub path: Option<YamlPath>,
    pub target: Target,
    /// A location known directly (serde's parse errors), used when there is
    /// no path to look up.
    pub location: Option<Location>,
    pub help: Option<String>,
}

impl Diagnostic {
    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            message: message.into(),
            file: None,
            path: None,
            target: Target::Value,
            location: None,
            help: None,
        }
    }

    #[must_use]
    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            ..Self::error(message)
        }
    }

    /// Point at the value at `path` in `file`.
    #[must_use]
    pub fn at(mut self, file: impl AsRef<Path>, path: YamlPath) -> Self {
        self.file = Some(file.as_ref().to_path_buf());
        self.path = Some(path);
        self.target = Target::Value;
        self
    }

    /// Point at the key whose value is at `path` in `file`.
    #[must_use]
    pub fn at_key(mut self, file: impl AsRef<Path>, path: YamlPath) -> Self {
        self = self.at(file, path);
        self.target = Target::Key;
        self
    }

    /// Point at a known location in `file` (for errors that come with one).
    #[must_use]
    pub fn at_location(mut self, file: impl AsRef<Path>, location: Option<Location>) -> Self {
        self.file = Some(file.as_ref().to_path_buf());
        self.location = location;
        self
    }

    #[must_use]
    pub fn help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    #[must_use]
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

/// An accumulator for diagnostics.
#[derive(Clone, Debug, Default)]
pub struct Diagnostics(Vec<Diagnostic>);

impl Diagnostics {
    pub fn push(&mut self, diagnostic: Diagnostic) {
        self.0.push(diagnostic);
    }

    pub fn extend(&mut self, other: Diagnostics) {
        self.0.extend(other.0);
    }

    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.0.iter().any(Diagnostic::is_error)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Diagnostic> {
        self.0.iter()
    }

    #[must_use]
    pub fn into_vec(self) -> Vec<Diagnostic> {
        self.0
    }

    /// Errors before warnings, then by file and position, so output is stable.
    pub fn sort(&mut self, files: &[SourceFile]) {
        self.0.sort_by_key(|d| {
            let location = resolve(d, files);
            (
                d.severity,
                d.file.clone(),
                location.map_or((0, 0), |l| (l.line, l.col)),
                d.message.clone(),
            )
        });
    }
}

impl IntoIterator for Diagnostics {
    type Item = Diagnostic;
    type IntoIter = std::vec::IntoIter<Diagnostic>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

/// Where a diagnostic points, if anywhere.
#[must_use]
pub fn resolve(diagnostic: &Diagnostic, files: &[SourceFile]) -> Option<Location> {
    if let Some(location) = diagnostic.location {
        return Some(location);
    }
    let file = files
        .iter()
        .find(|f| Some(f.path()) == diagnostic.file.as_deref())?;
    let path = diagnostic.path.as_ref()?;
    match diagnostic.target {
        Target::Value => file.index.value(path),
        Target::Key => file.index.key(path),
    }
}

/// Render one diagnostic, rustc-style:
///
/// ```text
/// error: relationship `order_customer` points to unknown dataset `customer`
///   --> model.ossie.yaml:22:9
///    |
/// 22 |     to: customer
///    |         ^^^^^^^^
///    = help: did you mean `customers`?
/// ```
#[must_use]
pub fn render(diagnostic: &Diagnostic, files: &[SourceFile]) -> String {
    let mut out = String::new();
    let label = match diagnostic.severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
    };
    let _ = writeln!(out, "{label}: {}", diagnostic.message);

    let location = resolve(diagnostic, files);
    let source = diagnostic
        .file
        .as_deref()
        .and_then(|path| files.iter().find(|f| f.path() == path));
    let gutter = location.map_or(1, |l| l.line.to_string().len());
    let pad = " ".repeat(gutter);

    if let Some(file) = &diagnostic.file {
        match location {
            Some(l) => {
                let _ = writeln!(out, "{pad}--> {}:{}:{}", file.display(), l.line, l.col);
            }
            None => {
                let _ = writeln!(out, "{pad}--> {}", file.display());
            }
        }
    }
    if let (Some(l), Some(text)) = (
        location,
        source.and_then(|s| location.and_then(|l| s.line(l.line))),
    ) {
        let _ = writeln!(out, "{pad} |");
        let _ = writeln!(out, "{} | {}", l.line, text);
        let _ = writeln!(
            out,
            "{pad} | {}{}",
            " ".repeat(l.col.saturating_sub(1)),
            "^".repeat(l.len)
        );
    }
    if let Some(help) = &diagnostic.help {
        let _ = writeln!(out, "{pad} = help: {help}");
    }
    out
}

/// Render many, separated by blank lines, with a summary line at the end.
#[must_use]
pub fn render_all(diagnostics: &Diagnostics, files: &[SourceFile]) -> String {
    let mut out = String::new();
    for diagnostic in diagnostics.iter() {
        out.push_str(&render(diagnostic, files));
        out.push('\n');
    }
    let errors = diagnostics.iter().filter(|d| d.is_error()).count();
    let warnings = diagnostics.len() - errors;
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    let _ = writeln!(
        out,
        "{}, {}",
        plural(errors, "error"),
        plural(warnings, "warning")
    );
    out
}

/// The closest of `candidates` to `wanted`, if any is close enough to be a
/// plausible typo: the "did you mean" in help texts.
#[must_use]
pub fn closest<'a>(wanted: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let limit = (wanted.chars().count() / 3).max(1);
    candidates
        .into_iter()
        .map(|candidate| (edit_distance(wanted, candidate), candidate))
        .filter(|(distance, _)| *distance <= limit)
        .min_by_key(|(distance, candidate)| (*distance, *candidate))
        .map(|(_, candidate)| candidate)
}

/// A help text for an unknown name: "did you mean …?" when one is close,
/// otherwise the list of what exists.
#[must_use]
pub fn suggest<'a>(
    what: &str,
    wanted: &str,
    candidates: impl IntoIterator<Item = &'a str> + Clone,
) -> String {
    if let Some(near) = closest(wanted, candidates.clone()) {
        return format!("did you mean `{near}`?");
    }
    let mut all: Vec<&str> = candidates.into_iter().collect();
    all.sort_unstable();
    if all.is_empty() {
        format!("there are no {what}")
    } else {
        format!("{what}: {}", all.join(", "))
    }
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut current = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(ca != *cb);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        previous = current;
    }
    previous[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_diagnostic_renders_with_its_line_underlined() {
        let file = SourceFile::new(
            "model.ossie.yaml",
            "name: retail\nrelationships:\n  - to: customer\n",
        );
        let diagnostic = Diagnostic::error("unknown dataset `customer`")
            .at(
                "model.ossie.yaml",
                YamlPath::root().key("relationships").index(0).key("to"),
            )
            .help("did you mean `customers`?");
        assert_eq!(
            render(&diagnostic, &[file]),
            "error: unknown dataset `customer`\n \
             --> model.ossie.yaml:3:9\n  \
             |\n\
             3 |   - to: customer\n  \
             |         ^^^^^^^^\n  \
             = help: did you mean `customers`?\n"
        );
    }

    #[test]
    fn close_names_are_suggested_and_far_ones_are_not() {
        assert_eq!(
            closest("customer", ["customers", "orders"]),
            Some("customers")
        );
        assert_eq!(closest("xyz", ["customers", "orders"]), None);
        assert_eq!(
            suggest("datasets", "xyz", ["orders", "customers"]),
            "datasets: customers, orders"
        );
    }
}
