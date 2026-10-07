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
just bench           # ingest throughput per format, release build (args: --rows N --batch N --clients N)
cargo run -p tidepool -- validate --config examples/retail    # check a config directory
cargo run -p tidepool -- plan --config new/ --base old/       # what changes, and what it costs
cargo insta review   # accept or reject changed golden snapshots (diagnostics, plans)
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
`config → expr → model → store → engine → server → tidepool`, and `protocol`
depends only on serde/schemars. (`expr` before `model`: the model compiler
resolves names in expressions.)

| crate | licence | what | status |
|---|---|---|---|
| `tidepool-protocol` | Apache-2.0 | wire types and the **endpoint table** (`api_docs.rs`) | M0 |
| `tidepool-config` | Apache-2.0 | serde types for every config file, JSON Schemas, diagnostics with `file:line:col` | M1 ✓ |
| `tidepool-expr` | AGPL | SQL expressions: syntax + reference analysis (M1 ✓); type-check, kernels (M3) | M1/M3 |
| `tidepool-model` | AGPL | compiled Model IR, cross-file validation, catalog of stable ids, diff → plan | M1 ✓ |
| `tidepool-store` | AGPL | values, dictionaries, segments, PK index, epochs/snapshots, the writer thread, ingest decoders | M2 ✓ / M5 (WAL) |
| `tidepool-engine` | AGPL | selection engine, aggregation, caches, subscriptions | M3/M4 |
| `tidepool-server` | AGPL | axum: router from the endpoint table, OpenAPI, UI serving, lifecycle | M0 |
| `tidepool-oracle` | AGPL | test-only DuckDB reference + random generators | M3 |
| `tidepool` | AGPL | the binary: CLI and wiring | M0 |
| `tidepool-bench` | AGPL | load/latency harness (`just bench`, not in CI) | M2 ✓ (ingest) |
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

`Access` is declared per endpoint and enforced by `auth::authorize` from the
same entry (see "Ingest over HTTP" below).

Probes live at the root (`/healthz`, `/readyz`); everything else is under `/v1`.

### Config files and diagnostics (`tidepool-config`)

`ConfigSet::from_files` takes (relative path, text) pairs, so a directory, a
test's inline files and (later) an API change all load the same way; which
file is which is decided by path (`tables/<name>.yaml`, …). Types are
deserialized with `serde_norway` and `deny_unknown_fields` everywhere, so a
typo is an error with serde's location.

Cross-file errors need locations serde can't give, so every file also gets a
`PathIndex` (built from `saphyr-parser` events) mapping each YAML path
(`relationships[1].to`) to line/column/length. **Report problems by path,
never by line number**: `Diagnostic::error(..).at(file, path)` for a value,
`.at_key(..)` for the key that introduces it. `diag::render` turns that into
rustc-style output; `suggest`/`closest` give the "did you mean" help.

Ossie: the flat 0.2 shape, with 0.1.1's `semantic_model:` wrapper accepted
(D8). Tidepool's own semantics are `custom_extensions` entries with
`vendor_name: TIDEPOOL` and a JSON string `data`; their shapes are
`MetricExtension` etc. in `ossie.rs`, also `deny_unknown_fields`.

### The model compiler (`tidepool-model`)

`compile(&ConfigSet, &previous_catalog)` runs four stages (tables → datasets
and relationships → metrics → dashboards) into one `Diagnostics`, and only
returns a `Model` when there are no errors. Rules that keep the output
readable, and that every new check must follow:

- **Never stop at the first problem**, but **never cascade**. An object that
  failed is recorded as broken (`Tables::broken`, `Tables::key_broken`,
  `Semantic::broken`), and later references to it are skipped silently.
  A failed relationship marks its many side as `uncertain_from`;
  "can't reach" errors use `Semantic::maybe_reaches` so they only appear when
  even a fixed relationship couldn't help.
- **Point at the token that is wrong**: an expression's string, an
  extension's `data`, a relationship's `name`. Never the enclosing object.
- **Every new check gets a golden case** in `tests/diagnostics.rs`: the retail
  example plus one edit, snapshotted with insta. A case must produce exactly
  the error it is about and nothing else. Review snapshot diffs like UI copy
  (`cargo insta review`).
- Names are labels, ids are identity: the `Catalog` keeps ids across
  versions, `renamed_from` carries an id over a rename, and ids are never
  reused.

`plan::diff(old, new)` compares two models by id and classes each change
safe / rebuild / destructive (plan §8.2). New change kinds get a scenario in
`tests/plans.rs`; scenarios can edit the base config too (`base: [...]`),
which is how "joins two tables that already hold data" is tested.

### The store (`tidepool-store`)

One writer thread owns the `Store`; everything else reads immutable
`Snapshot`s. There is no shared mutable state and no `unsafe`:

- **Writes** go through `WriterHandle::apply` into a bounded queue (64
  batches). A full queue is `Busy`, which HTTP turns into `503 Retry-After`.
  Never make the queue unbounded: backpressure is the point.
- **Epochs**: the writer publishes on its own clock (`epoch_interval`) when
  something changed: it freezes each table's builder into an immutable chunk,
  snapshots dictionaries, and swaps in a new `Snapshot` (`ArcSwap`).
  `?wait=visible` waits for that; it never forces an epoch, so clients can't
  drive the epoch rate (every epoch will cost work per live dashboard).
- **Segments** are immutable once published; deletes are a copy-on-write
  `RoaringBitmap` beside each one, and `version` changes whenever it does.
  Anything cached about a segment (M4) is keyed by its version.
- **Row ids never change** (sealing keeps them), so the primary-key index
  points at rows forever. Primary-key columns are dictionary-encoded, which
  the model enforces, so a single-column key is a dense array by code.
- **Upserts track changed columns** per epoch (`TableSnapshot::changed`): a
  rewrite with identical values changes nothing downstream.
- **Decoding** (JSON, NDJSON, Arrow) is strict: values are checked, never
  coerced; a batch is all or nothing; every problem is listed by row and
  column. Decimals are parsed from their text, never through `f64`.

Store tests run against the retail example's model (`tests/store.rs`); use
`Store::with_seal_rows` with a small number to exercise sealing.

### Ingest over HTTP (`tidepool-server::tables`)

Rows endpoints exist once in the endpoint table (`/v1/tables/{table}/rows`)
and dispatch by name at request time, so a table added to the model works
without touching the router. The OpenAPI document replaces the templated path
with one typed path per table (`Row_<table>` / `Key_<table>` schemas).
Decoding runs in `spawn_blocking`. `Idempotency-Key` is per table, in memory
for 24 h (into the WAL in M5).

Access is enforced from the endpoint table: `auth::authorize` wraps every
route with its declared `Access`, checking the bearer token configured for
that level (`TIDEPOOL_{INGEST,READ,ADMIN}_TOKEN`; unset = open).
`every_protected_endpoint_wants_its_token` walks the table to prove it.

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
