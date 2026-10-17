//! `tidepool.yaml`'s `listen` and `auth` (plan §9.2): what serde can't
//! check alone. Ingest credentials name tables, so this runs after the
//! tables stage.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use tidepool_config::diag::{Diagnostic, suggest};
use tidepool_config::server::{IngestCredential, IngestOperation, ServerConfig};
use tidepool_config::source::YamlPath;

use super::Cx;
use super::tables::Tables;

pub(super) fn check(cx: &mut Cx, tables: &Tables) {
    let Some(parsed) = &cx.set.server else {
        return;
    };
    let file = parsed.file.clone();
    let server: &ServerConfig = &parsed.value;
    let root = YamlPath::root();

    if server
        .listen
        .as_ref()
        .is_some_and(|l| l.listeners().is_none())
    {
        cx.push(
            Diagnostic::error("`listen` gives no address to listen on")
                .at(&file, root.key("listen"))
                .help("give `public` or `admin` an address; a surface left out is served by `admin`'s, or by `public`'s"),
        );
    }

    let auth = root.key("auth");
    if !server.auth.ingest.is_empty() && server.auth.ingest_token_env.is_some() {
        cx.push(
            Diagnostic::error("`ingest_token_env` and `ingest` can't both be set")
                .at_key(&file, auth.key("ingest_token_env"))
                .help("with named credentials each has its own `token_env`: move the variable to one of them"),
        );
    }

    let mut names = BTreeSet::new();
    // table → the credential without a token that writes it
    let mut anonymous: BTreeMap<&str, &str> = BTreeMap::new();
    for (i, credential) in server.auth.ingest.iter().enumerate() {
        let path = auth.key("ingest").index(i);
        identity(cx, &file, &path, credential, &mut names);
        grants(cx, &file, &path, credential, tables, &mut anonymous);
    }
}

/// A credential's name and token.
fn identity<'a>(
    cx: &mut Cx,
    file: &Path,
    path: &YamlPath,
    credential: &'a IngestCredential,
    names: &mut BTreeSet<&'a str>,
) {
    let name = credential.name.as_str();
    if !is_name(name) {
        cx.push(
            Diagnostic::error(format!("`{name}` can't be used as a credential name"))
                .at(file, path.key("name"))
                .help("names start with a letter and contain only letters, digits, `_` and `-`"),
        );
    } else if !names.insert(name) {
        cx.push(
            Diagnostic::error(format!("credential `{name}` is defined twice"))
                .at(file, path.key("name")),
        );
    }

    let no_token = credential.token.is_some();
    match (&credential.token_env, no_token) {
        (None, false) => cx.push(
            Diagnostic::error(format!("credential `{name}` has no token"))
                .at(file, path.key("name"))
                .help("set `token_env` to the variable holding its token, or `token: none` for a public credential that only appends"),
        ),
        (Some(_), true) => cx.push(
            Diagnostic::error("`token: none` and `token_env` can't both be set")
                .at(file, path.key("token")),
        ),
        (Some(_), false) if credential.token_envs().is_empty() => cx.push(
            Diagnostic::error("`token_env` names no variable")
                .at(file, path.key("token_env")),
        ),
        (None, true) if !credential.public => cx.push(
            Diagnostic::error(format!(
                "credential `{name}` has no token but isn't public"
            ))
            .at(file, path.key("token"))
            .help("`token: none` is for a public producer that can't keep a secret, such as a browser; set `public: true`, or give it a `token_env`"),
        ),
        _ => {}
    }
}

/// What a credential may do, and to which tables, from where.
fn grants<'a>(
    cx: &mut Cx,
    file: &Path,
    path: &YamlPath,
    credential: &'a IngestCredential,
    tables: &'a Tables,
    anonymous: &mut BTreeMap<&'a str, &'a str>,
) {
    let name = credential.name.as_str();
    let no_token = credential.token.is_some();
    let operations_path = path.key("operations");
    if credential.operations.as_ref().is_some_and(Vec::is_empty) {
        cx.push(
            Diagnostic::error(format!("credential `{name}` may do nothing"))
                .at(file, operations_path.clone())
                .help("list what it may do: append, upsert, delete, replace"),
        );
    }
    for (k, operation) in credential.operations.iter().flatten().enumerate() {
        if *operation == IngestOperation::Append {
            continue;
        }
        let at = operations_path.index(k);
        if no_token {
            cx.push(
                Diagnostic::error(format!(
                    "credential `{name}` has no token, so it may only append, not {}",
                    operation.label()
                ))
                .at(file, at),
            );
        } else if credential.public {
            cx.push(
                Diagnostic::warning(format!(
                    "public credential `{name}` may {}: anyone with its token can overwrite or delete rows",
                    operation.label()
                ))
                .at(file, at),
            );
        }
    }

    let tables_path = path.key("tables");
    if credential.tables.is_empty() {
        cx.push(
            Diagnostic::error(format!("credential `{name}` grants no tables"))
                .at(file, tables_path.clone())
                .help("list the tables it may write, or `\"*\"` for every table"),
        );
    }
    let mut granted = Vec::new();
    for (j, table) in credential.tables.iter().enumerate() {
        if table == "*" {
            granted.extend(tables.names());
            continue;
        }
        if tables.broken.contains(table) {
            continue;
        }
        if tables.named(table).is_none() {
            cx.push(
                Diagnostic::error(format!("no table `{table}`"))
                    .at(file, tables_path.index(j))
                    .help(suggest("tables", table, tables.names())),
            );
            continue;
        }
        granted.push(table.as_str());
    }
    if no_token {
        for table in granted {
            if let Some(other) = anonymous.insert(table, name) {
                cx.push(
                    Diagnostic::error(format!(
                        "`{other}` and `{name}` both write `{table}` without a token: a request couldn't say which it is"
                    ))
                    .at(file, path.key("token")),
                );
                break;
            }
        }
    }

    for (k, origin) in credential.origins.iter().enumerate() {
        if !is_origin(origin) {
            cx.push(
                Diagnostic::error(format!("`{origin}` isn't an origin"))
                    .at(file, path.key("origins").index(k))
                    .help("an origin is a scheme and a host, with a port if it isn't the default: `https://app.example.com`"),
            );
        }
    }
    if credential.limits.batch_rows == Some(0) {
        cx.push(
            Diagnostic::error("`batch_rows: 0` refuses every request")
                .at(file, path.key("limits").key("batch_rows")),
        );
    }
}

/// A credential's name: it goes into metrics labels and filled columns.
fn is_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && name.len() <= 64
}

/// `https://host[:port]`, as a browser sends it in `Origin`.
fn is_origin(origin: &str) -> bool {
    let Some(host) = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
    else {
        return false;
    };
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
}
