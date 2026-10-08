//! Who may call what: bearer tokens per access level, and browser sessions
//! (plan §9).
//!
//! Each endpoint declares its [`Access`] in the endpoint table, and the router
//! wraps it in [`authorize`] with that same value, so the documented access
//! and the enforced one can't drift. A level whose token isn't configured is
//! open, which is the right default on loopback and the reason `serve` warns
//! when it listens anywhere else. The admin token is good for every level.
//!
//! A browser exchanges a token for a session (`POST /v1/auth/session`): an
//! `HttpOnly` cookie, so the token isn't kept in page URLs, history or access
//! logs, and the WebSocket, which can't set headers, is authenticated by the
//! cookie it carries anyway. Sessions live in memory and end with the
//! process, as Kayak's do: revoking one is removing it.
//!
//! Not here yet (after v0.1): Kayak's accounts with passwords, JWT from an
//! identity provider, and the iframe handshake.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;
use tidepool_protocol::SessionInfo;
use tidepool_protocol::api_docs::{Access, ApiError};

use crate::AppState;

/// The configured tokens. `None` leaves that access level open.
#[derive(Clone, Debug, Default)]
pub struct Tokens {
    /// For `Access::Ingest`: writing and deleting rows.
    pub ingest: Option<String>,
    /// For `Access::Read`: looking at data and the model.
    pub read: Option<String>,
    /// For `Access::Admin`: changing configuration. Good for every level.
    pub admin: Option<String>,
}

impl Tokens {
    /// Read from `TIDEPOOL_INGEST_TOKEN`, `TIDEPOOL_READ_TOKEN` and
    /// `TIDEPOOL_ADMIN_TOKEN`; empty counts as unset.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_settings(&tidepool_config::server::AuthSettings::default())
    }

    /// Read from the variables `tidepool.yaml`'s `auth` names.
    #[must_use]
    pub fn from_settings(settings: &tidepool_config::server::AuthSettings) -> Self {
        let get = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        Self {
            ingest: get(&settings.ingest_token_env),
            read: get(&settings.read_token_env),
            admin: get(&settings.admin_token_env),
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

    /// Whether `access` needs no token at all.
    #[must_use]
    pub fn open(&self, access: Access) -> bool {
        self.required(access).is_none()
    }

    /// Whether `presented` may use `access`: its own token, or the admin's.
    #[must_use]
    pub fn allows(&self, presented: &str, access: Access) -> bool {
        let same = |expected: Option<&str>| {
            expected.is_some_and(|e| bool::from(presented.as_bytes().ct_eq(e.as_bytes())))
        };
        self.open(access) || same(self.required(access)) || same(self.admin.as_deref())
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

/// The session cookie's name.
pub const COOKIE: &str = "tidepool_session";

/// How long a session lasts without being used.
const IDLE: Duration = Duration::from_hours(12);

struct Session {
    /// The levels it was opened for (the token's).
    levels: Vec<Access>,
    used: Instant,
}

/// Browser sessions, by their cookie's value.
#[derive(Default)]
pub struct Sessions {
    open: Mutex<HashMap<String, Session>>,
}

impl Sessions {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        self.open.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A new session good for `levels`; its id.
    fn start(&self, levels: Vec<Access>) -> String {
        let mut bytes = [0u8; 32];
        // a session id nobody can guess; an operating system without a
        // random source can't run a server that hands out sessions
        if getrandom::fill(&mut bytes).is_err() {
            return String::new();
        }
        let id = bytes.iter().fold(String::with_capacity(64), |mut id, b| {
            let _ = std::fmt::Write::write_fmt(&mut id, format_args!("{b:02x}"));
            id
        });
        let mut open = self.lock();
        open.retain(|_, s| s.used.elapsed() < IDLE);
        open.insert(
            id.clone(),
            Session {
                levels,
                used: Instant::now(),
            },
        );
        id
    }

    /// The levels of a live session, refreshing its idle clock.
    fn levels(&self, id: &str) -> Option<Vec<Access>> {
        let mut open = self.lock();
        let session = open.get_mut(id).filter(|s| s.used.elapsed() < IDLE)?;
        session.used = Instant::now();
        Some(session.levels.clone())
    }

    fn end(&self, id: &str) {
        self.lock().remove(id);
    }
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
}

fn cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| {
            pair.trim()
                .strip_prefix(&format!("{COOKIE}="))
                .map(str::to_string)
        })
}

/// Let the request through if it may use `access`: no token needed, the
/// token in the `Authorization` header or `?token=`, or a session cookie.
pub async fn authorize(access: Access, state: &AppState, request: Request, next: Next) -> Response {
    let tokens = &state.tokens;
    if tokens.open(access) {
        return next.run(request).await;
    }
    // `?token=` for clients that can't set headers and have no session
    let from_query = request.uri().query().and_then(|q| {
        q.split('&')
            .find_map(|pair| pair.strip_prefix("token="))
            .map(str::to_string)
    });
    let presented = bearer(request.headers()).or(from_query);
    let by_token = presented
        .as_deref()
        .is_some_and(|token| tokens.allows(token, access));
    let by_session = !by_token
        && cookie(request.headers())
            .and_then(|id| state.sessions.levels(&id))
            .is_some_and(|levels| levels.contains(&access));
    if by_token || by_session {
        return next.run(request).await;
    }
    let message = if presented.is_some() {
        "the token isn't valid for this endpoint"
    } else {
        "this endpoint needs `Authorization: Bearer <token>` or a session"
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

const LEVELS: [Access; 3] = [Access::Read, Access::Ingest, Access::Admin];

fn info(levels: &[Access], state: &AppState) -> SessionInfo {
    let may = |a: Access| levels.contains(&a) || state.tokens.open(a);
    SessionInfo {
        read: may(Access::Read),
        ingest: may(Access::Ingest),
        admin: may(Access::Admin),
    }
}

/// `POST /v1/auth/session`: the bearer token for a session cookie.
pub async fn start_session(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let Some(token) = bearer(&headers) else {
        return crate::handlers::api_error(
            StatusCode::BAD_REQUEST,
            "send the token as `Authorization: Bearer <token>`",
        );
    };
    let levels: Vec<Access> = LEVELS
        .into_iter()
        .filter(|a| !state.tokens.open(*a) && state.tokens.allows(&token, *a))
        .collect();
    if levels.is_empty() {
        return crate::handlers::api_error(
            StatusCode::UNAUTHORIZED,
            "the token isn't valid for anything here",
        );
    }
    let id = state.sessions.start(levels.clone());
    if id.is_empty() {
        return crate::handlers::api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "no random source for a session id",
        );
    }
    let cookie = format!(
        "{COOKIE}={id}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        IDLE.as_secs()
    );
    let mut response = Json(info(&levels, &state)).into_response();
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

/// `GET /v1/auth/session`: what this request may do.
pub async fn session(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Json<SessionInfo> {
    let levels = cookie(&headers)
        .and_then(|id| state.sessions.levels(&id))
        .or_else(|| {
            bearer(&headers).map(|token| {
                LEVELS
                    .into_iter()
                    .filter(|a| state.tokens.allows(&token, *a))
                    .collect()
            })
        })
        .unwrap_or_default();
    Json(info(&levels, &state))
}

/// `DELETE /v1/auth/session`: sign out.
pub async fn end_session(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(id) = cookie(&headers) {
        state.sessions.end(&id);
    }
    let mut response = Json(info(&[], &state)).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("tidepool_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"),
    );
    response
}
