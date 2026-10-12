# Tidepool

A live, associative analytics server. Data is pushed in over HTTP and held in
a compressed, dictionary-encoded, in-memory column store. A semantic model
(tables, relationships, metrics, in [Apache Ossie](https://ossie.apache.org/))
and dashboards are declared as files. Dashboards update continuously as data
arrives and answer selections instantly: click a region and every number on
the page follows. It is one Rust binary with the web UI built in.

```bash
tidepool init shop                  # a project to start from
tidepool serve --config shop        # http://127.0.0.1:7070
tidepool feed                       # in another terminal: sample orders, live
```

From a checkout, `just build` makes the binary (`./target/release/tidepool`);
or with Docker, `docker build -t tidepool . && docker run -p 7070:7070 tidepool`.

What it does:

- **Ingest over HTTP**: JSON, NDJSON or Arrow IPC into tables, strictly
  checked, with idempotency keys and backpressure. Durable with a data
  directory: a write-ahead log with group commit, checkpoints, fast restarts.
- **Associative selections**: filter lists that show what's selected, possible
  and excluded; selections flow along relationships; back and forward.
- **Live**: every dashboard is kept up to date incrementally, epoch by epoch,
  over one WebSocket per browser.
- **Config as code, changed live**: edit the files, the config page or the API
  and the server plans the change (safe, rebuild, destructive), applies it
  while serving, and writes it back to the files (and git).
- **Agents**: an MCP endpoint to describe the model, query metrics and
  propose changes, under a policy of what they may apply on their own.
- **Operable**: `/metrics` for Prometheus, a status page, tokens per access
  level with browser sessions.
- **Connectors**: through [Kayak](https://github.com/niclasgrahm/kayak); see
  [examples/kayak-demo](examples/kayak-demo).

Documentation: <https://propell.dev/tidepool/> (the source is
[website/](website); `just docs-dev` serves it locally):

- [Getting started](website/getting-started.md)
- [The project](website/project/the-project.md): tables, the model, metrics,
  hierarchies, dashboards, selections, changes
- [Writing rows](website/data/writing-rows.md), [queries](website/data/queries.md),
  [the live protocol](website/data/live.md), [Kayak](website/data/kayak.md)
- [Running a server](website/operating/deployment.md): durability, access,
  monitoring, deployment
- [Agents (MCP)](website/agents.md)
- The reference (config files, HTTP API, MCP tools) is generated from the
  source by `just docs`.

For contributors: [docs/plan.md](docs/plan.md) is the design and the
milestones, [CLAUDE.md](CLAUDE.md) the conventions, [licensing.md](licensing.md)
the licence split (the protocol and config crates are Apache-2.0, the rest
AGPL-3.0-or-later).
