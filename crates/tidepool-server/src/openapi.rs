//! The OpenAPI 3.1 document, generated from the endpoint table and the
//! running model.
//!
//! Ported from Kayak's `openapi.rs`. The schemas come from schemars, whose
//! documents keep shared definitions under a local `$defs`; OpenAPI wants them
//! under `components/schemas`, so they are hoisted there and every `$ref`
//! rewritten to match.
//!
//! The one part that depends on the model is writing rows: each table gets
//! its own `/v1/tables/<name>/rows` path, with the exact row shape its columns
//! define, in place of the generic `{table}` one. A generated client then has
//! a typed method per table.

use serde_json::{Map, Value, json};
use tidepool_config::table::TableMode;
use tidepool_config::types::ColumnType;
use tidepool_model::Model;
use tidepool_model::ir::Table;
use tidepool_protocol::api_docs::{ApiDoc, Body, Operation, TAGS, endpoints, schemas};

const COMPONENTS: &str = "#/components/schemas/";
const DEFS: &str = "#/$defs/";
const ROWS_PATH: &str = "/v1/tables/{table}/rows";

/// What one listener's document shows (plan §9.2): which endpoints it
/// serves, and which tables each rows endpoint writes there.
pub struct Shown<'a> {
    pub endpoint: &'a dyn Fn(&ApiDoc) -> bool,
    pub rows: &'a dyn Fn(&Table, Operation) -> bool,
}

/// The whole document, as served at `GET /v1/openapi.json` on a listener
/// that serves everything.
#[must_use]
pub fn document(model: &Model) -> Value {
    document_shown(
        model,
        &Shown {
            endpoint: &|_| true,
            rows: &|_, _| true,
        },
    )
}

/// The document as one listener serves it: its endpoints, and only the
/// tables writable there. On the public listener that is the page to hand
/// whoever runs a public producer.
#[must_use]
pub fn document_shown(model: &Model, shown: &Shown) -> Value {
    let mut components = components();
    for table in &model.tables {
        components.insert(row_schema_name(table), row_schema(table, false));
        if table.mode == TableMode::Upsert {
            components.insert(key_schema_name(table), row_schema(table, true));
        }
    }
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "tidepool",
            "version": env!("CARGO_PKG_VERSION"),
            "description":
                "A live, associative analytics server. Data is pushed in over HTTP, \
                 held in an in-memory column store, and served as dashboards that \
                 update continuously.",
        },
        "tags": tags(),
        "paths": paths(model, shown),
        "components": {
            "schemas": components,
            "securitySchemes": {
                "bearer": {
                    "type": "http",
                    "scheme": "bearer",
                    "description":
                        "A token per access level, set with TIDEPOOL_INGEST_TOKEN, \
                         TIDEPOOL_READ_TOKEN and TIDEPOOL_ADMIN_TOKEN. A level without a \
                         token is open.",
                },
            },
        },
    })
}

fn tags() -> Value {
    Value::Array(
        TAGS.iter()
            .map(|tag| json!({ "name": tag.label(), "description": tag.description() }))
            .collect(),
    )
}

fn paths(model: &Model, shown: &Shown) -> Value {
    let mut paths = Map::new();
    let mut insert = |path: String, method: &str, operation: Value| {
        let item = paths
            .entry(path)
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(item) = item.as_object_mut() {
            item.insert(method.to_string(), operation);
        }
    };
    for endpoint in endpoints() {
        if !(shown.endpoint)(&endpoint) {
            continue;
        }
        let per_table = endpoint.path == ROWS_PATH && !model.tables.is_empty();
        if !per_table {
            insert(
                endpoint.path.to_string(),
                endpoint.method.key(),
                operation(&endpoint, None),
            );
            continue;
        }
        for table in &model.tables {
            let keyed = matches!(
                endpoint.operation,
                Operation::DeleteRows | Operation::ReplaceRows
            );
            if keyed && table.mode != TableMode::Upsert || !(shown.rows)(table, endpoint.operation)
            {
                continue;
            }
            insert(
                format!("/v1/tables/{}/rows", table.name),
                endpoint.method.key(),
                operation(&endpoint, Some(table)),
            );
        }
    }
    Value::Object(paths)
}

/// One operation; `table` makes it the per-table form of a rows endpoint.
fn operation(endpoint: &ApiDoc, table: Option<&Table>) -> Value {
    let mut operation = Map::new();
    let id = match table {
        Some(table) => format!("{}_{}", endpoint.operation_id(), table.name),
        None => endpoint.operation_id().to_string(),
    };
    operation.insert("operationId".into(), json!(id));
    operation.insert("summary".into(), json!(endpoint.summary));
    operation.insert("description".into(), json!(endpoint.description));
    operation.insert("tags".into(), json!([endpoint.tag.label()]));

    let path_params = endpoint
        .params
        .iter()
        .filter(|_| table.is_none())
        .map(|param| {
            json!({
                "name": param.name,
                "in": "path",
                "required": true,
                "description": param.description,
                "example": param.example,
                "schema": { "type": "string" },
            })
        });
    let parameters: Vec<Value> = path_params
        .chain(endpoint.query.iter().map(|param| {
            json!({
                "name": param.name,
                "in": "query",
                "required": false,
                "description": param.description,
                "example": param.example,
                "schema": { "type": "string" },
            })
        }))
        .chain(
            (endpoint.request.is_some_and(|r| r.body == Body::Rows)).then(|| {
                json!({
                    "name": "Idempotency-Key",
                    "in": "header",
                    "required": false,
                    "description": "Makes retries safe: a key seen in the last 24 hours gets the first response again.",
                    "schema": { "type": "string" },
                })
            }),
        )
        .collect();
    if !parameters.is_empty() {
        operation.insert("parameters".into(), Value::Array(parameters));
    }

    if let Some(request) = endpoint.request {
        let content = match (request.body, table) {
            (Body::Rows, Some(table)) => {
                rows_content(&if endpoint.operation == Operation::DeleteRows {
                    key_schema_name(table)
                } else {
                    row_schema_name(table)
                })
            }
            (Body::Rows, None) => rows_content(""),
            (body, _) => content(body).unwrap_or(Value::Null),
        };
        operation.insert(
            "requestBody".into(),
            json!({
                "required": true,
                "description": request.description,
                "content": content,
            }),
        );
    }

    let mut responses = Map::new();
    for response in &endpoint.responses {
        let mut body = Map::new();
        body.insert("description".into(), json!(response.description));
        if let Some(content) = content(response.body) {
            body.insert("content".into(), content);
        }
        responses.insert(response.status.to_string(), Value::Object(body));
    }
    if endpoint.access.is_protected() {
        responses.entry("401".to_string()).or_insert_with(|| {
            json!({
                "description": "No token, or the wrong one, for this endpoint's access level.",
                "content": content(Body::Json("ApiError")),
            })
        });
        operation.insert("security".into(), json!([{ "bearer": [] }]));
    }
    operation.insert("responses".into(), Value::Object(responses));
    operation.insert("x-tidepool-access".into(), json!(endpoint.access.label()));

    Value::Object(operation)
}

fn content(body: Body) -> Option<Value> {
    let content_type = body.content_type()?;
    let schema = match body {
        Body::None => return None,
        Body::Json(name) => json!({ "$ref": format!("{COMPONENTS}{name}") }),
        Body::JsonArray(name) => json!({
            "type": "array",
            "items": { "$ref": format!("{COMPONENTS}{name}") },
        }),
        Body::AnyJson => json!({ "type": "object" }),
        Body::Rows => return Some(rows_content("")),
        Body::JsonOrCsv(name) => {
            return Some(json!({
                content_type: { "schema": { "$ref": format!("{COMPONENTS}{name}") } },
                "text/csv": { "schema": { "type": "string" } },
            }));
        }
        Body::Text => json!({ "type": "string" }),
    };
    Some(json!({ content_type: { "schema": schema } }))
}

/// The three ingest formats, with rows of the named schema (or any object).
fn rows_content(row_schema: &str) -> Value {
    let row = if row_schema.is_empty() {
        json!({ "type": "object" })
    } else {
        json!({ "$ref": format!("{COMPONENTS}{row_schema}") })
    };
    json!({
        "application/json": { "schema": { "type": "array", "items": row } },
        "application/x-ndjson": {
            "schema": row,
            "description": "One row object per line.",
        },
        "application/vnd.apache.arrow.stream": {
            "schema": { "type": "string", "format": "binary" },
            "description": "An Arrow IPC stream; fields are matched to columns by name.",
        },
    })
}

fn row_schema_name(table: &Table) -> String {
    format!("Row_{}", table.name)
}

fn key_schema_name(table: &Table) -> String {
    format!("Key_{}", table.name)
}

/// The JSON shape of one row of `table` (or of its key, for deletes), as the
/// ingest decoders accept it: without the columns the server fills.
fn row_schema(table: &Table, key_only: bool) -> Value {
    let columns: Vec<&tidepool_model::ir::Column> = table
        .columns
        .iter()
        .filter(|c| !key_only || table.primary_key.contains(&c.id))
        .filter(|c| c.filled.is_none())
        .collect();
    let properties: Map<String, Value> = columns
        .iter()
        .map(|c| {
            let mut schema = value_schema(c.column_type);
            if c.nullable
                && !key_only
                && let Some(object) = schema.as_object_mut()
            {
                object.insert("nullable".into(), json!(true));
            }
            (c.name.clone(), schema)
        })
        .collect();
    let required: Vec<&str> = columns
        .iter()
        .filter(|c| key_only || !c.nullable)
        .map(|c| c.name.as_str())
        .collect();
    json!({
        "type": "object",
        "description": if key_only {
            format!("The primary key of a `{}` row.", table.name)
        } else {
            format!("A `{}` row.", table.name)
        },
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn value_schema(column_type: ColumnType) -> Value {
    let described = |schema: Value| {
        let mut schema = schema;
        if let Some(object) = schema.as_object_mut() {
            object.insert("description".into(), json!(column_type.to_string()));
        }
        schema
    };
    described(match column_type {
        ColumnType::Bool => json!({ "type": "boolean" }),
        ColumnType::Int32 => json!({ "type": "integer", "minimum": i32::MIN, "maximum": i32::MAX }),
        ColumnType::Int64 => json!({ "type": "integer" }),
        ColumnType::Float64 => json!({ "type": "number" }),
        ColumnType::Decimal { .. } => json!({
            "oneOf": [
                { "type": "string", "pattern": "^[+-]?[0-9]*\\.?[0-9]*$" },
                { "type": "number" },
            ],
        }),
        ColumnType::String => json!({ "type": "string" }),
        ColumnType::Date => json!({ "type": "string", "format": "date" }),
        ColumnType::Timestamp => {
            json!({ "type": "string", "pattern": "^\\d{4}-\\d{2}-\\d{2}[T ]\\d{2}:\\d{2}:\\d{2}(\\.\\d+)?$" })
        }
        ColumnType::TimestampTz => json!({ "type": "string", "format": "date-time" }),
    })
}

/// Every named schema plus every definition they share, all at the top level.
fn components() -> Map<String, Value> {
    let generated = schemas();
    let mut components = Map::new();
    for schema in generated.values() {
        if let Some(defs) = schema["$defs"].as_object() {
            for (name, definition) in defs {
                components
                    .entry(name.clone())
                    .or_insert_with(|| rewrite_refs(definition.clone()));
            }
        }
    }
    for (name, schema) in generated {
        components.insert(name.to_string(), rewrite_refs(bare(&schema)));
    }
    components
}

/// A schema without the parts that only make sense at a document's root.
fn bare(schema: &Value) -> Value {
    let mut schema = schema.clone();
    if let Some(object) = schema.as_object_mut() {
        object.remove("$defs");
        object.remove("$schema");
    }
    schema
}

/// Point every `#/$defs/X` reference at `#/components/schemas/X` instead.
fn rewrite_refs(schema: Value) -> Value {
    match schema {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, value)| {
                    let value = match (key.as_str(), &value) {
                        ("$ref", Value::String(reference)) => match reference.strip_prefix(DEFS) {
                            Some(name) => json!(format!("{COMPONENTS}{name}")),
                            None => value,
                        },
                        _ => rewrite_refs(value),
                    };
                    (key, value)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(rewrite_refs).collect()),
        other => other,
    }
}
