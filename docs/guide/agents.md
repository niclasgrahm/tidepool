# Agents (MCP)

Tidepool speaks the [Model Context Protocol](https://modelcontextprotocol.io)
at `POST /mcp` (Streamable HTTP, JSON responses). An agent can look at the
model, compute metrics, write rows, and plan, propose or apply changes to
the config.

Connecting Claude Code, for example:

```bash
claude mcp add --transport http tidepool http://127.0.0.1:7070/mcp \
  --header "Authorization: Bearer $TIDEPOOL_ADMIN_TOKEN"
```

(Without tokens configured, leave the header out.)

## Tools

| tool | what it does | needs |
|---|---|---|
| `describe_model` | tables with columns and row counts, datasets and fields, relationships, metrics with their SQL, dashboards | read |
| `preview_table` | the newest rows of a table | read |
| `query_metrics` | metrics grouped by fields under filters: the body of `POST /v1/query` | read |
| `get_config` | every config file, and the version changes are made against | admin |
| `plan_change` | what setting or removing files would do: problems, changes and their classes | admin |
| `apply_change` | applies the change if policy allows; otherwise queues it for a person | admin |
| `ingest_rows` | writes rows to a table | ingest |

Each tool is the HTTP endpoint it wraps, called with the agent's own
credentials, so an agent can do exactly what its token can do over HTTP,
and every answer and error reads the same.

## What agents may apply

`apply_change` applies a change whose class is listed in `tidepool.yaml`'s
`agents.auto_apply`, which is `[safe]` unless you say otherwise: adding a
table, a metric or a visual goes live; joining populated tables, changing
types or removing anything waits under **Config → Waiting for approval**,
where a person approves or rejects it. The history says which changes an
agent made (`an agent: …`).

```yaml
agents:
  auto_apply: [safe, rebuild]    # also let agents trigger background rebuilds
```

A good loop for an agent: `describe_model`, `get_config`, edit the files,
`plan_change` until there are no problems, then `apply_change` with the
`base_version` it started from.
