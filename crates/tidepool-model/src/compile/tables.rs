//! `tables/*.yaml` → [`Table`]s.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use tidepool_config::diag::{Diagnostic, suggest};
use tidepool_config::source::YamlPath;
use tidepool_config::table::{ColumnSpec, Filled, TableFile, TableMode};
use tidepool_config::types::ColumnType;

use super::{Cx, IDENTIFIER_HELP, is_identifier};
use crate::ir::{Column, ColumnId, Table, TableId};

/// The compiled tables, plus where each came from.
pub(super) struct Tables {
    pub all: Vec<Table>,
    /// Table name → id and file.
    pub by_name: BTreeMap<String, (TableId, PathBuf)>,
    /// Tables whose file is broken: already reported, so references to them
    /// are skipped silently.
    pub broken: BTreeSet<String>,
    /// Tables whose primary key has a reported problem: key checks elsewhere
    /// would only repeat it.
    pub key_broken: BTreeSet<TableId>,
}

impl Tables {
    pub fn get(&self, id: TableId) -> Option<&Table> {
        self.all.iter().find(|t| t.id == id)
    }

    pub fn named(&self, name: &str) -> Option<&Table> {
        self.by_name.get(name).and_then(|(id, _)| self.get(*id))
    }

    pub fn names(&self) -> impl Iterator<Item = &str> + Clone {
        self.by_name.keys().map(String::as_str)
    }
}

pub(super) fn compile(cx: &mut Cx) -> Tables {
    let mut tables = Vec::new();
    let mut key_broken = BTreeSet::new();
    let mut by_name: BTreeMap<String, (TableId, PathBuf)> = BTreeMap::new();
    let mut broken: BTreeSet<String> = cx
        .set
        .unparsed
        .iter()
        .filter(|p| p.starts_with(tidepool_config::project::TABLES_DIR))
        .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(String::from))
        .collect();

    for parsed in &cx.set.tables {
        let file = parsed.file.clone();
        let spec = &parsed.value;
        let root = YamlPath::root();

        if !is_identifier(&spec.name) {
            cx.push(
                Diagnostic::error(format!("`{}` can't be used as a table name", spec.name))
                    .at(&file, root.key("name"))
                    .help(IDENTIFIER_HELP),
            );
            broken.insert(spec.name.clone());
            continue;
        }
        if parsed.stem() != spec.name {
            cx.push(
                Diagnostic::error(format!(
                    "table `{}` is defined in `{}`; its file must be named `tables/{}.yaml`",
                    spec.name,
                    file.display(),
                    spec.name
                ))
                .at(&file, root.key("name")),
            );
            broken.insert(spec.name.clone());
            broken.insert(parsed.stem());
            continue;
        }
        if by_name.contains_key(&spec.name) {
            cx.push(
                Diagnostic::error(format!("table `{}` is defined twice", spec.name))
                    .at(&file, root.key("name")),
            );
            continue;
        }

        let id = cx.ids.table(&spec.name, spec.renamed_from.as_deref());
        let table = compile_table(cx, id, spec, &file);
        // an upsert table whose key didn't fully resolve (already reported)
        let key_ok = spec
            .primary_key
            .as_ref()
            .is_some_and(|key| !key.is_empty() && table.primary_key.len() == key.len());
        if spec.mode == TableMode::Upsert && !key_ok {
            key_broken.insert(id);
        }
        by_name.insert(spec.name.clone(), (id, file));
        tables.push(table);
    }

    // a rename whose old name is still in use would make two tables one
    for parsed in &cx.set.tables {
        if let Some(old) = &parsed.value.renamed_from
            && by_name.contains_key(old)
        {
            cx.push(
                Diagnostic::error(format!(
                    "`{}` says it was renamed from `{old}`, but `{old}` still exists",
                    parsed.value.name
                ))
                .at(&parsed.file, YamlPath::root().key("renamed_from")),
            );
        }
    }

    Tables {
        all: tables,
        by_name,
        broken,
        key_broken,
    }
}

fn compile_table(cx: &mut Cx, id: TableId, spec: &TableFile, file: &PathBuf) -> Table {
    let root = YamlPath::root();
    let columns_path = root.key("columns");

    if spec.columns.is_empty() {
        cx.push(
            Diagnostic::error(format!("table `{}` has no columns", spec.name))
                .at(file, columns_path.clone()),
        );
    }

    let mut columns = Vec::new();
    for (name, column) in &spec.columns {
        let path = columns_path.key(name);
        if !is_identifier(name) {
            cx.push(
                Diagnostic::error(format!("`{name}` can't be used as a column name"))
                    .at_key(file, path.clone())
                    .help(IDENTIFIER_HELP),
            );
            continue;
        }
        if let Some(old) = &column.renamed_from
            && spec.columns.contains_key(old)
        {
            cx.push(
                Diagnostic::error(format!(
                    "`{name}` says it was renamed from `{old}`, but `{old}` is still a column"
                ))
                .at(file, path.key("renamed_from")),
            );
        }
        let filled = filled(cx, file, spec, name, column);
        columns.push(Column {
            id: cx.ids.column(id, name, column.renamed_from.as_deref()),
            name: name.clone(),
            column_type: column.column_type,
            nullable: column.nullable,
            encoding: column.effective_encoding(),
            key_domain: None,
            filled,
        });
    }

    let column_names = || spec.columns.keys().map(String::as_str);
    let lookup = |name: &str| columns.iter().find(|c: &&Column| c.name == name);
    let primary_key = primary_key(cx, file, spec, &columns);

    let event_time = spec.event_time.as_ref().and_then(|name| {
        let path = root.key("event_time");
        match lookup(name) {
            None => {
                cx.push(
                    Diagnostic::error(format!(
                        "event_time column `{name}` is not a column of `{}`",
                        spec.name
                    ))
                    .at(file, path)
                    .help(suggest("columns", name, column_names())),
                );
                None
            }
            Some(column) if !column.column_type.is_temporal() => {
                cx.push(
                    Diagnostic::error(format!(
                        "event_time column `{name}` is {}, not a date or timestamp",
                        column.column_type
                    ))
                    .at(file, path),
                );
                None
            }
            Some(column) => Some(column.id),
        }
    });

    if spec.retention.is_some() && spec.event_time.is_none() {
        cx.push(
            Diagnostic::error(format!(
                "table `{}` has a retention but no event_time",
                spec.name
            ))
            .at_key(file, root.key("retention"))
            .help("retention is measured on event_time; name the column rows are timed by"),
        );
    }

    // Kept even when something above was wrong: the problem is reported, and
    // "unknown table" errors from everything that refers to it would only
    // bury it.
    Table {
        id,
        name: spec.name.clone(),
        mode: spec.mode,
        primary_key,
        event_time,
        retention: spec.retention,
        columns,
    }
}

/// What the server fills the column with, if that fits its type and it
/// isn't part of the key (reported otherwise).
fn filled(
    cx: &mut Cx,
    file: &PathBuf,
    spec: &TableFile,
    name: &str,
    column: &ColumnSpec,
) -> Option<Filled> {
    let filled = column.filled?;
    let path = YamlPath::root().key("columns").key(name).key("filled");
    let (fits, what, wants) = match filled {
        Filled::Credential => (
            column.column_type == ColumnType::String,
            "a credential's name",
            "string",
        ),
        Filled::Received => (
            matches!(
                column.column_type,
                ColumnType::Timestamp | ColumnType::TimestampTz
            ),
            "the time a row is received",
            "timestamp or timestamptz",
        ),
    };
    if !fits {
        cx.push(
            Diagnostic::error(format!(
                "`{name}` is {} and can't hold {what}",
                column.column_type
            ))
            .at(file, path)
            .help(format!("make the column {wants}, or remove `filled`")),
        );
        return None;
    }
    if spec
        .primary_key
        .as_ref()
        .is_some_and(|k| k.iter().any(|c| c == name))
    {
        cx.push(
            Diagnostic::error(format!(
                "`{name}` is part of the primary key, so producers must send it; the server can't fill it"
            ))
            .at(file, path),
        );
        return None;
    }
    Some(filled)
}

/// The primary key's columns, checked against the table's mode and columns.
fn primary_key(cx: &mut Cx, file: &PathBuf, spec: &TableFile, columns: &[Column]) -> Vec<ColumnId> {
    let root = YamlPath::root();
    let column_names = || spec.columns.keys().map(String::as_str);
    let lookup = |name: &str| columns.iter().find(|c| c.name == name);
    let mut primary_key = Vec::new();
    match (spec.mode, &spec.primary_key) {
        (TableMode::Upsert, None) => cx.push(
            Diagnostic::error(format!("upsert table `{}` has no primary_key", spec.name))
                .at(file, root.key("mode"))
                .help("upsert replaces rows by key; name the key columns in `primary_key`"),
        ),
        (TableMode::Append, Some(_)) => cx.push(
            Diagnostic::error(format!("append table `{}` has a primary_key", spec.name))
                .at_key(file, root.key("primary_key"))
                .help("only upsert tables are keyed; set `mode: upsert`, or remove `primary_key`"),
        ),
        (TableMode::Upsert, Some(key)) => {
            if key.is_empty() {
                cx.push(
                    Diagnostic::error("primary_key is empty").at(file, root.key("primary_key")),
                );
            }
            for (i, name) in key.iter().enumerate() {
                let path = root.key("primary_key").index(i);
                match lookup(name) {
                    None => cx.push(
                        Diagnostic::error(format!(
                            "primary key column `{name}` is not a column of `{}`",
                            spec.name
                        ))
                        .at(file, path)
                        .help(suggest("columns", name, column_names())),
                    ),
                    Some(column) if column.nullable => cx.push(
                        Diagnostic::error(format!("primary key column `{name}` is nullable"))
                            .at(file, path)
                            .help("a key must always have a value; remove `nullable: true`"),
                    ),
                    Some(column) if column.encoding == tidepool_config::table::Encoding::Plain => cx.push(
                        Diagnostic::error(format!("primary key column `{name}` is `encoding: plain`"))
                            .at(file, path)
                            .help("keys are looked up through their dictionary; remove `encoding: plain` from the column"),
                    ),
                    Some(column) => primary_key.push(column.id),
                }
            }
        }
        (TableMode::Append, None) => {}
    }
    primary_key
}
