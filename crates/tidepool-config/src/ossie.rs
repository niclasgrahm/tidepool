//! `model.ossie.yaml`: the semantic layer, in Apache Ossie's format.
//!
//! The types follow the Ossie core spec as of 0.2 (flat at the document root).
//! 0.1.1 documents, which wrap the model in a `semantic_model:` list, are
//! accepted too and unwrapped (see [`crate::project`]). Everything Ossie can't
//! say yet lives in `custom_extensions` entries with `vendor_name: TIDEPOOL`,
//! whose `data` is a JSON string (that is how the spec defines extensions);
//! [`TidepoolExtension`] and friends are what that JSON may contain.
//!
//! Only expressions in a dialect Tidepool reads are used ([`READ_DIALECTS`]);
//! others are kept so the file stays shareable with other Ossie tools.

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The vendor name of Tidepool's extensions. Matched case-insensitively.
pub const VENDOR: &str = "TIDEPOOL";

/// Dialects Tidepool reads, in order of preference. `OSSIE_SQL_2026` is
/// Ossie's own SQL subset (and its default); `ANSI_SQL` is what 0.1 documents
/// use. Both are parsed as standard SQL.
pub const READ_DIALECTS: [&str; 2] = ["OSSIE_SQL_2026", "ANSI_SQL"];

/// The whole document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OssieModel {
    pub version: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_context: Option<Value>,
    pub datasets: Vec<Dataset>,
    #[serde(default)]
    pub relationships: Vec<Relationship>,
    #[serde(default)]
    pub metrics: Vec<Metric>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_extensions: Vec<CustomExtension>,
}

/// An Ossie 0.1.1 document: the same model, wrapped.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OssieWrapped {
    pub version: String,
    pub semantic_model: Vec<WrappedModel>,
}

/// One entry of `semantic_model:` in 0.1.1: everything but the version.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WrappedModel {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub ai_context: Option<Value>,
    pub datasets: Vec<Dataset>,
    #[serde(default)]
    pub relationships: Vec<Relationship>,
    #[serde(default)]
    pub metrics: Vec<Metric>,
    #[serde(default)]
    pub custom_extensions: Vec<CustomExtension>,
}

impl WrappedModel {
    #[must_use]
    pub fn into_model(self, version: String) -> OssieModel {
        OssieModel {
            version,
            name: self.name,
            description: self.description,
            ai_context: self.ai_context,
            datasets: self.datasets,
            relationships: self.relationships,
            metrics: self.metrics,
            custom_extensions: self.custom_extensions,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Dataset {
    pub name: String,
    /// In Tidepool: the name of a table in `tables/`.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_key: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unique_keys: Option<Vec<Vec<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_context: Option<Value>,
    /// Declared fields. Every column of the source table is also a field of
    /// the dataset under its own name; declaring one adds a label, a
    /// description, or a derived expression.
    #[serde(default)]
    pub fields: Vec<Field>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_extensions: Vec<CustomExtension>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Field {
    pub name: String,
    pub expression: Expression,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimension: Option<Dimension>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub datatype: Option<DataType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_context: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_extensions: Vec<CustomExtension>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Dimension {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_time: Option<bool>,
}

/// Ossie's logical data types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum DataType {
    String,
    Integer,
    Decimal,
    Float,
    Boolean,
    Date,
    Time,
    DateTime,
    DateTimeTz,
    Opaque,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Expression {
    pub dialects: Vec<DialectExpression>,
}

impl Expression {
    /// The expression in the first dialect Tidepool reads, if any.
    #[must_use]
    pub fn readable(&self) -> Option<(usize, &DialectExpression)> {
        READ_DIALECTS.iter().find_map(|wanted| {
            self.dialects
                .iter()
                .enumerate()
                .find(|(_, d)| d.dialect.eq_ignore_ascii_case(wanted))
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DialectExpression {
    /// The dialect's name. Any string is kept; only [`READ_DIALECTS`] are read.
    pub dialect: String,
    pub expression: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Relationship {
    pub name: String,
    /// The many side.
    pub from: String,
    /// The one side.
    pub to: String,
    pub from_columns: Vec<String>,
    pub to_columns: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_context: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_extensions: Vec<CustomExtension>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Metric {
    pub name: String,
    pub expression: Expression,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub datatype: Option<DataType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_context: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_extensions: Vec<CustomExtension>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CustomExtension {
    pub vendor_name: String,
    /// A JSON document, as a string.
    pub data: String,
}

/// The index and parsed contents of the Tidepool extension in a list, if
/// there is one. More than one is an error the caller reports (by index).
/// Each entry's result is `Err` with serde's message when its JSON doesn't
/// parse into `T`.
#[must_use]
pub fn tidepool_extension<T: DeserializeOwned + Default>(
    extensions: &[CustomExtension],
) -> Vec<(usize, Result<T, String>)> {
    extensions
        .iter()
        .enumerate()
        .filter(|(_, e)| e.vendor_name.eq_ignore_ascii_case(VENDOR))
        .map(|(i, e)| {
            (
                i,
                serde_json::from_str(&e.data).map_err(|err| err.to_string()),
            )
        })
        .collect()
}

/// What a metric's Tidepool extension may say.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MetricExtension {
    /// `derived`: the expression is over other metrics, evaluated after
    /// aggregation (e.g. `revenue / order_count`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<MetricKind>,
    /// The dataset rows are counted on, for expressions that don't name one
    /// (`COUNT(*)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
    /// Evaluate as if these fields (`dataset.field`, or `*` for all) had no
    /// selection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore_selection: Vec<String>,
    /// Evaluate with the time selection on `time_field` shifted, e.g. `-1y`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_offset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_field: Option<String>,
    /// Display hint: `int`, `decimal`, `percent`, or `currency:XXX`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    Derived,
}

/// What a field's Tidepool extension may say.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FieldExtension {
    /// For time fields: the grains a visual may group by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grain: Option<Vec<TimeGrain>>,
}

/// What a dataset's Tidepool extension may say.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DatasetExtension {
    /// A parent–child hierarchy over the dataset's rows (plan §5.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hierarchy: Option<HierarchyExtension>,
}

/// A parent–child hierarchy: each row is a node, keyed by the table's
/// (single-column) primary key, and `parent` holds its parent's key. The
/// tree may be ragged: nodes are at whatever depth their parents put them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HierarchyExtension {
    /// The field the hierarchy adds to the dataset; its values are node keys.
    pub name: String,
    /// The column holding a node's parent's key; NULL for a root.
    pub parent: String,
    /// The column a node is shown by; the key when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// A column saying what a node is (`site`, `line`, `device`…), shown
    /// beside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// What a relationship's Tidepool extension may say.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RelationshipExtension {
    /// Distinguishes relationships that connect the same pair of tables
    /// (order date vs ship date).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

/// What the model's own Tidepool extension may say.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelExtension {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_direction: Option<FilterDirection>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FilterDirection {
    /// Selections flow from the one side to the many side (plan D5): a
    /// selected region filters orders, a selected order status doesn't
    /// filter customers. The default.
    OneToMany,
    /// Selections flow both ways (plan §5.4): a selected order status also
    /// narrows customers to those with such orders, and everything
    /// associated with them. Relationships may not form a loop.
    Both,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TimeGrain {
    Day,
    Week,
    Month,
    Quarter,
    Year,
}
