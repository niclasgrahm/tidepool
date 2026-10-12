//! `dashboards/<id>.yaml`: a dashboard. Its id is the file name.

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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VisualType {
    Kpi,
    Bar,
    Line,
    Table,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Visual {
    /// Unique within the dashboard.
    pub id: String,
    #[serde(rename = "type")]
    pub visual_type: VisualType,
    #[serde(default)]
    pub dimensions: Vec<DimensionRef>,
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
