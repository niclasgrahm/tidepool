# Operations

## Data and durability

With a data directory (`data_dir` in `tidepool.yaml`, or `--data`), every
write is in the write-ahead log before it's acknowledged: writes arriving
together share one fsync (`durability: fsync`, the default), or are
acknowledged before it (`async`: faster, and a crash can lose the last
moments). Checkpoints are written every minute in the background; a restart
loads the newest one and replays the log after it, which takes about a
second for a hundred million rows. Without a data directory, data lives in
memory and is gone when the server stops.

```
data/
├── wal/            the log since the last checkpoint
├── checkpoints/    the two newest checkpoints
├── segments/       immutable column data, written once
├── dict/           dictionaries, appended to
└── config/         the config's version history and the approval queue
```

Retention (`retention` on a table) removes old rows in the background.

The data directory remembers which config version it holds. Started on a
config directory that changed in the meantime, the server applies the
difference as a change; if that would destroy data, it refuses to start
until you pass `--allow-destructive`.

## Access

Three tokens, from environment variables (names configurable under `auth`):

| | grants |
|---|---|
| `TIDEPOOL_READ_TOKEN` | dashboards, queries, the model, status, metrics |
| `TIDEPOOL_INGEST_TOKEN` | writing and deleting rows |
| `TIDEPOOL_ADMIN_TOKEN` | the config and approvals, and everything else |

A level whose variable is unset is open to everyone: right on a laptop, and
the reason the server warns when it listens on anything but loopback with a
level open. Requests send `Authorization: Bearer <token>`. Browsers open the
UI with `?token=…` once: the page trades it for a session cookie
(`HttpOnly`, `SameSite=Strict`, twelve idle hours) and takes it out of the
address bar. The config pages ask for the admin token when they need it.

Accounts with passwords, single sign-on (JWT from an identity provider) and
embedding in other applications are planned after v0.1.

## Monitoring

- `GET /healthz`: the process is up. `GET /readyz`: it's ready for traffic
  (after recovery and startup changes).
- `GET /metrics`: Prometheus: rows, memory and segments per table, ingest
  requests and rows written, epoch publish and live refresh times, query
  times and errors, sessions, the log and checkpoints, MCP tool calls, a
  running rebuild.
- The **Status** page shows the same, for people.

## Docker

```bash
docker build -t tidepool .
docker run -p 7070:7070 -v ./my-project:/project -v tidepool-data:/data \
  -e TIDEPOOL_ADMIN_TOKEN=… \
  tidepool serve --config /project --data /data --listen 0.0.0.0:7070
```

The image runs as uid 10001; the mounted project must be writable by it for
changes made in the UI to be written back. Started with no arguments, it
serves the retail example in memory.

## Upgrading

v0.1 is the first release: the data directory's format may change before
1.0, and the release notes will say when a data directory has to start
over.
