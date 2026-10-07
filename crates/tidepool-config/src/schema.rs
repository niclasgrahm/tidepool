//! JSON Schemas for the config files, for editors (yaml-language-server), the
//! UI's config editor, and agents writing config.

use schemars::schema_for;
use serde_json::Value;

use crate::dashboard::DashboardFile;
use crate::ossie::{
    FieldExtension, MetricExtension, ModelExtension, OssieModel, RelationshipExtension,
};
use crate::server::ServerConfig;
use crate::table::TableFile;

/// The kinds of config file, as they appear in `/v1/schemas/{kind}`.
pub const KINDS: [&str; 8] = [
    "server",
    "table",
    "model",
    "dashboard",
    "metric-extension",
    "field-extension",
    "relationship-extension",
    "model-extension",
];

/// The JSON Schema for one kind of config file, or `None` for an unknown kind.
#[must_use]
pub fn schema(kind: &str) -> Option<Value> {
    let schema = match kind {
        "server" => schema_for!(ServerConfig),
        "table" => schema_for!(TableFile),
        "model" => schema_for!(OssieModel),
        "dashboard" => schema_for!(DashboardFile),
        "metric-extension" => schema_for!(MetricExtension),
        "field-extension" => schema_for!(FieldExtension),
        "relationship-extension" => schema_for!(RelationshipExtension),
        "model-extension" => schema_for!(ModelExtension),
        _ => return None,
    };
    serde_json::to_value(schema).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_a_schema_object() {
        for kind in KINDS {
            let Some(schema) = schema(kind) else {
                panic!("no schema for {kind}");
            };
            assert!(schema.is_object(), "{kind}");
        }
        assert!(schema("nope").is_none());
    }

    #[test]
    fn string_typed_values_carry_patterns() {
        let Some(table) = schema("table") else {
            panic!("no table schema");
        };
        assert!(
            table["$defs"]["ColumnType"]["pattern"].is_string(),
            "{table}"
        );
    }
}
