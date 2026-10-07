# CLAUDE.md

Tidepool is a live, associative analytics server: data pushed in over HTTP,
held in a dictionary-encoded in-memory column store, a semantic model declared
as code (Apache Ossie + extensions), and dashboards that update continuously and
respond instantly to selections. One Rust binary with the UI built in.

**Read [docs/plan.md](docs/plan.md) before working on anything.** It is the
spec: decisions (§1), config formats (§3), each subsystem (§4–10), testing
(§11) and the milestones with their exit criteria (§12). When the code and the
plan disagree, either the code is wrong or the plan needs updating in the same
change. Don't leave them disagreeing silently.

Sister project: [Kayak](../../rust/kayak), the stream processor that is
Tidepool's connector layer (plan §15). Many conventions here are Kayak's
(plan D14). When in doubt about style, look at how Kayak does it.

## Commands

```bash
just                 # list recipes
just ci              # fmt check + clippy -D warnings + all tests: what CI runs
just test            # cargo test --workspace --all-targets
just lint            # cargo clippy --workspace --all-targets -- -D warnings
just fmt             # cargo fmt --all
just deny            # cargo deny check (licences, advisories, sources)
just miri            # the store's unsafe code under Miri (needs nightly + miri)
just frontend        # build the UI into frontend/dist (trunk)
just build           # just frontend, then a release binary with the UI embedded
just test-embed      # tests of the embedded UI; needs `just build` first
just run             # debug server on :7070, UI served from frontend/dist
just dev             # server on :7070 + trunk on :7080 (hot reload); open :7080
```

Tooling the frontend needs: `rustup target add wasm32-unknown-unknown` and
`cargo install trunk --locked`. Nothing else: there is no node toolchain.

## Definition of done

Not negotiable, for every change however small:

1. **New behaviour ships with tests.** Any new or changed behaviour (an
   endpoint, a config field, an engine path, a bug fix) needs a test that
   fails without the change. A bug fix without a regression test is not a fix.
   If something can't be tested offline, say so and why.
2. **`just ci` is green before a task is called done.** The whole suite, not
   "it compiles" or "the new test passes". Never disable, `#[ignore]` or weaken
   an existing test to get to green; a test that encodes the wrong behaviour is
   a conversation first.
3. **Engine results are checked against the oracle** (from M3): any change to
   how results are computed or maintained runs the oracle suites, and new
   query features extend the generators so the oracle covers them.

Lints are strict by design: clippy `all` + `pedantic`, plus `unwrap_used` and
`expect_used`, which `clippy.toml` applies to tests as well. In tests, use
`match`/`let ... else` with `panic!` and a message that says what was expected.

## Architecture

Cargo workspace under `crates/`. Dependencies flow one way:
`config → model → expr → store → engine → server → tidepool`, and `protocol`
depends only on serde/schemars.

| crate | licence | what | status |
|---|---|---|---|
| `tidepool-protocol` | Apache-2.0 | wire types and the **endpoint table** (`api_docs.rs`) | M0 |
| `tidepool-config` | Apache-2.0 | serde types for every config file, JSON Schemas, parse errors with spans | M1 |
| `tidepool-model` | AGPL | compiled Model IR, validation, catalog of stable ids, diff → plan | M1 |
| `tidepool-expr` | AGPL | SQL expressions: parse, type-check, vectorised kernels | M3 |
| `tidepool-store` | AGPL | dictionaries, segments, tail, PK index, epochs, WAL | M2/M5 |
| `tidepool-engine` | AGPL | selection engine, aggregation, caches, subscriptions | M3/M4 |
| `tidepool-server` | AGPL | axum: router from the endpoint table, OpenAPI, UI serving, lifecycle | M0 |
| `tidepool-oracle` | AGPL | test-only DuckDB reference + random generators | M3 |
| `tidepool` | AGPL | the binary: CLI and wiring | M0 |
| `tidepool-bench` | AGPL | load/latency harness (`just bench`, not in CI) | M2+ |
| `frontend` (`tidepool-frontend`) | AGPL | the web UI: Leptos, client-side rendered, built by trunk | M0 shell, M6 |

The licence split is enforced by `crates/tidepool/tests/licensing.rs`; see
[licensing.md](licensing.md). A permissive crate must never depend on a
copyleft one.

### The endpoint table

`tidepool_protocol::api_docs::endpoints()` is the single description of the
HTTP surface (Kayak's pattern). `tidepool_server::api_router` is a fold over
it, `handler_for` matches exhaustively on `Operation` (so a documented endpoint
without a handler doesn't compile), and `openapi::document()` renders the same
entries. To add an endpoint: add an `Operation` variant (and to `OPERATIONS`),
add its `ApiDoc` entry with prose and an `Access`, add any body schema to
`schemas()`, then the handler. The tests in `tests/api.rs` walk the table, so a
new entry is covered by them automatically.

`Access` is declared per endpoint but not enforced until authentication lands
(M8). `every_endpoint_is_public_until_authentication_exists` fails the moment a
protected endpoint is added without enforcement: that is deliberate.

Probes live at the root (`/healthz`, `/readyz`); everything else is under `/v1`.

### UI serving

`tidepool_server::ui`. Every path the API doesn't match falls through to
`ui::serve`: unknown `/v1/...` is a JSON 404 (never the app), existing files
are served with their content type, extension-less paths are client routes and
get `index.html`, the rest is 404. Files under `assets/` are cached as
immutable (bundlers hash those names); `index.html` is `no-cache`.

The UI comes from one of three places: compiled in (`embed-assets` feature,
off by default so `cargo check` never needs a frontend build), a directory
(`--ui-dir`, for development), or neither (a placeholder page).

### The frontend

`frontend/` is a Leptos 0.8 client-side-rendered app (decision D10, from
`spikes/leptos-charts`). trunk builds it into `frontend/dist`, with every hashed
output under `assets/` (`data-target-path` in `index.html`) so the server's
immutable-cache rule applies to them. It depends on `tidepool-protocol`
directly: request and response types are shared, never generated or copied.

It is a workspace member, so `just ci` lints and tests it natively. Keep logic
that can be tested (state transitions, formatting, frame handling) in plain
functions that `cargo test` reaches; anything that needs a DOM goes through
`wasm-bindgen-test`. When charts arrive (M6), start from the spike's
`echarts.rs` and read the spike README's list of interop pitfalls first.

## Notes

- Spikes live in `spikes/`, outside the workspace, and are kept for reference
  once their decision is recorded in the plan.
- `examples/retail/` is the example project. Keep it covering every config
  feature as features land.
