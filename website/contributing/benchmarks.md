# benchmarks

The benchmarks are in `tidepool-bench`. They use release builds and are not
part of `just ci`.

| command | measures |
| --- | --- |
| `just bench` | ingest throughput, for each format |
| `just bench-query` | query latency on the reference dataset |
| `just bench-live` | freshness: 50 WebSocket sessions while tidepool ingests 100,000 rows/s |
| `just bench-restart` | the restart time of the reference dataset |

The reference dataset is the retail star schema with up to 100 million
`orders` rows.

## numbers

All on an Apple M1 Max (10 cores), from the milestone notes in
`docs/plan.md`.

**Ingest** (`just bench`, 8 clients, batches of 10,000 rows, timed until the
last row is visible):

| format | rows per second |
| --- | --- |
| NDJSON | about 2.2 million |
| Arrow IPC | about 2.4 million |

A stored `orders` row takes about 28 bytes.

**Queries** (`just bench-query`, 100 million orders, region × category, three
metrics, 200 selections that are each a new combination):

| | p50 | p95 |
| --- | --- | --- |
| a new selection | about 68 ms | about 113 ms |
| a selection toggled back | less than 1 ms | |

The first query after a start takes 170 to 290 ms. It builds the join
columns and the relationship maps.

**Live** (`just bench-live`, 200 ms epochs):

| | result |
| --- | --- |
| an epoch of 100,000 new orders, at 100 million orders | 0.6 to 0.9 ms |
| from the ingest acknowledgement to the frame, 50 sessions, 100,000 rows/s | p50 128 to 180 ms, p95 220 to 228 ms |

**Restart** (`just bench-restart`, 100 million orders): 1.2 s.

## arguments

```bash
just bench --rows 1000000 --batch 10000 --clients 8
just bench-query --orders 100000000 --runs 200 --metrics revenue,order_count
just bench-live --orders 10000000 --sessions 50 --rate 100000 --seconds 30
```

Other work on the machine moves the latency by 15 to 20 ms. When you report
a number, also report the load average of the machine.
