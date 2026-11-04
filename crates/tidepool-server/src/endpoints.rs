//! Where the documented endpoints and the running handlers meet.
//!
//! [`api_router`] is a fold over [`endpoints`] rather than a list of `.route()`
//! calls: a route that isn't in the table doesn't get registered, so the
//! documentation can't describe a server that doesn't exist. The other
//! direction is the compiler's job: [`handler_for`] matches exhaustively on
//! [`Operation`], so a new table entry fails to build until it has a handler.
//! [`route_of`] takes the method from the table too, so an entry documented as
//! `PUT` and wired to `post(...)` can't be written.
//!
//! Each listener gets its own fold ([`api_router_on`], plan §9.2): which
//! endpoints it serves follows from their [`Access`] ([`placed`]), and every
//! request carries the [`Listener`] it came in on.

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Request};
use axum::handler::Handler;
use axum::middleware::Next;
use axum::routing::{MethodRouter, delete, get, options, post, put};
use axum::{Extension, Router};
use tidepool_protocol::api_docs::{Access, ApiDoc, Body, Method, Operation, endpoints};

use crate::AppState;
use crate::auth::Listener;
use crate::{auth, config, handlers, live, mcp, metrics, model, query, tables};

/// The documented JSON surface, without the UI fallback, as one listener
/// serving everything has it.
pub fn api_router(state: Arc<AppState>) -> Router {
    api_router_on(state, Listener::ALL)
}

/// The documented JSON surface as `listener` serves it.
pub fn api_router_on(state: Arc<AppState>, listener: Listener) -> Router {
    let mut router = endpoints()
        .iter()
        .filter(|doc| placed(doc, listener, &state))
        .fold(Router::new(), |router, doc| {
            router.route(doc.path, guarded(doc, &state, listener))
        });
    // a browser asks before it writes from another origin (CORS); not an
    // endpoint of its own, so not in the table
    if let Some(rows) = endpoints()
        .iter()
        .find(|doc| doc.operation == Operation::IngestRows && placed(doc, listener, &state))
    {
        router = router.route(rows.path, options(tables::preflight));
    }
    router.layer(Extension(listener)).with_state(state)
}

/// Whether `path` is one the endpoint table documents (on some listener).
pub(crate) fn documented(path: &str) -> bool {
    let parts: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    endpoints().iter().any(|doc| {
        let template: Vec<&str> = doc.path.split('/').collect();
        template.len() == parts.len()
            && template
                .iter()
                .zip(&parts)
                .all(|(t, p)| t == p || (t.starts_with('{') && !p.is_empty()))
    })
}

/// Whether `listener` serves `doc`: reading everywhere, config and
/// `/metrics` on the admin surface, ingest on the ingest and admin
/// surfaces, and on the public one only when some credential is `public`
/// (which tables it writes there, the handler decides).
pub(crate) fn placed(doc: &ApiDoc, listener: Listener, state: &AppState) -> bool {
    match (doc.operation, doc.access) {
        (Operation::Metrics, _) | (_, Access::Admin) => listener.admin,
        (_, Access::Public | Access::Read) => true,
        (_, Access::Ingest) => {
            !listener.public_only() || state.tokens.ingest.iter().any(|c| c.public)
        }
    }
}

/// One endpoint's handler behind the access check its table entry declares,
/// and with a body limit that fits what it accepts.
///
/// `route_layer` rather than `layer`: it runs only for requests that matched
/// the route, so an unknown path is a 404, never a 401.
fn guarded(doc: &ApiDoc, state: &Arc<AppState>, listener: Listener) -> MethodRouter<Arc<AppState>> {
    let access = doc.access;
    let shared = Arc::clone(state);
    let mut route = handler_for(doc).route_layer(axum::middleware::from_fn(
        move |request: Request, next: Next| {
            let state = Arc::clone(&shared);
            async move { crate::auth::authorize(access, &state, request, next).await }
        },
    ));
    if doc.request.is_some_and(|r| r.body == Body::Rows) {
        // on the public listener, no more than the largest public body: a
        // bigger one isn't read before it is refused
        let max = if listener.public_only() {
            state
                .tokens
                .ingest
                .iter()
                .filter(|c| c.public)
                .filter_map(|c| c.limits.body)
                .map(|b| usize::try_from(b.0).unwrap_or(usize::MAX))
                .max()
                .unwrap_or(tables::MAX_BODY)
                .min(tables::MAX_BODY)
        } else {
            tables::MAX_BODY
        };
        route = route.layer(DefaultBodyLimit::max(max));
    }
    route
}

/// The handler serving one documented operation.
///
/// Exhaustive over [`Operation`] on purpose: this match is why a new endpoint
/// can't be documented and left unrouted.
fn handler_for(doc: &ApiDoc) -> MethodRouter<Arc<AppState>> {
    let method = doc.method;
    match doc.operation {
        Operation::Health => route_of(method, handlers::health),
        Operation::Ready => route_of(method, handlers::ready),
        Operation::GetInfo => route_of(method, handlers::info),
        Operation::GetOpenApi => route_of(method, handlers::openapi),
        Operation::GetSchema => route_of(method, handlers::schema),
        Operation::IngestRows => route_of(method, tables::ingest_rows),
        Operation::DeleteRows => route_of(method, tables::delete_rows),
        Operation::ReplaceRows => route_of(method, tables::replace_rows),
        Operation::ListTables => route_of(method, tables::list_tables),
        Operation::GetTable => route_of(method, tables::get_table),
        Operation::PreviewTable => route_of(method, tables::preview_table),
        Operation::Query => route_of(method, query::query),
        Operation::Rows => route_of(method, query::rows),
        Operation::Live => route_of(method, live::socket),
        Operation::GetModel => route_of(method, model::model),
        Operation::GetStatus => route_of(method, model::status),
        Operation::GetConfig => route_of(method, config::get_config),
        Operation::PlanConfig => route_of(method, config::plan_config),
        Operation::ApplyConfig => route_of(method, config::apply_config),
        Operation::ListVersions => route_of(method, config::list_versions),
        Operation::GetVersion => route_of(method, config::get_version),
        Operation::ListChanges => route_of(method, config::list_changes),
        Operation::ApproveChange => route_of(method, config::approve_change),
        Operation::RejectChange => route_of(method, config::reject_change),
        Operation::Metrics => route_of(method, metrics::metrics),
        Operation::StartSession => route_of(method, auth::start_session),
        Operation::GetSession => route_of(method, auth::session),
        Operation::EndSession => route_of(method, auth::end_session),
        Operation::Mcp => route_of(method, mcp::mcp),
    }
}

/// Register a handler under the method the table says it serves.
fn route_of<H, T>(method: Method, handler: H) -> MethodRouter<Arc<AppState>>
where
    H: Handler<T, Arc<AppState>>,
    T: 'static,
{
    match method {
        Method::Get => get(handler),
        Method::Post => post(handler),
        Method::Put => put(handler),
        Method::Delete => delete(handler),
    }
}
