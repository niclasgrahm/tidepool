# durability

Without a data directory, tidepool keeps the data in memory only. The data
is gone when the server stops. Use this for a test, a demo or a laptop.

With a data directory, tidepool writes every batch to a log before it
acknowledges the batch. Set the directory in `tidepool.yaml` (`data_dir`), or
with `--data`.

## acknowledgement

`durability` in `tidepool.yaml` sets when tidepool acknowledges a write:

| value | acknowledges | a crash can lose |
| --- | --- | --- |
| `fsync` (default) | after the log is on disk | nothing that was acknowledged |
| `async` | before the log is on disk | the last moments of acknowledged writes |

Batches that arrive together share one fsync. Thus `fsync` costs little when
many clients write.

## checkpoints and restarts

Every minute, tidepool writes a checkpoint in the background. A checkpoint
writes each immutable segment one time, and appends new dictionary values.
tidepool keeps the two newest checkpoints.

At startup, tidepool loads the newest checkpoint and replays the log after
it. For 100 million orders, a restart takes about 1.2 s on an Apple M1 Max.

## the directory

```
data/
├── wal/            the log since the last checkpoint
├── checkpoints/    the two newest checkpoints
├── segments/       immutable column data, written one time
├── dict/           dictionaries, appended to
└── config/         the version history of the config, and the approval queue
```

Do not edit the files in the data directory. Do not share one data directory
between two servers.

## idempotency keys

With a data directory, the idempotency keys of the last 24 hours are in the
log and in the checkpoints. A client can retry a batch whose reply it lost in
a crash. tidepool writes nothing and sends the first reply again. tidepool
keeps at most 100,000 keys and removes the oldest first.

## retention

`retention` on a table removes old rows in the background. Rows that expire
after the last checkpoint come back after a restart, until the next
retention pass removes them again.

## the config and the data

The data directory remembers the version of the config that it holds. When
the server starts on a project directory that changed while it was stopped,
it applies the difference as a change. A destructive change stops the start,
unless you start the server with `--allow-destructive`.

## what is tested

- 1,000 simulated power losses at random points, some of them during a
  checkpoint. After each, tidepool recovers exactly the acknowledged writes.
- 25 `kill -9` cycles of the real binary while it ingests. No acknowledged
  order is lost.

`just crash` runs both at full depth. See [testing](/contributing/testing).
