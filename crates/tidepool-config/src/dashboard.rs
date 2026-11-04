//! `dashboards/<id>.yaml`: a dashboard. Its id is the file name.

use std::fmt::Write as _;

use indexmap::IndexMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ossie::TimeGrain;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DashboardFile {
    /// The title shown in the UI.
    pub name: String,
    #[serde(default)]
    pub layout: Layout,
    /// Filter widgets, shown above the visuals.
    #[serde(default)]
    pub filters: Vec<FilterSpec>,
    #[serde(default)]
    pub visuals: Vec<Visual>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    #[serde(default = "default_columns")]
    pub columns: u16,
    /// Height of one grid row, in pixels.
    #[serde(default = "default_row_height")]
    pub row_height: u16,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            columns: default_columns(),
            row_height: default_row_height(),
        }
    }
}

fn default_columns() -> u16 {
    12
}

fn default_row_height() -> u16 {
    80
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FilterSpec {
    /// `dataset.field`.
    pub field: String,
    #[serde(default)]
    pub widget: FilterWidget,
    /// A `date_range`'s selection when the dashboard is opened: a window
    /// relative to now, such as `{ last: 7, unit: day }`. It moves as time
    /// passes; a viewer can change or clear it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<RelativeWindow>,
    /// A metric shown beside each value instead of its row count, computed
    /// for that value under the rest of the selection (OEE per line).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measure: Option<String>,
}

/// Whole periods relative to now: the `last` ones of `unit`, ending with the
/// current one, or `ago` periods before it. `{ last: 7, unit: day }` is today
/// and the six days before it; `{ last: 1, unit: month, ago: 1 }` is last
/// month. Periods start where time grains do (weeks on Monday), in UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RelativeWindow {
    pub last: u32,
    pub unit: WindowUnit,
    #[serde(default)]
    pub ago: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowUnit {
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Quarter,
    Year,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FilterWidget {
    /// A searchable list of values.
    #[default]
    List,
    /// A from/to range, for time fields.
    DateRange,
    /// An expandable tree, for a dataset's hierarchy field (the default
    /// for one).
    Tree,
    /// A text box, for a string field: the rows whose value contains what
    /// is typed are selected, ignoring the case of ASCII letters.
    Search,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VisualType {
    Kpi,
    Bar,
    Line,
    Table,
    /// The values of one dimension as a list on the grid. A click selects a
    /// value, as in a filter on the rail. It has no metrics.
    Filter,
    /// The newest rows of one table that pass the selection, newest first:
    /// a log tail. Its dimensions are the columns; `limit` is how many rows
    /// (100 when not given, 1000 at most). It has no metrics.
    Rows,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Visual {
    /// Unique within the dashboard.
    pub id: String,
    #[serde(rename = "type")]
    pub visual_type: VisualType,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dimensions: Vec<DimensionRef>,
    /// Metrics by name. A filter has none. A table without metrics shows
    /// each combination of its dimensions that the data has.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metrics: Vec<String>,
    /// KPI tiles: a second metric shown as the comparison.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compare: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<Sort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Bars and table rows: what a click does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_click: Option<ClickAction>,
    /// Line charts: what brushing the time axis does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_brush: Option<BrushAction>,
    /// How each metric shows in this visual, by the name of the metric:
    /// `{ revenue: compact }`. It replaces the `format` of the metric here.
    /// It changes only what shows on the screen, not the value.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub format: IndexMap<String, String>,
    pub at: GridPosition,
}

/// A dimension: `dataset.field`, or a time field at a grain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum DimensionRef {
    Field(String),
    Grained { field: String, grain: TimeGrain },
}

impl DimensionRef {
    #[must_use]
    pub fn field(&self) -> &str {
        match self {
            Self::Field(field) | Self::Grained { field, .. } => field,
        }
    }

    #[must_use]
    pub fn grain(&self) -> Option<TimeGrain> {
        match self {
            Self::Field(_) => None,
            Self::Grained { grain, .. } => Some(*grain),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Sort {
    /// A metric or a dimension of the visual.
    pub by: String,
    #[serde(default)]
    pub desc: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClickAction {
    Select,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BrushAction {
    SelectRange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GridPosition {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl DashboardFile {
    /// Read a dashboard file's text.
    ///
    /// # Errors
    /// The text isn't a dashboard file: serde's message, with its line.
    pub fn from_yaml(text: &str) -> Result<Self, String> {
        serde_norway::from_str(text).map_err(|err| err.to_string())
    }

    /// The file's text, written the way the examples are: one key per line,
    /// short things (`at`, lists of names, a sort) inline. Reading it back
    /// gives the same dashboard. Comments aren't kept: there are none here.
    #[must_use]
    pub fn to_yaml(&self) -> String {
        let mut out = format!("name: {}\n", scalar(&self.name));
        let _ = writeln!(
            out,
            "layout: {{ columns: {}, row_height: {} }}",
            self.layout.columns, self.layout.row_height
        );
        if !self.filters.is_empty() {
            out += "filters:\n";
            for filter in &self.filters {
                let _ = writeln!(out, "  - field: {}", scalar(&filter.field));
                if filter.widget != FilterWidget::List {
                    let _ = writeln!(out, "    widget: {}", word(&filter.widget));
                }
                if let Some(window) = filter.default {
                    let ago = if window.ago == 0 {
                        String::new()
                    } else {
                        format!(", ago: {}", window.ago)
                    };
                    let _ = writeln!(
                        out,
                        "    default: {{ last: {}, unit: {}{ago} }}",
                        window.last,
                        word(&window.unit)
                    );
                }
                if let Some(measure) = &filter.measure {
                    let _ = writeln!(out, "    measure: {}", scalar(measure));
                }
            }
        }
        if self.visuals.is_empty() {
            out += "visuals: []\n";
            return out;
        }
        out += "visuals:\n";
        for visual in &self.visuals {
            let _ = writeln!(out, "  - id: {}", scalar(&visual.id));
            let _ = writeln!(out, "    type: {}", word(&visual.visual_type));
            if !visual.dimensions.is_empty() {
                let dimensions: Vec<String> = visual
                    .dimensions
                    .iter()
                    .map(|d| match d {
                        DimensionRef::Field(field) => scalar(field),
                        DimensionRef::Grained { field, grain } => {
                            format!("{{ field: {}, grain: {} }}", scalar(field), word(grain))
                        }
                    })
                    .collect();
                let _ = writeln!(out, "    dimensions: [{}]", dimensions.join(", "));
            }
            if !visual.metrics.is_empty() {
                let metrics: Vec<String> = visual.metrics.iter().map(|m| scalar(m)).collect();
                let _ = writeln!(out, "    metrics: [{}]", metrics.join(", "));
            }
            if let Some(compare) = &visual.compare {
                let _ = writeln!(out, "    compare: {}", scalar(compare));
            }
            if let Some(sort) = &visual.sort {
                let desc = if sort.desc { ", desc: true" } else { "" };
                let _ = writeln!(out, "    sort: {{ by: {}{desc} }}", scalar(&sort.by));
            }
            if let Some(limit) = visual.limit {
                let _ = writeln!(out, "    limit: {limit}");
            }
            if let Some(action) = &visual.on_click {
                let _ = writeln!(out, "    on_click: {}", word(action));
            }
            if let Some(action) = &visual.on_brush {
                let _ = writeln!(out, "    on_brush: {}", word(action));
            }
            if !visual.format.is_empty() {
                let formats: Vec<String> = visual
                    .format
                    .iter()
                    .map(|(metric, format)| format!("{}: {}", scalar(metric), scalar(format)))
                    .collect();
                let _ = writeln!(out, "    format: {{ {} }}", formats.join(", "));
            }
            let at = visual.at;
            let _ = writeln!(
                out,
                "    at: {{ x: {}, y: {}, w: {}, h: {} }}",
                at.x, at.y, at.w, at.h
            );
        }
        out
    }
}

/// An enum's name as it's written (`date_range`).
fn word(value: &impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// A string as YAML: plain when that reads back as the same string (names,
/// `dataset.field`, titles), double-quoted otherwise. JSON's escapes are
/// YAML's too.
fn scalar(text: &str) -> String {
    let plain = text
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && !text.ends_with(' ')
        && !text.contains("  ")
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | ' ' | '/'))
        && !matches!(
            text.to_ascii_lowercase().as_str(),
            "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "y" | "n"
        );
    if plain {
        text.to_string()
    } else {
        serde_json::to_string(text).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/retail/dashboards")
            .join(name);
        match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => panic!("reading {}: {err}", path.display()),
        }
    }

    fn read(text: &str) -> DashboardFile {
        match DashboardFile::from_yaml(text) {
            Ok(file) => file,
            Err(err) => panic!("not a dashboard file ({err}):\n{text}"),
        }
    }

    #[test]
    fn the_examples_read_back_the_same() {
        for name in ["sales.yaml", "sales_recent.yaml"] {
            let file = read(&example(name));
            assert_eq!(read(&file.to_yaml()), file, "{name}");
        }
    }

    #[test]
    fn it_is_written_as_the_examples_are() {
        let file = read(&example("sales_recent.yaml"));
        assert_eq!(
            file.to_yaml(),
            "name: Recent sales
layout: { columns: 12, row_height: 80 }
filters:
  - field: orders.ordered_at
    widget: date_range
    default: { last: 7, unit: day }
  - field: customers.region
visuals:
  - id: kpi_revenue
    type: kpi
    metrics: [revenue]
    at: { x: 0, y: 0, w: 3, h: 2 }
  - id: kpi_orders
    type: kpi
    metrics: [order_count]
    at: { x: 3, y: 0, w: 3, h: 2 }
  - id: by_hour
    type: line
    dimensions: [{ field: orders.ordered_at, grain: hour }]
    metrics: [revenue, order_count]
    on_brush: select_range
    at: { x: 0, y: 2, w: 12, h: 4 }
  - id: category
    type: filter
    dimensions: [products.category]
    at: { x: 0, y: 6, w: 3, h: 4 }
  - id: who_buys_what
    type: table
    dimensions: [customers.region, products.category]
    at: { x: 3, y: 6, w: 9, h: 4 }
"
        );
    }

    #[test]
    fn awkward_names_are_quoted_and_read_back() {
        for name in [
            "Sales: Q1",
            "true",
            "No",
            "#1",
            "",
            " padded",
            "a, b",
            "[x]",
            "été",
            "say \"hi\"",
            "back\\slash",
            "12",
            "two  spaces",
            "line\nbreak",
            "- dash",
            "a #b",
        ] {
            let file = DashboardFile {
                name: name.to_string(),
                layout: Layout::default(),
                filters: Vec::new(),
                visuals: vec![Visual {
                    id: "v".into(),
                    visual_type: VisualType::Table,
                    dimensions: vec![DimensionRef::Field(name.to_string())],
                    metrics: vec![name.to_string()],
                    compare: None,
                    sort: Some(Sort {
                        by: name.to_string(),
                        desc: false,
                    }),
                    limit: None,
                    on_click: None,
                    on_brush: None,
                    format: IndexMap::from([
                        (name.to_string(), "currency:EUR compact".to_string()),
                        ("b".to_string(), name.to_string()),
                    ]),
                    at: GridPosition {
                        x: 0,
                        y: 0,
                        w: 1,
                        h: 1,
                    },
                }],
            };
            assert_eq!(read(&file.to_yaml()), file, "{name:?}");
        }
    }

    #[test]
    fn a_new_dashboard_has_an_empty_list_of_visuals() {
        let file = DashboardFile {
            name: "Empty".into(),
            layout: Layout::default(),
            filters: Vec::new(),
            visuals: Vec::new(),
        };
        assert_eq!(
            file.to_yaml(),
            "name: Empty\nlayout: { columns: 12, row_height: 80 }\nvisuals: []\n"
        );
        assert_eq!(read(&file.to_yaml()), file);
    }
}
