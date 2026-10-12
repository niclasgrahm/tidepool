# access

tidepool has three access levels. Each level has a bearer token, from an
environment variable:

| variable | gives access to |
| --- | --- |
| `TIDEPOOL_READ_TOKEN` | dashboards, queries, the model, the status, the metrics |
| `TIDEPOOL_INGEST_TOKEN` | writes and deletes of rows |
| `TIDEPOOL_ADMIN_TOKEN` | the config and approvals, and every other level |

The `auth` section of `tidepool.yaml` can give the variables other names.

A level whose variable is not set is open to everyone. This is correct on a
laptop. When the server listens on an address other than loopback with a
level open, it writes a warning.

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

## not yet

Accounts with passwords, single sign-on (JWT from an identity provider) and
the embedding of dashboards in other applications come after v0.1.
