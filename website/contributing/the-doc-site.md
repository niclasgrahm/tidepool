# the doc site

This site is `website/` in the repository. It uses
[VitePress](https://vitepress.dev). People write the prose. tidepool
generates every reference table.

```bash
just docs-dev     # :5173, hot reload
just docs         # generate the reference from the Rust source again
just docs-build   # production build into website/.vitepress/dist
```

`website/readme.md` lists the files and says where each kind of change goes.

## the generated reference

`tidepool-docsgen` writes markdown partials under
`website/reference/generated/`, `public/openapi.json` and
`.vitepress/generated/sidebar.json`. It reads:

- the JSON Schemas of the config files, from `tidepool-config`. The doc
  comments on the config types are the descriptions.
- the endpoint table, `tidepool_protocol::api_docs::endpoints()`, and the
  schemas of the bodies;
- the MCP tools, `tidepool_server::mcp_tools()`;
- the retail example, for the per-table paths of the OpenAPI document.

The generated files are committed, so the site builds without Rust.

## the tests

`crates/tidepool-docsgen/tests/site.rs` runs in `just ci`:

- **the committed reference matches the source.** Run `just docs` to fix it.
- **every generated partial is included by a page.** A new kind of config
  file needs a page, or a section of a page, with an `@include` comment for
  it.
- **every whole YAML sample compiles.** A sample whose first line names a
  file (`# tables/orders.yaml`) replaces that file in the retail example, and
  the result must compile with no errors. Start a part of a file with
  `# fragment`. Start a sample of kayak's config with
  `# fragment: kayak's config.yaml`.

## the words

All text follows `landing/copy-style.md`: Simplified Technical English,
lowercase headings, the name `tidepool` in lowercase. The landing page
follows `landing/product-and-copy.md` and `landing/visual-language.md`.
