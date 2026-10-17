# Tidepool as a paid SaaS: threads to follow up

Status: notes from a discussion, 2026-10-17. Nothing here is decided; it is a
list of threads to pick up later. When one becomes work, it gets a decision in
[plan.md](plan.md) §1 and a milestone, like everything else.

## The shape: a control plane and single-tenant cells

Keep D2 (single node) and D3 (single tenant) and build the service around
them, instead of making one process serve many tenants. The store, the writer
thread, the caches keyed by snapshot, the morsel executors and the config
directory all assume the process owns its machine; namespacing all of that
costs a lot and buys density we don't need yet.

```
                 ┌──────────────── control plane (separate, closed-source) ───────────────────┐
 signup/billing →│ accounts, orgs, users · billing · tenant registry · provisioning · API keys │
                 │ login (OIDC) → short-lived JWTs · metering · upgrades · backups             │
                 └───────────────┬────────────────────────────────────────────────────────────┘
                                 │ create / stop / upgrade / move
  acme.tidepool.cloud ──► edge proxy (wildcard TLS, WebSockets) ──► tenant cell
                                                                     ├─ tidepool serve (+ data volume)
                                                                     ├─ kayak sidecar (connectors)
                                                                     └─ checkpoints + WAL → object storage
```

The same binary runs dedicated, in the customer's cloud (BYOC) and on
premises, which may matter more than self-serve for industrial customers.

## Threads

### Where cells run

- [ ] Firecracker microVMs (Fly.io Machines or similar): a VM and a volume per
      tenant, stop/start, regions, wildcard routing, WebSockets. The fastest
      MVP: the control plane calls the Machines API.
- [ ] Kubernetes operator (StatefulSet + PVC per tenant): more operations
      work, but the operator doubles as the BYOC installer.
- [ ] Bin-packed own hosts: cheapest per GB of RAM, which is the main cost.

### Work in this repo

- [ ] **Memory budget, enforced.** `memory_budget` exists in `tidepool.yaml`
      and in plans, but nothing enforces it. Ingest past the budget must get a
      clear refusal (like the writer queue's `503`), never an OOM kill. It is
      also the natural pricing unit.
- [ ] **Checkpoints and WAL in object storage.** Segment files are immutable
      and the checkpoint file is written last, atomically: a good fit for S3.
      Restore = download the checkpoint, replay the WAL tail. Gives backups,
      moving a tenant, upgrades on fresh hosts and scale-to-zero. Decide the
      RPO per plan (WAL shipped every few seconds vs synchronous).
- [ ] **JWT auth** (already item 12 of "After v0.1"): `auth: jwt`, checked
      against the control plane's JWKS; claims carry the tenant and the
      `Access` level. Login on the control plane, redirect to the tenant with
      a one-time code that becomes today's `HttpOnly` session. Ingest keys
      stay bearer tokens, but many, hashed, revocable, managed by the control
      plane.
- [ ] **GitOps**: config from a git repository. See [gitops.md](gitops.md).
- [ ] **Lifecycle**: graceful drain (stop ingest, final checkpoint, exit),
      `/readyz` that says "recovering" during replay, boot from env or a
      template (`init` already embeds the retail example).
- [ ] **Metering**: the counters billing needs, on `/metrics`, scraped by the
      control plane.
- [ ] **CPU fairness**: rayon and the morsel executors must follow the
      cgroup CPU quota, not the host's core count.
- [ ] **Format versions**: WAL and checkpoint formats versioned explicitly,
      with a test that recovers every older version, before real customer
      data exists.

### Product mechanics

- [ ] Free tier and trials by scale-to-zero: stop a cell after N minutes
      without ingest or sessions; the proxy holds the next request while it
      restores from its checkpoint.
- [ ] Pricing on rows or GB held, plus connectors; editor seats; viewers
      free ("put it on the factory TV").
- [ ] Upgrades in cohorts, canaries first. Zero-downtime handover (start the
      new version, catch up, swap) later.
- [ ] Kayak as a sidecar per tenant; a shared Kayak is harder than it looks.

### Licensing

- [ ] A CLA (or DCO plus assignment) before outside contributions, so the
      commercial licence (D13) stays sellable.
- [ ] The control plane in its own closed repository, talking to Tidepool
      only over the documented API; `licensing.rs` guards this side.

### Phases

1. Managed single-tenant by hand: a provisioning script, a few design
   partners, static tokens.
2. Control plane MVP: signup, billing, provisioning API, wildcard
   subdomains, OIDC → JWT, checkpoints in S3.
3. Scale-to-zero, regions, GitOps, managed connectors, metered billing.
4. BYOC and on-premises: the operator, licence keys, air-gapped installs.

### Open questions

- Who buys first: self-serve developers (signup, free tier, scale-to-zero
  first) or industrial operations (EU residency, SSO, BYOC first)?
- Does a customer need several models (dev/staging/prod, one per site)? Then
  a tenant is an org with several cells, and the registry should say so from
  the start.
- What RPO can we promise at launch?
