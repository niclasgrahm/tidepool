//! `tidepool.yaml`: how the server runs. Not part of the data model, so
//! changing it never needs a plan beyond "restart" or "apply".

use std::net::SocketAddr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::types::{ByteSize, HumanDuration, MemorySize, Rate};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    /// Acknowledge ingest after the write-ahead log is fsynced.
    #[default]
    Fsync,
    /// Acknowledge before the fsync: faster, and a crash can lose the last
    /// moments of acknowledged data.
    Async,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Where to listen: one address for everything, or one per surface
    /// (`public`, `ingest`, `admin`). `tidepool serve --listen` overrides
    /// it with one address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<Listen>,
    /// Where the server keeps its state. `--data` overrides it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<String>,
    /// How often new data becomes visible, at most.
    #[serde(default = "default_epoch_interval")]
    pub epoch_interval: HumanDuration,
    /// Memory for data and caches together.
    #[serde(default = "default_memory_budget")]
    pub memory_budget: MemorySize,
    #[serde(default)]
    pub durability: Durability,
    /// What happens to the config directory when a change is applied
    /// through the API or the UI.
    #[serde(default)]
    pub config_writeback: Writeback,
    /// Where the access tokens come from.
    #[serde(default)]
    pub auth: AuthSettings,
    /// What agents may do over MCP.
    #[serde(default)]
    pub agents: Agents,
    /// The address people reach the server at, such as
    /// `https://tidepool.example.com`: where a view shown inside an AI app
    /// (an MCP App) loads the UI from and connects to. When left out, the
    /// address the MCP request came to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
}

impl ServerConfig {
    /// Where the server listens when `listen` is left out: loopback, so
    /// exposing the port is a decision made explicitly.
    pub const DEFAULT_LISTEN: SocketAddr = SocketAddr::V4(std::net::SocketAddrV4::new(
        std::net::Ipv4Addr::LOCALHOST,
        7070,
    ));

    /// `listen`, or the default.
    #[must_use]
    pub fn listen_or_default(&self) -> Listen {
        self.listen
            .clone()
            .unwrap_or(Listen::One(Self::DEFAULT_LISTEN))
    }
}

/// Where the server listens (plan §9.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Listen {
    /// One address serving everything.
    One(SocketAddr),
    /// An address per surface.
    Split(Surfaces),
}

/// An address per surface. A surface left out is served by `admin`'s
/// address, or by `public`'s when `admin` is left out too; surfaces given
/// the same address share one listener.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Surfaces {
    /// The UI, reading data, live dashboards, and ingest for credentials
    /// marked `public`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public: Option<SocketAddr>,
    /// Reading data and all ingest: where Kayak and other producers connect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingest: Option<SocketAddr>,
    /// Everything, including config changes, approvals and `/metrics`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin: Option<SocketAddr>,
}

/// One of the parts of the HTTP surface a listener can serve.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Surface {
    Public,
    Ingest,
    Admin,
}

impl Surface {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Ingest => "ingest",
            Self::Admin => "admin",
        }
    }
}

impl Listen {
    /// Each address to bind and the surfaces it serves; `None` when no
    /// address is given at all (an empty map).
    #[must_use]
    pub fn listeners(&self) -> Option<Vec<(SocketAddr, Vec<Surface>)>> {
        let Self::Split(s) = self else {
            let Self::One(address) = self else {
                return None;
            };
            return Some(vec![(
                *address,
                vec![Surface::Public, Surface::Ingest, Surface::Admin],
            )]);
        };
        let fallback = s.admin.or(s.public)?;
        let mut listeners: Vec<(SocketAddr, Vec<Surface>)> = Vec::new();
        for (surface, address) in [
            (Surface::Public, s.public),
            (Surface::Ingest, s.ingest),
            (Surface::Admin, s.admin),
        ] {
            let address = address.unwrap_or(fallback);
            match listeners.iter_mut().find(|(a, _)| *a == address) {
                Some((_, surfaces)) => surfaces.push(surface),
                None => listeners.push((address, vec![surface])),
            }
        }
        Some(listeners)
    }
}

impl std::fmt::Display for Listen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::One(address) => write!(f, "{address}"),
            Self::Split(s) => {
                let parts: Vec<String> = [
                    ("public", s.public),
                    ("ingest", s.ingest),
                    ("admin", s.admin),
                ]
                .into_iter()
                .filter_map(|(name, a)| a.map(|a| format!("{name} {a}")))
                .collect();
                write!(f, "{}", parts.join(", "))
            }
        }
    }
}

/// The access tokens (plan §9, §9.2), named by environment variable and
/// read when the server starts. A read or admin level whose variable is
/// unset or empty is open; the admin token is good for every level.
///
/// Ingest is either one token for every table and operation
/// (`ingest_token_env`, open when unset, as before), or a list of named
/// credentials (`ingest`), each scoped to tables and operations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuthSettings {
    /// Reading data, the model and dashboards.
    #[serde(default = "read_env")]
    pub read_token_env: String,
    /// One token for writing and deleting rows in every table, named
    /// `ingest`. Default `TIDEPOOL_INGEST_TOKEN`; not used with `ingest`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingest_token_env: Option<String>,
    /// Changing the config, approving changes.
    #[serde(default = "admin_env")]
    pub admin_token_env: String,
    /// Named ingest credentials, in place of `ingest_token_env`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ingest: Vec<IngestCredential>,
}

impl Default for AuthSettings {
    fn default() -> Self {
        Self {
            read_token_env: read_env(),
            ingest_token_env: None,
            admin_token_env: admin_env(),
            ingest: Vec::new(),
        }
    }
}

impl AuthSettings {
    /// The variable holding the single ingest token, when there are no
    /// named credentials.
    #[must_use]
    pub fn ingest_token_env(&self) -> &str {
        self.ingest_token_env
            .as_deref()
            .unwrap_or("TIDEPOOL_INGEST_TOKEN")
    }
}

/// The name of the credential `ingest_token_env` stands for.
pub const LEGACY_CREDENTIAL: &str = "ingest";

fn read_env() -> String {
    "TIDEPOOL_READ_TOKEN".into()
}
fn admin_env() -> String {
    "TIDEPOOL_ADMIN_TOKEN".into()
}

/// A producer allowed to write rows (plan §9.2): which tables, which
/// operations, and whether from the public listener.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestCredential {
    /// Its name: in metrics, and the scope of its idempotency keys.
    pub name: String,
    /// The environment variable holding its token, or several: any of them
    /// is accepted, for rotating a token without downtime. A credential
    /// whose variables are all unset can't be used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env: Option<TokenEnv>,
    /// `none`: no token at all, for a producer that can't keep a secret (a
    /// browser). Only for `public` credentials that only `append`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<NoToken>,
    /// The tables it may write; `"*"` is every table.
    #[serde(default = "every_table")]
    pub tables: Vec<String>,
    /// What it may do. Default: `append` for a public credential, every
    /// operation otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operations: Option<Vec<IngestOperation>>,
    /// Whether it may write through the public listener. Its limits then
    /// apply with defaults for what they leave out.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub public: bool,
    /// How much it may send.
    #[serde(default, skip_serializing_if = "Limits::is_empty")]
    pub limits: Limits,
    /// The browser origins it may write from (CORS), e.g.
    /// `https://app.example.com`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub origins: Vec<String>,
}

fn every_table() -> Vec<String> {
    vec!["*".into()]
}

impl IngestCredential {
    /// The operations it may do, defaults applied.
    #[must_use]
    pub fn operations(&self) -> Vec<IngestOperation> {
        self.operations.clone().unwrap_or_else(|| {
            if self.public {
                vec![IngestOperation::Append]
            } else {
                IngestOperation::ALL.to_vec()
            }
        })
    }

    /// Its limits, with the public defaults for what a public credential
    /// leaves out.
    #[must_use]
    pub fn effective_limits(&self) -> Limits {
        if !self.public {
            return self.limits;
        }
        let d = Limits::PUBLIC;
        Limits {
            rate: self.limits.rate.or(d.rate),
            batch_rows: self.limits.batch_rows.or(d.batch_rows),
            body: self.limits.body.or(d.body),
            new_values: self.limits.new_values.or(d.new_values),
        }
    }

    /// The environment variables naming its tokens.
    #[must_use]
    pub fn token_envs(&self) -> Vec<&str> {
        match &self.token_env {
            None => Vec::new(),
            Some(TokenEnv::One(name)) => vec![name.as_str()],
            Some(TokenEnv::Many(names)) => names.iter().map(String::as_str).collect(),
        }
    }
}

/// One environment variable, or several.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum TokenEnv {
    One(String),
    Many(Vec<String>),
}

/// `token: none`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NoToken {
    None,
}

/// A way of writing rows.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum IngestOperation {
    /// `POST` to an append table.
    Append,
    /// `POST` to an upsert table: rows replace the stored ones by key.
    Upsert,
    /// `DELETE`: rows removed by key.
    Delete,
    /// `PUT`: the table's whole contents (an empty body empties it).
    Replace,
}

impl IngestOperation {
    pub const ALL: [Self; 4] = [Self::Append, Self::Upsert, Self::Delete, Self::Replace];

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Append => "append",
            Self::Upsert => "upsert",
            Self::Delete => "delete",
            Self::Replace => "replace",
        }
    }
}

/// How much a credential may send. Over `rate` or `new_values` a request
/// is refused with 429, over `batch_rows` or `body` with 413.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Requests (batches) at most, e.g. `200/s`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<Rate>,
    /// Rows in one request at most.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_rows: Option<u64>,
    /// A request body's size at most, e.g. `1MiB`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<ByteSize>,
    /// Values new to a dictionary-encoded column at most, e.g. `1000/min`:
    /// dictionaries only grow, so this is what keeps unique junk from
    /// filling memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_values: Option<Rate>,
}

impl Limits {
    /// What a public credential gets for what it leaves out.
    pub const PUBLIC: Self = Self {
        rate: Some(Rate {
            count: 100,
            per: std::time::Duration::from_secs(1),
        }),
        batch_rows: Some(10_000),
        body: Some(ByteSize(4 << 20)),
        new_values: Some(Rate {
            count: 10_000,
            per: std::time::Duration::from_secs(3600),
        }),
    };

    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// What an agent's changes over MCP may do without a person (plan §8.5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Agents {
    /// Change classes an agent's `apply_change` applies directly; anything
    /// else waits for approval.
    #[serde(default = "safe_only")]
    pub auto_apply: Vec<ChangeClassName>,
}

impl Default for Agents {
    fn default() -> Self {
        Self {
            auto_apply: safe_only(),
        }
    }
}

fn safe_only() -> Vec<ChangeClassName> {
    vec![ChangeClassName::Safe]
}

/// A plan's change class, as written in `tidepool.yaml`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChangeClassName {
    Safe,
    Rebuild,
    Destructive,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: None,
            data_dir: None,
            epoch_interval: default_epoch_interval(),
            memory_budget: default_memory_budget(),
            durability: Durability::default(),
            config_writeback: Writeback::default(),
            auth: AuthSettings::default(),
            agents: Agents::default(),
            public_url: None,
        }
    }
}

/// Write-back of applied changes (plan §8.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Writeback {
    /// Write applied changes to the config files, so the directory always
    /// holds what the server runs.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Commit them, when the config directory is in a git repository.
    #[serde(default = "yes")]
    pub git_commit: bool,
}

impl Default for Writeback {
    fn default() -> Self {
        Self {
            enabled: true,
            git_commit: true,
        }
    }
}

fn yes() -> bool {
    true
}

fn default_epoch_interval() -> HumanDuration {
    HumanDuration(std::time::Duration::from_millis(200))
}

fn default_memory_budget() -> MemorySize {
    MemorySize::Percent(75)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> ServerConfig {
        serde_norway::from_str(text).unwrap_or_else(|e| panic!("{text}: {e}"))
    }

    fn address(text: &str) -> SocketAddr {
        text.parse().unwrap_or_else(|e| panic!("{text}: {e}"))
    }

    #[test]
    fn one_address_serves_every_surface() {
        let config = parse("listen: 0.0.0.0:7070\n");
        let listeners = config.listen.and_then(|l| l.listeners());
        assert_eq!(
            listeners,
            Some(vec![(
                address("0.0.0.0:7070"),
                vec![Surface::Public, Surface::Ingest, Surface::Admin]
            )])
        );
    }

    #[test]
    fn surfaces_left_out_go_to_admin_then_public() {
        let config = parse("listen:\n  public: 0.0.0.0:7070\n  admin: 127.0.0.1:7072\n");
        let listeners = config.listen.and_then(|l| l.listeners());
        assert_eq!(
            listeners,
            Some(vec![
                (address("0.0.0.0:7070"), vec![Surface::Public]),
                (
                    address("127.0.0.1:7072"),
                    vec![Surface::Ingest, Surface::Admin]
                ),
            ])
        );
        let config = parse("listen:\n  public: 0.0.0.0:7070\n");
        let listeners = config.listen.and_then(|l| l.listeners());
        assert_eq!(listeners.map(|l| l.len()), Some(1));
        let config = parse("listen: {}\n");
        assert_eq!(config.listen.and_then(|l| l.listeners()), None);
        assert!(serde_norway::from_str::<ServerConfig>("listen:\n  private: 0.0.0.0:1\n").is_err());
    }

    #[test]
    fn public_credentials_append_and_have_limits() {
        let config = parse(
            "auth:\n  ingest:\n    - name: sensors\n      token_env: SENSOR_TOKEN\n      tables: [readings]\n      public: true\n      limits: { rate: 10/s }\n    - name: kayak\n      token_env: [A, B]\n",
        );
        let [sensors, kayak] = config.auth.ingest.as_slice() else {
            panic!("expected two credentials");
        };
        assert_eq!(sensors.operations(), vec![IngestOperation::Append]);
        let limits = sensors.effective_limits();
        assert_eq!(limits.rate.map(|r| r.count), Some(10));
        assert_eq!(limits.batch_rows, Limits::PUBLIC.batch_rows);
        assert_eq!(kayak.operations(), IngestOperation::ALL.to_vec());
        assert_eq!(kayak.effective_limits(), Limits::default());
        assert_eq!(kayak.tables, vec!["*".to_string()]);
        assert_eq!(kayak.token_envs(), vec!["A", "B"]);
    }
}
