# command line

```
tidepool <command> [flags]
```

`tidepool --help` and `tidepool <command> --help` show the same as this page.

## serve

Runs the server.

| flag | what it does |
| --- | --- |
| `--config <dir>` | the project directory. Without it, the server runs with no tables. |
| `--listen <address>` | the address to listen on. Overrides `listen` in `tidepool.yaml`. The default is loopback, `127.0.0.1:7070`. |
| `--data <dir>` | keeps the data here. Overrides `data_dir`. Without either, the data is in memory only. |
| `--ui-dir <dir>` | serves the web UI from this directory, in place of the UI in the binary. For development. |
| `--allow-destructive` | applies a project directory that changed while the server was stopped, also when the change deletes data |

## init

Writes a new project: the retail example, with four tables, a model and two
dashboards.

| argument | what it is |
| --- | --- |
| `<dir>` | where to write it. The default is `tidepool-project`. |

## feed

Sends sample data to a server that runs an `init` project: customers,
products, categories, ninety days of orders, and then new orders.

| flag | what it does |
| --- | --- |
| `--server <url>` | the server. The default is `http://127.0.0.1:7070`. |
| `--rate <n>` | new orders each second. The default is 20. |
| `--seconds <n>` | stops after this time. Without it, `feed` runs until you stop it. |

When the server guards its ingest, `feed` sends the token in
`TIDEPOOL_INGEST_TOKEN`, or else the one in `TIDEPOOL_ADMIN_TOKEN`.

## validate

Checks a project directory: every file, and how the files fit together.
Exits with a failure status when there is an error. Warnings do not fail it.

| flag | what it does |
| --- | --- |
| `--config <dir>` | the project directory. The default is `.`. |

## plan

Shows what changes from one version of a project to another, and the class of
each change.

| flag | what it does |
| --- | --- |
| `--config <dir>` | the project directory with the changes. The default is `.`. |
| `--base <dir>` | the project directory to compare with. Without it, the plan is against an empty project. |
