//! The live protocol (plan §7): messages over the WebSocket at `/v1/ws`.
//!
//! Every message is one JSON text frame, tagged by `type`. (The plan's
//! binary framing with Arrow payloads is for large results; dashboard
//! results are small after grouping and `limit`, so v1 sends JSON.)
//!
//! A session holds a selection, its back/forward history, and the
//! dashboard it shows. The server sends one [`ServerMessage::Frame`] per
//! epoch with every visual and filter of that dashboard whose content
//! changed: the client applies it as a whole, so what it shows is always
//! one consistent epoch.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{QueryDimension, QueryResult};

/// What a client sends.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientMessage {
    /// Start, or resume a session (its id kept by the client).
    Hello {
        #[serde(default)]
        session_id: Option<String>,
    },
    /// Show a dashboard, by slug: subscribes its visuals and filters.
    OpenDashboard {
        id: String,
    },
    /// Select values of a field (`dataset.field`): a click, a list box.
    Select {
        field: String,
        values: Vec<Value>,
        #[serde(default)]
        mode: SelectMode,
    },
    /// Select values of several fields at once, as one step of the history
    /// (Back undoes all of it): a heatmap's cell, a pivot's.
    SelectMany {
        fields: Vec<FieldValues>,
        #[serde(default)]
        mode: SelectMode,
    },
    /// Select a half-open range `from <= value < to` (a brush, a date range).
    SelectRange {
        field: String,
        #[serde(default)]
        from: Option<Value>,
        #[serde(default)]
        to: Option<Value>,
    },
    /// Select a window relative to now on a time field ("the last 7
    /// days"): it moves as time passes, and the dashboard with it.
    SelectRelative {
        field: String,
        window: RelativeWindow,
    },
    /// Select the rows whose value of a string field contains `text`,
    /// ignoring the case of ASCII letters: a search box. An empty text
    /// clears the field.
    Search {
        field: String,
        text: String,
    },
    /// Clear one field's selection, or all of it.
    Clear {
        #[serde(default)]
        field: Option<String>,
    },
    /// The previous selection.
    Back,
    /// The next selection, after going back.
    Forward,
    /// Page or search a filter list.
    FilterWindow {
        field: String,
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_window")]
        limit: usize,
        #[serde(default)]
        search: Option<String>,
    },
    /// Open or close nodes of a tree filter (a hierarchy field), by key:
    /// an open node's children are listed.
    FilterOpen {
        field: String,
        keys: Vec<Value>,
        #[serde(default = "yes")]
        open: bool,
    },
    /// Sort a visual by one of its result columns, for this session.
    Sort {
        visual: String,
        column: String,
        #[serde(default)]
        desc: bool,
    },
    /// Stop live updates for this session; the last frame stays.
    Pause,
    Resume,
}

fn default_window() -> usize {
    100
}

fn yes() -> bool {
    true
}

/// How [`ClientMessage::Select`] combines with what's selected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SelectMode {
    /// These values instead (an empty list clears the field).
    #[default]
    Replace,
    /// Selected ones off, the others on.
    Toggle,
    /// These values as well.
    Add,
}

/// A window of whole periods relative to now: `last` periods of `unit`,
/// ending with the current one, or `ago` periods before it. "The last 7
/// days" is `{unit: day, last: 7}` (today and the six days before it);
/// "last month" is `{unit: month, last: 1, ago: 1}`. Periods start where
/// grains do (weeks on Monday), in UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RelativeWindow {
    pub unit: RelativeUnit,
    pub last: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ago: u32,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's signature
fn is_zero(n: &u32) -> bool {
    *n == 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RelativeUnit {
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Quarter,
    Year,
}

impl RelativeUnit {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Minute => "minute",
            Self::Hour => "hour",
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Quarter => "quarter",
            Self::Year => "year",
        }
    }

    /// Whether a date (no time of day) has periods this long.
    #[must_use]
    pub fn fits_dates(self) -> bool {
        !matches!(self, Self::Minute | Self::Hour)
    }
}

impl RelativeWindow {
    /// How a person would say it: "Today", "Last 7 days", "Last month",
    /// "Previous 7 days" (seven days ending yesterday).
    #[must_use]
    pub fn label(&self) -> String {
        let unit = self.unit.name();
        let plural = |n: u32| {
            if n == 1 {
                unit.to_string()
            } else {
                format!("{n} {unit}s")
            }
        };
        match (self.last, self.ago) {
            (1, 0) if self.unit == RelativeUnit::Day => "Today".into(),
            (1, 1) if self.unit == RelativeUnit::Day => "Yesterday".into(),
            (1, 0) => format!("This {unit}"),
            (1, 1) => format!("Last {unit}"),
            (n, 0) => format!("Last {}", plural(n)),
            (n, 1) => format!("Previous {}", plural(n)),
            (n, ago) => format!("{} ending {} ago", plural(n), plural(ago)),
        }
    }
}

/// Values of one field, in a [`ClientMessage::SelectMany`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FieldValues {
    pub field: String,
    pub values: Vec<Value>,
}

/// What the server sends.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Welcome {
        session_id: String,
        epoch: u64,
    },
    /// The dashboards there are; sent on connect.
    Model {
        dashboards: Vec<DashboardInfo>,
        /// The config version: sent again whenever the model changes, so
        /// clients reload what they show of it.
        #[serde(default)]
        version: u64,
    },
    /// The session's selection, after every change.
    Selection {
        fields: Vec<SelectedField>,
        can_back: bool,
        can_forward: bool,
    },
    /// What changed on the open dashboard as of `epoch`, applied as a whole.
    Frame {
        epoch: u64,
        visuals: Vec<VisualFrame>,
        filters: Vec<FilterFrame>,
    },
    /// About once a second: for the live indicator.
    Status {
        epoch: u64,
        rows_per_second: f64,
    },
    /// A view the session opened (views aren't in `Model`): what it shows.
    /// Sent again when a config change changes it.
    View {
        dashboard: DashboardInfo,
    },
    /// A message the server couldn't act on.
    Error {
        message: String,
    },
    /// One per client message, after everything that message caused (its
    /// selection and frame, when there are any): a client counting what it
    /// sent knows when the dashboard has caught up with a click.
    Done,
}

/// A live session's selection (`GET` and `PUT /v1/sessions/{id}/selection`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSelection {
    pub session: String,
    /// The dashboard or view it shows, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dashboard: Option<String>,
    pub fields: Vec<SelectedField>,
    pub can_back: bool,
    pub can_forward: bool,
}

/// A session's new selection, whole: one condition per field, as a query's
/// filters are written. One step in its history, so Back undoes it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SelectionChange {
    pub fields: Vec<crate::QueryFilter>,
}

/// A view (`POST /v1/views`): a dashboard that lives only in memory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ViewInfo {
    /// What `open_dashboard` opens it by: `~` and 32 hex digits.
    pub id: String,
    pub dashboard: DashboardInfo,
    /// Its dashboard file, as YAML: what saving it writes.
    pub yaml: String,
}

/// Why a view couldn't be made.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ViewProblems {
    pub error: String,
    pub problems: Vec<crate::config::ConfigProblem>,
    /// The problems as the CLI prints them, with the lines they point at.
    pub problems_text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DashboardInfo {
    /// The file name, and the dashboard's URL path.
    pub id: String,
    pub name: String,
    pub columns: u16,
    pub row_height: u16,
    pub filters: Vec<FilterInfo>,
    pub visuals: Vec<VisualInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FilterInfo {
    /// `dataset.field`.
    pub field: String,
    /// `list`, `tree`, `date_range` or `search`.
    pub widget: String,
    /// Whether the field has a time of day, so windows of hours and
    /// minutes apply to it (a date has none).
    #[serde(default)]
    pub timed: bool,
    /// The window a `date_range` starts with when the dashboard is opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<RelativeWindow>,
    /// The metric shown beside each value instead of its rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measure: Option<String>,
    /// The measure's format (`currency:EUR compact`, …; see the metric's
    /// `format`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measure_format: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct VisualInfo {
    pub id: String,
    /// `kpi`, `bar`, `line`, `table`, `pie`, `scatter`, `heatmap`,
    /// `pivot`, `filter` or `rows`.
    #[serde(rename = "type")]
    pub visual_type: String,
    /// What its header says, when the file gives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub dimensions: Vec<QueryDimension>,
    /// A pivot's dimensions along the top; its `dimensions` are down the
    /// side. Its frames' rows are grouped by both, `dimensions` first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub across: Vec<QueryDimension>,
    /// A pivot's total row and column come in its frames' `totals`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub totals: bool,
    pub metrics: Vec<String>,
    /// A KPI's comparison metric.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compare: Option<String>,
    /// Clicking a value selects it.
    pub on_click: bool,
    /// Brushing a range selects it.
    pub on_brush: bool,
    /// A `filter` visual's list: `list` or `tree`. Its frames are the
    /// filter frames of its dimension's field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widget: Option<String>,
    /// How each metric reads here, by name: the visual's own format for it,
    /// else the metric's. Metrics without one aren't listed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub formats: BTreeMap<String, String>,
    pub at: GridAt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GridAt {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

/// One field's selection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SelectedField {
    pub field: String,
    /// The values selected, for a value selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<Value>,
    /// A window relative to now; `from` and `to` are where it is now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relative: Option<RelativeWindow>,
    /// The text searched for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
}

/// A visual's new content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct VisualFrame {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<QueryResult>,
    /// Why the visual has no result (a metric the engine can't run yet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// A KPI's comparison value, when its metric can be computed; a pie's
    /// whole (its metric without the dimension), when the rest of it can be
    /// a slice (a sum or a count).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compare: Option<Value>,
    /// A pivot's totals, when it has them: each metric computed again
    /// without some dimensions (exact for every aggregate, never summed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totals: Option<PivotTotals>,
}

/// A pivot's totals, as query results.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PivotTotals {
    /// Per row: grouped by the dimensions down the side only. Absent
    /// without `across`, when each row is its own total.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<QueryResult>,
    /// Per column: grouped by the dimensions across only. Absent without
    /// `across`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<QueryResult>,
    /// The whole: no dimensions, one row.
    pub all: QueryResult,
}

/// A filter list's window: Qlik's states, per value. A tree filter's
/// window is of `nodes` instead: the visible ones, depth first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FilterFrame {
    pub field: String,
    /// Values there are (matching the search, if any), or visible nodes.
    pub total: usize,
    pub offset: usize,
    pub values: Vec<FilterValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<TreeNode>,
    /// A tree's selected nodes, with their paths: for the selection's
    /// chips, whether or not the nodes are in the window.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chosen: Vec<ChosenNode>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FilterValue {
    pub value: Value,
    pub state: ValueState,
    /// Rows with this value under the selection (0 when excluded).
    pub rows: i64,
    /// The filter's measure for this value, under the rest of the
    /// selection (null when excluded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measure: Option<Value>,
}

/// One visible node of a tree filter (plan §5.6).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[allow(clippy::struct_excessive_bools)] // independent facts a client reads as such
pub struct TreeNode {
    /// What selecting it sends.
    pub key: Value,
    pub label: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<Value>,
    /// 0 for a root.
    pub depth: u32,
    pub has_children: bool,
    /// Its children are listed (after it).
    pub open: bool,
    /// Selected, or possible when it or anything below it passes every
    /// other field's selection.
    pub state: ValueState,
    /// A node above it is selected, so it is too.
    pub inside: bool,
    /// Something below it is selected.
    pub holds: bool,
    /// Nodes in its subtree that pass every other field's selection.
    pub rows: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measure: Option<Value>,
}

/// A selected node of a tree filter, and the labels from its root down to
/// it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ChosenNode {
    pub key: Value,
    pub path: Vec<Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ValueState {
    Selected,
    /// Occurs in rows that pass every *other* field's selection.
    Possible,
    Excluded,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_read_the_way_people_say_them() {
        let window = |unit, last, ago| RelativeWindow { unit, last, ago }.label();
        assert_eq!(window(RelativeUnit::Day, 1, 0), "Today");
        assert_eq!(window(RelativeUnit::Day, 1, 1), "Yesterday");
        assert_eq!(window(RelativeUnit::Day, 7, 0), "Last 7 days");
        assert_eq!(window(RelativeUnit::Day, 7, 1), "Previous 7 days");
        assert_eq!(window(RelativeUnit::Month, 1, 0), "This month");
        assert_eq!(window(RelativeUnit::Month, 1, 1), "Last month");
        assert_eq!(window(RelativeUnit::Hour, 24, 0), "Last 24 hours");
        assert_eq!(
            window(RelativeUnit::Week, 2, 3),
            "2 weeks ending 3 weeks ago"
        );
    }

    #[test]
    fn a_relative_selection_reads_from_json() {
        let parsed: Result<ClientMessage, _> = serde_json::from_str(
            r#"{"type": "select_relative", "field": "orders.ordered_at", "window": {"unit": "day", "last": 7}}"#,
        );
        assert_eq!(
            parsed.ok(),
            Some(ClientMessage::SelectRelative {
                field: "orders.ordered_at".into(),
                window: RelativeWindow {
                    unit: RelativeUnit::Day,
                    last: 7,
                    ago: 0
                },
            })
        );
    }
}
