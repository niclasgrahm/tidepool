# licensing

Tidepool is split across two licences, the same way and for the same reasons as
Kayak.

| what | licence | why |
| --- | --- | --- |
| `tidepool-protocol`, `tidepool-config` | Apache-2.0 | the shared vocabulary: wire types, the endpoint table, the config file types. Anything talking to Tidepool needs these (starting with Kayak's `tidepool` output), so copyleft here would be a tax on writing a client. |
| everything else: the server, engine, store, frontend, bench | AGPL-3.0-or-later | the product. |

Copyright © 2026 Niclas Grahm.

## third-party code shipped with the UI

`frontend/vendor/echarts-6.1.0.min.js` is [Apache ECharts](https://echarts.apache.org)
6.1.0, unmodified, under the Apache License 2.0 (`frontend/vendor/echarts-LICENSE.txt`).
It is copied into the built UI as is, with its licence notice in the file header.

## why the server is AGPL

Self-hosting Tidepool, modifying it, running it inside a company, embedding it
in something you don't distribute: all of that is what the licence is *for*,
and none of it asks anything of you beyond keeping the notices.

What the AGPL adds over the GPL is section 13: if you modify Tidepool and offer
it to other people **over a network**, those users are entitled to your
modified source. That is the case this is aimed at.

If that doesn't suit, for example to embed a modified Tidepool in a
proprietary product, a commercial licence is available. Get in touch.

## why the protocol and config crates are not

They carry no engine, runtime or UI, only types and descriptions. That makes
them the natural thing to build a client, a config generator, an agent tool or
a test harness against, and every one of those is worth encouraging rather
than licensing.

Apache-2.0 code can be used inside an AGPL-3.0 work, so the split is one-way
and works: the server may depend on the permissive crates, and nothing about
their licence weakens the server's. The reverse must never happen: a
permissive crate may not depend on a copyleft one.

## contributing

Contributions are inbound = outbound: a patch to an Apache-2.0 crate is
Apache-2.0, a patch anywhere else is AGPL-3.0-or-later, unless the patch says
otherwise. A CLA will be in place before the first external contribution is
merged, so that the commercial licence stays possible to grant.

## adding a crate

Every workspace member declares a `license` in its `Cargo.toml`, and
`crates/tidepool/tests/licensing.rs` fails if one doesn't, or if it is on the
wrong side of the split. A permissive crate also carries its own `LICENSE`
(Apache text) and `NOTICE`.
