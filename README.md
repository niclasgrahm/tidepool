# Tidepool

A live, associative analytics server. Data is pushed in over HTTP and held in a compressed, dictionary-encoded in-memory column store. A semantic model ([Apache Ossie](https://ossie.apache.org/)) is declared as code. Dashboards update continuously as data streams in and react instantly to selections. It ships as one Rust binary with the web UI built in.

Status: M0 (skeleton) done. The server runs and serves an embedded UI shell; the engine starts in M1. See the milestones in the plan.

```bash
just build                       # needs: wasm32-unknown-unknown target, trunk
./target/release/tidepool serve  # http://127.0.0.1:7070
```

- [docs/plan.md](docs/plan.md): implementation plan (start here)
- [docs/architecture-options.md](docs/architecture-options.md): background, and the two architectures we considered
