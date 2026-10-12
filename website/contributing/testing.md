# testing

`just ci` is what CI runs: the format check, clippy with every warning as an
error, and every test. A change is complete only when `just ci` passes.

```bash
just ci
```

New behaviour needs a test that fails without the change. A bug fix needs a
regression test.

## the suites

| suite | what it checks | command |
| --- | --- | --- |
| unit and integration tests | every crate, the HTTP API through a real router, the live protocol through a real socket | `just test` |
| golden diagnostics | each check of the model compiler: the retail example with one edit, and the exact message | `cargo insta review` to accept a change |
| golden plans | each kind of change, and its class | `cargo insta review` |
| the oracle | query results against DuckDB | `just oracle 1000` |
| durability | power losses and `kill -9` | `just crash` |
| the embedded UI | the binary with the web UI in it | `just build && just test-embed` |
| the browser | headless Chrome against the built UI | `just e2e` |
| the doc site | the generated reference, and every YAML sample on the site | part of `just test` |

## the oracle

`tidepool-oracle` makes random schemas, data and queries from a seed. The
data arrives in random batches, with upserts, deletes and epochs between
them. Each query runs on three paths of the engine:

- the fast path (morsels, selection vectors, dense groups);
- the general path (every value decoded, every expression evaluated);
- a cached `Engine`.

Each answer must be the same as DuckDB's. A few queries also replay over
every snapshot through one cached engine. That catches a cache that outlives
its data.

The live oracle writes to tidepool and to DuckDB one change at a time. After
each epoch, the incremental answers must be the same as DuckDB's answer from
the start.

The migration oracle writes data under an earlier version of a project,
changes the model while it writes, and checks every query after the change.

`just test` runs 40 seeds. `just oracle 1000` runs more. A failure names the
seed and the query:

```bash
TIDEPOOL_ORACLE_SEED=1234 cargo test -p tidepool-oracle
```

When you add a query feature, extend the generator so that the oracle covers
it. Then plant a bug in the new code and make sure that the oracle finds it.

## durability

- `tests/durability.rs` in `tidepool-store`: 1,000 simulated power losses,
  some during a checkpoint. After each, the store must recover exactly the
  acknowledged writes.
- `tests/crash.rs` in `tidepool`: `kill -9` of the real binary while it
  ingests, 25 times. No acknowledged order may be lost.

`just test` runs fewer of each. `just crash` runs all of them.

## the browser

`just e2e` runs headless Chrome against the built UI, on a copy of the retail
example. It clicks a bar, watches a live update, goes back, and adds a table
in the config editor. It needs Chrome, so it is not part of `just ci`. Run it
after a change to the UI. Set `TIDEPOOL_CHROME` when Chrome is not found.
