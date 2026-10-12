# how the reference is generated

Nobody writes the tables in this section by hand. tidepool generates them
from its source code.

- **The config files.** The config types of tidepool derive `JsonSchema`.
  Their doc comments are the descriptions. A server serves the same schemas
  at `/v1/schemas/{kind}`, for editors and agents.
- **The HTTP API.** One table in `tidepool-protocol` describes every
  endpoint. The router of the server is built from that table. Thus an
  endpoint that is not in the table does not exist, and an entry without a
  handler does not compile. The OpenAPI document and this site come from the
  same table.
- **The MCP tools.** The list that `tools/list` answers with.

`just docs` writes the tables as markdown partials under
`website/reference/generated/`, and the pages here include them. A test
(`tests/site.rs` in `tidepool-docsgen`) fails when the committed partials do
not match the source.

::: tip if a table is wrong
Fix the Rust source: a doc comment in `tidepool-config`, an entry in
`tidepool-protocol/src/api_docs.rs`, or a tool in
`tidepool-server/src/mcp.rs`. Then run `just docs`.
:::

## the sections

| | |
| --- | --- |
| [`tidepool.yaml`](/reference/config/server) | how the server runs |
| [`tables/<name>.yaml`](/reference/config/table) | the columns of a table, its mode and its key |
| [`model.ossie.yaml`](/reference/config/model) | the Ossie document: datasets, fields, relationships, metrics |
| [extensions](/reference/config/extensions) | the TIDEPOOL extensions in the model |
| [`dashboards/<id>.yaml`](/reference/config/dashboard) | filters and visuals |
| [http api](/reference/api) | every endpoint, its access level, and each status it can return |
| [schemas](/reference/schemas) | the request and response bodies of the endpoints |
| [mcp tools](/reference/mcp) | the tools for agents, and their arguments |
| [command line](/reference/cli) | the commands and flags of the binary |

## how to read a table

A key marked **required** must be in the file. Another key has the default
in the table, or no value. A type in a link has its own section below the
table. Keys are in alphabetical order. An unknown key is an error in every
file.

::: details read it as data
A server serves the same content as JSON:

```bash
curl localhost:7070/v1/schemas/table      # the JSON Schema of a table file
curl localhost:7070/v1/openapi.json       # the complete HTTP surface
```

This site has the OpenAPI document of the retail example, at
[`/openapi.json`](/openapi.json).
:::
