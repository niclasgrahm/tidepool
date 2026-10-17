# monitoring

## probes

| endpoint | answers `200` when |
| --- | --- |
| `GET /healthz` | the process runs |
| `GET /readyz` | the server is ready for traffic: recovery and the changes at startup are complete |

## metrics

`GET /metrics` gives the state of the server in the Prometheus text format.
It is on the [admin listener](/operating/access#listeners), and it needs the
read token.

| metric | type | what it is |
| --- | --- | --- |
| `tidepool_build_info` | gauge | the running version, as the `version` label |
| `tidepool_uptime_seconds` | gauge | seconds since the server started |
| `tidepool_epoch` | gauge | the newest published epoch |
| `tidepool_config_version` | gauge | the config version that the server runs |
| `tidepool_table_rows` | gauge | live rows, per table |
| `tidepool_table_bytes` | gauge | memory of the segments of each table |
| `tidepool_table_segments` | gauge | segments, per table |
| `tidepool_dictionary_bytes` | gauge | memory of the dictionary values |
| `tidepool_cache_bytes` | gauge | memory of the caches of the query engine |
| `tidepool_rows_written_total` | counter | rows written or deleted through the ingest API, per table and credential |
| `tidepool_ingest_requests_total` | counter | ingest requests, per table, operation and status. A table that does not exist is `(none)`. |
| `tidepool_ingest_refused_total` | counter | ingest requests that the limits of a credential refused, per credential and limit |
| `tidepool_epoch_publish_seconds` | summary | the time to publish an epoch |
| `tidepool_live_refresh_seconds` | summary | the time to update every live session after an epoch |
| `tidepool_sessions` | gauge | live dashboard sessions |
| `tidepool_query_seconds` | summary | the time to answer `POST /v1/query` |
| `tidepool_query_errors_total` | counter | queries refused with `400` |
| `tidepool_mcp_tool_calls_total` | counter | MCP tool calls, per tool |
| `tidepool_rebuild_rows` | gauge | rows that a running model change rewrites. Only during a rebuild. |
| `tidepool_rebuild_rows_done` | gauge | rows rewritten so far. Only during a rebuild. |
| `tidepool_wal_last_lsn` | gauge | the last write in the log. Only with a data directory. |
| `tidepool_checkpoint_lsn` | gauge | the last write in the newest checkpoint. Only with a data directory. |
| `tidepool_checkpoint_age_seconds` | gauge | seconds since the newest checkpoint. Only with a data directory. |

## the status page

**Status** in the web UI shows the same values for people: memory, tables,
epochs, the epoch and refresh times, the log and the checkpoints. It updates
every few seconds. `GET /v1/status` gives the same as JSON.
