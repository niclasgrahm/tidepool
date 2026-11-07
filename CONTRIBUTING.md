# contributing to tidepool

tidepool is early and its surface changes often. These contributions help the
most now:

- bug reports
- issues that say "I tried to do X and I could not"
- small pull requests with one purpose

## before you write code

Open an issue first for a change that is not small. Much of the code has its
shape for a reason, and the code does not always show the reason.

- [docs/plan.md](docs/plan.md) is the design: the decisions, each subsystem and
  the milestones.
- [CLAUDE.md](CLAUDE.md) has the conventions of each crate.

## configure your machine

You need [Rust](https://rustup.rs), [`just`](https://github.com/casey/just), the
`wasm32-unknown-unknown` target and `trunk`:

```bash
rustup target add wasm32-unknown-unknown
cargo install trunk --locked
just dev       # the server on :7070, the UI with hot reload on :7080
```

The product does not use a node toolchain. Only the doc site (`website/`) uses
npm. `just` with no arguments lists all the recipes.

## the rules

These rules apply to every change.

**1. New behaviour ships with tests.** A new or changed endpoint, config field,
engine path or bug fix needs a test that fails without the change. If you cannot
test a change offline, say so in the pull request and give the reason.

[The testing guide](https://propell.dev/tidepool/contributing/testing) lists the
suites.

**2. `just ci` must pass.** It runs the format check, clippy with every warning
as an error, and the whole test suite.

Do not disable, `#[ignore]` or weaken an existing test to make it pass. If a test
encodes the wrong behaviour, talk about it in the issue first.

Clippy is strict: `all` and `pedantic`, plus `unwrap_used` and `expect_used`.
`clippy.toml` applies these to tests too. In a test, use `match` or `let ... else`
with `panic!`, and write a message that says what you expected.

**3. The oracle checks engine results.** A change to how tidepool computes or
maintains a result must keep the oracle suites green. A new query feature must
extend the generators, so that the oracle covers it.

## send a pull request

`main` moves only when a pull request is merged. CI must be green before the
merge.

```bash
git switch -c what-it-does
# work, with tests
just ci
git push -u origin what-it-does
gh pr create --fill
```

Write the commit message about the reason for the change. Make the first line
short and in the imperative. Put the reason and anything that you found in the
text after it.

These changes need one more step:

| you changed | what to do |
| --- | --- |
| a compiler diagnostic or a plan | run `cargo insta review`, and read each snapshot diff as user text |
| the web UI | run `just e2e`. It needs Chrome. |
| a doc comment, an endpoint, an MCP tool or the retail example | run `just docs`, and commit the result |
| any text for users | follow [landing/copy-style.md](landing/copy-style.md) |

## how contributions are licensed

tidepool is split. `tidepool-protocol` and `tidepool-config` are Apache-2.0.
Everything else is AGPL-3.0-or-later. [licensing.md](licensing.md) gives the
reason.

Contributions are inbound = outbound: a patch has the licence of the crate that
it goes into. Each contributor must also sign the
[contributor licence agreement](cla.md). With the CLA, tidepool can have a
commercial licence as well as the AGPL.

You keep the copyright in your work. You grant a licence. You do not assign the
work.

## report a security issue

Do not open a public issue. [SECURITY.md](SECURITY.md) gives the process.
