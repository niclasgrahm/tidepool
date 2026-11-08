//! Ossie metrics → [`Metric`]s.
//!
//! A base metric aggregates rows of one *anchor* table, and may read columns
//! of tables reachable from it many → one (`SUM(orders.quantity *
//! products.list_price)` is anchored on `orders`). A derived metric
//! (`kind: derived`) is an expression over other metrics, evaluated after
//! aggregation. Anything that would need two anchors in one expression has to
//! be split that way, which is what keeps fan-out impossible (plan §6.4).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use tidepool_config::diag::{Diagnostic, suggest};
use tidepool_config::format::NumberFormat;
use tidepool_config::ossie::{self, MetricExtension, MetricKind};
use tidepool_config::source::YamlPath;
use tidepool_expr::syntax;

use super::semantic::{Semantic, expression, extension};
use super::tables::Tables;
use super::{Cx, IDENTIFIER_HELP, is_identifier};
use crate::ir::{
    FieldRef, IgnoreSelection, Metric, MetricBody, MetricId, OffsetUnit, TableId, TimeOffset,
    Total, Type,
};

pub(super) struct Metrics {
    pub metrics: Vec<Metric>,
    pub by_name: BTreeMap<String, MetricId>,
}

impl Metrics {
    pub fn get(&self, id: MetricId) -> Option<&Metric> {
        self.metrics.iter().find(|m| m.id == id)
    }

    /// The anchor tables a metric's value comes from: its own for a base
    /// metric, its inputs' for a derived one.
    pub fn anchors(&self, id: MetricId) -> BTreeSet<TableId> {
        let mut anchors = BTreeSet::new();
        let mut stack = vec![id];
        let mut seen = BTreeSet::new();
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            match self.get(id).map(|m| &m.body) {
                Some(MetricBody::Base { anchor, .. }) => {
                    anchors.insert(*anchor);
                }
                Some(MetricBody::Derived { inputs, .. }) => stack.extend(inputs),
                None => {}
            }
        }
        anchors
    }
}

pub(super) fn compile(cx: &mut Cx, _tables: &Tables, semantic: &Semantic) -> Metrics {
    let mut out = Metrics {
        metrics: Vec::new(),
        by_name: BTreeMap::new(),
    };
    let Some(parsed) = cx.set.model.as_ref() else {
        return out;
    };
    let file = parsed.file.clone();
    let specs = &parsed.value.metrics;

    // names first, so a derived metric can use one defined below it
    let mut valid = Vec::new();
    for (i, spec) in specs.iter().enumerate() {
        let path = YamlPath::root().key("metrics").index(i);
        if !is_identifier(&spec.name) {
            cx.push(
                Diagnostic::error(format!("`{}` can't be used as a metric name", spec.name))
                    .at(&file, path.key("name"))
                    .help(IDENTIFIER_HELP),
            );
            continue;
        }
        if out.by_name.contains_key(&spec.name) {
            cx.push(
                Diagnostic::error(format!("metric `{}` is defined twice", spec.name))
                    .at(&file, path.key("name")),
            );
            continue;
        }
        out.by_name
            .insert(spec.name.clone(), cx.ids.metric(&spec.name));
        valid.push(i);
    }

    for i in valid {
        let spec = &specs[i];
        let path = YamlPath::root().key("metrics").index(i);
        let id = out.by_name[&spec.name];
        if let Some(metric) = metric(cx, &file, &path, id, spec, semantic, &out.by_name) {
            out.metrics.push(metric);
        }
    }

    check_cycles(cx, &file, specs, &out);
    type_derived(cx, &file, specs, &mut out);
    out
}

#[allow(clippy::too_many_lines)]
fn metric(
    cx: &mut Cx,
    file: &Path,
    path: &YamlPath,
    id: MetricId,
    spec: &ossie::Metric,
    semantic: &Semantic,
    names: &BTreeMap<String, MetricId>,
) -> Option<Metric> {
    let what = format!("metric `{}`", spec.name);
    let (ext, ext_path): (MetricExtension, _) = extension(cx, file, path, &spec.custom_extensions);
    let (text, ast, expr_path) = expression(cx, file, path, &what, &spec.expression)?;
    let analysis = syntax::analyze(&ast);

    if analysis.window || analysis.subquery {
        cx.push(
            Diagnostic::error(format!(
                "{what} uses a window function or subquery, which metrics can't (yet)"
            ))
            .at(file, expr_path),
        );
        return None;
    }

    let at = At {
        file,
        expr: expr_path.clone(),
        ext: ext_path.clone(),
        what: what.clone(),
    };
    let body = if ext.kind == Some(MetricKind::Derived) {
        if ext.table.is_some() {
            cx.push(
                Diagnostic::error(format!("{what} is derived, so it has no anchor table"))
                    .at(file, ext_path.clone())
                    .help(
                        "`table` is for base metrics; a derived metric's rows come from its inputs",
                    ),
            );
        }
        derived(cx, &at, text, &analysis, names)?
    } else {
        base(cx, &at, text, &analysis, &ext, semantic, names)?
    };
    // a base metric is typed now; a derived one once its inputs are
    let ty = match &body {
        MetricBody::Base { .. } => {
            match tidepool_expr::check_aggregation(&ast, &FieldScope { semantic }) {
                Ok(aggregation) => aggregation.combine.ty(),
                Err(message) => {
                    cx.push(
                        Diagnostic::error(format!("{what}: {message}")).at(file, at.expr.clone()),
                    );
                    return None;
                }
            }
        }
        MetricBody::Derived { .. } => Type::Null,
    };

    let ignore_selection = match ext.ignore_selection.as_slice() {
        [] => IgnoreSelection::None,
        [all] if all == "*" => IgnoreSelection::All,
        fields => {
            let mut refs = Vec::new();
            for field in fields {
                match semantic.resolve_qualified(field) {
                    Ok((table, f)) => refs.push(FieldRef {
                        table,
                        field: f.name.clone(),
                    }),
                    Err(None) => {}
                    Err(Some(message)) => cx.push(
                        Diagnostic::error(format!("{what}: ignore_selection: {message}"))
                            .at(file, ext_path.clone()),
                    ),
                }
            }
            IgnoreSelection::Fields(refs)
        }
    };

    let total = match ext.total.as_slice() {
        [] => Total::None,
        _ if matches!(body, MetricBody::Derived { .. }) => {
            cx.push(
                Diagnostic::error(format!(
                    "{what}: total goes on a base metric: a derived one is computed from the \
                     answers of others"
                ))
                .at(file, ext_path.clone())
                .help("put total on a base metric (revenue_total), and derive the share from it: revenue / revenue_total"),
            );
            Total::None
        }
        [all] if all == "*" => Total::All,
        fields => {
            let mut refs = Vec::new();
            for field in fields {
                match semantic.resolve_qualified(field) {
                    Ok((table, f)) => refs.push(FieldRef {
                        table,
                        field: f.name.clone(),
                    }),
                    Err(None) => {}
                    Err(Some(message)) => cx.push(
                        Diagnostic::error(format!("{what}: total: {message}"))
                            .at(file, ext_path.clone()),
                    ),
                }
            }
            Total::Fields(refs)
        }
    };

    let time_offset = match (&ext.time_offset, &ext.time_field) {
        (None, None) => None,
        (Some(_), None) | (None, Some(_)) => {
            cx.push(
                Diagnostic::error(format!("{what}: time_offset and time_field go together"))
                    .at(file, ext_path.clone())
                    .help("e.g. {\"time_offset\": \"-1y\", \"time_field\": \"orders.ordered_at\"}"),
            );
            None
        }
        (Some(offset), Some(field)) => time_offset(cx, &at, offset, field, &body, semantic),
    };

    if let Some(format) = &ext.format
        && let Err(problem) = format.parse::<NumberFormat>()
    {
        cx.push(
            Diagnostic::error(format!("{what}: format `{format}`: {problem}"))
                .at(file, ext_path)
                .help(tidepool_config::format::HELP),
        );
    }

    Some(Metric {
        id,
        name: spec.name.clone(),
        ty,
        description: spec.description.clone(),
        format: ext.format,
        body,
        ignore_selection,
        time_offset,
        total,
        ai_context: spec.ai_context.clone(),
    })
}

/// Where a metric's diagnostics go: its file, its expression, its extension.
struct At<'a> {
    file: &'a Path,
    expr: YamlPath,
    ext: YamlPath,
    what: String,
}

fn derived(
    cx: &mut Cx,
    at: &At,
    text: String,
    analysis: &syntax::Analysis,
    names: &BTreeMap<String, MetricId>,
) -> Option<MetricBody> {
    let (file, expr_path, what) = (at.file, &at.expr, &at.what);
    if !analysis.aggregates.is_empty() {
        cx.push(
            Diagnostic::error(format!("{what} is derived but aggregates rows"))
                .at(file, expr_path.clone())
                .help("a derived metric combines other metrics, e.g. `revenue / order_count`; put aggregates in base metrics"),
        );
        return None;
    }
    let mut inputs = Vec::new();
    let mut ok = true;
    for reference in &analysis.references {
        let found = match reference.parts.as_slice() {
            [name] => names
                .iter()
                .find(|(n, _)| name.matches(n))
                .map(|(_, id)| *id),
            _ => None,
        };
        if let Some(id) = found {
            inputs.push(id);
        } else {
            ok = false;
            cx.push(
                Diagnostic::error(format!(
                    "{what} refers to `{reference}`, which is not a metric"
                ))
                .at(file, expr_path.clone())
                .help(suggest(
                    "metrics",
                    &reference.to_string(),
                    names.keys().map(String::as_str),
                )),
            );
        }
    }
    if inputs.is_empty() && ok {
        cx.push(
            Diagnostic::error(format!("{what} is derived but uses no metrics"))
                .at(file, expr_path.clone()),
        );
        return None;
    }
    inputs.sort();
    inputs.dedup();
    ok.then_some(MetricBody::Derived {
        expression: text,
        inputs,
    })
}

fn base(
    cx: &mut Cx,
    at: &At,
    text: String,
    analysis: &syntax::Analysis,
    ext: &MetricExtension,
    semantic: &Semantic,
    names: &BTreeMap<String, MetricId>,
) -> Option<MetricBody> {
    let (file, expr_path, what) = (at.file, &at.expr, &at.what);
    if analysis.aggregates.is_empty() {
        cx.push(
            Diagnostic::error(format!("{what} doesn't aggregate anything"))
                .at(file, expr_path.clone())
                .help("a metric summarises rows, e.g. SUM(orders.amount); to combine metrics, add {\"kind\": \"derived\"}"),
        );
        return None;
    }
    if analysis.nested_aggregate {
        cx.push(
            Diagnostic::error(format!("{what} nests one aggregate inside another"))
                .at(file, expr_path.clone()),
        );
        return None;
    }

    let mut tables = BTreeSet::new();
    let mut ok = true;
    for reference in &analysis.references {
        if !reference.in_aggregate {
            ok = false;
            cx.push(
                Diagnostic::error(format!("{what} uses `{reference}` outside an aggregate"))
                    .at(file, expr_path.clone())
                    .help("every column in a metric must be inside SUM(…), COUNT(…) and so on"),
            );
            continue;
        }
        match resolve(reference, semantic, names) {
            Ok(table) => {
                tables.insert(table);
            }
            Err(None) => ok = false,
            Err(Some((problem, help))) => {
                ok = false;
                cx.push(
                    Diagnostic::error(format!("{what} {problem}"))
                        .at(file, expr_path.clone())
                        .help(help),
                );
            }
        }
    }
    if !ok {
        return None;
    }
    let anchor = anchor(cx, at, ext, semantic, &tables)?;
    tables.insert(anchor);
    Some(MetricBody::Base {
        expression: text,
        anchor,
        tables: tables.into_iter().collect(),
    })
}

/// The table whose rows a base metric aggregates: the one its extension
/// names, or the one among `tables` that reaches all the others.
fn anchor(
    cx: &mut Cx,
    at: &At,
    ext: &MetricExtension,
    semantic: &Semantic,
    tables: &BTreeSet<TableId>,
) -> Option<TableId> {
    let (file, what) = (at.file, &at.what);
    if let Some(table) = &ext.table {
        if semantic.broken.contains(table) {
            return None;
        }
        let Some(anchor) = semantic.dataset_names.get(table).copied() else {
            cx.push(
                Diagnostic::error(format!("{what}: anchor table `{table}` is not a dataset"))
                    .at(file, at.ext.clone())
                    .help(suggest(
                        "datasets",
                        table,
                        semantic.dataset_names.keys().map(String::as_str),
                    )),
            );
            return None;
        };
        if let Some(unreachable) = tables.iter().find(|t| !semantic.maybe_reaches(anchor, **t)) {
            cx.push(
                Diagnostic::error(format!(
                    "{what} is anchored on `{table}`, which can't reach `{}`",
                    semantic.dataset_name(*unreachable)
                ))
                .at(file, at.ext.clone()),
            );
            return None;
        }
        return Some(anchor);
    }
    let candidates: Vec<TableId> = tables
        .iter()
        .copied()
        .filter(|a| tables.iter().all(|t| semantic.graph.reaches(*a, *t)))
        .collect();
    match candidates.as_slice() {
        [anchor] => Some(*anchor),
        [] if tables.is_empty() => {
            cx.push(
                Diagnostic::error(format!("{what} doesn't say which table's rows it counts"))
                    .at(file, at.expr.clone())
                    .help("add a TIDEPOOL extension naming the dataset: {\"table\": \"orders\"}"),
            );
            None
        }
        // a broken relationship may be why; that error is the one to read
        _ if tables
            .iter()
            .any(|a| tables.iter().all(|t| semantic.maybe_reaches(*a, *t))) =>
        {
            None
        }
        _ => {
            let names: Vec<&str> = tables.iter().map(|t| semantic.dataset_name(*t)).collect();
            cx.push(
                Diagnostic::error(format!(
                    "{what} reads {}, and no one of them reaches the others",
                    names.join(", ")
                ))
                .at(file, at.expr.clone())
                .help("a metric aggregates one table's rows; split it into one metric per table and combine them with a derived metric"),
            );
            None
        }
    }
}

/// What's wrong with a reference: the end of a sentence starting "metric `x`",
/// and a help text.
type Problem = (String, String);

/// The table a column reference in a metric reads. `Err(None)`: the reference
/// may be into a broken dataset, which is already reported.
fn resolve(
    reference: &syntax::Reference,
    semantic: &Semantic,
    names: &BTreeMap<String, MetricId>,
) -> Result<TableId, Option<Problem>> {
    resolve_reported(reference, semantic, names).map_err(|problem| {
        let into_broken = match reference.parts.as_slice() {
            [dataset, _] => semantic.broken.iter().any(|b| dataset.matches(b)),
            _ => !semantic.broken.is_empty(),
        };
        (!into_broken).then_some(problem)
    })
}

fn resolve_reported(
    reference: &syntax::Reference,
    semantic: &Semantic,
    names: &BTreeMap<String, MetricId>,
) -> Result<TableId, Problem> {
    let unknown = |help: String| (format!("refers to unknown field `{reference}`"), help);
    let all_fields = || {
        semantic
            .datasets
            .iter()
            .flat_map(|d| {
                d.fields
                    .iter()
                    .map(move |f| format!("{}.{}", d.name, f.name))
            })
            .collect::<Vec<_>>()
    };
    match reference.parts.as_slice() {
        [dataset, field] => {
            let Some((name, table)) = semantic
                .dataset_names
                .iter()
                .find(|(n, _)| dataset.matches(n))
            else {
                return Err(unknown(suggest(
                    "datasets",
                    &dataset.value,
                    semantic.dataset_names.keys().map(String::as_str),
                )));
            };
            let found = semantic
                .dataset(*table)
                .is_some_and(|d| d.fields.iter().any(|f| field.matches(&f.name)));
            if found {
                Ok(*table)
            } else {
                let fields = all_fields();
                Err(unknown(suggest(
                    &format!("fields of `{name}`"),
                    &reference.to_string(),
                    fields
                        .iter()
                        .map(String::as_str)
                        .filter(|f| f.starts_with(&format!("{name}."))),
                )))
            }
        }
        [field] => {
            let matches: Vec<TableId> = semantic
                .datasets
                .iter()
                .filter(|d| d.fields.iter().any(|f| field.matches(&f.name)))
                .map(|d| d.table)
                .collect();
            match matches.as_slice() {
                [table] => Ok(*table),
                [] if names.keys().any(|n| field.matches(n)) => Err((
                    format!("uses metric `{field}` like a field"),
                    "a metric that combines metrics needs a TIDEPOOL extension: {\"kind\": \"derived\"}".to_string(),
                )),
                [] => {
                    let fields = all_fields();
                    Err(unknown(suggest("fields", &field.value, fields.iter().map(String::as_str))))
                }
                several => {
                    let owners: Vec<String> = several
                        .iter()
                        .map(|t| format!("{}.{}", semantic.dataset_name(*t), field.value))
                        .collect();
                    Err((
                        format!("refers to `{field}`, which is in more than one dataset"),
                        format!("write {}", owners.join(" or ")),
                    ))
                }
            }
        }
        _ => Err(unknown(
            "write fields as `dataset.field` or `field`".to_string(),
        )),
    }
}

fn time_offset(
    cx: &mut Cx,
    at: &At,
    offset: &str,
    field: &str,
    body: &MetricBody,
    semantic: &Semantic,
) -> Option<TimeOffset> {
    let (file, ext_path, what) = (at.file, &at.ext, &at.what);
    let MetricBody::Base { anchor, .. } = body else {
        cx.push(
            Diagnostic::error(format!("{what}: time_offset is for base metrics"))
                .at(file, ext_path.clone())
                .help(
                    "shift the inputs instead: define the shifted base metric and derive from it",
                ),
        );
        return None;
    };
    let unit_at = offset.len().saturating_sub(1);
    let (amount, unit) = offset.split_at(unit_at);
    let unit = match unit {
        "d" => Some(OffsetUnit::Day),
        "w" => Some(OffsetUnit::Week),
        "m" => Some(OffsetUnit::Month),
        "q" => Some(OffsetUnit::Quarter),
        "y" => Some(OffsetUnit::Year),
        _ => None,
    };
    let (Ok(amount), Some(unit)) = (amount.parse::<i32>(), unit) else {
        cx.push(
            Diagnostic::error(format!("{what}: `{offset}` is not a time offset"))
                .at(file, ext_path.clone())
                .help("write a signed number and a unit (d, w, m, q, y), e.g. -1y"),
        );
        return None;
    };
    let (table, found) = match semantic.resolve_qualified(field) {
        Ok(found) => found,
        Err(None) => return None,
        Err(Some(message)) => {
            cx.push(
                Diagnostic::error(format!("{what}: time_field: {message}"))
                    .at(file, ext_path.clone()),
            );
            return None;
        }
    };
    if !found.is_time {
        cx.push(
            Diagnostic::error(format!("{what}: time_field `{field}` is not a time field"))
                .at(file, ext_path.clone()),
        );
        return None;
    }
    if !semantic.maybe_reaches(*anchor, table) {
        cx.push(
            Diagnostic::error(format!(
                "{what}: time_field `{field}` can't be reached from `{}`",
                semantic.dataset_name(*anchor)
            ))
            .at(file, ext_path.clone()),
        );
        return None;
    }
    Some(TimeOffset {
        amount,
        unit,
        field: FieldRef {
            table,
            field: found.name.clone(),
        },
    })
}

/// Derived metrics may not depend on themselves, however indirectly.
fn check_cycles(cx: &mut Cx, file: &Path, specs: &[ossie::Metric], metrics: &Metrics) {
    let inputs = |id: MetricId| match metrics.get(id).map(|m| &m.body) {
        Some(MetricBody::Derived { inputs, .. }) => inputs.clone(),
        _ => Vec::new(),
    };
    let mut reported = BTreeSet::new();
    for metric in &metrics.metrics {
        // depth-first from this metric, looking for a way back to it
        let mut stack: Vec<(MetricId, Vec<MetricId>)> = vec![(metric.id, vec![metric.id])];
        let mut seen = BTreeSet::new();
        while let Some((id, route)) = stack.pop() {
            for next in inputs(id) {
                if next == metric.id {
                    let members: BTreeSet<MetricId> = route.iter().copied().collect();
                    if reported.insert(members) {
                        let names: Vec<&str> = route
                            .iter()
                            .chain(std::iter::once(&metric.id))
                            .filter_map(|id| metrics.get(*id).map(|m| m.name.as_str()))
                            .collect();
                        let index = specs
                            .iter()
                            .position(|s| s.name == metric.name)
                            .unwrap_or(0);
                        cx.push(
                            Diagnostic::error(format!(
                                "derived metrics depend on each other in a loop: {}",
                                names.join(" → ")
                            ))
                            .at(
                                file,
                                YamlPath::root()
                                    .key("metrics")
                                    .index(index)
                                    .key("expression"),
                            ),
                        );
                    }
                } else if seen.insert(next) {
                    let mut longer = route.clone();
                    longer.push(next);
                    stack.push((next, longer));
                }
            }
        }
    }
}

/// Names in a base metric: fields of any dataset, `dataset.field` or a bare
/// field name that only one dataset has. Slots are positions in a flat list
/// of every field; only the types matter here.
struct FieldScope<'a> {
    semantic: &'a Semantic,
}

impl tidepool_expr::Scope for FieldScope<'_> {
    fn resolve(&self, parts: &[syntax::Name]) -> Result<(u32, Type), String> {
        let mut slot = 0u32;
        let mut found = None;
        for dataset in &self.semantic.datasets {
            for field in &dataset.fields {
                let hit = match parts {
                    [name] => name.matches(&field.name),
                    [d, name] => d.matches(&dataset.name) && name.matches(&field.name),
                    _ => false,
                };
                if hit && found.is_none() {
                    found = Some((slot, field.ty));
                }
                slot += 1;
            }
        }
        found.ok_or_else(|| "unknown field".to_string())
    }
}

/// Names in a derived metric: other metrics, with their types.
struct MetricScope<'a> {
    metrics: &'a [Metric],
}

impl tidepool_expr::Scope for MetricScope<'_> {
    fn resolve(&self, parts: &[syntax::Name]) -> Result<(u32, Type), String> {
        let [name] = parts else {
            return Err("a derived metric refers to metrics by name".to_string());
        };
        self.metrics
            .iter()
            .enumerate()
            .find(|(_, m)| name.matches(&m.name))
            .map(|(i, m)| (u32::try_from(i).unwrap_or(u32::MAX), m.ty))
            .ok_or_else(|| format!("unknown metric `{name}`"))
    }
}

/// Type derived metrics once their inputs have types: repeatedly, until no
/// more can be typed (what's left depends on a loop or a broken metric, both
/// already reported).
fn type_derived(cx: &mut Cx, file: &Path, specs: &[ossie::Metric], out: &mut Metrics) {
    let pending = |m: &Metric| matches!(m.body, MetricBody::Derived { .. }) && m.ty == Type::Null;
    loop {
        let ready: Vec<usize> = (0..out.metrics.len())
            .filter(|i| pending(&out.metrics[*i]))
            .filter(|i| match &out.metrics[*i].body {
                MetricBody::Derived { inputs, .. } => inputs.iter().all(|input| {
                    out.get(*input)
                        .is_some_and(|m| !pending(m) && m.ty != Type::Null)
                }),
                MetricBody::Base { .. } => false,
            })
            .collect();
        if ready.is_empty() {
            return;
        }
        for index in ready {
            let metric = &out.metrics[index];
            let MetricBody::Derived { expression, .. } = &metric.body else {
                continue;
            };
            let at = specs
                .iter()
                .position(|s| s.name == metric.name)
                .map(|i| {
                    let dialect = specs[i].expression.readable().map_or(0, |(d, _)| d);
                    YamlPath::root()
                        .key("metrics")
                        .index(i)
                        .key("expression")
                        .key("dialects")
                        .index(dialect)
                        .key("expression")
                })
                .unwrap_or_default();
            let checked = syntax::parse(expression).map_err(|e| e.0).and_then(|ast| {
                tidepool_expr::check_row(
                    &ast,
                    &MetricScope {
                        metrics: &out.metrics,
                    },
                )
            });
            match checked {
                Ok(expr) => out.metrics[index].ty = expr.ty(),
                Err(message) => {
                    let name = out.metrics[index].name.clone();
                    cx.push(Diagnostic::error(format!("metric `{name}`: {message}")).at(file, at));
                    // typed as float so it isn't retried; the error stops compilation
                    out.metrics[index].ty = Type::Float;
                }
            }
        }
    }
}
