# Postgres logs, live

Postgres logs everything it does, as JSON, and a dashboard searches those
lines as you type. The search is a selection: type `deadlock` and the
counts, the trend, the severities, the conditions and the applications
answer for the lines that contain it. Click a bar and the lines narrow
again.

```bash
just pglogs          # or: cd examples/pglogs && docker compose up --build
```

Then open <http://localhost:7070/#/d/logs>. `TIDEPOOL_PORT` and
`POSTGRES_PORT` move the ports when 7070 or 5432 are taken.

## what runs

| service | what it does |
| --- | --- |
| `postgres` | Postgres 17, logging as JSON (`log_destination = jsonlog`): every statement with its duration, connections, lock waits, checkpoints, vacuum, and its own `DEBUG` lines. A new file every minute, under `PGDATA/log`. |
| `app` | a small shop (`app/app.py`): sign-ups, orders, payments, history. Now and then it gets something wrong on purpose: a duplicate sign-up, an overdrawn account, a typo in a report, a dropped table, two transfers that deadlock, a lock that times out, a slow report, a wrong password, a database that isn't there. It logs as JSON too. |
| `shipper` | tails Postgres' current log file and the app's log (`shipper/ship.py`, standard library only), and posts the lines to Tidepool as NDJSON every 250 ms. It writes the `severities` table and the SQLSTATE codes it knows when it starts, and any other code when it first sees it. |
| `tidepool` | the server, on the project in `tidepool/` |

The shipper saves where it read each file to, and a batch's idempotency
key is where the batch starts in its files. Restart the shipper or
Tidepool and nothing is lost or written twice.

## the model

`tables/logs.yaml` keeps the text `encoding: plain` (`message`, `detail`,
`hint`, `statement`): it is searched, never grouped. What lines are
filtered and grouped by comes out of them before they arrive, as columns
with a dictionary: the severity, the database, the application, the
backend, the SQLSTATE code, and the minute (the trend's buckets; computed
by the shipper, so the trend reads a small dictionary instead of every
timestamp). Two small tables give the codes meaning: `severities` (the
level of each severity) and `sqlstates` (the condition and class of each
code).

## things to try

- Type `deadlock`, then click `deadlock_detected` in the conditions table:
  the tail shows the deadlocks, with what each process waited for in
  `detail`. Back takes the click away, then the search, in two steps.
- Type `timeout`: statement timeouts and lock timeouts together. The level
  list says which are errors.
- Pick `fatal`: who tried a wrong password, and who asked for a database
  that doesn't exist (the `application` list says `intruder` and
  `archiver`).
- Type `duration` and brush the trend: the statements of a few minutes.
- `ACTIONS_PER_SECOND=50 docker compose up` for more lines.

## without docker

`sample.ndjson` is 310 lines from one run, as the shipper sent them. With a
server on the project (`tidepool serve --config examples/pglogs/tidepool`):

```bash
curl -X POST 'http://127.0.0.1:7070/v1/tables/logs/rows' \
  -H 'content-type: application/x-ndjson' --data-binary @examples/pglogs/sample.ndjson
```

The `problems` metric needs the `severities` table; `ship.py` has its
rows. `crates/tidepool-server/tests/pglogs.rs` loads the sample and checks
the searches against it.
