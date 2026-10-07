# Same verbs as Kayak's justfile (plan D14). `just` with no arguments lists them.

# Where `just run` and `just dev` listen. Kayak takes 6767; 7070 keeps them
# runnable side by side.
port := "7070"

# list recipes
default:
  @just --list

# fmt check + clippy + every test: what CI's `test` job runs
ci: fmt-check lint test

# cargo test --workspace --all-targets
test:
  cargo test --workspace --all-targets

# clippy over everything, warnings are errors
lint:
  cargo clippy --workspace --all-targets -- -D warnings

# format everything
fmt:
  cargo fmt --all

# fail if anything isn't formatted
fmt-check:
  cargo fmt --all --check

# dependency policy: licences, advisories, bans, sources
deny:
  cargo deny check

# the store's unsafe code under Miri (needs: rustup toolchain install nightly --component miri)
miri:
  cargo +nightly miri test -p tidepool-store

# build the web UI into frontend/dist (needs trunk and the wasm32 target)
frontend:
  cd frontend && trunk build --release

# one release binary with the UI compiled in: target/release/tidepool
build: frontend
  cargo build --release -p tidepool --features embed-assets

# lint and test the embedded-UI path; needs `just build` (or `just frontend`) first
test-embed:
  cargo clippy -p tidepool-server --features embed-assets --all-targets -- -D warnings
  cargo test -p tidepool-server --features embed-assets --test embed

# a debug server on :{{port}} with the retail example, serving the UI from frontend/dist
run: frontend
  cargo run -p tidepool -- serve --config examples/retail --listen 127.0.0.1:{{port}} --ui-dir frontend/dist

# server on :{{port}} plus trunk on :7080 with hot reload (open http://127.0.0.1:7080)
dev:
  #!/usr/bin/env sh
  # trunk serves the UI and proxies /v1 to the server (frontend/Trunk.toml);
  # killing the recipe takes the server down with it.
  cargo run -p tidepool -- serve --config examples/retail --listen 127.0.0.1:{{port}} &
  server=$!
  trap 'kill $server 2>/dev/null' EXIT INT TERM
  cd frontend && trunk serve

# ingest throughput, per format (release build); deliberately not part of `ci`
bench *args:
  cargo run --release -p tidepool-bench -- ingest {{args}}

# query latency on the reference dataset, release build (args: --orders N --runs N --metrics a,b,c)
bench-query *args:
  cargo run --release -p tidepool-bench -- query {{args}}

# the DuckDB oracle at depth, release build (args: number of seeds)
oracle seeds="400":
  TIDEPOOL_ORACLE_SEEDS={{seeds}} cargo test --release -p tidepool-oracle
