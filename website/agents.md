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
| `describe_model` | the tables, datasets, fields, relationships, metrics, dashboards with their visuals, the visual types, and how answers are calculated | read |
| `preview_table` | the newest rows of a table | read |
| `query_metrics` | metrics grouped by fields, under filters | read |
| `read_rows` | the newest rows of a table that pass filters. A `contains` filter finds text, for example in log lines. | read |
| `show_view` | shows an answer as a live view: a dashboard in memory, never written to the config. The answer has its id, its path in the UI and the numbers of each visual. | read |
| `save_view` | writes a view to `dashboards/<id>.yaml`, with the same policy as `apply_change` | admin |
| `get_selection` | what a person who looks at a view or dashboard has selected, by the id of their session | read |
| `set_selection` | replaces the selection of a session, and the view of the person follows. Back undoes it. | read |
| `get_config` | every config file, and the current version | admin |
| `plan_change` | what a change does: problems, and each part with its class | admin |
| `apply_change` | applies a change if the policy allows it, or puts it in the queue | admin |
| `ingest_rows` | writes rows to a table | ingest |

Each tool calls the HTTP endpoint that it wraps, with the credentials of the
agent. Thus an agent can do what its token can do over HTTP, and nothing
more. The answers and the errors are the same as over HTTP.

The [MCP reference](/reference/mcp) has the arguments of each tool.
`query_metrics`, `read_rows` and `plan_change` also declare the shape of
their answer (`outputSchema`). Each answer of `query_metrics` and
`read_rows` has an `epoch` and an `as_of` time: when the data it reads was
published.

## resources

The JSON Schema of each kind of config file is a resource, at
`tidepool://schemas/<kind>`: for example `tidepool://schemas/dashboard`.
An agent that writes a config file can read its schema first.

## help an agent ask well

An agent can only find what the model tells it about. Write it down in the
Ossie file, where `describe_model` shows it:

- `description` on the model, datasets, fields and metrics.
- `label` on a field: what to call it on screen.
- `ai_context` on the model, a dataset, a field, a relationship or a
  metric: instructions, synonyms and example questions. tidepool sends it
  to the agent as you write it.

```yaml
# fragment
metrics:
  - name: revenue
    ai_context: { synonyms: [sales, turnover], instructions: "every order, whatever its status" }
```

`describe_model` also gives the grains that each time field can be grouped
by, and `semantics`: how tidepool calculates answers. For example, a
division by zero gives NULL.

[`evals/questions.yaml`](https://github.com/niclasgrahm/tidepool/blob/main/evals/questions.yaml)
has questions about the retail example, each with the query that answers it
and the answer on known data. Use it to test how well an agent asks.

## what an agent can apply

`apply_change` applies a change when its class is in `agents.auto_apply` in
`tidepool.yaml`. The default is `[safe]`: an agent can add a table, a metric
or a visual. A change that joins tables with data, changes a type or removes
something waits on **Config → Waiting for approval**. A person approves it or
rejects it there.

An agent also cannot change or remove a dashboard that a person wrote last:
that change waits for approval too, with a warning that says why. An agent
can change the dashboards that it made itself.

The server applies these rules, not the agent: to each change that comes
through MCP, and to each change made with the agent token.

```yaml
# fragment
agents:
  auto_apply: [safe, rebuild]    # also let agents start rebuilds in the background
```

The history shows which changes an agent made (`by: agent`, and the
author `an agent: …`).

## the agent token

Give an agent the agent token (`TIDEPOOL_AGENT_TOKEN`; `auth.agent_token_env`
names another variable), not the admin token. With it, an agent can:

- do what the read token can,
- read the config, its versions and the changes that wait,
- plan and submit changes, which the rules above apply to.

It cannot approve or reject a change, also not its own: only a person with
the admin token can. It cannot write rows: give an agent that writes rows an
[ingest credential](/operating/access#ingest-credentials).

```bash
claude mcp add --transport http tidepool http://127.0.0.1:7070/mcp \
  --header "Authorization: Bearer $TIDEPOOL_AGENT_TOKEN"
```

## show an answer

`show_view` takes visuals as a dashboard file does (see `visual_types` in
`describe_model`). tidepool calculates every number, keeps the view live as
data arrives, and filters it when a person clicks. Open it at
`/#/v/<id>` on the server. If the person wants to keep it, `save_view`
makes it a dashboard. See [the live protocol](/data/live#views).

### in the conversation

AI apps that show MCP Apps (Claude, ChatGPT, VS Code and others) show the
view in the conversation, live: `show_view` names the app
`ui://tidepool/view`. The app is the web UI of tidepool. It loads from the
server and connects to it with a token for that view only (see
[embedded views](/operating/access#embedded-views)). Each selection that the
person makes goes to the model of the AI app, in words and as data. If the
AI app does not allow WebAssembly, the app shows the numbers of the view as
tables.

The AI app must reach the server at the address in the `public_url` setting
of `tidepool.yaml`. Without it, tidepool uses the address of the MCP
request.

```yaml
# fragment
public_url: https://tidepool.example.com
```

A change to `public_url` is a `security` change: a person approves it.

### what the person looks at

Each time the person selects something in the view, the AI app gets the
selection and the id of the live session. With `get_selection`, an agent
reads the selection of a session again. With `set_selection`, it shows the
person something, for example "the north region in March". The view changes
at once. The person can go back.

## a good loop

1. `describe_model` and `get_config`.
2. Edit the files.
3. `plan_change` until it reports no problems.
4. `apply_change` with the `base_version` from step 1.
