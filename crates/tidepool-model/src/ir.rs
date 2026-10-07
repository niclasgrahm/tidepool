//! The Model IR: the compiled, validated model the rest of Tidepool runs on.
//!
//! Everything here has been checked against everything else (names resolve,
//! relationships point many → one at a primary key, metrics are anchored on a
//! table everything they mention is reachable from), so later stages never
//! re-validate. Objects are identified by stable ids from the [`Catalog`],
//! not by name, so a rename doesn't look like a drop and an add.
//!
//! [`Catalog`]: crate::catalog::Catalog

use serde::{Deserialize, Serialize};
use tidepool_config::ossie::TimeGrain;
use tidepool_config::server::ServerConfig;
use tidepool_config::table::{Encoding, TableMode};
use tidepool_config::types::{ColumnType, HumanDuration};

macro_rules! id_type {
    ($name:ident, $prefix:literal) => {
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        pub struct $name(pub u32);

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, concat!($prefix, "{}"), self.0)
            }
        }
    };
}

id_type!(TableId, "t");
id_type!(ColumnId, "c");
id_type!(RelationshipId, "r");
id_type!(MetricId, "m");
id_type!(DashboardId, "d");

/// A key domain: columns that share one dictionary because relationships
/// join them. Identified by its smallest member column, which makes the id a
/// function of the domain's contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DomainId(pub ColumnId);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub server: ServerConfig,
    /// The Ossie model's name, or `None` without a model file.
    pub name: Option<String>,
    pub tables: Vec<Table>,
    pub datasets: Vec<Dataset>,
    pub relationships: Vec<Relationship>,
    pub key_domains: Vec<KeyDomain>,
    pub metrics: Vec<Metric>,
    pub dashboards: Vec<Dashboard>,
}

impl Model {
    #[must_use]
    pub fn table(&self, id: TableId) -> Option<&Table> {
        self.tables.iter().find(|t| t.id == id)
    }

    #[must_use]
    pub fn table_named(&self, name: &str) -> Option<&Table> {
        self.tables.iter().find(|t| t.name == name)
    }

    #[must_use]
    pub fn dataset(&self, table: TableId) -> Option<&Dataset> {
        self.datasets.iter().find(|d| d.table == table)
    }

    #[must_use]
    pub fn metric(&self, id: MetricId) -> Option<&Metric> {
        self.metrics.iter().find(|m| m.id == id)
    }

    /// `table.column` for messages.
    #[must_use]
    pub fn column_label(&self, table: TableId, column: ColumnId) -> String {
        let table = self.table(table);
        let column = table.and_then(|t| t.column(column));
        format!(
            "{}.{}",
            table.map_or("?", |t| t.name.as_str()),
            column.map_or("?", |c| c.name.as_str())
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Table {
    pub id: TableId,
    pub name: String,
    pub mode: TableMode,
    /// Empty for `append` tables.
    pub primary_key: Vec<ColumnId>,
    pub event_time: Option<ColumnId>,
    pub retention: Option<HumanDuration>,
    pub columns: Vec<Column>,
}

impl Table {
    #[must_use]
    pub fn column(&self, id: ColumnId) -> Option<&Column> {
        self.columns.iter().find(|c| c.id == id)
    }

    #[must_use]
    pub fn column_named(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    pub id: ColumnId,
    pub name: String,
    pub column_type: ColumnType,
    pub nullable: bool,
    /// The effective encoding (defaults applied).
    pub encoding: Encoding,
    /// Set when relationships make this column share a dictionary.
    pub key_domain: Option<DomainId>,
}

/// The semantic view of one table: its fields. At most one per table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Dataset {
    pub name: String,
    pub table: TableId,
    pub description: Option<String>,
    /// Every column (implicitly) plus declared derived fields.
    pub fields: Vec<Field>,
}

impl Dataset {
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    pub source: FieldSource,
    pub is_time: bool,
    /// Grains a visual may group this field by; empty means all, for time fields.
    pub grains: Vec<TimeGrain>,
    pub label: Option<String>,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FieldSource {
    /// A column of the dataset's table.
    Column(ColumnId),
    /// A row-level expression over columns of the same table.
    Derived {
        expression: String,
        columns: Vec<ColumnId>,
    },
}

/// A field, addressed through its dataset's table.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FieldRef {
    pub table: TableId,
    pub field: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relationship {
    pub id: RelationshipId,
    pub name: String,
    /// The many side.
    pub from: TableId,
    pub from_columns: Vec<ColumnId>,
    /// The one side; `to_columns` is its primary key.
    pub to: TableId,
    pub to_columns: Vec<ColumnId>,
    pub role: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyDomain {
    pub id: DomainId,
    pub columns: Vec<(TableId, ColumnId)>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    pub id: MetricId,
    pub name: String,
    pub description: Option<String>,
    pub format: Option<String>,
    pub body: MetricBody,
    /// Fields whose selection this metric ignores; `None` means it ignores
    /// none, `Some(empty)`… is never produced: `IgnoreSelection::All` is `*`.
    pub ignore_selection: IgnoreSelection,
    pub time_offset: Option<TimeOffset>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum MetricBody {
    /// Aggregates over rows of `anchor` (and of tables reachable from it).
    Base {
        expression: String,
        anchor: TableId,
        /// Every table the expression reads, including the anchor.
        tables: Vec<TableId>,
    },
    /// An expression over other metrics, evaluated after aggregation.
    Derived {
        expression: String,
        inputs: Vec<MetricId>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum IgnoreSelection {
    #[default]
    None,
    Fields(Vec<FieldRef>),
    All,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeOffset {
    /// Signed amount of `unit`s, e.g. -1 year.
    pub amount: i32,
    pub unit: TimeGrain,
    pub field: FieldRef,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Dashboard {
    pub id: DashboardId,
    /// The file name, and the dashboard's URL path.
    pub slug: String,
    pub name: String,
    pub columns: u16,
    pub row_height: u16,
    pub filters: Vec<Filter>,
    pub visuals: Vec<Visual>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Filter {
    pub field: FieldRef,
    pub widget: tidepool_config::dashboard::FilterWidget,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Visual {
    pub id: String,
    pub visual_type: tidepool_config::dashboard::VisualType,
    pub dimensions: Vec<Dimension>,
    pub metrics: Vec<MetricId>,
    pub compare: Option<MetricId>,
    pub sort: Option<VisualSort>,
    pub limit: Option<u32>,
    pub on_click: bool,
    pub on_brush: bool,
    pub at: tidepool_config::dashboard::GridPosition,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dimension {
    pub field: FieldRef,
    pub grain: Option<TimeGrain>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortKey {
    Metric(MetricId),
    Dimension(usize),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisualSort {
    pub key: SortKey,
    pub desc: bool,
}
