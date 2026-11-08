//! What the dashboard editor knows about visuals, without a DOM: which
//! types fit the fields chosen, what a new one is called and how big it
//! starts, which fields and metrics can go together, and the query that
//! previews it. The compiler has the last word (the editor plans every
//! draft on the server); this is what keeps the choices offered sensible.

use std::collections::{BTreeMap, BTreeSet};

use tidepool_config::dashboard::{BrushAction, ClickAction, DimensionRef, Visual, VisualType};
use tidepool_config::format::NumberFormat;
use tidepool_protocol::{ModelInfo, QueryDimension, QueryRequest, QuerySort};

/// Every type, in the order the editor offers them.
pub const TYPES: [VisualType; 6] = [
    VisualType::Table,
    VisualType::Rows,
    VisualType::Filter,
    VisualType::Bar,
    VisualType::Line,
    VisualType::Kpi,
];

/// A type's name, as people read it.
#[must_use]
pub fn type_label(t: VisualType) -> &'static str {
    match t {
        VisualType::Table => "Table",
        VisualType::Filter => "Filter",
        VisualType::Bar => "Bar",
        VisualType::Line => "Line",
        VisualType::Kpi => "KPI",
        VisualType::Rows => "Rows",
    }
}

/// Whether a visual of type `t` can show these dimensions and metrics
/// (the compiler's rules per type).
#[must_use]
pub fn fits(t: VisualType, dimensions: &[DimensionRef], metrics: usize) -> bool {
    let dims = dimensions.len();
    match t {
        VisualType::Table => dims + metrics > 0,
        VisualType::Filter => {
            dims == 1 && metrics == 0 && dimensions.iter().all(|d| d.grain().is_none())
        }
        VisualType::Bar | VisualType::Line => dims == 1 && metrics > 0,
        VisualType::Kpi => dims == 0 && metrics > 0,
        VisualType::Rows => {
            dims > 0 && metrics == 0 && dimensions.iter().all(|d| d.grain().is_none())
        }
    }
}

/// The type a new visual gets: a filter for one dimension alone (what it's
/// for: picking its values), a table otherwise.
#[must_use]
pub fn default_type(dimensions: &[DimensionRef], metrics: usize) -> VisualType {
    if fits(VisualType::Filter, dimensions, metrics) {
        VisualType::Filter
    } else {
        VisualType::Table
    }
}

/// The cells a new visual of type `t` takes: (wide, tall).
#[must_use]
pub fn size_of(t: VisualType) -> (u16, u16) {
    match t {
        VisualType::Kpi => (3, 2),
        VisualType::Filter => (3, 4),
        VisualType::Rows => (12, 5),
        VisualType::Table | VisualType::Bar | VisualType::Line => (6, 4),
    }
}

/// The part of `dataset.field` people read.
#[must_use]
pub fn short(field: &str) -> String {
    field.split_once('.').map_or(field, |(_, f)| f).to_string()
}

/// What a visual is called on screen: its metrics by its first dimension,
/// or its dimensions.
#[must_use]
pub fn title(metrics: &[String], dimensions: &[DimensionRef]) -> String {
    let dims: Vec<String> = dimensions.iter().map(|d| short(d.field())).collect();
    match (metrics.is_empty(), dims.first()) {
        (true, _) => dims.join(", "),
        (false, None) => metrics.join(", "),
        (false, Some(first)) => format!("{} by {first}", metrics.join(", ")),
    }
}

/// `text` as an identifier: lowercase letters, digits and underscores,
/// starting with a letter.
fn identifier(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    let out = out.trim_end_matches('_').to_string();
    match out.chars().next() {
        Some(c) if c.is_ascii_alphabetic() => out,
        Some(_) => format!("d_{out}"),
        None => String::new(),
    }
}

/// `base`, or `base_2`, `base_3`, … : the first not in `taken`.
fn unique(base: &str, taken: &BTreeSet<String>) -> String {
    if !taken.contains(base) {
        return base.to_string();
    }
    (2..=taken.len() + 2)
        .map(|n| format!("{base}_{n}"))
        .find(|id| !taken.contains(id))
        .unwrap_or_else(|| base.to_string())
}

/// An id for a new visual, from what it shows (`revenue_by_region`),
/// unused on the dashboard.
#[must_use]
pub fn visual_id(
    metrics: &[String],
    dimensions: &[DimensionRef],
    taken: &BTreeSet<String>,
) -> String {
    let base = identifier(&title(metrics, dimensions).replace(", ", "_and_"));
    unique(if base.is_empty() { "visual" } else { &base }, taken)
}

/// A dashboard's file name (and URL) from its title, unused among `taken`.
#[must_use]
pub fn slug(name: &str, taken: &BTreeSet<String>) -> String {
    let base = identifier(name);
    unique(if base.is_empty() { "dashboard" } else { &base }, taken)
}

/// Make `visual` show `dimensions` and `metrics`: its type kept if it still
/// fits (a new visual's default otherwise), and what no longer applies
/// dropped.
pub fn set_fields(
    visual: &mut Visual,
    dimensions: Vec<DimensionRef>,
    metrics: Vec<String>,
    is_time: &dyn Fn(&str) -> bool,
) {
    visual.dimensions = dimensions;
    visual.metrics = metrics;
    let keep = fits(visual.visual_type, &visual.dimensions, visual.metrics.len());
    let t = if keep {
        visual.visual_type
    } else {
        default_type(&visual.dimensions, visual.metrics.len())
    };
    retype(visual, t, is_time);
}

/// Make `visual` a `t`: a click selects on bars and tables, brushing a
/// line over time selects a range, a comparison only stays on a KPI, and a
/// sort only by something it still shows.
pub fn retype(visual: &mut Visual, t: VisualType, is_time: &dyn Fn(&str) -> bool) {
    visual.visual_type = t;
    visual.on_click = matches!(t, VisualType::Bar | VisualType::Table)
        .then_some(ClickAction::Select)
        .filter(|_| !visual.dimensions.is_empty());
    visual.on_brush = (t == VisualType::Line
        && visual
            .dimensions
            .first()
            .is_some_and(|d| is_time(d.field())))
    .then_some(BrushAction::SelectRange);
    if t != VisualType::Kpi {
        visual.compare = None;
    }
    if matches!(t, VisualType::Kpi | VisualType::Filter) {
        visual.sort = None;
        visual.limit = None;
    }
    // rows come newest first
    if t == VisualType::Rows {
        visual.sort = None;
    }
    let shown = |by: &str| {
        visual.metrics.iter().any(|m| m == by) || visual.dimensions.iter().any(|d| d.field() == by)
    };
    if visual.sort.as_ref().is_some_and(|s| !shown(&s.by)) {
        visual.sort = None;
    }
    // a format only for a metric it still shows
    let metrics: Vec<String> = visual
        .metrics
        .iter()
        .chain(&visual.compare)
        .cloned()
        .collect();
    visual.format.retain(|m, _| metrics.contains(m));
}

/// How `metric` reads in `visual`: the visual's own format for it, else
/// the metric's (`own`), else as it comes. A format that doesn't read
/// (the compiler says so) counts as none.
#[must_use]
pub fn format_of(visual: &Visual, metric: &str, own: Option<&str>) -> NumberFormat {
    visual
        .format
        .get(metric)
        .map(String::as_str)
        .or(own)
        .and_then(|f| f.parse().ok())
        .unwrap_or_default()
}

/// How each metric `visual` shows reads, by name, as the dashboard would
/// show it: what [`format_of`] says, for those that say something.
#[must_use]
pub fn formats_of(visual: &Visual, own: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    visual
        .metrics
        .iter()
        .chain(&visual.compare)
        .filter_map(|m| {
            let format = format_of(visual, m, own.get(m).map(String::as_str));
            (format != NumberFormat::default()).then(|| (m.clone(), format.to_string()))
        })
        .collect()
}

/// Set how `metric` reads in `visual`. Saying what the metric says anyway
/// (`own`) removes the visual's format: the metric's own then applies,
/// and goes on applying if it changes.
pub fn set_format(visual: &mut Visual, metric: &str, format: &NumberFormat, own: Option<&str>) {
    let inherited = own
        .and_then(|f| f.parse::<NumberFormat>().ok())
        .unwrap_or_default();
    if *format == inherited {
        visual.format.shift_remove(metric);
    } else {
        visual.format.insert(metric.to_string(), format.to_string());
    }
}

/// The query a visual runs, without a selection: what the editor previews.
#[must_use]
pub fn request(visual: &Visual) -> QueryRequest {
    let dimensions: Vec<QueryDimension> = visual
        .dimensions
        .iter()
        .map(|d| QueryDimension {
            field: d.field().to_string(),
            grain: d.grain().map(|g| format!("{g:?}").to_lowercase()),
            level: None,
        })
        .collect();
    let sort = visual
        .sort
        .iter()
        .map(|s| QuerySort {
            column: dimensions.iter().find(|d| d.field == s.by).map_or_else(
                || s.by.clone(),
                |d| match &d.grain {
                    Some(grain) => format!("{}:{grain}", d.field),
                    None => d.field.clone(),
                },
            ),
            desc: s.desc,
        })
        .collect();
    QueryRequest {
        metrics: visual.metrics.clone(),
        dimensions,
        filters: Vec::new(),
        sort,
        // a preview needn't list a whole dimension
        limit: Some(visual.limit.map_or(200, |l| (l as usize).min(200))),
    }
}

/// Which tables each table reaches, many → one (itself included): what
/// decides whether a metric can be grouped by a field.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reach {
    reaches: BTreeMap<String, BTreeSet<String>>,
    /// Dataset name → its table.
    tables: BTreeMap<String, String>,
    /// Metric name → the tables its value comes from (none known: derived).
    anchors: BTreeMap<String, Vec<String>>,
}

impl Reach {
    #[must_use]
    pub fn of(model: &ModelInfo) -> Self {
        let tables: BTreeMap<String, String> = model
            .datasets
            .iter()
            .map(|d| (d.name.clone(), d.table.clone()))
            .collect();
        let mut reaches = BTreeMap::new();
        for table in tables.values() {
            let mut seen = BTreeSet::new();
            let mut stack = vec![table.clone()];
            while let Some(t) = stack.pop() {
                if seen.insert(t.clone()) {
                    stack.extend(
                        model
                            .relationships
                            .iter()
                            .filter(|r| r.from == t)
                            .map(|r| r.to.clone()),
                    );
                }
            }
            reaches.insert(table.clone(), seen);
        }
        let anchors = model
            .metrics
            .iter()
            .map(|m| (m.name.clone(), m.anchor.iter().cloned().collect()))
            .collect();
        Self {
            reaches,
            tables,
            anchors,
        }
    }

    fn table_of(&self, field: &str) -> Option<&String> {
        field.split_once('.').and_then(|(d, _)| self.tables.get(d))
    }

    fn reaches(&self, from: &str, to: &str) -> bool {
        self.reaches.get(from).is_some_and(|r| r.contains(to))
    }

    /// Whether `field` can be added to a visual of `dimensions` and
    /// `metrics`: every metric's table reaches it, or without metrics, one
    /// table reaches it and every dimension.
    #[must_use]
    pub fn field_fits(&self, field: &str, dimensions: &[DimensionRef], metrics: &[String]) -> bool {
        let Some(table) = self.table_of(field) else {
            return true;
        };
        if metrics.is_empty() {
            let mut wanted: Vec<&String> = dimensions
                .iter()
                .filter_map(|d| self.table_of(d.field()))
                .collect();
            wanted.push(table);
            return self
                .reaches
                .keys()
                .any(|from| wanted.iter().all(|to| self.reaches(from, to)));
        }
        metrics.iter().all(|m| {
            self.anchors
                .get(m)
                .is_none_or(|anchors| anchors.iter().all(|a| self.reaches(a, table)))
        })
    }

    /// Whether `metric` can be grouped by every one of `dimensions`.
    #[must_use]
    pub fn metric_fits(&self, metric: &str, dimensions: &[DimensionRef]) -> bool {
        let Some(anchors) = self.anchors.get(metric) else {
            return true;
        };
        dimensions.iter().all(|d| {
            self.table_of(d.field())
                .is_none_or(|t| anchors.iter().all(|a| self.reaches(a, t)))
        })
    }
}

#[cfg(test)]
mod tests {
    use tidepool_config::dashboard::{GridPosition, Sort};
    use tidepool_config::ossie::TimeGrain;
    use tidepool_protocol::{DatasetInfo, MetricInfo, RelationshipInfo};

    use super::*;

    fn field(f: &str) -> DimensionRef {
        DimensionRef::Field(f.into())
    }

    fn by_day(f: &str) -> DimensionRef {
        DimensionRef::Grained {
            field: f.into(),
            grain: TimeGrain::Day,
        }
    }

    fn visual(t: VisualType, dims: Vec<DimensionRef>, metrics: &[&str]) -> Visual {
        Visual {
            id: "v".into(),
            visual_type: t,
            title: None,
            description: None,
            dimensions: dims,
            metrics: metrics.iter().map(|m| (*m).to_string()).collect(),
            compare: None,
            sort: None,
            limit: None,
            on_click: None,
            on_brush: None,
            format: indexmap::IndexMap::new(),
            at: GridPosition {
                x: 0,
                y: 0,
                w: 6,
                h: 4,
            },
        }
    }

    fn timed(field: &str) -> bool {
        field == "orders.ordered_at"
    }

    #[test]
    fn one_dimension_alone_is_a_filter_and_anything_else_a_table() {
        assert_eq!(
            default_type(&[field("customers.region")], 0),
            VisualType::Filter
        );
        assert_eq!(
            default_type(&[field("customers.region")], 1),
            VisualType::Table
        );
        assert_eq!(
            default_type(&[field("a.b"), field("c.d")], 0),
            VisualType::Table
        );
        assert_eq!(default_type(&[], 2), VisualType::Table);
        // a grain makes periods, not values to pick
        assert_eq!(
            default_type(&[by_day("orders.ordered_at")], 0),
            VisualType::Table
        );
    }

    #[test]
    fn each_type_takes_what_the_compiler_allows() {
        let one = [field("customers.region")];
        let allowed = |dims: &[DimensionRef], metrics| {
            TYPES
                .iter()
                .filter(|t| fits(**t, dims, metrics))
                .map(|t| type_label(*t))
                .collect::<Vec<_>>()
        };
        assert_eq!(allowed(&one, 0), ["Table", "Rows", "Filter"]);
        assert_eq!(allowed(&one, 2), ["Table", "Bar", "Line"]);
        assert_eq!(allowed(&[], 1), ["Table", "KPI"]);
        assert_eq!(allowed(&[field("a.b"), field("c.d")], 1), ["Table"]);
        assert_eq!(allowed(&[field("a.b"), field("c.d")], 0), ["Table", "Rows"]);
        // rows show each row's own values: no grain
        let grained = [DimensionRef::Grained {
            field: "orders.ordered_at".into(),
            grain: tidepool_config::ossie::TimeGrain::Day,
        }];
        assert_eq!(allowed(&grained, 0), ["Table"]);
        assert_eq!(allowed(&[], 0), Vec::<&str>::new());
    }

    #[test]
    fn a_visual_formats_over_its_metrics_own() {
        let mut kpi = visual(VisualType::Kpi, vec![], &["revenue"]);
        let parse = |f: &str| match f.parse::<NumberFormat>() {
            Ok(f) => f,
            Err(e) => panic!("expected {f} to read: {e}"),
        };
        // nothing said: the metric's own, else as it comes
        assert_eq!(
            format_of(&kpi, "revenue", Some("currency:EUR")),
            parse("currency:EUR")
        );
        assert_eq!(format_of(&kpi, "revenue", None), NumberFormat::default());
        // compact on top of the metric's euros is the visual's own
        set_format(
            &mut kpi,
            "revenue",
            &parse("currency:EUR compact"),
            Some("currency:EUR"),
        );
        assert_eq!(
            kpi.format.get("revenue").map(String::as_str),
            Some("currency:EUR compact")
        );
        assert_eq!(
            format_of(&kpi, "revenue", Some("currency:EUR")),
            parse("currency:EUR compact")
        );
        // back to what the metric says: the visual says nothing again
        set_format(
            &mut kpi,
            "revenue",
            &parse("currency:EUR"),
            Some("currency:EUR"),
        );
        assert!(kpi.format.is_empty(), "{:?}", kpi.format);
        set_format(&mut kpi, "revenue", &NumberFormat::default(), None);
        assert!(kpi.format.is_empty(), "{:?}", kpi.format);
        // what the preview reads by: the visual's own over the metric's
        let own: BTreeMap<String, String> =
            [("revenue".to_string(), "currency:EUR".to_string())].into();
        set_format(&mut kpi, "revenue", &parse("compact"), Some("currency:EUR"));
        assert_eq!(
            formats_of(&kpi, &own),
            [("revenue".to_string(), "compact".to_string())].into()
        );
        assert_eq!(
            formats_of(&visual(VisualType::Kpi, vec![], &["revenue"]), &own),
            own
        );
        // a metric the visual stops showing loses its format
        set_format(&mut kpi, "revenue", &parse("compact"), None);
        set_fields(&mut kpi, vec![], vec!["order_count".into()], &|_| false);
        assert!(kpi.format.is_empty(), "{:?}", kpi.format);
    }

    #[test]
    fn ids_and_slugs_are_identifiers_not_taken_yet() {
        let taken: BTreeSet<String> = ["revenue_by_region".to_string()].into();
        assert_eq!(
            visual_id(&["revenue".into()], &[field("customers.region")], &taken),
            "revenue_by_region_2"
        );
        assert_eq!(
            visual_id(&[], &[field("customers.region")], &taken),
            "region"
        );
        assert_eq!(
            visual_id(&["revenue".into(), "order_count".into()], &[], &taken),
            "revenue_and_order_count"
        );
        assert_eq!(visual_id(&[], &[], &taken), "visual");
        let dashboards: BTreeSet<String> = ["sales".to_string()].into();
        assert_eq!(slug("Sales", &dashboards), "sales_2");
        assert_eq!(slug("Q3: north & south!", &dashboards), "q3_north_south");
        assert_eq!(slug("2026 review", &dashboards), "d_2026_review");
        assert_eq!(slug("…", &dashboards), "dashboard");
    }

    #[test]
    fn titles_say_what_is_shown() {
        assert_eq!(
            title(&["revenue".into()], &[field("customers.region")]),
            "revenue by region"
        );
        assert_eq!(
            title(
                &[],
                &[field("customers.region"), field("products.category")]
            ),
            "region, category"
        );
        assert_eq!(title(&["revenue".into()], &[]), "revenue");
    }

    #[test]
    fn changing_fields_keeps_a_type_that_still_fits() {
        let mut v = visual(
            VisualType::Bar,
            vec![field("customers.region")],
            &["revenue"],
        );
        set_fields(
            &mut v,
            vec![field("products.category")],
            vec!["revenue".into(), "order_count".into()],
            &timed,
        );
        assert_eq!(v.visual_type, VisualType::Bar);
        // a second dimension: not a bar any more
        set_fields(
            &mut v,
            vec![field("a.b"), field("c.d")],
            vec!["revenue".into()],
            &timed,
        );
        assert_eq!(v.visual_type, VisualType::Table);
        // a table still fits one dimension alone: it stays one
        set_fields(&mut v, vec![field("customers.region")], Vec::new(), &timed);
        assert_eq!(v.visual_type, VisualType::Table);
        assert_eq!(v.on_click, Some(ClickAction::Select));
        // a bar doesn't: it becomes what a new visual would, a filter
        let mut bar = visual(
            VisualType::Bar,
            vec![field("customers.region")],
            &["revenue"],
        );
        set_fields(
            &mut bar,
            vec![field("customers.region")],
            Vec::new(),
            &timed,
        );
        assert_eq!(bar.visual_type, VisualType::Filter);
        assert_eq!(bar.on_click, None);
    }

    #[test]
    fn a_new_type_brings_its_interactions_and_drops_the_rest() {
        let mut v = visual(
            VisualType::Table,
            vec![by_day("orders.ordered_at")],
            &["revenue"],
        );
        v.sort = Some(Sort {
            by: "revenue".into(),
            desc: true,
        });
        retype(&mut v, VisualType::Table, &timed);
        assert_eq!(v.on_click, Some(ClickAction::Select));
        retype(&mut v, VisualType::Line, &timed);
        assert_eq!(v.on_click, None);
        assert_eq!(v.on_brush, Some(BrushAction::SelectRange));
        assert!(v.sort.is_some(), "a line keeps its sort");
        let mut kpi = visual(VisualType::Table, Vec::new(), &["revenue"]);
        kpi.compare = Some("revenue_prev_year".into());
        kpi.sort = Some(Sort {
            by: "revenue".into(),
            desc: false,
        });
        retype(&mut kpi, VisualType::Kpi, &timed);
        assert_eq!(kpi.compare.as_deref(), Some("revenue_prev_year"));
        assert_eq!(kpi.sort, None);
        assert_eq!(kpi.on_click, None, "a total has nothing to click");
        retype(&mut kpi, VisualType::Table, &timed);
        assert_eq!(kpi.compare, None);
        // a sort by something no longer shown goes
        let mut gone = visual(
            VisualType::Table,
            vec![field("customers.region")],
            &["revenue"],
        );
        gone.sort = Some(Sort {
            by: "order_count".into(),
            desc: true,
        });
        retype(&mut gone, VisualType::Table, &timed);
        assert_eq!(gone.sort, None);
    }

    #[test]
    fn a_preview_is_the_visuals_query_with_its_sort_by_column() {
        let mut v = visual(
            VisualType::Line,
            vec![by_day("orders.ordered_at")],
            &["revenue"],
        );
        v.sort = Some(Sort {
            by: "orders.ordered_at".into(),
            desc: true,
        });
        v.limit = Some(1000);
        let request = request(&v);
        assert_eq!(request.metrics, ["revenue"]);
        assert_eq!(request.dimensions[0].grain.as_deref(), Some("day"));
        assert_eq!(request.sort[0].column, "orders.ordered_at:day");
        assert_eq!(request.limit, Some(200));
    }

    fn retail() -> ModelInfo {
        let dataset = |name: &str| DatasetInfo {
            name: name.into(),
            table: name.into(),
            description: None,
            ai_context: None,
            fields: Vec::new(),
        };
        let rel = |from: &str, to: &str| RelationshipInfo {
            name: format!("{from}_{to}"),
            from: from.into(),
            from_columns: Vec::new(),
            to: to.into(),
            to_columns: Vec::new(),
            role: None,
            ai_context: None,
        };
        let metric = |name: &str, anchor: Option<&str>| MetricInfo {
            format: None,
            name: name.into(),
            description: None,
            expression: String::new(),
            kind: if anchor.is_some() { "base" } else { "derived" }.into(),
            anchor: anchor.map(str::to_string),
            ignores: Vec::new(),
            time_offset: None,
            ai_context: None,
            dashboards: Vec::new(),
        };
        ModelInfo {
            datasets: ["orders", "customers", "products", "categories", "stores"]
                .map(dataset)
                .to_vec(),
            relationships: vec![
                rel("orders", "customers"),
                rel("orders", "products"),
                rel("products", "categories"),
            ],
            metrics: vec![
                metric("revenue", Some("orders")),
                metric("product_count", Some("products")),
                metric("avg_order_value", None),
            ],
            filter_direction: "one_to_many".into(),
            ..ModelInfo::default()
        }
    }

    #[test]
    fn fields_fit_the_metrics_tables_reach() {
        let reach = Reach::of(&retail());
        let revenue = ["revenue".to_string()];
        assert!(reach.field_fits("customers.region", &[], &revenue));
        assert!(reach.field_fits("categories.tree", &[], &revenue));
        assert!(!reach.field_fits("stores.city", &[], &revenue));
        // products don't reach customers
        assert!(!reach.field_fits("customers.region", &[], &["product_count".into()]));
        // derived: the server decides
        assert!(reach.field_fits("stores.city", &[], &["avg_order_value".into()]));
        assert!(reach.metric_fits("revenue", &[field("customers.region")]));
        assert!(!reach.metric_fits("product_count", &[field("customers.region")]));
    }

    #[test]
    fn without_metrics_fields_fit_when_one_table_relates_them() {
        let reach = Reach::of(&retail());
        assert!(reach.field_fits("stores.city", &[], &[]));
        assert!(reach.field_fits("products.category", &[field("customers.region")], &[]));
        assert!(!reach.field_fits("stores.city", &[field("customers.region")], &[]));
    }
}
