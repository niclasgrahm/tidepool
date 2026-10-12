# deployment

tidepool is one binary. The web UI is in it. It needs no database, no JVM
and no other server. It listens on port 7070.

## the container image

There is no published image yet. Build it from a checkout:

```bash
docker build -t tidepool .
```

The image is about 112 MB. It runs as uid 10001. Without arguments, it
serves the retail example, in memory.

Run it with your project and a volume for the data:

```bash
docker run -p 7070:7070 \
  -v ./my-project:/project -v tidepool-data:/data \
  -e TIDEPOOL_ADMIN_TOKEN=… -e TIDEPOOL_READ_TOKEN=… -e TIDEPOOL_INGEST_TOKEN=… \
  tidepool serve --config /project --data /data --listen 0.0.0.0:7070
```

The project directory must be writable by uid 10001. Without that, tidepool
cannot write back the changes from the web UI.

Set all three tokens when the port is reachable from other machines. See
[access](/operating/access).

## the binary

```bash
just build
./target/release/tidepool serve --config my-project
```

The [command line reference](/reference/cli) lists every command and flag.

## memory

tidepool keeps all data in memory. An `orders` row of the retail example
takes about 28 bytes. 100 million orders take about 2.8 GB. Give the machine
memory for the data, the caches of the query engine and the dictionaries.
`/metrics` shows each of them.

`memory_budget` in `tidepool.yaml` is accepted, but tidepool does not
enforce it yet.

## the config in git

Mount the project directory from a git checkout. With
`config_writeback.git_commit: true`, tidepool commits each change that it
applies. The commit message summarizes the change and names its author:
`tidepool: … [by …]`.

To keep the changes out of history, set `git_commit: false`. tidepool then
writes the files and does not commit them.

## upgrades

tidepool is pre-1.0. The format of the config and of the data directory can
change between minor versions. The release notes say when a data directory
must start again empty.
