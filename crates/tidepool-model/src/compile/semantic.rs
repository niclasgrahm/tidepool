//! `model.ossie.yaml` datasets and relationships → [`Dataset`]s,
//! [`Relationship`]s, the relationship graph and the key domains.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use tidepool_config::diag::{Diagnostic, suggest};
use tidepool_config::ossie::{
    self, CustomExtension, FieldExtension, ModelExtension, RelationshipExtension,
    tidepool_extension,
};
use tidepool_config::source::YamlPath;
use tidepool_config::table::{Encoding, TableMode};
use tidepool_expr::syntax;

use super::tables::Tables;
use super::{Cx, IDENTIFIER_HELP, is_identifier};
use crate::ir::{
    ColumnId, Dataset, DomainId, Field, FieldSource, KeyDomain, Relationship, RelationshipId,
    TableId,
};

pub(super) struct Semantic {
    pub datasets: Vec<Dataset>,
    /// Dataset name → its table.
    pub dataset_names: BTreeMap<String, TableId>,
    pub relationships: Vec<Relationship>,
    pub key_domains: Vec<KeyDomain>,
    pub graph: Graph,
    /// The model file, for later stages' diagnostics.
    pub file: PathBuf,
    /// Datasets that exist but couldn't be compiled (already reported):
    /// references to them are skipped silently instead of cascading.
    pub broken: BTreeSet<String>,
    /// Tables a broken relationship leads out of: what they reach is unknown.
    pub uncertain_from: BTreeSet<TableId>,
    /// A broken relationship whose many side is unknown too.
    pub uncertain_all: bool,
}

impl Semantic {
    pub fn dataset(&self, table: TableId) -> Option<&Dataset> {
        self.datasets.iter().find(|d| d.table == table)
    }

    /// Resolve `dataset.field`, as written in dashboards and extensions.
    /// `Err(None)` means the dataset is broken and the problem is already
    /// reported.
    pub fn resolve_qualified(
        &self,
        text: &str,
    ) -> Result<(TableId, &crate::ir::Field), Option<String>> {
        let Some((dataset, field)) = text.split_once('.') else {
            return Err(Some(format!("`{text}` should be written `dataset.field`")));
        };
        if self.broken.contains(dataset) {
            return Err(None);
        }
        let Some(table) = self.dataset_names.get(dataset) else {
            return Err(Some(format!(
                "unknown dataset `{dataset}` in `{text}`; {}",
                suggest(
                    "datasets",
                    dataset,
                    self.dataset_names.keys().map(String::as_str)
                )
            )));
        };
        let Some(found) = self.dataset(*table).and_then(|d| d.field(field)) else {
            let fields = self
                .dataset(*table)
                .map(|d| d.fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>());
            return Err(Some(format!(
                "dataset `{dataset}` has no field `{field}`; {}",
                suggest("fields", field, fields.unwrap_or_default())
            )));
        };
        Ok((*table, found))
    }

    /// Whether `from` reaches `to`, or might if the broken relationships
    /// were fixed. "Can't reach" is only worth reporting when this is false.
    pub fn maybe_reaches(&self, from: TableId, to: TableId) -> bool {
        self.graph.reaches(from, to)
            || self.uncertain_all
            || self
                .uncertain_from
                .iter()
                .any(|uncertain| self.graph.reaches(from, *uncertain))
    }

    pub fn dataset_name(&self, table: TableId) -> &str {
        self.dataset(table).map_or("?", |d| d.name.as_str())
    }
}

/// The relationship graph, many → one.
#[derive(Default)]
pub(super) struct Graph {
    edges: BTreeMap<TableId, Vec<(TableId, RelationshipId)>>,
}

impl Graph {
    /// Whether rows of `from` can reach `to` by following relationships from
    /// the many side to the one side (a table reaches itself).
    pub fn reaches(&self, from: TableId, to: TableId) -> bool {
        let mut seen = BTreeSet::new();
        let mut stack = vec![from];
        while let Some(table) = stack.pop() {
            if table == to {
                return true;
            }
            if seen.insert(table) {
                stack.extend(
                    self.edges
                        .get(&table)
                        .into_iter()
                        .flatten()
                        .map(|(next, _)| *next),
                );
            }
        }
        false
    }
}

/// Parse the Tidepool extension in a `custom_extensions` list, reporting a
/// malformed or repeated one. Absent means the default. Also returns the path
/// of the extension's `data`, for problems with what it says.
pub(super) fn extension<T: DeserializeOwned + Default>(
    cx: &mut Cx,
    file: &Path,
    owner: &YamlPath,
    extensions: &[CustomExtension],
) -> (T, YamlPath) {
    let mut found = tidepool_extension::<T>(extensions).into_iter();
    let first = found.next();
    let data_path = first.as_ref().map_or_else(
        || owner.key("custom_extensions"),
        |(index, _)| owner.key("custom_extensions").index(*index).key("data"),
    );
    for (index, _) in found {
        cx.push(
            Diagnostic::error(format!("more than one `{}` extension", ossie::VENDOR))
                .at(file, owner.key("custom_extensions").index(index)),
        );
    }
    let value = match first {
        None => T::default(),
        Some((_, Ok(value))) => value,
        Some((_, Err(message))) => {
            cx.push(
                Diagnostic::error(format!("invalid `{}` extension: {message}", ossie::VENDOR))
                    .at(file, data_path.clone()),
            );
            T::default()
        }
    };
    (value, data_path)
}

/// The expression of an Ossie object in a dialect we read, parsed. Reports
/// and returns `None` if there is none or it doesn't parse.
pub(super) fn expression(
    cx: &mut Cx,
    file: &Path,
    owner: &YamlPath,
    what: &str,
    expression: &ossie::Expression,
) -> Option<(String, syntax::Ast, YamlPath)> {
    let Some((index, dialect)) = expression.readable() else {
        let present: Vec<&str> = expression
            .dialects
            .iter()
            .map(|d| d.dialect.as_str())
            .collect();
        cx.push(
            Diagnostic::error(format!(
                "{what} has no expression in a dialect Tidepool reads"
            ))
            .at(file, owner.key("expression"))
            .help(format!(
                "add one in {} (found: {})",
                ossie::READ_DIALECTS.join(" or "),
                if present.is_empty() {
                    "none".to_string()
                } else {
                    present.join(", ")
                }
            )),
        );
        return None;
    };
    let path = owner
        .key("expression")
        .key("dialects")
        .index(index)
        .key("expression");
    match syntax::parse(&dialect.expression) {
        Ok(ast) => Some((dialect.expression.clone(), ast, path)),
        Err(err) => {
            cx.push(Diagnostic::error(format!("{what}: {err}")).at(file, path));
            None
        }
    }
}

pub(super) fn compile(cx: &mut Cx, tables: &Tables) -> Semantic {
    let mut semantic = Semantic {
        datasets: Vec::new(),
        dataset_names: BTreeMap::new(),
        relationships: Vec::new(),
        key_domains: Vec::new(),
        graph: Graph::default(),
        file: PathBuf::new(),
        broken: BTreeSet::new(),
        uncertain_from: BTreeSet::new(),
        uncertain_all: false,
    };
    let Some(parsed) = cx.set.model.as_ref() else {
        return semantic;
    };
    let file = parsed.file.clone();
    let model = &parsed.value;
    semantic.file.clone_from(&file);
    let root = YamlPath::root();

    if !(model.version.starts_with("0.1") || model.version.starts_with("0.2")) {
        cx.push(
            Diagnostic::warning(format!(
                "Ossie version `{}` is newer than Tidepool knows",
                model.version
            ))
            .at(&file, root.key("version"))
            .help("Tidepool reads Ossie 0.1 and 0.2 documents"),
        );
    }
    let _: (ModelExtension, _) = extension(cx, &file, &root, &model.custom_extensions);

    datasets(cx, tables, &file, model, &mut semantic);
    relationships(cx, tables, &file, model, &mut semantic);
    check_graph(cx, &file, model, &semantic);
    semantic.key_domains = key_domains(&semantic.relationships);
    semantic
}

fn datasets(
    cx: &mut Cx,
    tables: &Tables,
    file: &Path,
    model: &ossie::OssieModel,
    semantic: &mut Semantic,
) {
    let mut by_table: BTreeMap<TableId, String> = BTreeMap::new();
    for (i, spec) in model.datasets.iter().enumerate() {
        let path = YamlPath::root().key("datasets").index(i);
        if !is_identifier(&spec.name) {
            cx.push(
                Diagnostic::error(format!("`{}` can't be used as a dataset name", spec.name))
                    .at(file, path.key("name"))
                    .help(IDENTIFIER_HELP),
            );
            semantic.broken.insert(spec.name.clone());
            continue;
        }
        if semantic.dataset_names.contains_key(&spec.name) {
            cx.push(
                Diagnostic::error(format!("dataset `{}` is defined twice", spec.name))
                    .at(file, path.key("name")),
            );
            continue;
        }
        if tables.broken.contains(&spec.source) {
            semantic.broken.insert(spec.name.clone());
            continue;
        }
        let Some(table) = tables.named(&spec.source) else {
            semantic.broken.insert(spec.name.clone());
            cx.push(
                Diagnostic::error(format!(
                    "dataset `{}` reads table `{}`, which doesn't exist",
                    spec.name, spec.source
                ))
                .at(file, path.key("source"))
                .help(suggest("tables", &spec.source, tables.names())),
            );
            continue;
        };
        if let Some(other) = by_table.get(&table.id) {
            semantic.broken.insert(spec.name.clone());
            cx.push(
                Diagnostic::error(format!(
                    "table `{}` already has dataset `{other}`; a table has at most one dataset",
                    spec.source
                ))
                .at(file, path.key("source"))
                .help("to use a table in two roles (order date, ship date), give the relationships a `role`"),
            );
            continue;
        }
        by_table.insert(table.id, spec.name.clone());

        dataset_keys(cx, file, &path, spec, table, tables);

        let fields = fields(cx, file, &path, spec, table);
        semantic.dataset_names.insert(spec.name.clone(), table.id);
        semantic.datasets.push(Dataset {
            name: spec.name.clone(),
            table: table.id,
            description: spec.description.clone(),
            fields,
        });
    }
}

/// One declared field: its expression resolved against the table's columns.
fn declared_field(
    cx: &mut Cx,
    file: &Path,
    path: &YamlPath,
    spec: &ossie::Dataset,
    table: &crate::ir::Table,
    field: &ossie::Field,
) -> Option<Field> {
    let what = format!("field `{}.{}`", spec.name, field.name);
    if !is_identifier(&field.name) {
        cx.push(
            Diagnostic::error(format!("`{}` can't be used as a field name", field.name))
                .at(file, path.key("name"))
                .help(IDENTIFIER_HELP),
        );
        return None;
    }
    let (ext, ext_path): (FieldExtension, _) = extension(cx, file, path, &field.custom_extensions);
    let (text, ast, expr_path) = expression(cx, file, path, &what, &field.expression)?;
    let analysis = syntax::analyze(&ast);
    if !analysis.aggregates.is_empty() {
        cx.push(
            Diagnostic::error(format!("{what} uses an aggregate"))
                .at(file, expr_path.clone())
                .help("fields are row-level; aggregates belong in metrics"),
        );
        return None;
    }
    if analysis.window || analysis.subquery {
        cx.push(
            Diagnostic::error(format!(
                "{what} uses a window function or subquery, which fields can't"
            ))
            .at(file, expr_path.clone()),
        );
        return None;
    }

    let columns = field_columns(cx, file, &expr_path, &what, spec, table, &analysis)?;

    // a field named like a column annotates that column, and must be it
    let column = table.column_named(&field.name);
    let is_plain_column = columns.len() == 1
        && analysis.functions.is_empty()
        && matches!(ast, syntax::Ast::Identifier(_));
    let source = match column {
        Some(column) if is_plain_column && columns[0] == column.id => {
            FieldSource::Column(column.id)
        }
        Some(_) => {
            cx.push(
                Diagnostic::error(format!(
                    "{what} has the same name as a column of `{}` but a different expression",
                    table.name
                ))
                .at(file, path.key("name"))
                .help("a field named like a column describes that column; give a derived field its own name"),
            );
            return None;
        }
        None if is_plain_column => FieldSource::Column(columns[0]),
        None => FieldSource::Derived {
            expression: text,
            columns: columns.clone(),
        },
    };

    let column_is_temporal = match &source {
        FieldSource::Column(id) => table
            .column(*id)
            .is_some_and(|c| c.column_type.is_temporal()),
        FieldSource::Derived { .. } => false,
    };
    let datatype_is_temporal = matches!(
        field.datatype,
        Some(
            ossie::DataType::Date
                | ossie::DataType::Time
                | ossie::DataType::DateTime
                | ossie::DataType::DateTimeTz
        )
    );
    let is_time = field
        .dimension
        .as_ref()
        .and_then(|d| d.is_time)
        .unwrap_or(column_is_temporal || datatype_is_temporal);
    if ext.grain.is_some() && !is_time {
        cx.push(
            Diagnostic::error(format!("{what} has grains but isn't a time field"))
                .at(file, ext_path),
        );
    }
    Some(Field {
        name: field.name.clone(),
        source,
        is_time,
        grains: ext.grain.unwrap_or_default(),
        label: field.label.clone(),
        description: field.description.clone(),
    })
}

/// The columns a field expression reads, all of which must be its own table's.
fn field_columns(
    cx: &mut Cx,
    file: &Path,
    expr_path: &YamlPath,
    what: &str,
    spec: &ossie::Dataset,
    table: &crate::ir::Table,
    analysis: &syntax::Analysis,
) -> Option<Vec<ColumnId>> {
    let mut columns: Vec<ColumnId> = Vec::new();
    let mut ok = true;
    for reference in &analysis.references {
        let column = match reference.parts.as_slice() {
            [column] => Some(column),
            [dataset, column] if dataset.matches(&spec.name) => Some(column),
            _ => None,
        };
        let found = column.and_then(|c| table.columns.iter().find(|col| c.matches(&col.name)));
        match (column, found) {
            (_, Some(found)) => columns.push(found.id),
            (None, _) => {
                ok = false;
                cx.push(
                    Diagnostic::error(format!("{what} refers to `{reference}`"))
                        .at(file, expr_path.clone())
                        .help(format!(
                            "a field may only use columns of its own table (`{}`)",
                            table.name
                        )),
                );
            }
            (Some(column), None) => {
                ok = false;
                cx.push(
                    Diagnostic::error(format!("{what} refers to unknown column `{column}`"))
                        .at(file, expr_path.clone())
                        .help(suggest(
                            "columns",
                            &column.value,
                            table.columns.iter().map(|c| c.name.as_str()),
                        )),
                );
            }
        }
    }
    columns.dedup();
    ok.then_some(columns)
}

/// Keys declared in Ossie must name columns, and an upsert table's primary
/// key must be the table's.
fn dataset_keys(
    cx: &mut Cx,
    file: &Path,
    path: &YamlPath,
    spec: &ossie::Dataset,
    table: &crate::ir::Table,
    tables: &Tables,
) {
    // keys declared in Ossie must at least name columns, and an upsert
    // table's must be its key
    for (k, key) in spec
        .primary_key
        .iter()
        .chain(spec.unique_keys.iter().flatten())
        .enumerate()
    {
        let key_path = if k == 0 && spec.primary_key.is_some() {
            path.key("primary_key")
        } else {
            path.key("unique_keys")
                .index(k - usize::from(spec.primary_key.is_some()))
        };
        for (c, column) in key.iter().enumerate() {
            if table.column_named(column).is_none() {
                cx.push(
                    Diagnostic::error(format!("`{column}` is not a column of `{}`", spec.source))
                        .at(file, key_path.index(c))
                        .help(suggest(
                            "columns",
                            column,
                            table.columns.iter().map(|c| c.name.as_str()),
                        )),
                );
            }
        }
    }
    if let Some(key) = &spec.primary_key
        && table.mode == TableMode::Upsert
        && !tables.key_broken.contains(&table.id)
    {
        let declared: BTreeSet<&str> = key.iter().map(String::as_str).collect();
        let actual: BTreeSet<&str> = table
            .primary_key
            .iter()
            .filter_map(|id| table.column(*id).map(|c| c.name.as_str()))
            .collect();
        if declared != actual {
            cx.push(
                Diagnostic::error(format!(
                    "dataset `{}` declares primary key ({}), but table `{}` is keyed by ({})",
                    spec.name,
                    key.join(", "),
                    table.name,
                    actual.into_iter().collect::<Vec<_>>().join(", ")
                ))
                .at(file, path.key("primary_key")),
            );
        }
    }
}

fn fields(
    cx: &mut Cx,
    file: &Path,
    dataset_path: &YamlPath,
    spec: &ossie::Dataset,
    table: &crate::ir::Table,
) -> Vec<Field> {
    let mut declared: BTreeMap<String, Field> = BTreeMap::new();
    for (i, field) in spec.fields.iter().enumerate() {
        let path = dataset_path.key("fields").index(i);
        if declared.contains_key(&field.name) {
            cx.push(
                Diagnostic::error(format!(
                    "field `{}.{}` is defined twice",
                    spec.name, field.name
                ))
                .at(file, path.key("name")),
            );
            continue;
        }
        if let Some(compiled) = declared_field(cx, file, &path, spec, table, field) {
            declared.insert(field.name.clone(), compiled);
        }
    }

    // every column is a field; a declared field of the same name replaces it
    let mut fields: Vec<Field> = table
        .columns
        .iter()
        .map(|column| {
            declared.remove(&column.name).unwrap_or_else(|| Field {
                name: column.name.clone(),
                source: FieldSource::Column(column.id),
                is_time: column.column_type.is_temporal(),
                grains: Vec::new(),
                label: None,
                description: None,
            })
        })
        .collect();
    fields.extend(declared.into_values());
    fields
}

fn relationships(
    cx: &mut Cx,
    tables: &Tables,
    file: &Path,
    model: &ossie::OssieModel,
    semantic: &mut Semantic,
) {
    let mut names = BTreeSet::new();
    for (i, spec) in model.relationships.iter().enumerate() {
        let path = YamlPath::root().key("relationships").index(i);
        let ok_name = if !is_identifier(&spec.name) {
            cx.push(
                Diagnostic::error(format!(
                    "`{}` can't be used as a relationship name",
                    spec.name
                ))
                .at(file, path.key("name"))
                .help(IDENTIFIER_HELP),
            );
            false
        } else if !names.insert(spec.name.clone()) {
            cx.push(
                Diagnostic::error(format!("relationship `{}` is defined twice", spec.name))
                    .at(file, path.key("name")),
            );
            false
        } else {
            true
        };
        let compiled = if ok_name {
            relationship(cx, tables, file, &path, spec, semantic)
        } else {
            None
        };
        match compiled {
            Some(relationship) => semantic.relationships.push(relationship),
            // A relationship that should exist doesn't: anything "unreachable"
            // from its many side may only be so because of that, so later
            // stages stay quiet about it.
            None => match semantic.dataset_names.get(&spec.from) {
                Some(from) => {
                    semantic.uncertain_from.insert(*from);
                }
                None => semantic.uncertain_all = true,
            },
        }
    }

    for relationship in &semantic.relationships {
        semantic
            .graph
            .edges
            .entry(relationship.from)
            .or_default()
            .push((relationship.to, relationship.id));
    }
}

fn relationship(
    cx: &mut Cx,
    tables: &Tables,
    file: &Path,
    path: &YamlPath,
    spec: &ossie::Relationship,
    semantic: &Semantic,
) -> Option<Relationship> {
    let what = format!("relationship `{}`", spec.name);
    let (ext, _): (RelationshipExtension, _) = extension(cx, file, path, &spec.custom_extensions);

    let mut side = |key: &str, dataset: &str| -> Option<TableId> {
        let found = semantic.dataset_names.get(dataset).copied();
        if found.is_none() && !semantic.broken.contains(dataset) {
            cx.push(
                Diagnostic::error(format!("{what} points to unknown dataset `{dataset}`"))
                    .at(file, path.key(key))
                    .help(suggest(
                        "datasets",
                        dataset,
                        semantic.dataset_names.keys().map(String::as_str),
                    )),
            );
        }
        found
    };
    let (from, to) = (side("from", &spec.from), side("to", &spec.to));
    let (from, to) = (from?, to?);
    let (from_table, to_table) = (tables.get(from)?, tables.get(to)?);
    if from == to {
        cx.push(
            Diagnostic::error(format!("{what} connects `{}` to itself", spec.from))
                .at(file, path.key("to"))
                .help("self-relationships (hierarchies) aren't supported yet"),
        );
        return None;
    }
    if spec.from_columns.is_empty() || spec.from_columns.len() != spec.to_columns.len() {
        cx.push(
            Diagnostic::error(format!(
                "{what} has {} from_columns and {} to_columns; they must pair up",
                spec.from_columns.len(),
                spec.to_columns.len()
            ))
            .at(file, path.key("to_columns")),
        );
        return None;
    }

    let mut resolve =
        |key: &str, table: &crate::ir::Table, names: &[String]| -> Option<Vec<ColumnId>> {
            let mut ids = Vec::new();
            for (c, name) in names.iter().enumerate() {
                match table.column_named(name) {
                    Some(column) => ids.push(column.id),
                    None => cx.push(
                        Diagnostic::error(format!(
                            "{what}: `{name}` is not a column of `{}`",
                            table.name
                        ))
                        .at(file, path.key(key).index(c))
                        .help(suggest(
                            "columns",
                            name,
                            table.columns.iter().map(|c| c.name.as_str()),
                        )),
                    ),
                }
            }
            (ids.len() == names.len()).then_some(ids)
        };
    let from_columns = resolve("from_columns", from_table, &spec.from_columns);
    let to_columns = resolve("to_columns", to_table, &spec.to_columns);
    let (from_columns, to_columns) = (from_columns?, to_columns?);

    // the one side's key is broken: that error is the one to read
    if tables.key_broken.contains(&to) {
        return None;
    }
    let from_side = Side {
        table: from_table,
        columns: &from_columns,
    };
    let to_side = Side {
        table: to_table,
        columns: &to_columns,
    };
    if !to_is_keyed(cx, file, path, &what, &spec.to, &to_side)
        || !key_pairs(cx, file, path, &what, &from_side, &to_side)
    {
        return None;
    }

    Some(Relationship {
        id: cx.ids.relationship(&spec.name),
        name: spec.name.clone(),
        from,
        from_columns,
        to,
        to_columns,
        role: ext.role,
    })
}

/// One end of a relationship.
struct Side<'a> {
    table: &'a crate::ir::Table,
    columns: &'a [ColumnId],
}

/// The one side must be an upsert table, and `to_columns` its whole key.
fn to_is_keyed(
    cx: &mut Cx,
    file: &Path,
    path: &YamlPath,
    what: &str,
    to_name: &str,
    to: &Side,
) -> bool {
    let table = to.table;
    let key: BTreeSet<ColumnId> = table.primary_key.iter().copied().collect();
    let to_set: BTreeSet<ColumnId> = to.columns.iter().copied().collect();
    if table.mode == TableMode::Upsert && key == to_set {
        return true;
    }
    let help = if table.mode == TableMode::Upsert {
        let key_names: Vec<&str> = table
            .primary_key
            .iter()
            .filter_map(|id| table.column(*id))
            .map(|c| c.name.as_str())
            .collect();
        format!("`{}` is keyed by ({})", table.name, key_names.join(", "))
    } else {
        format!(
            "`{}` is an append table; the one side must be an upsert table keyed by to_columns",
            table.name
        )
    };
    cx.push(
        Diagnostic::error(format!(
            "{what}: to_columns must be the primary key of `{to_name}`"
        ))
        .at(file, path.key("to_columns"))
        .help(help),
    );
    false
}

/// Each joined pair of columns has one type, and both are dictionary-encoded.
fn key_pairs(
    cx: &mut Cx,
    file: &Path,
    path: &YamlPath,
    what: &str,
    from: &Side,
    to: &Side,
) -> bool {
    let mut ok = true;
    for (c, (f, t)) in from.columns.iter().zip(to.columns).enumerate() {
        let (Some(fc), Some(tc)) = (from.table.column(*f), to.table.column(*t)) else {
            continue;
        };
        if fc.column_type != tc.column_type {
            ok = false;
            cx.push(
                Diagnostic::error(format!(
                    "{what}: `{}.{}` is {} but `{}.{}` is {}",
                    from.table.name,
                    fc.name,
                    fc.column_type,
                    to.table.name,
                    tc.name,
                    tc.column_type
                ))
                .at(file, path.key("from_columns").index(c))
                .help("joined columns must have the same type"),
            );
        }
        for (table, column, key) in [
            (from.table, fc, "from_columns"),
            (to.table, tc, "to_columns"),
        ] {
            if column.encoding == Encoding::Plain {
                ok = false;
                cx.push(
                    Diagnostic::error(format!(
                        "{what} joins `{}.{}`, which is `encoding: plain`",
                        table.name, column.name
                    ))
                    .at(file, path.key(key).index(c))
                    .help("relationship keys are dictionary-encoded; remove `encoding: plain` from the column"),
                );
            }
        }
    }
    ok
}

/// No cycles, and no two paths between the same tables that nothing tells apart.
fn check_graph(cx: &mut Cx, file: &Path, model: &ossie::OssieModel, semantic: &Semantic) {
    // the path enumeration below would not terminate usefully on a cycle
    if !check_cycles(cx, file, model, semantic) {
        check_ambiguity(cx, file, model, semantic);
    }
}

/// Where a relationship is written in the model file, for pointing at it.
fn relationship_index(model: &ossie::OssieModel, semantic: &Semantic, id: RelationshipId) -> usize {
    let name = semantic
        .relationships
        .iter()
        .find(|r| r.id == id)
        .map(|r| r.name.as_str());
    model
        .relationships
        .iter()
        .position(|r| Some(r.name.as_str()) == name)
        .unwrap_or(0)
}

/// Report the first cycle in the relationship graph. Whether there was one.
fn check_cycles(cx: &mut Cx, file: &Path, model: &ossie::OssieModel, semantic: &Semantic) -> bool {
    let mut done = BTreeSet::new();
    let cycle = semantic.datasets.iter().find_map(|d| {
        find_cycle(
            &semantic.graph,
            d.table,
            &mut Vec::new(),
            &mut Vec::new(),
            &mut done,
        )
    });
    let Some(cycle) = cycle else {
        return false;
    };
    let name = |id: TableId| semantic.dataset_name(id).to_string();
    let mut route: Vec<String> = cycle.iter().map(|(t, _)| name(*t)).collect();
    route.extend(cycle.first().map(|(t, _)| name(*t)));
    let first = cycle
        .first()
        .map_or(0, |(_, r)| relationship_index(model, semantic, *r));
    cx.push(
        Diagnostic::error(format!("relationships form a cycle: {}", route.join(" → ")))
            .at(
                file,
                YamlPath::root()
                    .key("relationships")
                    .index(first)
                    .key("name"),
            )
            .help("relationships point from the many side to the one side and can't loop back"),
    );
    true
}

/// Two paths from one table to another with the same roles: nothing says
/// which one a query means.
fn check_ambiguity(cx: &mut Cx, file: &Path, model: &ossie::OssieModel, semantic: &Semantic) {
    let relationship = |id: &RelationshipId| semantic.relationships.iter().find(|r| r.id == *id);
    let describe = |route: &[RelationshipId]| {
        route
            .iter()
            .filter_map(relationship)
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>()
            .join(" → ")
    };
    for from in semantic.datasets.iter().map(|d| d.table) {
        // every route from `from`, grouped by where it ends
        let mut routes: BTreeMap<TableId, Vec<Vec<RelationshipId>>> = BTreeMap::new();
        let mut stack: Vec<(TableId, Vec<RelationshipId>)> = vec![(from, Vec::new())];
        while let Some((table, route)) = stack.pop() {
            for (to, rel) in semantic.graph.edges.get(&table).into_iter().flatten() {
                let mut next = route.clone();
                next.push(*rel);
                routes.entry(*to).or_default().push(next.clone());
                stack.push((*to, next));
            }
        }
        for (to, routes) in routes {
            let mut by_roles: BTreeMap<Vec<String>, &Vec<RelationshipId>> = BTreeMap::new();
            for route in &routes {
                let roles: Vec<String> = route
                    .iter()
                    .filter_map(relationship)
                    .filter_map(|r| r.role.clone())
                    .collect();
                let Some(other) = by_roles.insert(roles, route) else {
                    continue;
                };
                let at = route
                    .last()
                    .map_or(0, |r| relationship_index(model, semantic, *r));
                cx.push(
                    Diagnostic::error(format!(
                        "`{}` reaches `{}` two ways ({} and {}), and nothing tells them apart",
                        semantic.dataset_name(from),
                        semantic.dataset_name(to),
                        describe(other),
                        describe(route)
                    ))
                    .at(file, YamlPath::root().key("relationships").index(at).key("name"))
                    .help("give the relationships distinct roles in a TIDEPOOL extension: {\"role\": \"…\"}"),
                );
                break;
            }
        }
    }
}

/// A cycle reachable from `table`, as (table, relationship leaving it) steps.
/// `path` is the tables from the search's start to here and `rels` the
/// relationships between them; `done` holds tables known to reach no cycle.
fn find_cycle(
    graph: &Graph,
    table: TableId,
    path: &mut Vec<TableId>,
    rels: &mut Vec<RelationshipId>,
    done: &mut BTreeSet<TableId>,
) -> Option<Vec<(TableId, RelationshipId)>> {
    if let Some(at) = path.iter().position(|t| *t == table) {
        return Some(
            path[at..]
                .iter()
                .copied()
                .zip(rels[at..].iter().copied())
                .collect(),
        );
    }
    if done.contains(&table) {
        return None;
    }
    path.push(table);
    for (to, rel) in graph.edges.get(&table).into_iter().flatten() {
        rels.push(*rel);
        if let Some(cycle) = find_cycle(graph, *to, path, rels, done) {
            return Some(cycle);
        }
        rels.pop();
    }
    path.pop();
    done.insert(table);
    None
}

/// Union-find's find, with path compression.
fn find(
    parent: &mut BTreeMap<(TableId, ColumnId), (TableId, ColumnId)>,
    x: (TableId, ColumnId),
) -> (TableId, ColumnId) {
    let p = *parent.entry(x).or_insert(x);
    if p == x {
        return x;
    }
    let root = find(parent, p);
    parent.insert(x, root);
    root
}

/// Union the columns relationships join into key domains.
fn key_domains(relationships: &[Relationship]) -> Vec<KeyDomain> {
    let mut parent: BTreeMap<(TableId, ColumnId), (TableId, ColumnId)> = BTreeMap::new();
    for relationship in relationships {
        for (f, t) in relationship
            .from_columns
            .iter()
            .zip(&relationship.to_columns)
        {
            let a = find(&mut parent, (relationship.from, *f));
            let b = find(&mut parent, (relationship.to, *t));
            if a != b {
                parent.insert(a, b);
            }
        }
    }
    let members: Vec<(TableId, ColumnId)> = parent.keys().copied().collect();
    let mut groups: BTreeMap<(TableId, ColumnId), Vec<(TableId, ColumnId)>> = BTreeMap::new();
    for member in members {
        let root = find(&mut parent, member);
        groups.entry(root).or_default().push(member);
    }
    groups
        .into_values()
        .map(|mut columns| {
            columns.sort_by_key(|(_, c)| *c);
            let id = DomainId(columns.iter().map(|(_, c)| *c).min().unwrap_or(ColumnId(0)));
            KeyDomain { id, columns }
        })
        .collect()
}
