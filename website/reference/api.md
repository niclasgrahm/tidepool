# http api

Everything the web UI does goes through this API. The probes and the
metrics are at the root (`/healthz`, `/readyz`, `/metrics`, `/mcp`). Every
other endpoint is under `/v1`.

A failure is `{"error": "…"}`, with a status that says what kind of failure
it is. The access level of each endpoint is in its heading. See
[access](/operating/access) for the tokens.

A server describes itself at `/v1/openapi.json`, with one typed path for the
rows of each table. This site has the document of the retail example at
[`/openapi.json`](/openapi.json).

<!--@include: ./generated/api/endpoints.md-->
