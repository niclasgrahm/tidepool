# access

tidepool has three access levels:

| level | gives access to |
| --- | --- |
| read | dashboards, queries, the model, the status, the metrics |
| ingest | writes and deletes of rows |
| admin | the config and approvals, and every other level |

The read and admin levels each have a bearer token, from an environment
variable: `TIDEPOOL_READ_TOKEN` and `TIDEPOOL_ADMIN_TOKEN`. Ingest has one
token for every table (`TIDEPOOL_INGEST_TOKEN`), or named
[credentials](#ingest-credentials) with their own tokens. The `auth` section
of `tidepool.yaml` can give the variables other names.

A level without a token is open to everyone. This is correct on a laptop.
On an address other than loopback, tidepool does not start with ingest or
admin open. See [fail closed](#fail-closed).

The [HTTP API reference](/reference/api) gives the level of each endpoint.
The probes (`/healthz`, `/readyz`) and the descriptions of the software
(`/v1/openapi.json`, `/v1/schemas/...`) are public.

## requests

Send the token in the `Authorization` header:

```bash
curl -H "Authorization: Bearer $TIDEPOOL_READ_TOKEN" http://127.0.0.1:7070/v1/model
```

## browsers

Open the web UI one time with the token in the address:
`http://127.0.0.1:7070/?token=…`. The page exchanges the token for a session
cookie (`HttpOnly`, `SameSite=Strict`, twelve hours of idle time) and removes
the token from the address bar. The config pages ask for the admin token when
they need it.

Sessions are in memory. A restart of the server ends them.

## listeners

The people who look at dashboards and the programs that write rows are
usually in different places. Thus tidepool can listen on one address for
each part of its surface:

```yaml
# tidepool.yaml
listen:
  public: 0.0.0.0:7070      # the web UI, reads, live dashboards
  ingest: 10.0.0.5:7071     # reads and all ingest, for kayak
  admin: 127.0.0.1:7072     # everything, also the config and /metrics
epoch_interval: 200ms
durability: fsync
config_writeback: { enabled: true, git_commit: false }
```

| listener | serves |
| --- | --- |
| `public` | the web UI, the read endpoints, `/v1/ws`, and ingest for [public credentials](#public-ingest) only |
| `ingest` | the read endpoints and all ingest |
| `admin` | every endpoint: the config, approvals, `/metrics`, and all ingest |

A listener that you do not give an address uses the address of `admin`. If
`admin` has no address, it uses the address of `public`. Two listeners with
the same address are one listener. One address (`listen: 0.0.0.0:7070`)
serves everything.

A path that a listener does not serve gets `404`. On the `public` listener,
a table that no public credential writes gets `404` too, with the same
message as a table that does not exist.

## ingest credentials

Give each producer its own credential under `auth.ingest`. A credential
says which tables it writes and which operations it can do:

```yaml
# tidepool.yaml
listen: 127.0.0.1:7070
epoch_interval: 200ms
durability: fsync
config_writeback: { enabled: true, git_commit: false }
auth:
  ingest:
    - name: kayak
      token_env: KAYAK_TOKEN
    - name: crm
      token_env: [CRM_TOKEN, CRM_TOKEN_NEXT]
      tables: [customers]
      operations: [upsert]
```

| key | what it is |
| --- | --- |
| `name` | the name in metrics, in [filled columns](/project/tables#filled-columns), and in the scope of its idempotency keys |
| `token_env` | the variable with its token, or a list of variables. tidepool accepts any token of the list. |
| `tables` | the tables that it writes. The default, `"*"`, is every table. |
| `operations` | `append`, `upsert`, `delete`, `replace`. The default is every operation, or `append` for a public credential. |
| `public` | the credential can write through the `public` listener |
| `limits` | how much it can send. See [limits](#limits). |
| `origins` | the browser origins that it can write from |

`append` is a `POST` to an append table. `upsert` is a `POST` to an upsert
table. `delete` is a `DELETE`, and `replace` is a `PUT`.

A request without a valid token gets `401`. A request for a table or an
operation that the credential cannot do gets `403`. With `auth.ingest`, do
not set `ingest_token_env`. The single ingest token is a credential with the
name `ingest`, for every table and operation.

To change a token without downtime, do these steps:

1. Add the new variable to `token_env`, and restart the server.
2. Move the producer to the new token.
3. Remove the old variable, and restart the server.

## public ingest

Sometimes a producer outside your network must write rows: a sensor, a job
in a CI system, a partner. Give it a credential with `public: true`:

```yaml
# tidepool.yaml
listen:
  public: 0.0.0.0:7070
  admin: 127.0.0.1:7072
epoch_interval: 200ms
durability: fsync
config_writeback: { enabled: true, git_commit: false }
auth:
  ingest:
    - name: kayak
      token_env: KAYAK_TOKEN
    - name: sensors
      token_env: SENSOR_TOKEN
      public: true
      tables: [orders]
      limits: { rate: 200/s, batch_rows: 1000 }
```

A public credential appends by default. Do not give it `upsert`, `delete`
or `replace` unless the producer needs them. Anyone with its token can then
change or delete rows. `tidepool validate` and the plan warn about it.

The OpenAPI document on the `public` listener shows only the tables and
operations that public credentials can write. Give it to the people who run
the producer.

### without a token

A producer that cannot keep a secret, for example a page in a browser, can
write without a token. Set `token: none`. Such a credential must be public,
and it can only append:

```yaml
# fragment
    - name: web
      token: none
      public: true
      tables: [orders]
      origins: ["https://shop.example.com"]
```

Anyone who can reach the `public` listener can then append rows to these
tables. The limits are the only protection.

## limits

A credential can have limits. A public credential always has them. For each
limit that it does not set, it gets the default:

| limit | what it is | public default |
| --- | --- | --- |
| `rate` | requests at most, for example `200/s`, `1000/min` or `50000/h` | `100/s` |
| `batch_rows` | rows in one request at most | `10000` |
| `body` | the size of one body at most, for example `1MiB` | `4MiB` |
| `new_values` | values that are new to a dictionary-encoded column, at most, for example `1000/min` | `10000/h` |

Over `rate` or `new_values`, tidepool answers `429` with a `Retry-After`
header. Over `batch_rows` or `body`, it answers `413`. tidepool writes
nothing from a refused request. `tidepool_ingest_refused_total` counts the
refusals.

Dictionaries do not get smaller when rows go. Thus `new_values` keeps a
producer from filling the memory with values that are all different.

The batches of all public credentials together can use a quarter of the
write queue: 16 of 64 batches. When they use all of it, the next public
batch gets `429`. Private producers still get into the queue.

The limits are per process and approximate. Requests of one credential that
arrive at the same time can each see the same budget.

## browsers that write rows

A page on another origin can write rows when its origin is in the `origins`
of the credential. tidepool answers the preflight request of the browser
(`OPTIONS`) and adds `Access-Control-Allow-Origin` to the reply. A request
from an origin that the credential does not list gets `403`.

## idempotency keys

Each credential has its own idempotency keys. Two producers that send the
same key to the same table write two batches. One producer cannot answer or
remove the keys of another. See [retries](/data/writing-rows#retries).

## fail closed

On an address other than loopback, tidepool does not start in these cases:

- A listener that serves ingest has no ingest token.
- A listener that serves admin has no admin token.
- Two credentials have the same token.

To start a demo without tokens, add `--insecure-open ingest,admin`, or set
`TIDEPOOL_INSECURE_OPEN=ingest,admin`. Do not use it on a network that other
people can reach. Anyone who can reach the port can then write rows and
change the config.

A read level without a token on such an address starts with a warning.
Public dashboards without a token are a correct deployment.

## changes to access

A change to `listen`, `auth` or `agents` in `tidepool.yaml` has the class
`security`. The plan shows each credential that comes, goes or changes.
Such a change needs approval, also when it comes from the files. An agent
cannot apply it, whatever `agents.auto_apply` says. See
[changes](/project/changes).

The server reads the tokens and the listeners when it starts. Restart it to
use a change to them.

## not yet

These come after v0.1:

- accounts with passwords
- single sign-on (JWT from an identity provider)
- the embedding of dashboards in other applications
- signed requests or client certificates for producers
