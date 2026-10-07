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
use axum::handler::Handler;
use axum::routing::{MethodRouter, delete, get, post, put};
use tidepool_protocol::api_docs::{ApiDoc, Method, Operation, endpoints};

use crate::AppState;
use crate::handlers;

/// The documented JSON surface, without the UI fallback.
pub fn api_router(state: Arc<AppState>) -> Router {
    endpoints()
        .iter()
        .fold(Router::new(), |router, doc| {
            router.route(doc.path, handler_for(doc))
        })
        .with_state(state)
}

/// The handler serving one documented operation.
///
/// Exhaustive over [`Operation`] on purpose: this match is why a new endpoint
/// can't be documented and left unrouted. Access checks wrap this once
/// authentication exists (M8), from `doc.access`.
fn handler_for(doc: &ApiDoc) -> MethodRouter<Arc<AppState>> {
    let method = doc.method;
    match doc.operation {
        Operation::Health => route_of(method, handlers::health),
        Operation::Ready => route_of(method, handlers::ready),
        Operation::GetInfo => route_of(method, handlers::info),
        Operation::GetOpenApi => route_of(method, handlers::openapi),
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
