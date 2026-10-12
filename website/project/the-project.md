# the project directory

A tidepool project is a directory of YAML files. Keep it in version control.
The files are the source of truth for the server: a change from the web UI,
the API or an agent is written back to them.

```
my-project/
├── tidepool.yaml             how the server runs
├── tables/<name>.yaml        one file per table
├── model.ossie.yaml          the semantic model: datasets, relationships, metrics
└── dashboards/<id>.yaml      one file per dashboard; the id is the file name
```

| file | what it holds | page |
| --- | --- | --- |
| `tidepool.yaml` | the address, the data directory, the epoch interval, durability, write-back, tokens, the policy for agents | [reference](/reference/config/server) |
| `tables/<name>.yaml` | the columns of one table, its mode and its key | [tables](/project/tables) |
| `model.ossie.yaml` | datasets, fields, relationships and metrics, in Apache Ossie | [the model](/project/the-model) |
| `dashboards/<id>.yaml` | the filters and visuals of one dashboard | [dashboards](/project/dashboards) |

`tidepool init <dir>` writes a complete project to start from: the retail
example.

## tidepool.yaml

```yaml
# tidepool.yaml
listen: 127.0.0.1:7070         # --listen overrides it
data_dir: data                 # relative to this directory; --data overrides it
epoch_interval: 200ms          # how often new data becomes visible, at most
memory_budget: 75%             # of system memory, or for example 8GiB (not enforced yet)
durability: fsync              # fsync: acknowledge after the log is on disk; async: before
config_writeback:              # changes from the API or the UI...
  enabled: true                # ...are written to these files
  git_commit: true             # ...and committed, when the directory is a git repository
auth:                          # the environment variables that hold the tokens
  read_token_env: TIDEPOOL_READ_TOKEN
  ingest_token_env: TIDEPOOL_INGEST_TOKEN
  admin_token_env: TIDEPOOL_ADMIN_TOKEN
agents:
  auto_apply: [safe]           # the change classes an agent can apply without approval
```

Each key in this sample has its default value, except `data_dir`. Without
`data_dir` (and without `--data`), tidepool keeps the data in memory only.

`listen`, `data_dir`, `auth` and `durability` take effect when the server
starts again. The other keys take effect at once.

## check a project

```bash
tidepool validate --config my-project
```

`validate` reads every file and checks how the files fit together. It reports
every problem it finds, and it points at the token that is wrong:

```
error: metric `customer_count` refers to unknown field `orders.customr_id`
  --> model.ossie.yaml:69:63
   |
69 |     expression: { dialects: [{ dialect: ANSI_SQL, expression: "COUNT(DISTINCT orders.customr_id)" }] }
   |                                                               ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   = help: did you mean `orders.customer_id`?

1 error, 0 warnings
```

An unknown key is an error in every file. A typo cannot pass without a
message.

## see what a change does

```bash
tidepool plan --config my-project --base my-project-before
```

`plan` compares two versions of a project and lists each change with its
class:

```
2 changes (1 safe, 0 rebuild, 1 destructive)

  + column       orders.comment  string  safe
  - column       orders.note             destructive  (its data is deleted)

warning: `orders.note` was removed and `orders.comment` added with the same type: if this is a rename, add `renamed_from: note` to the column to keep its data
```

[Changes](/project/changes) explains the classes, and how a running server
applies a change.

## editors

Each kind of config file has a JSON Schema. A server serves them at
`/v1/schemas/{kind}`: `server`, `table`, `model`, `dashboard`, and the JSON
inside the extensions (`metric-extension`, `field-extension`,
`dataset-extension`, `relationship-extension`, `model-extension`).

With the YAML language server (VS Code, Neovim, Zed), add a modeline to the
top of a file to get completion and checks as you type:

```yaml
# fragment
# yaml-language-server: $schema=http://127.0.0.1:7070/v1/schemas/table
name: orders
```
