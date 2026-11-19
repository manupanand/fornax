# LOG.md — Fornax decision log

Append-only. Never edit past entries; add a new entry that supersedes them.
Format: date · decision · alternatives · reasoning · evidence · consequences · status
(`proposed` / `accepted` / `superseded`).

---

## D-001 · 2026-11-19 · Write README before other project files
- **Decision:** Start with a README that states vision, status and principles.
- **Alternatives:** Start with CLAUDE.md/TASKS.md/LOG.md as the brief specified.
- **Reasoning:** User request; gives a public-facing anchor for the repo.
- **Evidence:** User instruction in session.
- **Consequences:** README links to files that are created later; all design details
  in it are labelled "proposed".
- **Status:** accepted

## D-002 · 2026-11-19 · Working memory = CLAUDE.md + TASKS.md + LOG.md
- **Decision:** Keep these three files at repo root and update them every session.
- **Alternatives:** Issue tracker (GitHub Issues/Projects); ADR directory (`docs/adr/`).
- **Reasoning:** Single repo, single maintainer, AI-assisted workflow; plain files are
  diff-able and always loaded into context. ADRs can be split out later if LOG grows.
- **Evidence:** Kickoff brief, Step 0.
- **Consequences:** LOG.md is append-only; TASKS.md is the source of truth for status.
- **Status:** accepted

## D-003 · 2026-11-19 · Rust as implementation language; Apache-2.0 for CE
- **Decision:** Rust; CE licensed Apache-2.0 (LICENSE already in repo).
- **Alternatives:** Go (ecosystem of Drone/Woodpecker/Gitea/Argo), AGPL/MIT for CE.
- **Reasoning:** Given by the brief. Apache-2.0 is permissive with patent grant,
  compatible with open-core; constrains what code we may reuse (no GPL/AGPL copying).
- **Evidence:** Kickoff brief; `LICENSE` (Apache 2.0, Jan 2004).
- **Consequences:** Research of GPL/AGPL/MIT-Expat projects is fine for ideas;
  copying code only from Apache-2.0/MIT/BSD-compatible sources, with attribution.
- **Status:** accepted

---

## D-004 · 2026-11-19 · Step 1 — DevOps plan (pre-research)
- **Status:** proposed (awaiting answers to Q1–Q5) → see D-005

### 1. Users and their jobs-to-be-done
| Persona | Scale | Cares about |
|---|---|---|
| Solo dev / homelab | 1 user, 1 machine | 5-minute install, low RAM, SQLite, zero external deps |
| Small team / startup | 5–50 users, 1–3 nodes | Git provider webhooks, PR pipelines, simple deploys to K8s or a VM, secrets, cheap to run |
| Platform team / enterprise | 100s–1000s users, many runners | SSO, RBAC, audit, HA, policy, air-gapped installs, compliance, cost control (EE target) |
| Day-2 operator (any scale) | — | See §2 |

### 2. What a day-2 operator cares about
- **Upgrades:** in-place binary swap, automatic forward-only DB migrations, N-1
  agent/server protocol compatibility, rollback guidance.
- **Backup/restore:** one command for DB + config + artifact index; documented RPO/RTO;
  artifacts in object storage backed up separately.
- **HA:** stateless server replicas behind a LB with Postgres; leader election for the
  scheduler/reconciler; agents reconnect and resume.
- **Secrets:** encrypted at rest (envelope encryption, rotatable key), pluggable
  external backends (Vault, cloud KMS) later; masked in logs.
- **RBAC & audit:** org/project/environment scopes; immutable audit log of every
  mutation and every deploy.
- **Observability:** Prometheus metrics (queue depth, job latency, runner utilisation,
  reconcile lag), OTel traces, structured JSON logs, health/readiness endpoints.
- **Air-gapped:** no runtime calls home; offline images/bundles; mirrorable registries.
- **Resource cost:** idle server < ~100 MB RSS (target, **assumption**); runners
  scale to zero; log/artifact retention policies.

### 3. Failure modes to design for
- Server crash mid-pipeline → jobs keep running on agents; state recovered from DB;
  no duplicate execution (lease + idempotent job claims).
- Agent loss → lease expiry, job marked lost, retry policy applies.
- DB unavailable → API degrades read-only/503; agents buffer logs locally.
- Webhook loss/duplication → idempotent event IDs + periodic poll fallback.
- Git provider outage → reconciler holds last-known-good, flags "source unreachable".
- Partial deploy / failed sync → health checks, automatic or manual rollback to last
  synced revision.
- Reconciler fights a human (drift loop) → drift reported, self-heal opt-in, backoff.
- Disk full (logs/artifacts/cache) → quotas, retention, alerts before exhaustion.
- Clock skew between nodes → leases based on server time, not agent time.

### 4. Security threat model (initial)
| Threat | Example | Planned mitigation |
|---|---|---|
| Untrusted PR code | Fork PR exfiltrates secrets | No secrets / no protected env for untrusted refs; approval gate to run fork PRs |
| Runner escape | Job breaks out of container to host or other jobs | Ephemeral, per-job isolation (rootless containers, optional microVM/K8s pod); no shared Docker socket by default |
| Secret leakage | Secrets echoed to logs, cached in artifacts | Masking, short-lived job tokens, secrets injected only into jobs that declare them |
| Supply chain | Malicious image/action/dependency, tampered artifacts | Pin images by digest, artifact checksums, SBOM + signing (cosign/Sigstore), provenance (SLSA) |
| Compromised agent | Stolen agent token used to pull other jobs | Per-agent identity, mTLS, scoped job tokens, capability labels, revocation |
| Control-plane abuse | Webhook spoofing, CSRF, over-privileged API tokens | HMAC webhook verification, scoped/expiring tokens, RBAC on every endpoint |
| GitOps privilege | Reconciler holds cluster-admin | Per-environment least-privilege credentials; pull-based agent in the target cluster |
| Insider / audit gaps | Silent changes to pipelines or deploys | Immutable audit log; protected branches/environments; approvals |

### 5. Success metrics
- Install-to-first-green-pipeline ≤ 10 minutes on a fresh VM.
- Idle server memory and binary size targets (to be set after benchmarks).
- Job dispatch latency p95 < 1 s from webhook to job start (warm runner) — **assumption**.
- Reconcile lag p95 < 30 s from commit to detected drift/sync — **assumption**.
- Zero known secret-exposure paths for untrusted PRs (security test suite).
- Upgrade between minor versions with zero manual DB steps.
- Community: external contributors and production users after beta.

### 6. Open questions (asked to user, 2026-11-19)
- **Q1 MVP scope:** CI-only first, or CI + minimal GitOps deploy in the MVP?
- **Q2 Topology:** single-node only until 1.0, or HA (Postgres + multiple servers)
  required for 1.0?
- **Q3 Execution/deploy targets:** Kubernetes only, or also Docker hosts, VMs / bare
  metal (SSH) for both runners and deploy targets?
- **Q4 Git hosting:** which providers first (GitHub, GitLab, Gitea/Forgejo,
  Bitbucket, generic Git)?
- **Q5 Auth:** in CE — local users only, or OIDC (and LDAP?) in CE, leaving SAML/SCIM
  for EE?

---

## D-005 · 2026-11-19 · Step 1 answers recorded; D-004 plan accepted with changes below
- **Decision:** D-004 plan accepted as the baseline, amended by D-006 … D-011.
- **Evidence:** User answers in session, 2026-11-19.
- **Status:** accepted

## D-006 · 2026-11-19 · MVP = CI + GitOps (Q1)
- **Decision:** The MVP includes both CI pipelines and a GitOps delivery path.
- **Alternatives:** CI-only MVP, GitOps added in beta.
- **Reasoning:** Combining CI and CD is the core differentiator; an MVP without CD
  would not validate the product thesis.
- **Consequences:** MVP scope is larger; GitOps in the MVP must stay minimal
  (one source path → one target, manual + auto sync, drift report, rollback to last
  synced revision). Progressive delivery stays out of the MVP.
- **Status:** accepted

## D-007 · 2026-11-19 · Single node now; durable DB-backed job state (Q2)
- **Decision:** Single-node deployment until further notice. CI jobs and runner-side
  jobs are persisted in the database (durable queue + job/lease records), not held in
  process memory.
- **Alternatives:** In-memory queue (simpler, loses work on crash); external broker
  (NATS/Redis/RabbitMQ) — rejected for single binary; HA in 1.0 — deferred.
- **Reasoning:** User answer. DB-backed jobs give crash recovery and an audit trail
  today and keep the HA path open (multiple servers can share the same DB queue later).
- **Consequences:** Scheduler uses a transactional DB queue (e.g. `SELECT … FOR UPDATE
  SKIP LOCKED` on Postgres; serialized writer on SQLite) — **design detail to confirm
  in Step 4**. HA is removed from the 1.0 must-haves until re-decided.
- **Status:** accepted

## D-008 · 2026-11-19 · Executors: host → Docker → Kubernetes (Q3)
- **Decision:** First executor runs jobs directly on the runner host (shell executor);
  Docker executor next; Kubernetes executor later. Deploy targets follow the same order
  (host/SSH → Docker/Compose → Kubernetes), **assumption to confirm in Step 4**.
- **Alternatives:** Container-first (stronger isolation from day one); K8s-first.
- **Reasoning:** User answer; host executor is the simplest path to a working MVP and
  matches the existing srfserver/Jenkins-style workflows.
- **Consequences / security:** A host executor offers **no isolation** between jobs or
  from the host. Mitigations required in the design: dedicated unprivileged runner user,
  per-job workspace dirs cleaned after each job, secrets passed via env/files only to
  jobs that declare them, and **host runners must not run untrusted (fork/external) PR
  code by default**. Threat model D-004 §4 updated accordingly. Executor trait must be
  designed so Docker/K8s slot in without changing the runner protocol.
- **Status:** accepted

## D-009 · 2026-11-19 · Scope change: Fornax includes its own Git hosting (Q4)
- **Decision:** Fornax is a Git hosting + CI/CD + GitOps platform (closer to GitLab /
  Gitea than to Jenkins/Argo). It also integrates with external Git hosts.
- **Supersedes:** CLAUDE.md non-goal "Not a Git hosting service" (removed).
- **Alternatives:** CI/CD only, integrating with external hosts (the original brief).
- **Reasoning:** User decision.
- **Consequences:** Much larger surface: repo storage, Git smart-HTTP/SSH transport,
  access control on repos, likely merge/pull requests, code review UI, protected
  branches. Gitea and gitlab-foss research (A-010, A-012) must now also cover the
  hosting side. **Open questions for Step 4:** which hosting features are in the MVP
  (bare repo hosting + push/pull only, or also merge requests, issues, wiki)? Use a
  Rust Git library (gitoxide / git2-rs) or shell out to `git`?
- **Status:** accepted (scope); hosting feature set = open

## D-010 · 2026-11-19 · First external Git integration: Google Cloud Source Repositories (Q4)
- **Decision:** First external integration targets GCP Cloud Source Repositories (CSR).
- **Evidence (verified 2026-11-19):** CSR has been **closed to new customers since
  2024-06-17**; organisations that used it before that date can keep using it and enable
  it on new projects in the same org. Google says a shutdown date will be announced at
  least one year ahead; Google's migration guide points to **Secure Source Manager (SSM)**
  as the successor. Sources:
  [CSR release notes](https://cloud.google.com/source-repositories/docs/release-notes),
  [migration guide](https://docs.cloud.google.cn/source-repositories/docs/migration-guides?hl=en),
  [community thread](https://discuss.google.dev/t/cloud-source-repositories-will-be-discontinued/154676).
- **Risk:** Building the first integration on a closed-to-new-customers service limits
  its value to other users. **Proposed mitigation:** design a provider-agnostic
  integration layer (clone/fetch over HTTPS with Google auth, change events via CSR
  Pub/Sub notifications or polling) so SSM / GitHub / GitLab / Gitea follow with little
  extra work. **Assumption (unverified):** SSM is built on Gitea, which would make a
  Gitea-compatible integration cover SSM — to verify in Step 2.
- **Status:** accepted (CSR first); provider abstraction = proposed

## D-011 · 2026-11-19 · Auth order: OIDC → LDAP → SAML → SCIM (Q5)
- **Decision:** OIDC login first, then LDAP, then SAML, then SCIM provisioning.
- **Alternatives:** Local users first; SAML-first for enterprise.
- **Reasoning:** User answer; OIDC covers Google, Keycloak, Entra ID, Okta, Dex.
- **Consequences:** Need a break-glass local admin for bootstrap/recovery
  (**proposed**). CE vs EE placement of LDAP/SAML/SCIM is still **open** (decide in
  Step 4 open-core boundary).
- **Status:** accepted (order); CE/EE placement = open
