//! Who may call what: bearer tokens per access level (plan §9, v1).
//!
//! Each endpoint declares its [`Access`] in the endpoint table, and the router
//! wraps it in [`authorize`] with that same value, so the documented access
//! and the enforced one can't drift. A level whose token isn't configured is
//! open, which is the right default on loopback and the reason `serve` warns
//! when it listens anywhere else. Kayak's richer auth (accounts, sessions,
//! JWT) replaces this in M8; the per-endpoint `Access` stays.

use axum::Json;
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;
use tidepool_protocol::api_docs::{Access, ApiError};

/// The configured tokens. `None` leaves that access level open.
#[derive(Clone, Debug, Default)]
pub struct Tokens {
    /// For `Access::Ingest`: writing and deleting rows.
    pub ingest: Option<String>,
    /// For `Access::Read`: looking at data and the model.
    pub read: Option<String>,
    /// For `Access::Admin`: changing configuration.
    pub admin: Option<String>,
}

impl Tokens {
    /// Read from `TIDEPOOL_INGEST_TOKEN`, `TIDEPOOL_READ_TOKEN` and
    /// `TIDEPOOL_ADMIN_TOKEN`; empty counts as unset.
    #[must_use]
    pub fn from_env() -> Self {
        let get = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        Self {
            ingest: get("TIDEPOOL_INGEST_TOKEN"),
            read: get("TIDEPOOL_READ_TOKEN"),
            admin: get("TIDEPOOL_ADMIN_TOKEN"),
        }
    }

    /// The token an access level requires, if any.
    #[must_use]
    pub fn required(&self, access: Access) -> Option<&str> {
        match access {
            Access::Public => None,
            Access::Read => self.read.as_deref(),
            Access::Ingest => self.ingest.as_deref(),
            Access::Admin => self.admin.as_deref(),
        }
    }

    /// The access levels that are open because their token is unset.
    #[must_use]
    pub fn open_levels(&self) -> Vec<&'static str> {
        [
            (Access::Read, &self.read),
            (Access::Ingest, &self.ingest),
            (Access::Admin, &self.admin),
        ]
        .iter()
        .filter(|(_, token)| token.is_none())
        .map(|(access, _)| access.label())
        .collect()
    }
}

/// Let the request through if it carries the token `access` requires.
pub async fn authorize(access: Access, tokens: &Tokens, request: Request, next: Next) -> Response {
    let Some(expected) = tokens.required(access) else {
        return next.run(request).await;
    };
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let ok = presented.is_some_and(|token| bool::from(token.as_bytes().ct_eq(expected.as_bytes())));
    if ok {
        return next.run(request).await;
    }
    let message = if presented.is_some() {
        "the token isn't valid for this endpoint"
    } else {
        "this endpoint needs `Authorization: Bearer <token>`"
    };
    let mut response = (
        StatusCode::UNAUTHORIZED,
        Json(ApiError {
            error: message.to_string(),
        }),
    )
        .into_response();
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"tidepool\""),
    );
    response
}
