# Tidepool

A live, associative analytics server. Data is pushed in over HTTP and held in a compressed, dictionary-encoded in-memory column store. A semantic model ([Apache Ossie](https://ossie.apache.org/)) is declared as code. Dashboards update continuously as data streams in and react instantly to selections. It ships as one Rust binary with the web UI built in.

Status: M2 (store and ingest) done. Point the server at a config directory and push rows into its tables over HTTP (JSON, NDJSON or Arrow); queries and dashboards start in M3. See the milestones in the plan.

```bash
just build                       # needs: wasm32-unknown-unknown target, trunk
./target/release/tidepool serve --config examples/retail   # http://127.0.0.1:7070
curl -X POST 'http://127.0.0.1:7070/v1/tables/customers/rows?wait=visible' \
  -H 'content-type: application/json' \
  -d '[{"customer_id": "c1", "name": "Ann", "region": "north"}]'
curl 'http://127.0.0.1:7070/v1/tables/customers/preview'
```

- [docs/plan.md](docs/plan.md): implementation plan (start here)
- [docs/architecture-options.md](docs/architecture-options.md): background, and the two architectures we considered
