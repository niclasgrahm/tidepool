//! `tables/<name>.yaml`: one physical table. Its shape, its types, and how
//! rows arrive. This file is also what the ingest API for the table is
//! generated from.

use indexmap::IndexMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::types::{ColumnType, HumanDuration};

/// How rows arrive in a table (plan D6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TableMode {
    /// Rows are only ever added. The cheapest mode, and the default: facts.
    #[default]
    Append,
    /// Rows are keyed by `primary_key`; a row with an existing key replaces the
    /// old one. Dimensions.
    Upsert,
}

/// How a column is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    /// Dictionary-encoded: every distinct value gets a small integer code.
    /// Cheap to filter and group by. The default for string, bool and date.
    Dict,
    /// Stored as values. The default for numbers and timestamps, and the right
    /// choice for high-cardinality ids nobody filters by.
    Plain,
}

impl Encoding {
    /// The encoding a column of this type gets when the file doesn't say.
    #[must_use]
    pub fn default_for(column_type: ColumnType) -> Self {
        match column_type {
            ColumnType::String | ColumnType::Bool | ColumnType::Date => Self::Dict,
            _ => Self::Plain,
        }
    }
}

/// One column.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ColumnSpec {
    #[serde(rename = "type")]
    pub column_type: ColumnType,
    /// Whether the column may hold NULL. Columns are required by default.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub nullable: bool,
    /// Overrides the default encoding for the column's type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<Encoding>,
    /// The column's previous name, to make a rename keep its data. Removed
    /// again once the rename is applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renamed_from: Option<String>,
}

impl ColumnSpec {
    #[must_use]
    pub fn effective_encoding(&self) -> Encoding {
        self.encoding
            .unwrap_or_else(|| Encoding::default_for(self.column_type))
    }
}

/// A table file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TableFile {
    /// The table's name. Must match the file name (`tables/<name>.yaml`).
    pub name: String,
    #[serde(default)]
    pub mode: TableMode,
    /// The key of an `upsert` table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_key: Option<Vec<String>>,
    /// A temporal column that orders rows in time; drives retention and time
    /// pruning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_time: Option<String>,
    /// How long rows are kept, measured on `event_time`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<HumanDuration>,
    /// The table's previous name, to make a rename keep its data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renamed_from: Option<String>,
    /// The columns, in order.
    pub columns: IndexMap<String, ColumnSpec>,
}
