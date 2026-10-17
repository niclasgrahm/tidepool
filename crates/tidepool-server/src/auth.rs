//! Who may call what: bearer tokens per access level, named ingest
//! credentials, and browser sessions (plan §9, §9.2).
//!
//! Each endpoint declares its [`Access`] in the endpoint table, and the router
//! wraps it in [`authorize`] with that same value, so the documented access
//! and the enforced one can't drift. A read or admin level whose token isn't
//! configured is open; the admin token is good for every level.
//!
//! Ingest is done by [`Credential`]s, each scoped to tables and operations:
//! the single ingest token of old is one named `ingest`, good for
//! everything, and open when its variable is unset. [`authorize`] works out
//! who is calling ([`Caller`]); which table and operation it may write is
//! decided by the handler, which knows them ([`Caller::writer`]), together
//! with the listener the request came in on ([`Listener`]): only
//! credentials marked `public` write through the public one.
//!
//! A browser exchanges a token for a session (`POST /v1/auth/session`): an
//! `HttpOnly` cookie, so the token isn't kept in page URLs, history or access
//! logs, and the WebSocket, which can't set headers, is authenticated by the
//! cookie it carries anyway. Sessions live in memory and end with the
//! process, as Kayak's do: revoking one is removing it.
//!
//! Not here yet (after v0.1): Kayak's accounts with passwords, JWT from an
//! identity provider, and the iframe handshake.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;
use tidepool_config::server::{
    AuthSettings, IngestCredential, IngestOperation, LEGACY_CREDENTIAL, Limits, Surface,
};
use tidepool_protocol::SessionInfo;
use tidepool_protocol::api_docs::{Access, ApiError};

use crate::AppState;

/// The parts of the surface a listener serves (plan §9.2): a request
/// carries the listener it came in on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Listener {
    pub public: bool,
    pub ingest: bool,
    pub admin: bool,
}

impl Listener {
    /// One listener serving everything: the single `listen` address.
    pub const ALL: Self = Self {
        public: true,
        ingest: true,
        admin: true,
    };

    #[must_use]
    pub fn of(surfaces: &[Surface]) -> Self {
        Self {
            public: surfaces.contains(&Surface::Public),
            ingest: surfaces.contains(&Surface::Ingest),
            admin: surfaces.contains(&Surface::Admin),
        }
    }

    /// Only the public surface: ingest here is for `public` credentials.
    #[must_use]
    pub fn public_only(self) -> bool {
        !self.ingest && !self.admin
    }
}

/// A producer that may write rows: an `auth.ingest` entry with its tokens
/// read from the environment.
#[derive(Debug)]
pub struct Credential {
    pub name: String,
    tokens: Vec<String>,
    /// Needs no token: `token: none`, or the single ingest token unset.
    pub open: bool,
    /// The tables it may write; `None` is every table.
    tables: Option<BTreeSet<String>>,
    operations: Vec<IngestOperation>,
    pub public: bool,
    /// Its limits, defaults applied.
    pub limits: Limits,
    /// Browser origins it may write from.
    pub origins: Vec<String>,
    /// Variables it names that are unset: it can't be used through them.
    pub unset: Vec<String>,
}

impl Credential {
    /// The single ingest token (`ingest_token_env`): every table and
    /// operation, open when `token` is `None`.
    #[must_use]
    pub fn single(token: Option<String>) -> Self {
        Self {
            name: LEGACY_CREDENTIAL.to_string(),
            open: token.is_none(),
            tokens: token.into_iter().collect(),
            tables: None,
            operations: IngestOperation::ALL.to_vec(),
            public: false,
            limits: Limits::default(),
            origins: Vec::new(),
            unset: Vec::new(),
        }
    }

    /// An `auth.ingest` entry, its tokens read with `env`.
    #[must_use]
    pub fn from_settings(spec: &IngestCredential, env: impl Fn(&str) -> Option<String>) -> Self {
        let mut tokens = Vec::new();
        let mut unset = Vec::new();
        for name in spec.token_envs() {
            match env(name) {
                Some(token) => tokens.push(token),
                None => unset.push(name.to_string()),
            }
        }
        let tables =
            (!spec.tables.iter().any(|t| t == "*")).then(|| spec.tables.iter().cloned().collect());
        Self {
            name: spec.name.clone(),
            tokens,
            open: spec.token.is_some(),
            tables,
            operations: spec.operations(),
            public: spec.public,
            limits: spec.effective_limits(),
            origins: spec.origins.clone(),
            unset,
        }
    }

    /// Whether it may write `table` at all.
    #[must_use]
    pub fn writes(&self, table: &str) -> bool {
        self.tables.as_ref().is_none_or(|t| t.contains(table))
    }

    /// Whether it may do `operation` on `table`.
    #[must_use]
    pub fn may(&self, table: &str, operation: IngestOperation) -> bool {
        self.writes(table) && self.operations.contains(&operation)
    }

    /// Whether `presented` is one of its tokens (constant time, each).
    fn matches(&self, presented: &str) -> bool {
        self.tokens.iter().fold(false, |found, t| {
            found | bool::from(presented.as_bytes().ct_eq(t.as_bytes()))
        })
    }
}

/// The configured tokens and credentials.
#[derive(Clone, Debug)]
pub struct Tokens {
    /// For `Access::Read`: looking at data and the model.
    pub read: Option<String>,
    /// For `Access::Admin`: changing configuration. Good for every level.
    pub admin: Option<String>,
    /// For `Access::Ingest`: writing rows.
    pub ingest: Vec<Arc<Credential>>,
}

impl Default for Tokens {
    /// Everything open: the single ingest credential without a token.
    fn default() -> Self {
        Self {
            read: None,
            admin: None,
            ingest: vec![Arc::new(Credential::single(None))],
        }
    }
}

impl Tokens {
    /// A token per level, with the single ingest credential: `None` leaves
    /// that level open.
    #[must_use]
    pub fn levels(ingest: Option<&str>, read: Option<&str>, admin: Option<&str>) -> Self {
        Self {
            read: read.map(str::to_string),
            admin: admin.map(str::to_string),
            ingest: vec![Arc::new(Credential::single(ingest.map(str::to_string)))],
        }
    }

    /// Read from `TIDEPOOL_INGEST_TOKEN`, `TIDEPOOL_READ_TOKEN` and
    /// `TIDEPOOL_ADMIN_TOKEN`; empty counts as unset.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_settings(&AuthSettings::default())
    }

    /// Read from the variables `tidepool.yaml`'s `auth` names.
    #[must_use]
    pub fn from_settings(settings: &AuthSettings) -> Self {
        let get = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let ingest = if settings.ingest.is_empty() {
            vec![Arc::new(Credential::single(get(
                settings.ingest_token_env()
            )))]
        } else {
            settings
                .ingest
                .iter()
                .map(|spec| Arc::new(Credential::from_settings(spec, get)))
                .collect()
        };
        Self {
            read: get(&settings.read_token_env),
            admin: get(&settings.admin_token_env),
            ingest,
        }
    }

    /// The token a level wants, if any: for ingest, the first credential's.
    #[must_use]
    pub fn required(&self, access: Access) -> Option<&str> {
        match access {
            Access::Public => None,
            Access::Ingest => self
                .ingest
                .first()
                .and_then(|c| c.tokens.first())
                .map(String::as_str),
            Access::Read => self.read.as_deref(),
            Access::Admin => self.admin.as_deref(),
        }
    }

    /// Whether `access` needs no token at all. Ingest is open when a
    /// private credential needs none (the single token, unset): a public
    /// one without a token is a grant to the internet, made on purpose.
    #[must_use]
    pub fn open(&self, access: Access) -> bool {
        match access {
            Access::Public => true,
            Access::Ingest => self.ingest.iter().any(|c| c.open && !c.public),
            Access::Read | Access::Admin => self.required(access).is_none(),
        }
    }

    /// The token a read or admin level requires: never an ingest one.
    fn level_token(&self, access: Access) -> Option<&str> {
        match access {
            Access::Read | Access::Admin => self.required(access),
            Access::Public | Access::Ingest => None,
        }
    }

    fn is_admin(&self, presented: &str) -> bool {
        self.admin
            .as_deref()
            .is_some_and(|a| bool::from(presented.as_bytes().ct_eq(a.as_bytes())))
    }

    /// The credential whose token `presented` is.
    #[must_use]
    pub fn credential(&self, presented: &str) -> Option<&Arc<Credential>> {
        self.ingest.iter().find(|c| c.matches(presented))
    }

    /// Whether `presented` may use `access`: its own token, or the admin's.
    #[must_use]
    pub fn allows(&self, presented: &str, access: Access) -> bool {
        if self.is_admin(presented) {
            return true;
        }
        match access {
            Access::Public => true,
            Access::Ingest => self.open(access) || self.credential(presented).is_some(),
            Access::Read | Access::Admin => self
                .level_token(access)
                .is_none_or(|expected| bool::from(presented.as_bytes().ct_eq(expected.as_bytes()))),
        }
    }

    /// The access levels that are open because their token is unset.
    #[must_use]
    pub fn open_levels(&self) -> Vec<&'static str> {
        [Access::Read, Access::Ingest, Access::Admin]
            .into_iter()
            .filter(|a| self.open(*a))
            .map(Access::label)
            .collect()
    }

    /// What is wrong with the tokens as configured: errors (two credentials
    /// sharing a token, which would make one the other) and warnings
    /// (variables that are unset).
    #[must_use]
    pub fn problems(&self) -> (Vec<String>, Vec<String>) {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();
        let mut owners: Vec<(&str, &str)> = Vec::new();
        for (owner, token) in [("read", &self.read), ("admin", &self.admin)] {
            if let Some(token) = token {
                owners.push((owner, token));
            }
        }
        for credential in &self.ingest {
            for token in &credential.tokens {
                owners.push((&credential.name, token));
            }
            if !credential.unset.is_empty() {
                let usable = if credential.tokens.is_empty() {
                    "it can't be used"
                } else {
                    "its other token still works"
                };
                warnings.push(format!(
                    "ingest credential `{}`: {} unset, so {usable}",
                    credential.name,
                    credential.unset.join(", ")
                ));
            }
        }
        for (i, (a, token)) in owners.iter().enumerate() {
            if let Some((b, _)) = owners[i + 1..].iter().find(|(_, t)| t == token)
                && a != b
            {
                errors.push(format!(
                    "`{a}` and `{b}` have the same token: a request couldn't say which it is"
                ));
            }
        }
        (errors, warnings)
    }
}

/// Who is calling an ingest endpoint, as far as [`authorize`] can tell
/// before the handler knows the table.
#[derive(Clone, Debug)]
pub enum Caller {
    /// The admin token, or a session opened with it.
    Admin,
    Credential(Arc<Credential>),
    /// No token: one of the credentials that need none, if one writes the
    /// table.
    Anonymous,
}

/// Who writes a batch: the name its idempotency keys and metrics go by,
/// and its limits.
#[derive(Clone, Debug)]
pub enum Writer {
    Admin,
    Credential(Arc<Credential>),
}

impl Writer {
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Admin => "admin",
            Self::Credential(c) => &c.name,
        }
    }

    #[must_use]
    pub fn credential(&self) -> Option<&Arc<Credential>> {
        match self {
            Self::Admin => None,
            Self::Credential(c) => Some(c),
        }
    }
}

/// Why a write isn't allowed.
#[derive(Debug, PartialEq, Eq)]
pub enum Denied {
    /// As if the table didn't exist: on the public listener, a table this
    /// caller can't write there.
    Hidden,
    /// No token, and no credential writes the table without one.
    NeedsToken,
    /// The credential may not do this.
    Forbidden(String),
}

impl Caller {
    /// Who may do `operation` on `table` through `listener`, if anyone.
    ///
    /// # Errors
    /// [`Denied`] says why not.
    pub fn writer(
        &self,
        tokens: &Tokens,
        listener: Listener,
        table: &str,
        operation: IngestOperation,
    ) -> Result<Writer, Denied> {
        let public_only = listener.public_only();
        let credential = match self {
            Self::Admin if public_only => return Err(Denied::Hidden),
            Self::Admin => return Ok(Writer::Admin),
            Self::Credential(c) => Arc::clone(c),
            Self::Anonymous => {
                let open = tokens
                    .ingest
                    .iter()
                    .find(|c| c.open && c.writes(table) && (c.public || !public_only));
                match open {
                    Some(c) => Arc::clone(c),
                    None if public_only => return Err(Denied::Hidden),
                    None => return Err(Denied::NeedsToken),
                }
            }
        };
        if public_only && !(credential.public && credential.writes(table)) {
            return Err(Denied::Hidden);
        }
        if !credential.writes(table) {
            return Err(Denied::Forbidden(format!(
                "credential `{}` may not write `{table}`",
                credential.name
            )));
        }
        if !credential.may(table, operation) {
            return Err(Denied::Forbidden(format!(
                "credential `{}` may not {} `{table}`",
                credential.name,
                operation.label()
            )));
        }
        Ok(Writer::Credential(credential))
    }
}

/// The session cookie's name.
pub const COOKIE: &str = "tidepool_session";

/// How long a session lasts without being used.
const IDLE: Duration = Duration::from_hours(12);

struct Session {
    /// The levels it was opened for (the token's).
    levels: Vec<Access>,
    /// Who it writes as, when it may ingest.
    caller: Option<Caller>,
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
    fn start(&self, levels: Vec<Access>, caller: Option<Caller>) -> String {
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
                caller,
                used: Instant::now(),
            },
        );
        id
    }

    /// The levels of a live session, refreshing its idle clock.
    fn levels(&self, id: &str) -> Option<Vec<Access>> {
        self.get(id).map(|(levels, _)| levels)
    }

    /// The levels and the ingest caller of a live session, refreshing its
    /// idle clock.
    fn get(&self, id: &str) -> Option<(Vec<Access>, Option<Caller>)> {
        let mut open = self.lock();
        let session = open.get_mut(id).filter(|s| s.used.elapsed() < IDLE)?;
        session.used = Instant::now();
        Some((session.levels.clone(), session.caller.clone()))
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

/// The token a request presents: the `Authorization` header, or `?token=`
/// for clients that can't set headers and have no session.
fn presented(request: &Request) -> Option<String> {
    let from_query = request.uri().query().and_then(|q| {
        q.split('&')
            .find_map(|pair| pair.strip_prefix("token="))
            .map(str::to_string)
    });
    bearer(request.headers()).or(from_query)
}

/// Who is calling an ingest endpoint: the credential or admin token
/// presented, a session's, or no one ([`Caller::Anonymous`]). `None`: a
/// token that is neither, where every credential needs one.
fn caller(state: &AppState, request: &Request) -> Option<Caller> {
    let tokens = &state.tokens;
    if let Some(token) = presented(request) {
        if tokens.is_admin(&token) {
            return Some(Caller::Admin);
        }
        if let Some(credential) = tokens.credential(&token) {
            return Some(Caller::Credential(Arc::clone(credential)));
        }
        // a token for something else (a read token, say) is no token here,
        // as it is at any open level
        return tokens
            .ingest
            .iter()
            .any(|c| c.open)
            .then_some(Caller::Anonymous);
    }
    let session = cookie(request.headers())
        .and_then(|id| state.sessions.get(&id))
        .and_then(|(_, caller)| caller);
    // without a token, only where some credential needs none: otherwise
    // 401 here, before anything is said about the table
    session.or_else(|| {
        tokens
            .ingest
            .iter()
            .any(|c| c.open)
            .then_some(Caller::Anonymous)
    })
}

/// Let the request through if it may use `access`: no token needed, the
/// token in the `Authorization` header or `?token=`, or a session cookie.
/// An ingest request goes on with its [`Caller`] in its extensions, for
/// the handler to decide about the table.
pub async fn authorize(
    access: Access,
    state: &AppState,
    mut request: Request,
    next: Next,
) -> Response {
    let tokens = &state.tokens;
    if access == Access::Ingest {
        return match caller(state, &request) {
            Some(caller) => {
                request.extensions_mut().insert(caller);
                next.run(request).await
            }
            None => unauthorized(true),
        };
    }
    if tokens.open(access) {
        return next.run(request).await;
    }
    let presented = presented(&request);
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
    unauthorized(presented.is_some())
}

/// 401, saying whether the token presented was wrong or none was.
pub(crate) fn unauthorized(presented: bool) -> Response {
    let message = if presented {
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
    let caller = if state.tokens.is_admin(&token) {
        Some(Caller::Admin)
    } else {
        state
            .tokens
            .credential(&token)
            .map(|c| Caller::Credential(Arc::clone(c)))
    };
    if levels.is_empty() {
        return crate::handlers::api_error(
            StatusCode::UNAUTHORIZED,
            "the token isn't valid for anything here",
        );
    }
    let id = state.sessions.start(levels.clone(), caller);
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
