# security policy

## report a vulnerability

**Do not open a public issue.**

Use the private vulnerability reports of GitHub:

1. Go to the [Security tab](https://github.com/niclasgrahm/tidepool/security).
2. Choose **Report a vulnerability**.

This opens a private thread that only the maintainers can see.

Include these items:

- what an attacker can do
- how to reproduce it
- the commit where you saw it
- what you published about it, if anything

tidepool has one maintainer. Expect a reply within a week. The time for a fix
depends on the problem. The fix credits you, unless you ask it not to.

## what is in scope

tidepool is a server that holds data, answers queries and serves a web UI. A
report is in scope when someone can do one of these things:

- reach an endpoint without the token that its access level requires
- write to a table, or do an operation, that their ingest credential does not
  permit
- reach an endpoint or a table through a listener that does not serve it
- read a token from a response, a log line, an error message or the web UI
- read or write a file outside the config directory and the data directory
- apply a `security` change without approval
- go past the limits of a public credential, or stop the server with requests
  that are in those limits

## what is not in scope

These items are documented behaviour. You can report a case that the
documentation does not cover. Otherwise, expect a reply with a link to
[access](https://propell.dev/tidepool/operating/access).

- **An access level without a token is open.** On an address other than
  loopback, tidepool does not start with ingest or admin open, unless you set
  `--insecure-open`. A read level without a token starts with a warning.
- **The admin level can change the server.** A person with the admin token, or
  who can write to the config directory, can change the model and remove data.
  The boundary is who gets that access.
- **`token: none` credentials.** Anyone who can reach the `public` listener can
  append rows to their tables. The limits are the only protection.
- **The limits are per process and approximate.** Requests of one credential
  that arrive at the same time can each see the same budget.
- **tidepool does not do TLS.** Terminate TLS in front of the server, for example
  in a load balancer.
- **Some paths are public.** These are the probes (`/healthz`, `/readyz`) and
  the descriptions of the software (`/v1/openapi.json`, `/v1/schemas/...`).
- **Advisories in the build tools of the doc site.** `website/` builds to static
  HTML. Its dev server does not run in CI or in production.

## supported versions

tidepool has no releases yet. Only the tip of `main` gets fixes.
