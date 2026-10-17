# deployment

tidepool is one binary. The web UI is in it. It needs no database, no JVM
and no other server. It listens on port 7070.

## the container image

There is no published image yet. Build it from a checkout:

```bash
docker build -t tidepool .
```

The image is about 112 MB. It runs as uid 10001. Without arguments, it
serves the retail example, in memory, as a demo with ingest and admin open.
Publish its port on loopback only:

```bash
docker run -p 127.0.0.1:7070:7070 tidepool
```

Run it with your project and a volume for the data:

```bash
docker run -p 7070:7070 \
  -v ./my-project:/project -v tidepool-data:/data \
  -e TIDEPOOL_ADMIN_TOKEN=… -e TIDEPOOL_READ_TOKEN=… -e TIDEPOOL_INGEST_TOKEN=… \
  tidepool serve --config /project --data /data --listen 0.0.0.0:7070
```

The project directory must be writable by uid 10001. Without that, tidepool
cannot write back the changes from the web UI.

On an address other than loopback, tidepool does not start without the
ingest and admin tokens. Set the read token too, unless the dashboards are
for everyone. See [access](/operating/access).

## a public web UI and private ingest

The web UI must be where its users are. The ingest endpoints must be only
where the producers are. Give each its own listener:

```yaml
# fragment: tidepool.yaml
listen:
  public: 0.0.0.0:7070
  ingest: 10.0.0.5:7071
  admin: 127.0.0.1:7072
```

Publish only the `public` port to the internet. Put the `ingest` address on
the network of kayak and the other producers. Reach `admin` through SSH or a
VPN. To let one producer write from the internet, give it a [public
credential](/operating/access#public-ingest).

Terminate TLS in front of tidepool, for example in a load balancer or a
reverse proxy.

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
