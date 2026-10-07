//! The OpenAPI 3.1 document, generated from the endpoint table.
//!
//! Ported from Kayak's `openapi.rs`. The schemas come from schemars, whose
//! documents keep shared definitions under a local `$defs`; OpenAPI wants them
//! under `components/schemas`, so they are hoisted there and every `$ref`
//! rewritten to match.

use serde_json::{Map, Value, json};
use tidepool_protocol::api_docs::{ApiDoc, Body, TAGS, endpoints, schemas};

const COMPONENTS: &str = "#/components/schemas/";
const DEFS: &str = "#/$defs/";

/// The whole document, as served at `GET /v1/openapi.json`.
#[must_use]
pub fn document() -> Value {
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
        "paths": paths(),
        "components": { "schemas": components() },
    })
}

fn tags() -> Value {
    Value::Array(
        TAGS.iter()
            .map(|tag| json!({ "name": tag.label(), "description": tag.description() }))
            .collect(),
    )
}

fn paths() -> Value {
    let mut paths = Map::new();
    for endpoint in endpoints() {
        let item = paths
            .entry(endpoint.path.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(item) = item.as_object_mut() {
            item.insert(endpoint.method.key().to_string(), operation(&endpoint));
        }
    }
    Value::Object(paths)
}

fn operation(endpoint: &ApiDoc) -> Value {
    let mut operation = Map::new();
    operation.insert("operationId".into(), json!(endpoint.operation_id()));
    operation.insert("summary".into(), json!(endpoint.summary));
    operation.insert("description".into(), json!(endpoint.description));
    operation.insert("tags".into(), json!([endpoint.tag.label()]));

    let parameters: Vec<Value> = endpoint
        .params
        .iter()
        .map(|param| {
            json!({
                "name": param.name,
                "in": "path",
                "required": true,
                "description": param.description,
                "example": param.example,
                "schema": { "type": "string" },
            })
        })
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
        .collect();
    if !parameters.is_empty() {
        operation.insert("parameters".into(), Value::Array(parameters));
    }

    if let Some(request) = endpoint.request {
        operation.insert(
            "requestBody".into(),
            json!({
                "required": true,
                "description": request.description,
                "content": content(request.body),
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
    };
    Some(json!({ content_type: { "schema": schema } }))
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
