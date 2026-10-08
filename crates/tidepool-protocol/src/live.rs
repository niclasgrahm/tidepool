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
    /// Select a half-open range `from <= value < to` (a brush, a date range).
    SelectRange {
        field: String,
        #[serde(default)]
        from: Option<Value>,
        #[serde(default)]
        to: Option<Value>,
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
    /// A message the server couldn't act on.
    Error {
        message: String,
    },
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
    /// `list` or `date_range`.
    pub widget: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct VisualInfo {
    pub id: String,
    /// `kpi`, `bar`, `line`, `table`, …
    #[serde(rename = "type")]
    pub visual_type: String,
    pub dimensions: Vec<QueryDimension>,
    pub metrics: Vec<String>,
    /// A KPI's comparison metric.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compare: Option<String>,
    /// Clicking a value selects it.
    pub on_click: bool,
    /// Brushing a range selects it.
    pub on_brush: bool,
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
    /// A KPI's comparison value, when its metric can be computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compare: Option<Value>,
}

/// A filter list's window: Qlik's states, per value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FilterFrame {
    pub field: String,
    /// Values there are (matching the search, if any).
    pub total: usize,
    pub offset: usize,
    pub values: Vec<FilterValue>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FilterValue {
    pub value: Value,
    pub state: ValueState,
    /// Rows with this value under the selection (0 when excluded).
    pub rows: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ValueState {
    Selected,
    /// Occurs in rows that pass every *other* field's selection.
    Possible,
    Excluded,
}
