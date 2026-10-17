# GitOps: config from a git repository

Status: design draft, 2026-10-17. Not in the plan yet: when work starts, this
becomes plan §8.7 and a milestone, and this file goes away.

## The idea

Today the config directory is the source of truth on the server's disk.
Write-back keeps it equal to what runs, and when the directory is a git
repository, every applied change is committed there (`config::commit`). That
is git as a log, not as the source of truth.

GitOps turns it around: **the head of a branch in a remote repository is the
desired config, and the server converges on it.** It's the Flux/Argo model,
applied to a semantic model and dashboards:

- a pull request gets a check that shows the plan against the *live* server:
  what changes, its class, how many rows a rebuild touches;
- merging is the approval;
- the server fetches the new head and applies it through the same pipeline
  as every other change (`config::submit`);
- changes made in the UI, the dashboard editor or by agents become commits on
  that branch, so git stays the one history.

Nothing in the engine or store changes. It is a new *source* for the pipeline
in plan §8.1, next to startup, the directory watcher, the API and MCP.

## What the code has already

- `config::submit(state, ConfigChange, source)`: plan, policy, apply, record a
  version, under one lock, with optimistic concurrency on `base_version`
  (`Refused::Stale` → `409`).
- `config::look`: the watcher's logic. It reads a file set, works out the
  difference, auto-applies safe changes and queues the rest, replacing an
  earlier queued proposal from the same source. The git reconciler is the
  same function with a different reader.
- `ConfigChange { replace: true }`: the named files are the whole config,
  which is what "the tree at commit X" is.
- The pending queue with approve/reject, and `VersionInfo { author, source }`.
- `tidepool plan --config new/ --base old/`: a plan between two directories,
  offline.
- `POST /v1/config/plan`: a plan against the running server.

## Configuration

Where the repository is can't live in the repository, so it comes from the
command line or the environment:

```bash
tidepool serve --data ./data \
  --config-repo https://github.com/acme/analytics.git \
  --config-branch main \
  --config-path tidepool/          # the config is this directory in the repo
# credentials: TIDEPOOL_GIT_TOKEN (HTTPS) or GIT_SSH_COMMAND with a deploy key
# webhook:     TIDEPOOL_GIT_WEBHOOK_SECRET
```

How the server behaves is ordinary config, in `tidepool.yaml` in the repo,
so it changes through the pipeline like anything else:

```yaml
# fragment
git:
  poll: 30s                  # fetch this often; the webhook makes it immediate
  apply: [safe, rebuild]     # classes a merge applies by itself
  ui_changes: push           # push | off   (later: pull_request)
```

`--config <dir>` and `--config-repo` are exclusive. With a repo, the server
clones it into `<data>/git/`, and the directory watcher and write-back are
off: nobody edits that clone but the server.

## Inbound: from the branch to the server

```
webhook or poll tick
  → git fetch origin <branch>          (into <data>/git/, a bare clone is enough)
  → head unchanged since last look? stop
  → read the tree at head under <path> → Files
  → ConfigChange { replace: true, base_version: current, author: commit author }
  → submit(.., source = "git")
       invalid      → stay; report "head abc123 doesn't compile" with problems
       safe/rebuild → apply, if the class is in git.apply
       otherwise    → queue it (one queued change per source, as for "files")
  → remember head; record it in the version: VersionInfo.commit = "abc123…"
```

Rules:

- **Convergence, not events.** Each look compares the head's tree with what
  runs; it doesn't replay commits one by one. Ten commits merged during a
  rebuild arrive as one change. An apply that failed (`Busy` during a
  rebuild) is simply retried at the next look.
- **The server never goes back.** If head doesn't compile, or waits for
  approval, the server keeps running the last version it applied and says it
  is behind: `GET /v1/config` gets a `git` block (`head`, `applied`, `state`:
  `in_sync | behind_invalid | behind_needs_approval | fetch_failed`, the
  problems), and the config page shows it. A later commit that fixes things
  moves it on.
- **Destructive changes** wait in the queue by default, and approving them
  in the UI applies that head. A commit trailer such as
  `Tidepool-Approve: destructive` could count as approval for teams that
  review in PRs only; decide when building it.
- **Startup** is the same look, with `--allow-destructive` behaving as it
  does for a directory today.
- **Webhook**: `POST /v1/config/git/hook`, a new endpoint-table entry. It
  checks a shared secret (GitHub's `X-Hub-Signature-256` HMAC, GitLab's
  `X-Gitlab-Token`), ignores pushes to other branches and only triggers a
  look; it never trusts the payload's content. Its own `Access` (the
  secret, not a bearer token).

## Outbound: changes made in the UI and by agents

With `ui_changes: push`, a change submitted through the API is **pushed
first and applied after**:

```
submit(change) from the API / UI / MCP, with a repo configured
  → under the lock: plan it (invalid / needs approval → refuse, as today)
  → commit the merged files on top of the head the server last applied,
    author = the change's author, message = the plan summary
  → git push origin HEAD:<branch>     (fast-forward only, never force)
       accepted → apply it (it is now head) and remember the new head
       rejected → someone pushed meanwhile: fetch, run the inbound look,
                  and answer 409 Stale; the UI re-plans against the new
                  version, which it already does for a stale base_version
```

Git becomes the serialisation point and reuses the concurrency the pipeline
already has. If the server dies between push and apply, the next look finds
the commit at head and applies it: the state converges either way.

`ui_changes: off` makes the config pages and editors read-only, for teams
that want every change reviewed. A `pull_request` mode (the UI opens a PR
instead of pushing) needs a forge's API and waits for a later phase, but it's
the answer for protected branches, where the push above is always refused.
Until then, a refused push that isn't a fast-forward conflict is reported as
"the branch doesn't accept pushes: use `ui_changes: off`".

`renamed_from` hints: today write-back strips them after a rename is applied.
In GitOps the server doesn't rewrite the repository behind people's backs.
A stale hint is harmless (the catalog finds the new name first), except that
it becomes an error if someone later reuses the old name. Options: the
server pushes a follow-up commit removing it, or the compiler warns about a
hint whose rename is already applied. Decide when building it.

## Pull-request checks

This is the most useful part, and needs no server changes beyond what
exists. Grow the CLI:

```bash
tidepool plan --config tidepool/ \
  --server https://acme.tidepool.cloud --token "$TIDEPOOL_ADMIN_TOKEN" \
  --format markdown            # text | markdown | json
```

- Sends the directory as `ConfigChange { replace: true }` to
  `POST /v1/config/plan` at the server's current version, so the plan
  includes the real row counts and rebuild estimates.
- `--format markdown` is a PR comment: the change table, classes, warnings,
  dashboards affected, and problems with `file:line:col`.
- Exit codes a CI job can gate on: `0` valid (any class), `1` doesn't
  compile, `2` destructive (with `--fail-on destructive`).
- A small GitHub Action and a GitLab CI template wrap it, published with the
  doc site.

A plan needs the admin token today. A narrower "plan only" access level
may be worth it, so CI never holds a token that can apply.

## Implementation notes

- **git**: shell out to the `git` binary, like `config::commit` does. gix
  can't push, and libgit2 adds C to the build. The container image needs
  `git`. Pass HTTPS tokens through `GIT_ASKPASS` or
  `-c http.extraHeader`, never in the URL, and never log them.
- **State**: the last applied commit is part of each version in
  `<data>/config/` (`VersionInfo.commit`), so a restart knows where it was.
  The clone under `<data>/git/` is a cache: deleting it costs one clone.
- **Module**: `tidepool-server::git` (fetch, read tree, commit, push) and the
  reconciler loop next to `config::watch`; `look` split so the directory and
  git share the "difference → change → submit" half.
- **Protocol**: `VersionInfo.commit`, a `git` block in `ConfigState`, the
  webhook entry. `just docs` afterwards, and a site page for GitOps.

## Tests (offline)

All against a bare repository on disk (`file://`), no network:

- a commit pushed to the bare repo is applied, and its version records the
  commit and its author;
- a commit that doesn't compile isn't applied; the state says
  `behind_invalid` with the problem located; a fixing commit catches up;
- a destructive commit is queued; approving it applies it;
- several commits during a rebuild arrive as one change;
- a UI change appears as a commit on the branch with the change's author;
- a UI change racing a pushed commit gets `409`, and succeeds after
  re-planning;
- a restart resumes from the recorded commit without re-applying;
- the webhook refuses a bad signature and ignores other branches;
- `tidepool plan --server` with markdown output: insta snapshots, and the
  exit codes.

## Phases

1. **PR checks**: `tidepool plan --server … --format markdown`, exit codes,
   a GitHub Action. Small, useful with any setup, no server changes.
2. **Inbound reconciler**: `--config-repo`, poll and webhook, commit in the
   history, the `git` state in the API and the config page.
3. **Outbound push** for UI, editor and agent changes (`ui_changes: push |
   off`).
4. **Forge integration**: `pull_request` mode and commit statuses through a
   GitHub App. In the SaaS, the control plane owns that app (see
   [saas.md](saas.md)).
