//! The HTTP surface, described as data.
//!
//! The same pattern as Kayak's `api_docs`: the server's router is **built from
//! this table**, so an endpoint that isn't here doesn't get registered, and an
//! entry without a handler doesn't compile (the server matches exhaustively on
//! [`Operation`]). The OpenAPI document, the access checks and (later) the
//! `/docs` page all read the same entries. The prose lives here, and handlers
//! carry a one-line `///` pointing at it.
//!
//! Bodies are named rather than inlined ([`Body::Json`] carries a schema name),
//! and [`schemas`] maps those names to generated JSON Schemas.
//!
//! The per-table ingest endpoints are the one part of the surface that isn't
//! static: their bodies depend on the running model. They will still have one
//! entry here (with a generic body), and the server adds the per-table schemas
//! to the OpenAPI document at request time.

use std::collections::BTreeMap;

use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    IngestError, IngestResponse, ModelInfo, Preview, ProbeStatus, QueryRequest, QueryResult,
    ServerInfo, SessionInfo, StatusInfo, TableInfo, config,
};

/// The error body every failing request comes back with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApiError {
    /// What went wrong, as one line.
    pub error: String,
}

/// The HTTP methods this API uses. Not the full set: a method Tidepool does not
/// serve has no business being spellable here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
}

impl Method {
    /// How the method is written in a request line.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
        }
    }

    /// The lowercase spelling OpenAPI keys an operation by.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Post => "post",
            Self::Put => "put",
            Self::Delete => "delete",
        }
    }
}

/// Every operation the API serves, as a closed set.
///
/// An enum rather than a string because the server's handler match is over it,
/// which is what makes the router provably complete. The ids from
/// [`Operation::id`] are wire format: they are what a generated client calls
/// its methods.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Operation {
    Health,
    Ready,
    GetInfo,
    GetOpenApi,
    GetSchema,
    IngestRows,
    DeleteRows,
    ReplaceRows,
    ListTables,
    GetTable,
    PreviewTable,
    Query,
    Live,
    GetModel,
    GetStatus,
    GetConfig,
    PlanConfig,
    ApplyConfig,
    ListVersions,
    GetVersion,
    ListChanges,
    ApproveChange,
    RejectChange,
    Metrics,
    StartSession,
    GetSession,
    EndSession,
    Mcp,
}

/// Every operation, for tests that need to walk them all.
pub const OPERATIONS: [Operation; 28] = [
    Operation::Health,
    Operation::Ready,
    Operation::GetInfo,
    Operation::GetOpenApi,
    Operation::GetSchema,
    Operation::IngestRows,
    Operation::DeleteRows,
    Operation::ReplaceRows,
    Operation::ListTables,
    Operation::GetTable,
    Operation::PreviewTable,
    Operation::Query,
    Operation::Live,
    Operation::GetModel,
    Operation::GetStatus,
    Operation::GetConfig,
    Operation::PlanConfig,
    Operation::ApplyConfig,
    Operation::ListVersions,
    Operation::GetVersion,
    Operation::ListChanges,
    Operation::ApproveChange,
    Operation::RejectChange,
    Operation::Metrics,
    Operation::StartSession,
    Operation::GetSession,
    Operation::EndSession,
    Operation::Mcp,
];

impl Operation {
    /// The `operationId` in the spec, and the method name in a generated client.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Health => "health",
            Self::Ready => "ready",
            Self::GetInfo => "getInfo",
            Self::GetOpenApi => "getOpenApi",
            Self::GetSchema => "getSchema",
            Self::IngestRows => "ingestRows",
            Self::DeleteRows => "deleteRows",
            Self::ReplaceRows => "replaceRows",
            Self::ListTables => "listTables",
            Self::GetTable => "getTable",
            Self::PreviewTable => "previewTable",
            Self::Query => "query",
            Self::Live => "live",
            Self::GetModel => "getModel",
            Self::GetStatus => "getStatus",
            Self::GetConfig => "getConfig",
            Self::PlanConfig => "planConfig",
            Self::ApplyConfig => "applyConfig",
            Self::ListVersions => "listConfigVersions",
            Self::GetVersion => "getConfigVersion",
            Self::ListChanges => "listChanges",
            Self::ApproveChange => "approveChange",
            Self::RejectChange => "rejectChange",
            Self::Metrics => "metrics",
            Self::StartSession => "startSession",
            Self::GetSession => "getSession",
            Self::EndSession => "endSession",
            Self::Mcp => "mcp",
        }
    }
}

/// How endpoints are grouped in the generated reference. The order of [`TAGS`]
/// is the order a page lists them in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tag {
    Tables,
    Queries,
    Config,
    Server,
    Reference,
}

/// Every tag, in the order pages list them.
pub const TAGS: [Tag; 5] = [
    Tag::Tables,
    Tag::Queries,
    Tag::Config,
    Tag::Server,
    Tag::Reference,
];

impl Tag {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Tables => "tables",
            Self::Queries => "queries",
            Self::Config => "config",
            Self::Server => "server",
            Self::Reference => "reference",
        }
    }

    /// What the group is for, shown as a heading's subtitle.
    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::Tables => {
                "The data: writing rows into tables, and looking at what they hold. \
                 Tables are declared in `tables/*.yaml`; every table gets these endpoints."
            }
            Self::Queries => {
                "Asking the model: metrics grouped by fields, under a selection. \
                 Names are the semantic layer's (`model.ossie.yaml`), not tables'."
            }
            Self::Config => {
                "Changing what the server runs while it runs: the config files, plans \
                 of what a change does, applying it, the versions so far and changes \
                 waiting for approval. Every change goes through the same pipeline, and \
                 is written back to the config directory."
            }
            Self::Server => {
                "The process itself: probes for orchestrators, and what server this is."
            }
            Self::Reference => "The API describing itself.",
        }
    }
}

/// Who may call an endpoint.
///
/// Declared here so the access an endpoint is documented with is the access
/// the server enforces. Every new endpoint has to answer the question, in one
/// place. Until authentication lands (M8) nobody is identified, so nothing is
/// checked; the declarations still drive the spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// No credentials needed even when authentication is on: the probes, and
    /// the endpoints that describe the software rather than the deployment.
    Public,
    /// Any authenticated user (the `viewer` role or above).
    Read,
    /// Ingest endpoints. A data plane with its own token, separate from the
    /// people who look at dashboards and the people who change the config.
    Ingest,
    /// Changes what the server runs: config plan/apply, approvals. Requires the
    /// `admin` role.
    Admin,
}

impl Access {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Read => "read",
            Self::Ingest => "ingest",
            Self::Admin => "admin",
        }
    }

    /// Whether reaching this endpoint takes credentials at all: what decides
    /// whether the spec attaches a security requirement and a 401 to it.
    #[must_use]
    pub fn is_protected(self) -> bool {
        !matches!(self, Self::Public)
    }
}

/// What a request or response carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Body {
    /// No body at all.
    None,
    /// A JSON object of the named schema (a key of [`schemas`]).
    Json(&'static str),
    /// A JSON array of the named schema.
    JsonArray(&'static str),
    /// A JSON document that no schema here describes usefully (the OpenAPI
    /// document itself).
    AnyJson,
    /// Rows for a table, in any of the ingest formats: a JSON array of row
    /// objects, newline-delimited JSON, or an Arrow IPC stream. The OpenAPI
    /// document spells out each table's row shape.
    Rows,
    /// A JSON object of the named schema, or the same rows as CSV
    /// (`?format=csv`).
    JsonOrCsv(&'static str),
    /// Plain text (the Prometheus exposition format).
    Text,
}

impl Body {
    /// The schema this body names, if it names one.
    #[must_use]
    pub fn schema_name(self) -> Option<&'static str> {
        match self {
            Self::Json(name) | Self::JsonArray(name) | Self::JsonOrCsv(name) => Some(name),
            Self::None | Self::AnyJson | Self::Rows | Self::Text => None,
        }
    }

    /// The media type, or `None` for no body. For [`Body::Rows`], the
    /// default of the three it accepts.
    #[must_use]
    pub fn content_type(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Json(_)
            | Self::JsonArray(_)
            | Self::AnyJson
            | Self::Rows
            | Self::JsonOrCsv(_) => Some("application/json"),
            Self::Text => Some("text/plain"),
        }
    }
}

/// A path or query parameter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParamDoc {
    pub name: &'static str,
    pub description: &'static str,
    /// A value that works: shown in the spec, and used by tests that call
    /// every endpoint.
    pub example: &'static str,
}

/// A request body.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestDoc {
    pub description: &'static str,
    pub body: Body,
}

/// One possible response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseDoc {
    pub status: u16,
    pub description: &'static str,
    pub body: Body,
}

/// One endpoint: everything the router, the spec and the docs need to know.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiDoc {
    /// The route as axum spells it, placeholders and all
    /// (`/v1/tables/{table}/rows`). OpenAPI uses the same braces, and it is the
    /// same string the router registers.
    pub path: &'static str,
    pub method: Method,
    /// Which operation this is. The router's handler match is over this.
    pub operation: Operation,
    /// One line, shown in endpoint lists.
    pub summary: &'static str,
    /// The full explanation. Blank lines separate paragraphs, `backticks` are code.
    pub description: &'static str,
    pub tag: Tag,
    /// Who may call it.
    pub access: Access,
    /// Path placeholders, in the order they appear in `path`.
    pub params: Vec<ParamDoc>,
    /// Query parameters. Always optional.
    pub query: Vec<ParamDoc>,
    pub request: Option<RequestDoc>,
    pub responses: Vec<ResponseDoc>,
}

impl ApiDoc {
    #[must_use]
    pub fn operation_id(&self) -> &'static str {
        self.operation.id()
    }
}

/// Every endpoint the server serves.
///
/// Long by nature (one entry per endpoint, prose included), as in Kayak.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn endpoints() -> Vec<ApiDoc> {
    vec![
        ApiDoc {
            path: "/v1/tables/{table}/rows",
            method: Method::Post,
            operation: Operation::IngestRows,
            summary: "Write rows into a table",
            description: "The body is the rows, in one of three formats, chosen by `Content-Type`: \
                          `application/json` (an array of row objects), `application/x-ndjson` \
                          (one row object per line) or `application/vnd.apache.arrow.stream` \
                          (an Arrow IPC stream, matched to columns by field name).\n\n\
                          Values are checked against the table's column types and never coerced. \
                          A batch is all or nothing: one bad value rejects it with every problem \
                          listed by row and column, and nothing is written. On an upsert table a \
                          row whose key exists replaces the old row.\n\n\
                          Rows become visible to queries at the next epoch (within \
                          `epoch_interval`); `?wait=visible` answers only once they are. An \
                          `Idempotency-Key` header makes retries safe: a key seen in the last 24 \
                          hours gets the first response again and writes nothing. A `503` means \
                          the server is busy; retry after `Retry-After` seconds.",
            tag: Tag::Tables,
            access: Access::Ingest,
            params: vec![ParamDoc {
                name: "table",
                description: "The table, by name.",
                example: "orders",
            }],
            query: vec![ParamDoc {
                name: "wait",
                description: "`visible`: answer once the rows are visible to queries.",
                example: "visible",
            }],
            request: Some(RequestDoc {
                description: "The rows.",
                body: Body::Rows,
            }),
            responses: vec![
                ResponseDoc {
                    status: 200,
                    description: "Written.",
                    body: Body::Json("IngestResponse"),
                },
                ResponseDoc {
                    status: 400,
                    description: "The body didn't decode, or a value doesn't fit its column. Nothing was written.",
                    body: Body::Json("IngestError"),
                },
                ResponseDoc {
                    status: 404,
                    description: "No such table.",
                    body: Body::Json("ApiError"),
                },
                ResponseDoc {
                    status: 409,
                    description: "A request with the same `Idempotency-Key` is still being processed, \
                                  or the table was removed by a config change meanwhile.",
                    body: Body::Json("ApiError"),
                },
                ResponseDoc {
                    status: 415,
                    description: "A `Content-Type` that isn't one of the three formats.",
                    body: Body::Json("ApiError"),
                },
                ResponseDoc {
                    status: 503,
                    description: "Busy: the write queue is full. Retry after `Retry-After` seconds.",
                    body: Body::Json("ApiError"),
                },
            ],
        },
        ApiDoc {
            path: "/v1/tables/{table}/rows",
            method: Method::Delete,
            operation: Operation::DeleteRows,
            summary: "Delete rows from an upsert table, by key",
            description: "The body lists keys, in the same formats as writing: each row holds the \
                          table's primary key columns and nothing else. Keys that match no row are \
                          counted in `missing`, not treated as errors. Only upsert tables have \
                          keys, so only they can be deleted from this way.",
            tag: Tag::Tables,
            access: Access::Ingest,
            params: vec![ParamDoc {
                name: "table",
                description: "The table, by name.",
                example: "customers",
            }],
            query: vec![ParamDoc {
                name: "wait",
                description: "`visible`: answer once the deletes are visible to queries.",
                example: "visible",
            }],
            request: Some(RequestDoc {
                description: "The keys of the rows to delete.",
                body: Body::Rows,
            }),
            responses: vec![
                ResponseDoc {
                    status: 200,
                    description: "Deleted.",
                    body: Body::Json("IngestResponse"),
                },
                ResponseDoc {
                    status: 400,
                    description: "The body didn't decode, or the table isn't an upsert table.",
                    body: Body::Json("IngestError"),
                },
                ResponseDoc {
                    status: 404,
                    description: "No such table.",
                    body: Body::Json("ApiError"),
                },
                ResponseDoc {
                    status: 409,
                    description: "The table was removed by a config change meanwhile.",
                    body: Body::Json("ApiError"),
                },
                ResponseDoc {
                    status: 503,
                    description: "Busy: retry after `Retry-After` seconds.",
                    body: Body::Json("ApiError"),
                },
            ],
        },
        ApiDoc {
            path: "/v1/tables/{table}/rows",
            method: Method::Put,
            operation: Operation::ReplaceRows,
            summary: "Replace an upsert table's contents",
            description: "The body is the table's complete new contents, in the same formats as \
                          writing: its rows are written (a row whose key exists replaces the old \
                          one), and every row whose key isn't among them is deleted, in one \
                          batch that becomes visible at once. For keeping a dimension in step \
                          with a system of record that can only be read whole (an asset tree, a \
                          product catalogue): send all of it every time, and what disappeared \
                          there disappears here. `replaced` counts rows replaced and rows \
                          deleted. Only upsert tables have keys, so only they can be replaced.",
            tag: Tag::Tables,
            access: Access::Ingest,
            params: vec![ParamDoc {
                name: "table",
                description: "The table, by name.",
                example: "customers",
            }],
            query: vec![ParamDoc {
                name: "wait",
                description: "`visible`: answer once the new contents are visible to queries.",
                example: "visible",
            }],
            request: Some(RequestDoc {
                description: "Every row the table should hold.",
                body: Body::Rows,
            }),
            responses: vec![
                ResponseDoc {
                    status: 200,
                    description: "Replaced.",
                    body: Body::Json("IngestResponse"),
                },
                ResponseDoc {
                    status: 400,
                    description: "The body didn't decode, a value doesn't fit its column, or the \
                                  table isn't an upsert table. Nothing was written.",
                    body: Body::Json("IngestError"),
                },
                ResponseDoc {
                    status: 404,
                    description: "No such table.",
                    body: Body::Json("ApiError"),
                },
                ResponseDoc {
                    status: 409,
                    description: "A request with the same `Idempotency-Key` is still being processed, \
                                  or the table was removed by a config change meanwhile.",
                    body: Body::Json("ApiError"),
                },
                ResponseDoc {
                    status: 415,
                    description: "A `Content-Type` that isn't one of the three formats.",
                    body: Body::Json("ApiError"),
                },
                ResponseDoc {
                    status: 503,
                    description: "Busy: the write queue is full. Retry after `Retry-After` seconds.",
                    body: Body::Json("ApiError"),
                },
            ],
        },
        ApiDoc {
            path: "/v1/tables",
            method: Method::Get,
            operation: Operation::ListTables,
            summary: "Every table, with its columns and how much it holds",
            description: "The tables declared in `tables/*.yaml`, with live row counts, write \
                          rates and memory, as of the latest epoch.",
            tag: Tag::Tables,
            access: Access::Read,
            params: vec![],
            query: vec![],
            request: None,
            responses: vec![ResponseDoc {
                status: 200,
                description: "The tables, in name order.",
                body: Body::JsonArray("TableInfo"),
            }],
        },
        ApiDoc {
            path: "/v1/tables/{table}",
            method: Method::Get,
            operation: Operation::GetTable,
            summary: "One table",
            description: "The same as one entry of `GET /v1/tables`.",
            tag: Tag::Tables,
            access: Access::Read,
            params: vec![ParamDoc {
                name: "table",
                description: "The table, by name.",
                example: "orders",
            }],
            query: vec![],
            request: None,
            responses: vec![
                ResponseDoc {
                    status: 200,
                    description: "The table.",
                    body: Body::Json("TableInfo"),
                },
                ResponseDoc {
                    status: 404,
                    description: "No such table.",
                    body: Body::Json("ApiError"),
                },
            ],
        },
        ApiDoc {
            path: "/v1/tables/{table}/preview",
            method: Method::Get,
            operation: Operation::PreviewTable,
            summary: "The newest rows of a table",
            description: "Newest first, decoded: decimals, dates and timestamps as strings, so \
                          nothing is lost on the way through JSON.",
            tag: Tag::Tables,
            access: Access::Read,
            params: vec![ParamDoc {
                name: "table",
                description: "The table, by name.",
                example: "orders",
            }],
            query: vec![ParamDoc {
                name: "limit",
                description: "How many rows, at most 1000. Default 100.",
                example: "10",
            }],
            request: None,
            responses: vec![
                ResponseDoc {
                    status: 200,
                    description: "The rows.",
                    body: Body::Json("Preview"),
                },
                ResponseDoc {
                    status: 404,
                    description: "No such table.",
                    body: Body::Json("ApiError"),
                },
            ],
        },
        ApiDoc {
            path: "/v1/query",
            method: Method::Post,
            operation: Operation::Query,
            summary: "Run a query",
            description: "Metrics grouped by dimensions, under a selection, from the newest \
                          snapshot: the engine dashboards use, without a subscription. \
                          Filters are one-directional: a metric is filtered by the fields \
                          its table reaches through relationships, and the rest don't \
                          apply to it.",
            tag: Tag::Queries,
            access: Access::Read,
            params: vec![],
            query: vec![ParamDoc {
                name: "format",
                description: "`csv` for the answer as CSV (a header of column names, then \
                              the rows), for spreadsheets and exports. JSON otherwise.",
                example: "json",
            }],
            request: Some(RequestDoc {
                description: "What to compute.",
                body: Body::Json("QueryRequest"),
            }),
            responses: vec![
                ResponseDoc {
                    status: 200,
                    description: "The answer.",
                    body: Body::JsonOrCsv("QueryResult"),
                },
                ResponseDoc {
                    status: 400,
                    description: "The query names something the model doesn't have, or \
                                  asks for something the engine can't do.",
                    body: Body::Json("ApiError"),
                },
            ],
        },
        ApiDoc {
            path: "/v1/ws",
            method: Method::Get,
            operation: Operation::Live,
            summary: "Live dashboards (WebSocket)",
            description: "Upgrades to a WebSocket speaking the live protocol: JSON messages \
                          tagged by `type` (`ClientMessage` in, `ServerMessage` out). A \
                          session holds a selection with back/forward history and shows one \
                          dashboard; the server sends one frame per epoch with whatever \
                          changed on it, and answers selection changes right away. Browsers \
                          can't set headers on a WebSocket, so the read token may also come \
                          as `?token=`.",
            tag: Tag::Queries,
            access: Access::Read,
            params: vec![],
            query: vec![ParamDoc {
                name: "token",
                description: "The read token, when one is configured.",
                example: "s3cret",
            }],
            request: None,
            responses: vec![ResponseDoc {
                status: 101,
                description: "Switching to the WebSocket protocol.",
                body: Body::None,
            }],
        },
        ApiDoc {
            path: "/v1/model",
            method: Method::Get,
            operation: Operation::GetModel,
            summary: "The semantic model",
            description: "Datasets and their fields, relationships, and metrics with their \
                          SQL, anchor table and the dashboards that show them: what the model \
                          browser shows.",
            tag: Tag::Queries,
            access: Access::Read,
            params: vec![],
            query: vec![],
            request: None,
            responses: vec![ResponseDoc {
                status: 200,
                description: "The model.",
                body: Body::Json("ModelInfo"),
            }],
        },
        ApiDoc {
            path: "/v1/status",
            method: Method::Get,
            operation: Operation::GetStatus,
            summary: "What the server is doing",
            description: "Epoch, memory per table and for caches, live sessions, and the \
                          write-ahead log and checkpoints when data is kept on disk.",
            tag: Tag::Server,
            access: Access::Read,
            params: vec![],
            query: vec![],
            request: None,
            responses: vec![ResponseDoc {
                status: 200,
                description: "The status.",
                body: Body::Json("StatusInfo"),
            }],
        },
        ApiDoc {
            path: "/v1/config",
            method: Method::Get,
            operation: Operation::GetConfig,
            summary: "The config",
            description: "Every config file of the current version, the version the server \
                          runs (behind while a change is rebuilt), and edits in the config \
                          directory that aren't applied.",
            tag: Tag::Config,
            access: Access::Admin,
            params: vec![],
            query: vec![],
            request: None,
            responses: vec![ResponseDoc {
                status: 200,
                description: "The config.",
                body: Body::Json("ConfigState"),
            }],
        },
        ApiDoc {
            path: "/v1/config/plan",
            method: Method::Post,
            operation: Operation::PlanConfig,
            summary: "What a change would do",
            description: "Compiles the config with the change and compares it with the \
                          current one: every problem, located, and every change with its \
                          class (safe, rebuild, destructive). Changes nothing.",
            tag: Tag::Config,
            access: Access::Admin,
            params: vec![],
            query: vec![],
            request: Some(RequestDoc {
                description: "Files to set or remove, against a version.",
                body: Body::Json("ConfigChange"),
            }),
            responses: vec![
                ResponseDoc {
                    status: 200,
                    description: "The plan (also when the config has errors: `valid` says).",
                    body: Body::Json("PlanInfo"),
                },
                ResponseDoc {
                    status: 409,
                    description: "`base_version` isn't the current version.",
                    body: Body::Json("ConfigError"),
                },
            ],
        },
        ApiDoc {
            path: "/v1/config/apply",
            method: Method::Post,
            operation: Operation::ApplyConfig,
            summary: "Apply a change",
            description: "Plans the change and applies it: safe changes are live at the next \
                          epoch, rebuilds when their background rebuild is done (the old \
                          version serves until then). A destructive change needs `approve`. \
                          With `propose` it waits in the approval queue instead. Applied \
                          changes are written back to the config directory, and committed \
                          when it is a git repository.",
            tag: Tag::Config,
            access: Access::Admin,
            params: vec![],
            query: vec![],
            request: Some(RequestDoc {
                description: "Files to set or remove, against a version.",
                body: Body::Json("ConfigChange"),
            }),
            responses: vec![
                ResponseDoc {
                    status: 200,
                    description: "Applied, rebuilding, queued, or nothing to change.",
                    body: Body::Json("ApplyResult"),
                },
                ResponseDoc {
                    status: 409,
                    description: "`base_version` isn't current, the change destroys data and \
                                  isn't approved, or a rebuild is still running.",
                    body: Body::Json("ConfigError"),
                },
                ResponseDoc {
                    status: 422,
                    description: "The config doesn't compile with the change.",
                    body: Body::Json("ConfigError"),
                },
            ],
        },
        ApiDoc {
            path: "/v1/config/versions",
            method: Method::Get,
            operation: Operation::ListVersions,
            summary: "Versions so far",
            description: "Every applied version, newest first: who, when, from where, and \
                          what changed. Reverting is applying an old version's files.",
            tag: Tag::Config,
            access: Access::Admin,
            params: vec![],
            query: vec![],
            request: None,
            responses: vec![ResponseDoc {
                status: 200,
                description: "The versions.",
                body: Body::JsonArray("VersionInfo"),
            }],
        },
        ApiDoc {
            path: "/v1/config/versions/{version}",
            method: Method::Get,
            operation: Operation::GetVersion,
            summary: "One version's files",
            description: "The files of an applied version, to compare or revert to.",
            tag: Tag::Config,
            access: Access::Admin,
            params: vec![ParamDoc {
                name: "version",
                description: "The version number.",
                example: "1",
            }],
            query: vec![],
            request: None,
            responses: vec![
                ResponseDoc {
                    status: 200,
                    description: "The version.",
                    body: Body::Json("ConfigVersion"),
                },
                ResponseDoc {
                    status: 404,
                    description: "No such version.",
                    body: Body::Json("ApiError"),
                },
            ],
        },
        ApiDoc {
            path: "/v1/changes",
            method: Method::Get,
            operation: Operation::ListChanges,
            summary: "Changes waiting for approval",
            description: "Proposed changes, and edits in the config directory that aren't \
                          safe to apply on their own, with their plans.",
            tag: Tag::Config,
            access: Access::Admin,
            params: vec![],
            query: vec![],
            request: None,
            responses: vec![ResponseDoc {
                status: 200,
                description: "The pending changes.",
                body: Body::JsonArray("PendingChange"),
            }],
        },
        ApiDoc {
            path: "/v1/changes/{id}/approve",
            method: Method::Post,
            operation: Operation::ApproveChange,
            summary: "Approve a pending change",
            description: "Applies it, destructive or not. It must still be against the \
                          current version.",
            tag: Tag::Config,
            access: Access::Admin,
            params: vec![ParamDoc {
                name: "id",
                description: "The pending change.",
                example: "1",
            }],
            query: vec![],
            request: None,
            responses: vec![
                ResponseDoc {
                    status: 200,
                    description: "Applied or rebuilding.",
                    body: Body::Json("ApplyResult"),
                },
                ResponseDoc {
                    status: 404,
                    description: "No such pending change.",
                    body: Body::Json("ApiError"),
                },
                ResponseDoc {
                    status: 409,
                    description: "It was made against an older version, or a rebuild is running.",
                    body: Body::Json("ConfigError"),
                },
            ],
        },
        ApiDoc {
            path: "/v1/changes/{id}/reject",
            method: Method::Post,
            operation: Operation::RejectChange,
            summary: "Reject a pending change",
            description: "Drops it from the queue.",
            tag: Tag::Config,
            access: Access::Admin,
            params: vec![ParamDoc {
                name: "id",
                description: "The pending change.",
                example: "1",
            }],
            query: vec![],
            request: None,
            responses: vec![
                ResponseDoc {
                    status: 200,
                    description: "The change, now dropped.",
                    body: Body::Json("PendingChange"),
                },
                ResponseDoc {
                    status: 404,
                    description: "No such pending change.",
                    body: Body::Json("ApiError"),
                },
            ],
        },
        ApiDoc {
            path: "/v1/auth/session",
            method: Method::Post,
            operation: Operation::StartSession,
            summary: "Start a browser session",
            description: "Trades the token in `Authorization: Bearer <token>` for a session \
                          cookie (`HttpOnly`, `SameSite=Strict`), good for the levels the \
                          token is: the UI does this so tokens stay out of URLs and logs. \
                          Sessions end after 12 idle hours, on sign-out, or when the server \
                          stops.",
            tag: Tag::Server,
            access: Access::Public,
            params: vec![],
            query: vec![],
            request: None,
            responses: vec![
                ResponseDoc {
                    status: 200,
                    description: "The session's levels; the cookie is set.",
                    body: Body::Json("SessionInfo"),
                },
                ResponseDoc {
                    status: 400,
                    description: "No bearer token.",
                    body: Body::Json("ApiError"),
                },
                ResponseDoc {
                    status: 401,
                    description: "The token is good for nothing here.",
                    body: Body::Json("ApiError"),
                },
            ],
        },
        ApiDoc {
            path: "/v1/auth/session",
            method: Method::Get,
            operation: Operation::GetSession,
            summary: "What this request may do",
            description: "The levels open to this request: by its session cookie, its bearer \
                          token, or because they need no token.",
            tag: Tag::Server,
            access: Access::Public,
            params: vec![],
            query: vec![],
            request: None,
            responses: vec![ResponseDoc {
                status: 200,
                description: "The levels.",
                body: Body::Json("SessionInfo"),
            }],
        },
        ApiDoc {
            path: "/v1/auth/session",
            method: Method::Delete,
            operation: Operation::EndSession,
            summary: "Sign out",
            description: "Ends this browser's session and clears its cookie.",
            tag: Tag::Server,
            access: Access::Public,
            params: vec![],
            query: vec![],
            request: None,
            responses: vec![ResponseDoc {
                status: 200,
                description: "Signed out: what's left open to everyone.",
                body: Body::Json("SessionInfo"),
            }],
        },
        ApiDoc {
            path: "/mcp",
            method: Method::Post,
            operation: Operation::Mcp,
            summary: "MCP for agents",
            description: "The Model Context Protocol over Streamable HTTP: JSON-RPC 2.0 \
                          messages, each answered with one JSON body. Tools: describe_model, \
                          preview_table, query_metrics, get_config, plan_change, \
                          apply_change and ingest_rows. Each tool is the HTTP endpoint it \
                          wraps, called with the caller's credentials, so tools that change \
                          the config need the admin token and ingest_rows the ingest token. \
                          apply_change applies what `agents.auto_apply` allows and proposes \
                          the rest.",
            tag: Tag::Queries,
            access: Access::Read,
            params: vec![],
            query: vec![],
            request: Some(RequestDoc {
                description: "A JSON-RPC message, or a batch of them.",
                body: Body::AnyJson,
            }),
            responses: vec![
                ResponseDoc {
                    status: 200,
                    description: "The JSON-RPC reply (or replies).",
                    body: Body::AnyJson,
                },
                ResponseDoc {
                    status: 202,
                    description: "A notification: nothing to reply.",
                    body: Body::None,
                },
            ],
        },
        ApiDoc {
            path: "/metrics",
            method: Method::Get,
            operation: Operation::Metrics,
            summary: "Prometheus metrics",
            description: "The server in the Prometheus text format: rows, memory and \
                          segments per table, ingest requests and rows, epoch publish and \
                          live refresh times, query times, sessions, the log and \
                          checkpoints, a running rebuild. At the root, where scrapers look.",
            tag: Tag::Server,
            access: Access::Read,
            params: vec![],
            query: vec![],
            request: None,
            responses: vec![ResponseDoc {
                status: 200,
                description: "The metrics.",
                body: Body::Text,
            }],
        },
        ApiDoc {
            path: "/healthz",
            method: Method::Get,
            operation: Operation::Health,
            summary: "Liveness probe",
            description: "Answers `200` whenever the process is serving HTTP at all. \
                          It says nothing about whether the data is ready to query; \
                          that is `GET /readyz`.",
            tag: Tag::Server,
            access: Access::Public,
            params: vec![],
            query: vec![],
            request: None,
            responses: vec![ResponseDoc {
                status: 200,
                description: "The process is up.",
                body: Body::Json("ProbeStatus"),
            }],
        },
        ApiDoc {
            path: "/readyz",
            method: Method::Get,
            operation: Operation::Ready,
            summary: "Readiness probe",
            description: "Answers `200` once the server can take traffic, and `503` \
                          while it is still starting. Later, starting includes \
                          applying the config and recovering data from the WAL, which \
                          is why this is separate from `GET /healthz`: a restart \
                          should not be killed for being slow to recover.",
            tag: Tag::Server,
            access: Access::Public,
            params: vec![],
            query: vec![],
            request: None,
            responses: vec![
                ResponseDoc {
                    status: 200,
                    description: "Ready.",
                    body: Body::Json("ProbeStatus"),
                },
                ResponseDoc {
                    status: 503,
                    description: "Up, but not ready yet.",
                    body: Body::Json("ProbeStatus"),
                },
            ],
        },
        ApiDoc {
            path: "/v1/info",
            method: Method::Get,
            operation: Operation::GetInfo,
            summary: "Which server this is",
            description: "The server's name, version and API version.",
            tag: Tag::Server,
            access: Access::Public,
            params: vec![],
            query: vec![],
            request: None,
            responses: vec![ResponseDoc {
                status: 200,
                description: "The server's identity.",
                body: Body::Json("ServerInfo"),
            }],
        },
        ApiDoc {
            path: "/v1/openapi.json",
            method: Method::Get,
            operation: Operation::GetOpenApi,
            summary: "This API, as OpenAPI 3.1",
            description: "Generated from the same endpoint table the router is built \
                          from, so it can't describe a route that isn't served.",
            tag: Tag::Reference,
            access: Access::Public,
            params: vec![],
            query: vec![],
            request: None,
            responses: vec![ResponseDoc {
                status: 200,
                description: "The OpenAPI document.",
                body: Body::AnyJson,
            }],
        },
        ApiDoc {
            path: "/v1/schemas/{kind}",
            method: Method::Get,
            operation: Operation::GetSchema,
            summary: "The JSON Schema of a config file",
            description: "For editors (yaml-language-server), the config editor and agents writing \
                          config. Kinds: `server` (tidepool.yaml), `table` (tables/*.yaml), \
                          `model` (model.ossie.yaml), `dashboard` (dashboards/*.yaml), and the \
                          JSON inside TIDEPOOL extensions: `metric-extension`, \
                          `field-extension`, `dataset-extension`, `relationship-extension`, \
                          `model-extension`.",
            tag: Tag::Reference,
            access: Access::Public,
            params: vec![ParamDoc {
                name: "kind",
                description: "Which kind of config file.",
                example: "table",
            }],
            query: vec![],
            request: None,
            responses: vec![
                ResponseDoc {
                    status: 200,
                    description: "The schema.",
                    body: Body::AnyJson,
                },
                ResponseDoc {
                    status: 404,
                    description: "No such kind.",
                    body: Body::Json("ApiError"),
                },
            ],
        },
    ]
}

/// The JSON Schema of every body named in [`endpoints`], keyed by name.
#[must_use]
pub fn schemas() -> BTreeMap<&'static str, Value> {
    // schemars only produces serialisable schemas, so the fallback is unreachable
    let of = |schema: schemars::Schema| serde_json::to_value(schema).unwrap_or(Value::Null);

    let mut schemas = BTreeMap::new();
    schemas.insert("ApiError", of(schema_for!(ApiError)));
    schemas.insert("ProbeStatus", of(schema_for!(ProbeStatus)));
    schemas.insert("ServerInfo", of(schema_for!(ServerInfo)));
    schemas.insert("IngestResponse", of(schema_for!(IngestResponse)));
    schemas.insert("IngestError", of(schema_for!(IngestError)));
    schemas.insert("TableInfo", of(schema_for!(TableInfo)));
    schemas.insert("Preview", of(schema_for!(Preview)));
    schemas.insert("QueryRequest", of(schema_for!(QueryRequest)));
    schemas.insert("QueryResult", of(schema_for!(QueryResult)));
    schemas.insert("ModelInfo", of(schema_for!(ModelInfo)));
    schemas.insert("StatusInfo", of(schema_for!(StatusInfo)));
    schemas.insert("ConfigState", of(schema_for!(config::ConfigState)));
    schemas.insert("ConfigChange", of(schema_for!(config::ConfigChange)));
    schemas.insert("PlanInfo", of(schema_for!(config::PlanInfo)));
    schemas.insert("ApplyResult", of(schema_for!(config::ApplyResult)));
    schemas.insert("ConfigError", of(schema_for!(config::ConfigError)));
    schemas.insert("VersionInfo", of(schema_for!(config::VersionInfo)));
    schemas.insert("ConfigVersion", of(schema_for!(config::ConfigVersion)));
    schemas.insert("PendingChange", of(schema_for!(config::PendingChange)));
    schemas.insert("SessionInfo", of(schema_for!(SessionInfo)));
    schemas
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn every_operation_appears_exactly_once() {
        let documented: Vec<Operation> = endpoints().iter().map(|e| e.operation).collect();
        for operation in OPERATIONS {
            let count = documented.iter().filter(|o| **o == operation).count();
            assert_eq!(count, 1, "{operation:?} is documented {count} times");
        }
        assert_eq!(
            documented.len(),
            OPERATIONS.len(),
            "OPERATIONS is missing an entry"
        );
    }

    #[test]
    fn path_and_method_pairs_are_unique() {
        let mut seen = HashSet::new();
        for endpoint in endpoints() {
            assert!(
                seen.insert((endpoint.path, endpoint.method)),
                "{} {} is documented twice",
                endpoint.method.label(),
                endpoint.path
            );
        }
    }

    #[test]
    fn operation_ids_are_unique() {
        let ids: HashSet<&str> = OPERATIONS.iter().map(|o| o.id()).collect();
        assert_eq!(ids.len(), OPERATIONS.len());
    }

    #[test]
    fn every_named_body_has_a_schema() {
        let schemas = schemas();
        for endpoint in endpoints() {
            let bodies = endpoint
                .responses
                .iter()
                .map(|r| r.body)
                .chain(endpoint.request.map(|r| r.body));
            for body in bodies {
                if let Some(name) = body.schema_name() {
                    assert!(
                        schemas.contains_key(name),
                        "{} names schema {name}, which schemas() doesn't have",
                        endpoint.path
                    );
                }
            }
        }
    }

    #[test]
    fn path_params_match_the_placeholders_in_the_path() {
        for endpoint in endpoints() {
            let placeholders: Vec<&str> = endpoint
                .path
                .split('/')
                .filter_map(|s| s.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
                .collect();
            let documented: Vec<&str> = endpoint.params.iter().map(|p| p.name).collect();
            assert_eq!(placeholders, documented, "params of {}", endpoint.path);
        }
    }

    #[test]
    fn versioned_api_lives_under_v1_and_probes_at_the_root() {
        // probes, Prometheus metrics and the MCP endpoint are where their
        // clients look for them (plan §9); everything else is versioned
        for endpoint in endpoints() {
            let probe = matches!(
                endpoint.operation,
                Operation::Health | Operation::Ready | Operation::Metrics | Operation::Mcp
            );
            assert_eq!(
                !probe,
                endpoint.path.starts_with("/v1/"),
                "{} is in the wrong namespace",
                endpoint.path
            );
        }
    }
}
