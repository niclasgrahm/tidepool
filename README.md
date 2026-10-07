# Tidepool

A live, associative analytics server. Data is pushed in over HTTP and held in a compressed, dictionary-encoded in-memory column store. A semantic model ([Apache Ossie](https://ossie.apache.org/)) is declared as code. Dashboards update continuously as data streams in and react instantly to selections. It ships as one Rust binary with the web UI built in.

Status: M1 (config and model compiler) done. `tidepool validate` and `tidepool plan` work on a config directory; the server runs and serves an embedded UI shell; the store and engine start in M2. See the milestones in the plan.

```bash
just build                       # needs: wasm32-unknown-unknown target, trunk
./target/release/tidepool serve  # http://127.0.0.1:7070
./target/release/tidepool validate --config examples/retail
```

- [docs/plan.md](docs/plan.md): implementation plan (start here)
- [docs/architecture-options.md](docs/architecture-options.md): background, and the two architectures we considered
