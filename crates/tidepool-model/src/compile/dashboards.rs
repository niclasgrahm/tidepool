//! `dashboards/*.yaml` → [`Dashboard`]s.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use tidepool_config::dashboard::{
    DashboardFile, FilterWidget, Visual as VisualSpec, VisualType, WindowUnit,
};
use tidepool_config::diag::{Diagnostic, suggest};
use tidepool_config::source::YamlPath;

use super::metrics::Metrics;
use super::semantic::Semantic;
use super::tables::Tables;
use super::{Cx, IDENTIFIER_HELP, is_identifier};
use crate::ir::{
    Dashboard, Dimension, FieldRef, Filter, MetricId, SortKey, TableId, Type, Visual, VisualSort,
};

pub(super) fn compile(
    cx: &mut Cx,
    _tables: &Tables,
    semantic: &Semantic,
    metrics: &Metrics,
) -> Vec<Dashboard> {
    if cx.set.model.is_none() {
        return Vec::new();
    }
    let mut dashboards = Vec::new();
    for parsed in &cx.set.dashboards {
        let slug = parsed.stem();
        if !is_identifier(&slug) {
            cx.push(
                Diagnostic::error(format!(
                    "`{slug}` can't be used as a dashboard id (it's the file name)"
                ))
                .at_location(&parsed.file, None)
                .help(IDENTIFIER_HELP),
            );
            continue;
        }
        let file = parsed.file.clone();
        if let Some(dashboard) = dashboard(cx, &file, &slug, &parsed.value, semantic, metrics) {
            dashboards.push(dashboard);
        }
    }
    dashboards
}

fn dashboard(
    cx: &mut Cx,
    file: &Path,
    slug: &str,
    spec: &DashboardFile,
    semantic: &Semantic,
    metrics: &Metrics,
) -> Option<Dashboard> {
    let root = YamlPath::root();
    let errors_before = cx.diagnostics.iter().filter(|d| d.is_error()).count();

    let mut filters = Vec::new();
    for (i, filter) in spec.filters.iter().enumerate() {
        let path = root.key("filters").index(i);
        match semantic.resolve_qualified(&filter.field) {
            Err(None) => {}
            Err(Some(message)) => {
                cx.push(
                    Diagnostic::error(format!("filter: {message}")).at(file, path.key("field")),
                );
            }
            Ok((table, field)) => {
                if filter.widget == FilterWidget::DateRange && !field.is_time {
                    cx.push(
                        Diagnostic::error(format!(
                            "`{}` is not a time field, so it can't have a date_range",
                            filter.field
                        ))
                        .at(file, path.key("widget")),
                    );
                }
                if let Some(window) = filter.default {
                    check_default(cx, file, &path, filter, field, window);
                }
                filters.push(Filter {
                    field: FieldRef {
                        table,
                        field: field.name.clone(),
                    },
                    widget: filter.widget,
                    default: filter.default,
                });
            }
        }
    }

    let mut ids = BTreeSet::new();
    let mut visuals = Vec::new();
    for (i, spec_visual) in spec.visuals.iter().enumerate() {
        let path = root.key("visuals").index(i);
        if !ids.insert(spec_visual.id.clone()) {
            cx.push(
                Diagnostic::error(format!("visual id `{}` is used twice", spec_visual.id))
                    .at(file, path.key("id")),
            );
            continue;
        }
        if let Some(visual) = visual(cx, file, &path, spec_visual, semantic, metrics) {
            visuals.push(visual);
        }
    }

    // layout: inside the grid, and not on top of each other
    for (i, visual) in spec.visuals.iter().enumerate() {
        let at = visual.at;
        let path = root.key("visuals").index(i).key("at");
        if at.w == 0 || at.h == 0 {
            cx.push(
                Diagnostic::error(format!("visual `{}` has no size", visual.id)).at(file, path),
            );
        } else if at.x + at.w > spec.layout.columns {
            cx.push(
                Diagnostic::error(format!(
                    "visual `{}` ends at column {}, past the grid's {}",
                    visual.id,
                    at.x + at.w,
                    spec.layout.columns
                ))
                .at(file, path),
            );
        } else if let Some(other) = spec.visuals[..i].iter().find(|o| {
            let b = o.at;
            at.x < b.x + b.w && b.x < at.x + at.w && at.y < b.y + b.h && b.y < at.y + at.h
        }) {
            cx.push(
                Diagnostic::error(format!("visual `{}` overlaps `{}`", visual.id, other.id))
                    .at(file, path),
            );
        }
    }

    let errors_after = cx.diagnostics.iter().filter(|d| d.is_error()).count();
    (errors_after == errors_before).then(|| Dashboard {
        id: cx.ids.dashboard(slug),
        slug: slug.to_string(),
        name: spec.name.clone(),
        columns: spec.layout.columns,
        row_height: spec.layout.row_height,
        filters,
        visuals,
    })
}

/// Where a visual's diagnostics go.
struct Site<'a> {
    file: &'a Path,
    path: &'a YamlPath,
    what: String,
}

fn visual(
    cx: &mut Cx,
    file: &Path,
    path: &YamlPath,
    spec: &VisualSpec,
    semantic: &Semantic,
    metrics: &Metrics,
) -> Option<Visual> {
    let site = Site {
        file,
        path,
        what: format!("visual `{}`", spec.id),
    };
    let (metric_ids, compare, metrics_ok) = visual_metrics(cx, &site, spec, metrics);
    let (dimensions, dimension_index, dimensions_ok) = visual_dimensions(cx, &site, spec, semantic);
    let shape_ok = visual_shape(cx, &site, spec, semantic, &dimensions);
    let reach_ok = visual_reach(
        cx,
        &site,
        spec,
        semantic,
        metrics,
        &metric_ids
            .iter()
            .chain(compare.iter())
            .copied()
            .collect::<Vec<_>>(),
        &dimensions,
        &dimension_index,
    );
    // a sort is only checkable when everything it could name resolved
    let all_resolved =
        metric_ids.len() == spec.metrics.len() && dimensions.len() == spec.dimensions.len();
    let (sort, sort_ok) = if all_resolved {
        visual_sort(cx, &site, spec, &metric_ids)
    } else {
        (None, false)
    };

    let ok = metrics_ok && dimensions_ok && shape_ok && reach_ok && sort_ok;
    ok.then(|| Visual {
        id: spec.id.clone(),
        visual_type: spec.visual_type,
        dimensions,
        metrics: metric_ids,
        compare,
        sort,
        limit: spec.limit,
        on_click: spec.on_click.is_some(),
        on_brush: spec.on_brush.is_some(),
        at: spec.at,
    })
}

/// The visual's metrics and comparison metric, resolved.
fn visual_metrics(
    cx: &mut Cx,
    site: &Site,
    spec: &VisualSpec,
    metrics: &Metrics,
) -> (Vec<MetricId>, Option<MetricId>, bool) {
    let mut ok = true;
    let mut lookup = |cx: &mut Cx, name: &str, at: YamlPath| -> Option<MetricId> {
        let found = metrics.by_name.get(name).copied();
        if found.is_none() {
            ok = false;
            cx.push(
                Diagnostic::error(format!("{} uses unknown metric `{name}`", site.what))
                    .at(site.file, at)
                    .help(suggest(
                        "metrics",
                        name,
                        metrics.by_name.keys().map(String::as_str),
                    )),
            );
        }
        found
    };
    let ids: Vec<MetricId> = spec
        .metrics
        .iter()
        .enumerate()
        .filter_map(|(i, name)| lookup(cx, name, site.path.key("metrics").index(i)))
        .collect();
    let compare = spec
        .compare
        .as_ref()
        .and_then(|name| lookup(cx, name, site.path.key("compare")));
    if spec.metrics.is_empty() {
        ok = false;
        cx.push(
            Diagnostic::error(format!("{} shows no metrics", site.what))
                .at_key(site.file, site.path.key("metrics")),
        );
    }
    (ids, compare, ok)
}

/// The visual's dimensions, resolved, with where each is in the file (some
/// may have failed).
fn visual_dimensions(
    cx: &mut Cx,
    site: &Site,
    spec: &VisualSpec,
    semantic: &Semantic,
) -> (Vec<Dimension>, Vec<usize>, bool) {
    let mut ok = true;
    let mut dimensions = Vec::new();
    let mut index = Vec::new();
    for (i, dimension) in spec.dimensions.iter().enumerate() {
        let at = site.path.key("dimensions").index(i);
        let (table, field) = match semantic.resolve_qualified(dimension.field()) {
            Ok(found) => found,
            Err(message) => {
                ok = false;
                if let Some(message) = message {
                    cx.push(
                        Diagnostic::error(format!("{}: {message}", site.what)).at(site.file, at),
                    );
                }
                continue;
            }
        };
        if let Some(grain) = dimension.grain() {
            let problem = if !field.is_time {
                Some(format!(
                    "`{}` is not a time field, so it has no grain",
                    dimension.field()
                ))
            } else if !field.grains.is_empty() && !field.grains.contains(&grain) {
                Some(format!(
                    "`{}` doesn't allow grain `{grain:?}`",
                    dimension.field()
                ))
            } else {
                None
            };
            if let Some(problem) = problem {
                ok = false;
                cx.push(
                    Diagnostic::error(format!("{}: {problem}", site.what))
                        .at(site.file, at.key("grain")),
                );
            }
        }
        index.push(i);
        dimensions.push(Dimension {
            field: FieldRef {
                table,
                field: field.name.clone(),
            },
            grain: dimension.grain(),
        });
    }
    (dimensions, index, ok)
}

/// Rules per visual type: how many dimensions, and which interactions.
fn visual_shape(
    cx: &mut Cx,
    site: &Site,
    spec: &VisualSpec,
    semantic: &Semantic,
    dimensions: &[Dimension],
) -> bool {
    let mut problems: Vec<(&str, &str)> = Vec::new();
    let dims = spec.dimensions.len();
    match spec.visual_type {
        VisualType::Kpi if dims > 0 => {
            problems.push(("dimensions", "a KPI shows totals, so it has no dimensions"));
        }
        VisualType::Bar | VisualType::Line if dims != 1 => {
            problems.push((
                "dimensions",
                "bar and line charts have exactly one dimension",
            ));
        }
        _ => {}
    }
    if spec.compare.is_some() && spec.visual_type != VisualType::Kpi {
        problems.push(("compare", "only KPI tiles have a `compare`"));
    }
    if spec.on_click.is_some() && !matches!(spec.visual_type, VisualType::Bar | VisualType::Table) {
        problems.push(("on_click", "only bars and tables select on click"));
    }
    // only checkable when the dimension resolved; otherwise that's the error
    if spec.on_brush.is_some() && dimensions.len() == spec.dimensions.len() {
        let time_axis = spec.visual_type == VisualType::Line
            && dimensions.first().is_some_and(|d| {
                semantic
                    .dataset(d.field.table)
                    .and_then(|ds| ds.field(&d.field.field))
                    .is_some_and(|f| f.is_time)
            });
        if !time_axis {
            problems.push((
                "on_brush",
                "only line charts over a time field can be brushed",
            ));
        }
    }
    for (key, message) in &problems {
        cx.push(
            Diagnostic::error(format!("{}: {message}", site.what))
                .at_key(site.file, site.path.key(*key)),
        );
    }
    problems.is_empty()
}

/// Every dimension must be reachable from every metric's anchor. One error
/// per dimension and anchor, naming all the metrics it affects.
#[allow(clippy::too_many_arguments)]
fn visual_reach(
    cx: &mut Cx,
    site: &Site,
    spec: &VisualSpec,
    semantic: &Semantic,
    metrics: &Metrics,
    metric_ids: &[MetricId],
    dimensions: &[Dimension],
    dimension_index: &[usize],
) -> bool {
    let mut ok = true;
    for (dimension, i) in dimensions.iter().zip(dimension_index) {
        let mut unreachable: BTreeMap<TableId, Vec<&str>> = BTreeMap::new();
        for id in metric_ids {
            for anchor in metrics.anchors(*id) {
                if semantic.graph.reaches(anchor, dimension.field.table) {
                    continue;
                }
                ok = false;
                if !semantic.maybe_reaches(anchor, dimension.field.table) {
                    let name = metrics.get(*id).map_or("?", |m| m.name.as_str());
                    unreachable.entry(anchor).or_default().push(name);
                }
            }
        }
        for (anchor, names) in unreachable {
            let anchor_name = semantic.dataset_name(anchor);
            let listed: Vec<String> = names.iter().map(|n| format!("`{n}`")).collect();
            let subject = if names.len() == 1 {
                format!("metric {} counts", listed.join(""))
            } else {
                format!("metrics {} count", listed.join(", "))
            };
            cx.push(
                Diagnostic::error(format!(
                    "{}: {subject} rows of `{anchor_name}`, which can't be grouped by `{}`",
                    site.what,
                    spec.dimensions[*i].field()
                ))
                .at(site.file, site.path.key("dimensions").index(*i))
                .help(format!(
                    "no relationship path leads from `{anchor_name}` to `{}`",
                    semantic.dataset_name(dimension.field.table)
                )),
            );
        }
    }
    ok
}

/// The sort key, which must be one of the visual's metrics or dimensions.
fn visual_sort(
    cx: &mut Cx,
    site: &Site,
    spec: &VisualSpec,
    metric_ids: &[MetricId],
) -> (Option<VisualSort>, bool) {
    let Some(sort) = &spec.sort else {
        return (None, true);
    };
    let key = spec
        .metrics
        .iter()
        .position(|m| *m == sort.by)
        .and_then(|i| metric_ids.get(i).copied().map(SortKey::Metric))
        .or_else(|| {
            spec.dimensions
                .iter()
                .position(|d| d.field() == sort.by)
                .map(SortKey::Dimension)
        });
    let Some(key) = key else {
        let mut options: Vec<&str> = spec.metrics.iter().map(String::as_str).collect();
        options.extend(
            spec.dimensions
                .iter()
                .map(tidepool_config::dashboard::DimensionRef::field),
        );
        cx.push(
            Diagnostic::error(format!(
                "{} sorts by `{}`, which it doesn't show",
                site.what, sort.by
            ))
            .at(site.file, site.path.key("sort").key("by"))
            .help(suggest("its metrics and dimensions", &sort.by, options)),
        );
        return (None, false);
    };
    (
        Some(VisualSort {
            key,
            desc: sort.desc,
        }),
        true,
    )
}

/// A filter's default window: on a `date_range`, at least one period long,
/// and in units its field has.
fn check_default(
    cx: &mut Cx,
    file: &Path,
    path: &YamlPath,
    filter: &tidepool_config::dashboard::FilterSpec,
    field: &crate::ir::Field,
    window: tidepool_config::dashboard::RelativeWindow,
) {
    let at = path.key("default");
    let problem = if filter.widget != FilterWidget::DateRange {
        Some((
            at,
            format!(
                "`{}` is a list; only a date_range can start on a window",
                filter.field
            ),
        ))
    } else if window.last == 0 {
        Some((
            at.key("last"),
            "a window needs at least one period: `last` is 0".into(),
        ))
    } else if field.ty == Type::Date && matches!(window.unit, WindowUnit::Minute | WindowUnit::Hour)
    {
        Some((
            at.key("unit"),
            format!(
                "`{}` is a date, so it has no {}s",
                filter.field,
                if window.unit == WindowUnit::Hour {
                    "hour"
                } else {
                    "minute"
                }
            ),
        ))
    } else if field.is_time && !matches!(field.ty, Type::Date | Type::Timestamp | Type::TimestampTz)
    {
        Some((
            at,
            format!(
                "`{}` holds {}, not dates or times, so it has no window",
                filter.field,
                field.ty.a()
            ),
        ))
    } else {
        None
    };
    if let Some((path, message)) = problem {
        cx.push(Diagnostic::error(message).at(file, path));
    }
}
