//! Where the documented endpoints and the running handlers meet.
//!
//! [`api_router`] is a fold over [`endpoints`] rather than a list of `.route()`
//! calls: a route that isn't in the table doesn't get registered, so the
//! documentation can't describe a server that doesn't exist. The other
//! direction is the compiler's job: [`handler_for`] matches exhaustively on
//! [`Operation`], so a new table entry fails to build until it has a handler.
//! [`route_of`] takes the method from the table too, so an entry documented as
//! `PUT` and wired to `post(...)` can't be written.

use std::sync::Arc;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Request};
use axum::handler::Handler;
use axum::middleware::Next;
use axum::routing::{MethodRouter, delete, get, post, put};
use tidepool_protocol::api_docs::{ApiDoc, Body, Method, Operation, endpoints};

use crate::AppState;
use crate::{auth, config, handlers, live, mcp, metrics, model, query, tables};

/// The documented JSON surface, without the UI fallback.
pub fn api_router(state: Arc<AppState>) -> Router {
    endpoints()
        .iter()
        .fold(Router::new(), |router, doc| {
            router.route(doc.path, guarded(doc, &state))
        })
        .with_state(state)
}

/// One endpoint's handler behind the access check its table entry declares,
/// and with a body limit that fits what it accepts.
///
/// `route_layer` rather than `layer`: it runs only for requests that matched
/// the route, so an unknown path is a 404, never a 401.
fn guarded(doc: &ApiDoc, state: &Arc<AppState>) -> MethodRouter<Arc<AppState>> {
    let access = doc.access;
    let state = Arc::clone(state);
    let mut route = handler_for(doc).route_layer(axum::middleware::from_fn(
        move |request: Request, next: Next| {
            let state = Arc::clone(&state);
            async move { crate::auth::authorize(access, &state, request, next).await }
        },
    ));
    if doc.request.is_some_and(|r| r.body == Body::Rows) {
        route = route.layer(DefaultBodyLimit::max(tables::MAX_BODY));
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
