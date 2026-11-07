//! `dashboards/*.yaml` → [`Dashboard`]s.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use tidepool_config::dashboard::{
    DashboardFile, FilterWidget, Grain, Visual as VisualSpec, VisualType, WindowUnit, Zoom,
};
use tidepool_config::diag::{Diagnostic, suggest};
use tidepool_config::format::NumberFormat;
use tidepool_config::ossie::TimeGrain;
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

    let filters: Vec<Filter> = spec
        .filters
        .iter()
        .enumerate()
        .filter_map(|(i, f)| {
            filter(
                cx,
                file,
                &root.key("filters").index(i),
                f,
                semantic,
                metrics,
            )
        })
        .collect();

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

/// One filter: its field resolved, its widget fitting the field, and its
/// default and measure checked.
fn filter(
    cx: &mut Cx,
    file: &Path,
    path: &YamlPath,
    filter: &tidepool_config::dashboard::FilterSpec,
    semantic: &Semantic,
    metrics: &Metrics,
) -> Option<Filter> {
    let (table, field) = match semantic.resolve_qualified(&filter.field) {
        Ok(found) => found,
        Err(None) => return None,
        Err(Some(message)) => {
            cx.push(Diagnostic::error(format!("filter: {message}")).at(file, path.key("field")));
            return None;
        }
    };
    if filter.widget == FilterWidget::Search && field.ty != tidepool_expr::Type::String {
        cx.push(
            Diagnostic::error(format!(
                "`{}` is not a string field, so it can't be searched",
                filter.field
            ))
            .at(file, path.key("widget"))
            .help("a search box finds text in a string field: a log message, a name"),
        );
    }
    if filter.widget == FilterWidget::DateRange && !field.is_time {
        cx.push(
            Diagnostic::error(format!(
                "`{}` is not a time field, so it can't have a date_range",
                filter.field
            ))
            .at(file, path.key("widget")),
        );
    }
    // a hierarchy is shown as a tree, whatever the default says
    let widget = match (filter.widget, field.hierarchy().is_some()) {
        (FilterWidget::List, true) => FilterWidget::Tree,
        (FilterWidget::Tree, false) => {
            cx.push(
                Diagnostic::error(format!(
                    "`{}` is not a hierarchy, so it can't be shown as a tree",
                    filter.field
                ))
                .at(file, path.key("widget"))
                .help("a tree shows a dataset's hierarchy field; declare one with a TIDEPOOL extension on the dataset: {\"hierarchy\": {\"name\": …, \"parent\": …}}"),
            );
            filter.widget
        }
        (widget, _) => widget,
    };
    if let Some(window) = filter.default {
        check_default(cx, file, path, filter, field, window);
    }
    let measure = filter
        .measure
        .as_ref()
        .and_then(|name| measure(cx, file, path, filter, name, table, semantic, metrics));
    Some(Filter {
        field: FieldRef {
            table,
            field: field.name.clone(),
        },
        widget,
        default: filter.default,
        measure,
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
    let combinations_ok = dimensions.len() < spec.dimensions.len()
        || combinations_reach(cx, &site, spec, semantic, &dimensions);
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

    let (formats, formats_ok) = visual_formats(cx, &site, spec, metrics);
    let (steps, zoom, zoom_ok) = if dimensions_ok {
        visual_zoom(cx, &site, spec, semantic, &dimensions)
    } else {
        (Vec::new(), false, false)
    };

    let ok = metrics_ok
        && dimensions_ok
        && shape_ok
        && combinations_ok
        && reach_ok
        && sort_ok
        && formats_ok
        && zoom_ok;
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
        formats,
        steps,
        zoom,
        at: spec.at,
    })
}

/// The grains a field has, finest first: its own list, or all, and none
/// shorter than a day without a time of day. None for a time field that
/// holds something other than dates or times.
fn field_steps(field: &crate::ir::Field) -> Vec<TimeGrain> {
    if !matches!(field.ty, Type::Date | Type::Timestamp | Type::TimestampTz) {
        return Vec::new();
    }
    TimeGrain::ALL
        .into_iter()
        .filter(|g| field.grains.is_empty() || field.grains.contains(g))
        .filter(|g| !g.is_sub_day() || field.ty.has_time_of_day())
        .collect()
}

/// The steps of the visual's grained dimension, finest first, and whether a
/// viewer may zoom between them (plan §5.8). A filter's or a rows visual's
/// grain is an error of its own, so it isn't reported again here.
fn visual_zoom(
    cx: &mut Cx,
    site: &Site,
    spec: &VisualSpec,
    semantic: &Semantic,
    dimensions: &[crate::ir::Dimension],
) -> (Vec<TimeGrain>, bool, bool) {
    let grained: Vec<usize> = (0..dimensions.len())
        .filter(|i| dimensions[*i].grain.is_some())
        .collect();
    let refused_grain =
        matches!(spec.visual_type, VisualType::Filter | VisualType::Rows) && !grained.is_empty();
    let [one] = grained[..] else {
        return if refused_grain {
            (Vec::new(), false, false)
        } else {
            several_or_none(cx, site, spec, &grained)
        };
    };
    if refused_grain {
        return (Vec::new(), false, false);
    }
    let dimension = &dimensions[one];
    let Some(field) = semantic
        .dataset(dimension.field.table)
        .and_then(|d| d.field(&dimension.field.field))
    else {
        return (Vec::new(), false, false);
    };
    let name = spec.dimensions[one].field();
    let allowed = field_steps(field);
    if allowed.is_empty() {
        // a time field of numbers has fixed grains or none, as it always did
        let auto = dimension.grain == Some(Grain::Auto);
        let at = if auto {
            site.path.key("dimensions").index(one).key("grain")
        } else {
            site.path.key("zoom")
        };
        return if auto || spec.zoom.is_some() {
            cx.push(
                Diagnostic::error(format!(
                    "{}: `{name}` holds {}, not dates or times, so it can't be zoomed",
                    site.what,
                    field.ty.a()
                ))
                .at(site.file, at),
            );
            (Vec::new(), false, false)
        } else {
            (Vec::new(), false, true)
        };
    }
    let steps = match &spec.zoom {
        Some(Zoom::Steps(listed)) => {
            let Some(steps) = listed_steps(cx, site, field, name, listed, &allowed) else {
                return (Vec::new(), false, false);
            };
            steps
        }
        _ => allowed,
    };
    if let Some(Grain::Fixed(grain)) = dimension.grain
        && !steps.contains(&grain)
        && matches!(spec.zoom, Some(Zoom::Steps(_)))
        && field_steps(field).contains(&grain)
    {
        cx.push(
            Diagnostic::error(format!(
                "{}: `zoom` leaves out `{}`, the grain it starts at",
                site.what,
                grain.name()
            ))
            .at_key(site.file, site.path.key("zoom"))
            .help(format!("add `{}` to the list", grain.name())),
        );
        return (Vec::new(), false, false);
    }
    let zoom = !matches!(spec.zoom, Some(Zoom::Enabled(false)));
    (steps, zoom, true)
}

/// `zoom` or `auto` on a visual with no grained dimension, or with several.
fn several_or_none(
    cx: &mut Cx,
    site: &Site,
    spec: &VisualSpec,
    grained: &[usize],
) -> (Vec<TimeGrain>, bool, bool) {
    if grained.is_empty() {
        if spec.zoom.is_none() {
            return (Vec::new(), false, true);
        }
        cx.push(
            Diagnostic::error(format!(
                "{}: `zoom` changes the grain of a time dimension, and it has none with a grain",
                site.what
            ))
            .at_key(site.file, site.path.key("zoom"))
            .help("give a time dimension a grain: `{ field: dataset.field, grain: auto }`"),
        );
        return (Vec::new(), false, false);
    }
    let auto = grained
        .iter()
        .find(|i| spec.dimensions[**i].grain() == Some(Grain::Auto));
    let at = match (auto, &spec.zoom) {
        (Some(i), _) => site.path.key("dimensions").index(*i).key("grain"),
        (None, Some(_)) => site.path.key("zoom"),
        (None, None) => return (Vec::new(), false, true),
    };
    cx.push(
        Diagnostic::error(format!(
            "{}: only a visual with exactly one grained dimension can be zoomed or use `auto`, and it has {}",
            site.what,
            grained.len()
        ))
        .at(site.file, at),
    );
    (Vec::new(), false, false)
}

/// A `zoom:` list checked against what the field allows: finest first.
fn listed_steps(
    cx: &mut Cx,
    site: &Site,
    field: &crate::ir::Field,
    name: &str,
    listed: &[TimeGrain],
    allowed: &[TimeGrain],
) -> Option<Vec<TimeGrain>> {
    let at = site.path.key("zoom");
    if listed.is_empty() {
        cx.push(
            Diagnostic::error(format!("{}: `zoom` lists no grains", site.what))
                .at(site.file, at)
                .help("for no zoom, write `zoom: false`"),
        );
        return None;
    }
    let mut ok = true;
    for (i, step) in listed.iter().enumerate() {
        if allowed.contains(step) {
            continue;
        }
        ok = false;
        let message = if step.is_sub_day() && !field.ty.has_time_of_day() {
            format!(
                "`{name}` has no time of day, so it has no grain `{}`",
                step.name()
            )
        } else {
            format!("`{name}` doesn't allow grain `{}`", step.name())
        };
        let its: Vec<&str> = allowed.iter().map(|g| g.name()).collect();
        cx.push(
            Diagnostic::error(format!("{}: {message}", site.what))
                .at(site.file, at.index(i))
                .help(format!("its grains are {}", its.join(", "))),
        );
    }
    ok.then(|| {
        TimeGrain::ALL
            .into_iter()
            .filter(|g| listed.contains(g))
            .collect()
    })
}

/// The visual's formats: each for a metric it shows, each one that reads.
/// A metric that didn't resolve is that metric's error, not this one's.
fn visual_formats(
    cx: &mut Cx,
    site: &Site,
    spec: &VisualSpec,
    metrics: &Metrics,
) -> (Vec<(MetricId, String)>, bool) {
    let shown: Vec<&String> = spec.metrics.iter().chain(spec.compare.iter()).collect();
    let mut ok = true;
    let mut formats = Vec::new();
    for (name, format) in &spec.format {
        let at = site.path.key("format").key(name.as_str());
        if !shown.contains(&name) {
            ok = false;
            let help = if shown.is_empty() {
                "this visual shows no metrics, so it has nothing to format".to_string()
            } else {
                suggest("its metrics", name, shown.iter().map(|m| m.as_str()))
            };
            cx.push(
                Diagnostic::error(format!(
                    "{} formats `{name}`, which it doesn't show",
                    site.what
                ))
                .at_key(site.file, at)
                .help(help),
            );
            continue;
        }
        if let Err(problem) = format.parse::<NumberFormat>() {
            ok = false;
            cx.push(
                Diagnostic::error(format!("{}: format `{format}`: {problem}", site.what))
                    .at(site.file, at)
                    .help(tidepool_config::format::HELP),
            );
            continue;
        }
        if let Some(id) = metrics.by_name.get(name) {
            formats.push((*id, format.clone()));
        }
    }
    (formats, ok)
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
            let name = dimension.field();
            let problem = match grain {
                _ if !field.is_time => {
                    Some(format!("`{name}` is not a time field, so it has no grain"))
                }
                Grain::Fixed(g) if !field.grains.is_empty() && !field.grains.contains(&g) => {
                    Some(format!("`{name}` doesn't allow grain `{}`", g.name()))
                }
                Grain::Fixed(g) if g.is_sub_day() && !field.ty.has_time_of_day() => Some(format!(
                    "`{name}` has no time of day, so it has no grain `{}`",
                    g.name()
                )),
                _ => None,
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
        VisualType::Filter if dims != 1 => {
            problems.push((
                "dimensions",
                "a filter lists the values of exactly one dimension",
            ));
        }
        _ => {}
    }
    let grained_rows =
        spec.visual_type == VisualType::Rows && !rows_shape(cx, site, spec, &mut problems);
    let grained = spec.visual_type == VisualType::Filter
        && spec.dimensions.len() == 1
        && spec.dimensions[0].grain().is_some();
    if grained {
        cx.push(
            Diagnostic::error(format!(
                "{}: a filter lists a field's own values, so it has no grain",
                site.what
            ))
            .at(site.file, site.path.key("dimensions").index(0).key("grain"))
            .help("for a range of time, use a `date_range` filter beside the grid"),
        );
    }
    if spec.visual_type == VisualType::Filter && !spec.metrics.is_empty() {
        problems.push(("metrics", "a filter lists values, so it has no metrics"));
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
    // without metrics, only a table (of its dimensions' values) and a
    // filter have anything to show: the type is what's wrong
    let empty = match spec.visual_type {
        _ if !spec.metrics.is_empty() => None,
        VisualType::Kpi => Some((
            "a KPI shows metrics, and it has none",
            "give it `metrics: [...]`",
        )),
        VisualType::Bar | VisualType::Line => Some((
            "a chart shows metrics, and it has none",
            "give it `metrics: [...]`, or make it a `filter` or a `table` to list its dimension's values",
        )),
        VisualType::Table if dims == 0 => Some((
            "it has no dimensions and no metrics, so there's nothing to show",
            "give it `dimensions: [...]`, `metrics: [...]` or both",
        )),
        VisualType::Rows if dims == 0 => Some((
            "a rows visual shows columns, and it has none",
            "give it `dimensions: [...]`: the fields to show of each row",
        )),
        VisualType::Table | VisualType::Filter | VisualType::Rows => None,
    };
    if let Some((message, help)) = empty {
        cx.push(
            Diagnostic::error(format!("{}: {message}", site.what))
                .at(site.file, site.path.key("type"))
                .help(help),
        );
    }
    for (key, message) in &problems {
        cx.push(
            Diagnostic::error(format!("{}: {message}", site.what))
                .at_key(site.file, site.path.key(*key)),
        );
    }
    problems.is_empty() && empty.is_none() && !grained && !grained_rows
}

/// What only a `rows` visual can get wrong: a limit past [`ROWS_LIMIT`],
/// metrics or a sort (into `problems`), a grain (reported here; `false`).
fn rows_shape(
    cx: &mut Cx,
    site: &Site,
    spec: &VisualSpec,
    problems: &mut Vec<(&str, &str)>,
) -> bool {
    if spec.limit.is_some_and(|l| l as usize > ROWS_LIMIT) {
        problems.push(("limit", "a rows visual shows 1000 rows at most"));
    }
    if !spec.metrics.is_empty() {
        problems.push(("metrics", "a rows visual shows rows, so it has no metrics"));
    }
    if spec.sort.is_some() {
        problems.push((
            "sort",
            "a rows visual shows the newest rows first, so it has no sort",
        ));
    }
    let Some(at) = spec.dimensions.iter().position(|d| d.grain().is_some()) else {
        return true;
    };
    cx.push(
        Diagnostic::error(format!(
            "{}: a rows visual shows each row's own values, so its columns have no grain",
            site.what
        ))
        .at(
            site.file,
            site.path.key("dimensions").index(at).key("grain"),
        ),
    );
    false
}

/// Rows a `rows` visual shows at most (`tidepool_engine::MAX_ROWS`).
const ROWS_LIMIT: usize = 1000;

/// A table without metrics shows the combinations of its dimensions that
/// rows of one table have, so some table must reach every dimension.
fn combinations_reach(
    cx: &mut Cx,
    site: &Site,
    spec: &VisualSpec,
    semantic: &Semantic,
    dimensions: &[Dimension],
) -> bool {
    if !spec.metrics.is_empty() || dimensions.len() < 2 {
        return true;
    }
    let tables: Vec<TableId> = semantic.datasets.iter().map(|d| d.table).collect();
    let relates = |reach: &dyn Fn(TableId, TableId) -> bool| {
        tables
            .iter()
            .any(|from| dimensions.iter().all(|d| reach(*from, d.field.table)))
    };
    if relates(&|from, to| semantic.graph.reaches(from, to)) {
        return true;
    }
    if !relates(&|from, to| semantic.maybe_reaches(from, to)) {
        let mut names: Vec<&str> = dimensions
            .iter()
            .map(|d| semantic.dataset_name(d.field.table))
            .collect();
        names.sort_unstable();
        names.dedup();
        cx.push(
            Diagnostic::error(format!(
                "{}: no table relates `{}`, so their values can't be shown side by side",
                site.what,
                names.join("`, `")
            ))
            .at_key(site.file, site.path.key("dimensions"))
            .help("without metrics, a table shows the combinations of its dimensions that rows of one table have: one that reaches all of them through relationships"),
        );
    }
    false
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

/// A filter's measure: a metric that can be grouped by the filter's field,
/// on a filter that lists values.
#[allow(clippy::too_many_arguments)]
fn measure(
    cx: &mut Cx,
    file: &Path,
    path: &YamlPath,
    filter: &tidepool_config::dashboard::FilterSpec,
    name: &str,
    table: TableId,
    semantic: &Semantic,
    metrics: &Metrics,
) -> Option<MetricId> {
    let at = path.key("measure");
    let Some(id) = metrics.by_name.get(name).copied() else {
        cx.push(
            Diagnostic::error(format!(
                "filter `{}` measures unknown metric `{name}`",
                filter.field
            ))
            .at(file, at)
            .help(suggest(
                "metrics",
                name,
                metrics.by_name.keys().map(String::as_str),
            )),
        );
        return None;
    };
    if filter.widget == FilterWidget::DateRange {
        cx.push(
            Diagnostic::error(format!(
                "`{}` is a date_range, which shows no values to measure",
                filter.field
            ))
            .at(file, at),
        );
        return None;
    }
    let mut ok = true;
    for anchor in metrics.anchors(id) {
        if semantic.graph.reaches(anchor, table) {
            continue;
        }
        ok = false;
        if !semantic.maybe_reaches(anchor, table) {
            let anchor_name = semantic.dataset_name(anchor);
            cx.push(
                Diagnostic::error(format!(
                    "filter `{}`: metric `{name}` counts rows of `{anchor_name}`, which can't be grouped by `{}`",
                    filter.field, filter.field
                ))
                .at(file, at.clone())
                .help(format!(
                    "no relationship path leads from `{anchor_name}` to `{}`",
                    semantic.dataset_name(table)
                )),
            );
        }
    }
    ok.then_some(id)
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
                "`{}` is a {}; only a date_range can start on a window",
                filter.field,
                if field.hierarchy().is_some() {
                    "tree"
                } else {
                    "list"
                }
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
