# website/

The documentation site of tidepool, at propell.dev/tidepool. It uses
[VitePress](https://vitepress.dev). People write the prose. tidepool
generates every reference table.

```bash
just docs-dev     # :5173, hot reload (installs the packages the first time)
just docs         # generate the reference from the Rust source again
just docs-build   # production build into .vitepress/dist
```

## the files

| | |
| --- | --- |
| `index.md`, `.vitepress/theme/Landing.vue` | the landing page. `index.md` mounts the component. The copy, the replica of a dashboard and the styles are in the `.vue` file. The source material is `landing/` at the repository root. |
| `getting-started.md`, `project/`, `data/`, `operating/`, `agents.md`, `contributing/` | the prose: the guide. It replaces `docs/guide/`. |
| `reference/*.md`, `reference/config/*.md` | short prose pages. Each includes generated partials with an `@include` comment. |
| `reference/generated/`, `public/openapi.json`, `.vitepress/generated/sidebar.json` | **generated. Do not edit.** `just docs` writes them. |
| `.vitepress/theme/tidepool.css` | the palette and type of the web UI, for VitePress |

The generated files are committed, so the site builds without Rust.
`crates/tidepool-docsgen/tests/site.rs` fails when they do not match the
source. Thus a stale reference makes `just ci` red.

## where a change goes

| you changed | what to do |
| --- | --- |
| a config type or its doc comments, in `tidepool-config` | run `just docs` |
| an endpoint, in `tidepool-protocol/src/api_docs.rs` | run `just docs`. The OpenAPI document and the sidebar follow. |
| an MCP tool, in `tidepool-server/src/mcp.rs` | run `just docs` |
| the retail example | run `just docs`: the OpenAPI document has a path for each of its tables |
| a new kind of config file | add it to `KINDS`, run `just docs`, and include its partial in a page |
| a command or a flag | edit `reference/cli.md` (it is not generated) |
| a metric family in `/metrics` | edit `operating/monitoring.md` (it is not generated) |
| any other text | edit the page under `project/`, `data/`, `operating/` or `contributing/` |
| the navigation | edit `.vitepress/config.mts`. The entries of the HTTP API and the MCP tools are generated. |

## yaml samples

The test compiles every YAML sample on the site:

- A sample whose first line names a config file (`# tables/orders.yaml`)
  replaces that file in `examples/retail`. The result must compile.
- A sample whose first line is `# fragment` is a part of a file. The test
  skips it. `# fragment: kayak's config.yaml` says what it is a part of.

## words and design

All text follows `landing/copy-style.md`. The colours, the type and the
shapes come from `landing/visual-language.md`, which comes from
`frontend/style.css`. The site has a light and a dark theme, as the web UI
has.
