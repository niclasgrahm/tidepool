# agents

tidepool speaks the [Model Context Protocol](https://modelcontextprotocol.io)
at `POST /mcp`. An agent can read the model, calculate metrics, write rows,
and plan, propose or apply changes to the config.

To connect Claude Code:

```bash
claude mcp add --transport http tidepool http://127.0.0.1:7070/mcp \
  --header "Authorization: Bearer $TIDEPOOL_ADMIN_TOKEN"
```

Omit the header when the server has no tokens.

## tools

| tool | what it does | token |
| --- | --- | --- |
| `describe_model` | the tables, datasets, fields, relationships, metrics and dashboards | read |
| `preview_table` | the newest rows of a table | read |
| `query_metrics` | metrics grouped by fields, under filters | read |
| `read_rows` | the newest rows of a table that pass filters. A `contains` filter finds text, for example in log lines. | read |
| `get_config` | every config file, and the current version | admin |
| `plan_change` | what a change does: problems, and each part with its class | admin |
| `apply_change` | applies a change if the policy allows it, or puts it in the queue | admin |
| `ingest_rows` | writes rows to a table | ingest |

Each tool calls the HTTP endpoint that it wraps, with the credentials of the
agent. Thus an agent can do what its token can do over HTTP, and nothing
more. The answers and the errors are the same as over HTTP.

The [MCP reference](/reference/mcp) has the arguments of each tool.

## what an agent can apply

`apply_change` applies a change when its class is in `agents.auto_apply` in
`tidepool.yaml`. The default is `[safe]`: an agent can add a table, a metric
or a visual. A change that joins tables with data, changes a type or removes
something waits on **Config → Waiting for approval**. A person approves it or
rejects it there.

```yaml
# fragment
agents:
  auto_apply: [safe, rebuild]    # also let agents start rebuilds in the background
```

The history shows which changes an agent made (`an agent: …`).

## a good loop

1. `describe_model` and `get_config`.
2. Edit the files.
3. `plan_change` until it reports no problems.
4. `apply_change` with the `base_version` from step 1.
