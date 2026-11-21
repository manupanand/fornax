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

---

## D-012 · 2026-11-19 · Step 2 research completed (A-010 … A-016)
- **Decision:** Research notes written to `docs/research/` (gitlab-foss, jenkins, gitea,
  others, job-queues, gcp-source, comparison). Repos shallow-cloned to
  `~/code-personal/fornax-research/` (outside the repo, not committed). Licenses checked
  before reading; all MIT or Apache-2.0. GitLab `ee/`/`jh/` absent from clone; a fixture
  dir named `ee` was not read. GitLab `doc/` is CC BY-SA → no prose copied.
- **Evidence:** `THIRD_PARTY.md` (commit hashes), `docs/research/*.md` (file citations).
- **Consequences:** Conclusions D-013 … D-018 below are **proposed** inputs to Step 4,
  not final architecture.
- **Status:** accepted

## D-013 · 2026-11-19 · Target shape validated by Gitea and Harness OSS
- **Finding:** Two Go products already ship Git hosting + CI in one binary on SQLite
  (Gitea; Harness OSS, the successor of Drone — `harness/drone` README).
- **Consequence:** Fornax must differentiate on Rust performance/footprint, **built-in
  GitOps reconciliation** (neither has it), and secure-by-default runner trust.
- **Status:** proposed

## D-014 · 2026-11-19 · Queue: DB truth + conditional-UPDATE claim + leases + in-process notify
- **Alternatives:** GitLab (Redis locks + optimistic locking), Woodpecker (in-memory FIFO
  mirrored to DB), Harness (global mutex), Jenkins (in-memory push), `FOR UPDATE SKIP
  LOCKED` only (Postgres-only).
- **Reasoning:** Portable to SQLite + Postgres, no Redis, crash-safe (D-007), HA-ready.
  Details in `docs/research/job-queues.md`.
- **Status:** proposed

## D-015 · 2026-11-19 · Runner protocol: small versioned gRPC, agent-initiated, Woodpecker-style verbs
- **Alternatives:** GitLab REST long-poll (simple, proven), Gitea Connect RPC + version
  counter, Jenkins Remoting (rejected: Java serialization / code shipping, CVE history).
- **Reasoning:** gRPC over the HTTPS port (tonic) gives streaming logs and a typed
  contract; verbs `Register, Next (long-poll), Init, Extend (lease), Update, Log, Done`.
  REST fallback can be decided in Step 4.
- **Status:** proposed

## D-016 · 2026-11-19 · Pipeline spec: Fornax-native typed YAML; no server-side scripting
- **Alternatives:** GitHub Actions compatibility (Gitea), Groovy DSL (Jenkins), GitLab CI
  compatibility.
- **Reasoning:** A typed, versioned schema (serde + exported JSON Schema) is easier to
  validate and secure; Jenkins' CPS shows the cost of a server-side language. Importers
  for GitLab/GH Actions syntax can come later. Parity candidates go to Step 3.
- **Status:** proposed

## D-017 · 2026-11-19 · GitOps model: Argo CD state model + Flux artifact/inventory model, non-K8s first
- **Adopt:** desired/live/diff/health, sync phases + hooks, history-based rollback, opt-in
  prune/self-heal (Argo CD); source → artifact → apply, inventory-based prune, `dependsOn`,
  source signature verification (Flux).
- **Different:** targets are host → Docker/Compose → Kubernetes (D-008), so health and
  diff must be pluggable per target type.
- **Status:** proposed

## D-018 · 2026-11-19 · Security defaults from prior art
- Fork/untrusted PRs: no secrets + maintainer approval (Gitea `models/secret/secret.go`,
  `services/actions/approve.go`; GitLab fork MR pipelines).
- Job token with permission ceiling/clamping (Gitea `token_permission_design.md`).
- "Trusted project" gate for privileged features (Woodpecker security-boundaries doc).
- Host executor = no isolation; trusted refs only (GitLab Runner security doc, Woodpecker
  local backend doc) — confirms D-008 mitigations.
- No `pull_request_target`-style "secrets with untrusted code" event.
- **Status:** proposed

## D-019 · 2026-11-19 · Git layer: `RepoStore` trait, git CLI first, gitoxide for hot reads
- **Evidence:** Gitea (`modules/git/gitcmd`) and Harness (`git/command/command.go:149`)
  both shell out to `git`; GitLab isolates Git in Gitaly.
- **Reasoning:** `git` CLI is complete and battle-tested for merges/rebases/pack
  negotiation; gitoxide avoids process spawn on read-heavy paths. Trait keeps a
  Gitaly-like split possible.
- **Status:** proposed

## D-020 · 2026-11-19 · GCP integration findings
- CSR change events come via **Pub/Sub** (JSON or protobuf), not webhooks; no PR or
  commit-status API (**assumption**). SSM uses HTTP webhooks with secret or Google-signed
  OIDC token. "SSM is built on Gitea" remains **unverified** — not used as a design input.
- **Proposed:** CSR provider = Pub/Sub subscriber + `ls-remote` polling fallback; SSM as
  second provider sharing GCP auth. Recommendation recorded for user decision A-005.
- **Evidence:** `docs/research/gcp-source.md`.
- **Status:** proposed

---

## D-021 · 2026-11-19 · Step 3 — rust-anything used only as a parity oracle
- **Decision (proposed):** Use rust-anything (MIT, @ `9029714`) only to record
  characterization corpora (`cases.jsonl`) from a few existing implementations, in scratch
  copies outside the repo. Fornax code keeps its own API; the generated crate's `port.rs` is a
  thin adapter over the Fornax crate so `diff`/`gate` can prove parity. Only the corpora are
  committed, replayed by plain `cargo test`. No Go/Ruby/Java at Fornax build time.
- **Alternatives:** (a) don't use it — hand-write golden tests (cheaper, but misses edge cases
  its trap generator finds); (b) use it as intended — keep the originals' APIs via bridges
  (rejected: Fornax is original design, and bridges would add cgo/Ruby/JVM dependencies).
- **Evidence:** `docs/research/rust-anything.md` §1–2.
- **Consequences:** needs `cargo install rust-anything` + a Go toolchain in WSL (user-level,
  no sudo). Do not run `rust-anything setup` (it edits agent configs) unless asked.
- **Status:** proposed — awaiting approval

## D-022 · 2026-11-19 · Step 3 — proposed parity units
- **U1** cron parse + next-fire: robfig/cron v3 via Gitea `models/actions/schedule_spec.go` (MIT)
- **U2** branch/tag/path filters: Gitea `modules/actions/workflowpattern` (MIT); optional
  **U2b** artifact globs: `bmatcuk/doublestar/v4` (MIT)
- **U3** variable expansion: **U3a** gitlab-runner `common/spec/variables.go` → Go `os.Expand`
  (MIT / BSD-3) and **U3b** gitlab-foss `lib/expand_variables.rb` (MIT); record both, then
  choose Fornax semantics
- **U4** log secret masking: gitlab-runner `common/buildlogger/internal/masker` (MIT)
- **U5** (deferred to K8s phase) health assessment: Argo CD `gitops-engine/pkg/health` (Apache-2.0)
- **Rejected:** pipeline YAML parsing, DAG ordering, GitLab `rules:` expressions, Jenkins
  `CronTab`, and all core components (runner protocol, scheduler, reconciler, API, UI).
- **Evidence:** `docs/research/rust-anything.md` §3–4.
- **Status:** proposed — awaiting approval (nothing recorded or run) → see D-025

---

## D-023 · 2026-11-19 · Web UI (incl. Git hosting UI) in Svelte
- **Decision:** The web UI — repository browsing, diffs, pipelines, deployments, admin — is
  built with **Svelte** (SvelteKit vs plain Svelte + Vite to be decided in Step 4).
- **Alternatives:** server-rendered templates (Gitea-style Go templates / Askama), React,
  Leptos/Yew (Rust WASM).
- **Reasoning:** User decision.
- **Consequences:** UI is a separate frontend talking to the REST API; built assets are
  **embedded into the single binary** (e.g. `rust-embed`) to keep "one binary" (D-003
  principle). The API must be complete enough for the UI (no server-only HTML paths). Open in
  Step 4: SSR or SPA, and how the build is wired into `cargo build`.
- **Status:** accepted (Svelte); build/SSR details = open

## D-024 · 2026-11-19 · Pipelines are YAML only, stored in `.fornax/` in each repository
- **Decision:** CI **and** CD configuration is declarative **YAML only** — no Groovy or any
  server-side scripting language (confirms D-016). Fornax reads pipeline config from a
  **`.fornax/` directory at the repository root**, like `.github/workflows/`: several YAML
  files, each one a pipeline/workflow.
- **Note:** the user wrote `.fronax`; recorded as `.fornax` (project name) — **confirm**.
- **Proposed layout (to settle in Step 4):**
  `.fornax/pipelines/*.yaml` (CI) and `.fornax/deployments/*.yaml` (CD/GitOps), or a flat
  `.fornax/*.yaml` with a `kind:` field per file.
- **Alternatives:** single root file (`.gitlab-ci.yml` style); Groovy/Jenkinsfile (rejected).
- **Consequences:** discovery = list `.fornax/**/*.y{a,}ml` at the commit being built; each
  file validated against a versioned JSON Schema; scripts only run inside job steps on
  runners.
- **Status:** accepted (YAML only, `.fornax/`); file layout = open

## D-025 · 2026-11-19 · rust-anything approved; scope widened to Gitea / GitLab / Jenkins features
- **Decision:** U1–U4 approved. The user asked to use rust-anything wherever Fornax needs
  functionality similar to Gitea, GitLab or Jenkins, for both architecture input and code.
- **How (unchanged from D-021):** unit-level only — record the original's behaviour, then
  write Fornax's own Rust (own API) and prove parity with `diff`/`gate`. **No wholesale port**
  of any project (kickoff rule); core components stay original design.
- **Added candidates** (details in `docs/research/rust-anything.md` §3b): U6 unified-diff
  parsing (Gitea `services/gitdiff`), U7 ANSI job-log → structured lines/sections (GitLab
  `Gitlab::Ci::Ansi2json`), U8 CODEOWNERS parsing (Gitea `models/issues/code_owner.go`),
  U9 Git ref-name validation (Gitea `modules/git/ref.go` and `git check-ref-format`),
  U1b Jenkins `H` cron token (`hudson/scheduler/CronTab.java`, optional).
- **Architecture use:** run `rust-anything analyze` (static analysis; does not execute code)
  on a **copy** of Gitea to get its unit map and Go-dependency → Rust-crate mapping, as input
  to the Fornax crate layout in Step 4.
- **Tooling:** `cargo install rust-anything` (user-level). Go toolchain to be installed under
  `~` before recording Go units. `rust-anything setup` still **not** run.
- **Status:** accepted

## D-026 · 2026-11-19 · Backend and engine are Rust only; frontend may use other languages
- **Decision:** Every backend component — API, Git hosting, scheduler, runner agent,
  GitOps engine, CLI — is written in Rust. Other platforms are **inspiration only**. Only the
  web frontend (Svelte, D-023) uses another language.
- **Clarification on external tools:** the server invokes the system **`git` binary** for
  wire transport (`upload-pack`/`receive-pack`) and complex writes (D-019), the same way
  Gitea and Harness do. This is an external runtime tool, not backend code in another
  language. Plan to move hot read paths to gitoxide (pure Rust). **If you want zero
  non-Rust runtime tools, say so** — server-side pack negotiation in pure Rust is not mature
  in gitoxide today (**assumption**, to verify), so it would be a large extra work item.
- **Consequence for rust-anything (D-025):** parity corpora may be recorded from Go/Ruby/Java
  originals, but nothing from those languages ships or is linked.
- **Status:** accepted

## D-027 · 2026-11-19 · Config directory `.fornax/` confirmed
- Confirms D-024 (resolves A-006). Layout proposed in `docs/architecture.md` §5.
- **Status:** accepted

## D-028 · 2026-11-19 · Build order: GitHub/GitLab-like hosting platform first, then CI, then CD
- **Decision:** Milestone M1 = Git hosting core (users, tokens, repositories, Git smart
  HTTP clone/push, browse API, audit). CI (M2) and GitOps (M3) build on it. Coding starts now
  (user instruction), with `docs/architecture.md` as the reference.
- **Resolves A-004 partly:** hosting is GitHub/GitLab-like. M1 = repos + push/pull +
  browse; pull requests, reviews and issues follow in M4 (order proposed in TASKS.md).
- **Status:** accepted

## D-029 · 2026-11-20 · M1 implementation choices
- **sqlx with runtime-checked queries** (not `query!` macros) so building needs no live DB;
  SQLite migrations in `crates/fornax-db/migrations/sqlite/`, Postgres set added later.
- **Case-insensitive names** via `lower_name` unique columns (Gitea pattern), portable to
  Postgres.
- **Tokens**: `fnx_` + 32 random bytes (hex), SHA-256 stored; Bearer for API, Basic
  `user:token` for git. Invalid credentials → 401, never silently anonymous.
- **Access model M1**: owner/admin write, public read; private repos 404 to other users and
  401 to anonymous callers (so git offers to log in).
- **git safety**: revisions resolved via `rev-parse --verify --end-of-options`; later calls
  only receive resolved hex IDs; tested with `--output=…` / `--all` revisions.
- **Smart HTTP**: `upload-pack`/`receive-pack --stateless-rpc`; protocol v0/v1 only; request
  body buffered with a 1 GiB limit (H-009 streaming, H-010 v2 are follow-ups).
- **Raw files** always `application/octet-stream` + `nosniff` (no stored XSS from repo content).
- **Evidence:** 15 tests incl. real clone/push in `crates/fornax-api/tests/http.rs`;
  `clippy -D warnings` clean; manual smoke test of the binary.
- **Status:** accepted

## D-030 · 2026-11-20 · License header, RULES.md, CONTRIBUTING.md, Docker for local testing
- **Decision:** Every source file starts with the 3-line Apache header ("Copyright 2026
  Manu P Anand …"), enforced by `scripts/license-headers.sh` (check / `--fix`) for `.rs`,
  `.sql`, `.sh`, `.ts`, `.svelte`, `.css`, `.html`, `Dockerfile`, `compose.yaml`. Project rules
  in `RULES.md`; contributor workflow in `CONTRIBUTING.md` (standard file name; the user asked
  for "contributions.md"), DCO sign-off, Conventional Commits.
- **Docker:** multi-stage `Dockerfile` (web → build → runtime | test), runtime as uid 10001
  with healthcheck; `compose.yaml` binds HTTP 3000 and SSH 2222 to 127.0.0.1 only; test
  profile runs the suite in a container. Docker parser directive dropped because the header
  must come first (BuildKit cache mounts work without it on Docker 29).
- **Status:** accepted

## D-031 · 2026-11-20 · M1 completion design (H-009 … H-016)
- **Organisations** are rows in `users` with `kind = 'org'` (one name space, Gitea pattern);
  repos keep a single `owner_id`. Orgs can't hold tokens/sessions/keys.
- **Permissions:** `none < read < write < admin`; effective = max(site admin, owner,
  org role [owner→admin, member→read], team grants, collaborator grant, public→read),
  computed in one SQL `UNION ALL` (`fornax-db::repos::permission`). 404 hides private repos
  from signed-in users without access; 401 for anonymous; 403 when visible but not allowed.
- **Streaming git HTTP:** body → `StreamReader` → optional `GzipDecoder` → `take(cap+1)` →
  git stdin. Over-cap requests close stdin so git aborts without writing refs (tested).
- **Protocol v2:** `Git-Protocol` header validated and forwarded as `GIT_PROTOCOL`; no
  `# service=` preamble for v2 (same as git http-backend).
- **OIDC:** hand-rolled code flow (reqwest + jsonwebtoken 9) rather than the
  `openidconnect` crate — smaller surface, easier to audit; PKCE S256, single-use state
  (hashed in DB, 10 min), nonce check, issuer/audience/exp validation, RS*/PS256/ES*/HS256
  only. Usernames derived from `preferred_username`/email, made valid and unique.
- **Sessions:** random 32-byte id in an HttpOnly, SameSite=Lax cookie (Secure on https);
  SHA-256 stored; 7-day TTL; hourly pruning. CSRF: state-changing endpoints need JSON
  bodies or non-simple methods (CORS preflight) and the server never sends CORS headers.
  **Revisit** if a form-encoded endpoint is ever added.
- **SSH:** russh 0.65 in `fornax-ssh`; Ed25519 host key generated in Rust on first start
  (`data/ssh_host_ed25519_key`, 0600); only `git-upload-pack`/`git-receive-pack` with a
  strictly parsed path; key fingerprints in OpenSSH `SHA256:` format.
- **Backup:** directory format (manifest, `VACUUM INTO` DB copy, repos, host key); restore
  only into an empty data dir and verifies every repo exists.
- **UI:** Svelte 5 + Vite 8 SPA, history-API router mirroring GitHub URLs, no UI library;
  embedded with rust-embed (`allow_missing`, so Rust builds without Node). Token sign-in
  keeps the token in `sessionStorage` only. Strict CSP. README shown as plain text until
  H-019.
- **Tooling installed (user-level, no sudo):** Node 24 LTS in `~/.local/node` (SHA-256
  verified against nodejs.org SHASUMS256.txt).
- **Evidence:** 42 tests (unit + integration: HTTP/SSH git with real `git`/`ssh`, mock
  OIDC IdP, backup round trip, UI), all passing natively and in the Docker test image;
  `clippy -D warnings`, `fmt`, header check, `svelte-check` clean; UI exercised in a browser
  (browse tree/file/commits, branch with `/`, token sign-in, create repo, add SSH key then
  clone with it).
- **Status:** accepted

## D-032 · 2026-11-20 · Multi-agent workflow in Claude Code
- **Decision:** Independent work is deployed as parallel Claude Code subagents from
  `.claude/agents/` (`fornax-backend`, `fornax-frontend`, `fornax-parity`,
  `fornax-researcher`, `fornax-reviewer`, `fornax-tester`); the main session orchestrates.
  Rules in `CLAUDE.md` ("Multi-agent workflow") and `RULES.md` §6: one writer per file set,
  worktrees for parallel writers, orchestrator alone edits shared files (LOG/TASKS/CLAUDE,
  root Cargo.toml/lock, migration numbers), verification by `fornax-tester` (+ reviewer).
- **Alternatives:** single agent only (simpler, slower for M2's independent crates);
  free-form agents without definitions (inconsistent rules, conflicting edits).
- **Reasoning:** User request. M2 has several independent crates (spec, proto, agent) and
  parity units that parallelise cleanly; shared-file ownership avoids merge conflicts.
- **Consequences:** briefs must be self-contained; read-only agents use `sonnet` to save cost.
- **Status:** accepted

## D-033 · 2026-11-20 · M2 execution plan (multi-agent)
- **Parallel, disjoint ownership:** A = new crate `fornax-spec` (C-001/C-002/C-009);
  B = `fornax-db` CI module + migration `0004_ci.sql` (C-004); C = new crates
  `fornax-proto` + `fornax-agent` (C-005/C-006). Orchestrator then does C-003 (push →
  pipeline) and the runner RPC server, then tester + reviewer.
- **Isolation without commits:** M1 is uncommitted (user hasn't approved a commit), so
  agents work in full copies under `~/code-personal/fornax-wt/<task>/` with their own
  `target/` and merge back only the paths they own; root `Cargo.toml` additions are merged
  by the orchestrator.
- **Status:** accepted

## D-034 · 2026-11-20 · `if:` expression language (C-002)
- **Decision (proposed default, user may override):** a small, side-effect-free expression
  language inside `${{ … }}` or bare in `if:`: string/number/bool literals, context paths
  (`event`, `ref`, `branch`, `tag`, `sha`, `repo`, `vars.NAME`), `== != && || !`, parentheses,
  and functions `startsWith`, `endsWith`, `contains`, `matches` (glob). Evaluated server-side
  in `fornax-spec`; no loops, no user-defined functions, bounded size.
- **Alternatives:** CEL (`cel-interpreter` crate: powerful, larger surface, less familiar to
  CI users); GitLab `rules: if` syntax (`$VAR == "x"`, regex) — familiar to GitLab users but
  the variable-expansion semantics differ between server and runner (see U3).
- **Reasoning:** GitHub-Actions-style subset is the most widely known; a hand-written parser
  keeps it Rust-only, auditable and fuzzable.
- **Status:** proposed

## D-035 · 2026-11-20 · UI design language "liquid glass" + collapsible file tree (H-022)
- **Decision:** The web UI uses a translucent "liquid glass" design language: chrome (header,
  toolbar, sidebar, panels) is blurred, translucent glass over a soft colour backdrop, with a
  specular top edge, soft shadows, rounded concentric shapes and capsule controls
  (`.glass`, `.box`, `.segmented`, pill buttons in `web/src/app.css`). Solid surfaces for
  `prefers-reduced-transparency` or without `backdrop-filter`; no motion for
  `prefers-reduced-motion`. No third-party UI kit, no brand assets; icons are our own inline SVG.
- **Repository view:** collapsible sidebar with a lazy file tree (`FileTree.svelte`): folders
  load on first expand, ancestors of the current path open automatically, active entry
  highlighted. Toggle in the repo toolbar (aria-expanded/aria-controls), remembered in
  localStorage `fornax.sidebar` on wide screens; below 900px an overlay drawer with a scrim
  that closes on navigation and Escape. Hidden on the commits view.
- **Gotcha:** an ancestor `transform` (e.g. an entrance animation) makes `position: fixed`
  children position relative to it; the repo page container must not animate transform.
- **Evidence:** svelte-check 0/0, build, header check, `cargo test -p fornax-api --test ui`;
  browser-checked light/dark/375 px (separate session, branch claude/hopeful-clarke-d4fd52).
- **Status:** accepted

## D-036 · 2026-11-20 · CI queue implementation (C-004) merged
- **Built by agent B** in a copy, merged by the orchestrator: migration `0004_ci.sql`
  (runners, pipelines, jobs, job_needs, job_attempts, job_logs), `fornax-core::ci` (statuses,
  transitions, `pipeline_result`, `labels_match`, `validate_dag`), `fornax-db::ci` (claim,
  leases, retries, DAG promotion/skip, cancel, chunked idempotent logs, runner registry).
- **Design choices (agent):** no separate queue table (`jobs.status` + index); label subset
  check in Rust over batches of 64, trust filter in SQL; leases valid only while
  `lease_expires_at >= now` (late finishes rejected even before reaping); cancel turns a
  running job's non-success into `canceled` without retry; pipeline result precedence
  cancel > failed > canceled > success; columns `git_ref`/`byte_offset` avoid SQL keywords.
- **Orchestrator fix after merge:** the concurrency test failed 1 in 6 runs on the main repo
  under CPU load (`SQLITE_BUSY` after 5 s): SQLite's busy handler is unfair, so a waiting
  `BEGIN IMMEDIATE` could starve. CI write transactions now also queue on a per-process FIFO
  `tokio::sync::Mutex` (`ci::WriteTx`), and `busy_timeout` is 15 s for cross-process writers.
  Evidence: 10/10 passes at load average up to 14.
- **Follow-ups:** `DbError::Invalid` for DAG errors (now mapped to Conflict/NotFound);
  `append_log` must be checked against the authenticated runner in the RPC layer; Postgres
  needs a counter row for per-repo numbers; waiting reasons approval/resource/no_runner.
- **Status:** accepted

## D-037 · 2026-11-20 · `.fornax/` spec crate (C-001, C-002, C-009) merged
- **Built by agent A**, merged: `fornax-spec` (pure library): typed Pipeline/Deployment
  model, `load()` with file:line:col diagnostics (many per file), validation, matrix
  expansion (≤ 256, `include`/`exclude` not yet), Kahn topological order, `if:` expression
  language, GitHub-style trigger patterns, UTC cron, hand-written JSON Schema.
- **YAML:** `saphyr-parser` 0.3 (MIT/Apache, YAML 1.2, event API → own positioned tree), so
  `on:` stays a string and every node has a position. Rejected `serde_yaml` (archived),
  `serde_yml` (deprecated), `serde-saphyr` (rc, stops at first error). Limits: 1 MiB/file,
  100k nodes after aliases, depth 64, one document, duplicate keys rejected.
- **Expressions (implements D-034, now accepted):** case-sensitive string comparison (refs
  are case-sensitive, unlike GitHub), GitHub-style truthiness/coercion, unknown root names
  are parse errors, `matrix.NAME` added, 4 KiB / depth 64.
- **Patterns:** Thompson-NFA matcher (no backtracking blow-up); `?`/`+` apply to the preceding
  atom, `{a,b}` supported. Known divergences from Gitea to settle with the U2 corpus:
  `**/README.md` does not match `fooREADME.md`; `!` inside `*-ignore` lists uses last-match-
  wins. Triggers: `branches` + `branches-ignore` together is an error; pushes without path
  info pass path filters.
- **Cron:** robfig semantics incl. DOM/DOW OR rule; no seconds/`@every`/`TZ=`/`H`.
- **Status:** accepted

## D-038 · 2026-11-20 · Runner protocol + agent (C-005, C-006) merged
- **Built by agent C**, merged: `fornax-proto` (tonic 0.14 + `tonic-prost`, compiled with
  `protox` 0.10 — no `protoc`), service `fornax.runner.v1.Runner` with `Register, Next,
  Extend, Update, Log (client stream), Done`; additive-only compatibility rules. `Init` from
  D-015 dropped (job payload arrives with `Next`).
- `fornax-agent` library: capacity-limited loop, heartbeats (CANCEL → Done CANCELED, LOST →
  ERROR), ≤ 64 KiB log chunks acked by offset, Done retried until accepted, backoff with
  jitter, graceful shutdown.
- **Host executor:** per-attempt workspace removed afterwards; env cleared to a small
  allow-list (runner token never reaches jobs); job token passed to `git fetch` only via
  `GIT_CONFIG_*` env (not argv); process-group kill (SIGTERM → SIGKILL via `nix::killpg`, no
  `unsafe`); leftover group processes are killed at step end.
- **Masking:** union of all phrase occurrences becomes one `[MASKED]` per run, independent of
  chunk splits (proptest); intentionally differs from GitLab's stacked maskers on
  overlapping phrases — U4 corpus will document it.
- **Boundary exception accepted:** `fornax-agent` spawns `git` itself (it runs on runner
  machines, not the server, and must not depend on `fornax-git`). Rule updated to "only
  `fornax-git` spawns git on the server".
- **Status:** accepted

## D-039 · 2026-11-20 · Push → pipeline (C-003) merged and wired; CI read API
- **Built by agent D**, merged: `fornax-ci` (`diff_refs`, `on_push`, versioned `JobPayload`
  v1) and `fornax-git` additions (`snapshot_refs`, `changed_paths`, `fornax_files`).
  Semantics: jobs whose `if:` is false are left out together with their transitive
  dependents (no pipeline if nothing is left); spec errors never fail a push; new refs/tags
  have no path info (path filters pass); ≤ 20 refs per push, ≤ 20 pipelines per ref; push
  pipelines are `trusted`, `max_attempts = 1`, `labels = runs-on`.
- **Orchestrator wiring:** `fornax_ci::{before_push, after_push, Wakeup}`; HTTP
  `receive-pack` (fornax-api) and SSH (`fornax-ssh`) snapshot refs before the push and call
  `after_push` after git succeeds, then wake runners. Known limit: concurrent pushes to one
  repo may attribute updates to the other push (each update is still processed); a
  `post-receive` hook would make it exact (follow-up).
- **CI read API** (orchestrator): `GET …/pipelines`, `…/pipelines/{n}` (jobs + attempts +
  needs; job payload hidden), `…/pipelines/{n}/jobs/{id}/log?from=&attempt=` (raw, masked,
  `X-Fornax-Next-Offset`), `POST …/pipelines/{n}/cancel` (write). `Db::pipeline_by_number`.
- **Evidence:** fornax-ci 14 tests, `tests/ci.rs`, `tests/push_ci.rs` (real `git push`).
- **Status:** accepted

## D-040 · 2026-11-20 · Runner server merged; M2 runs end to end
- **Built by agent E**, merged: `fornax-runner` (tonic `Runner` service mounted on the HTTP
  port via h2c — `route_service("/fornax.runner.v1.Runner/{*rest}")` because tonic's
  `into_axum_router` adds a fallback that conflicts with ours), long-poll `Next` (≤ 25 s,
  `Notify` enabled before each claim so wake-ups aren't missed), leases 60 s, reaper every
  15 s, per-attempt job tokens `fnj_…` (migration `0005_job_tokens.sql`, hashed, revoked on
  Done/expiry), `Update` kept in memory, `Log` checks attempt ownership, `Register` with a
  shared registration token (runners always `trusted_only`). CLI: `fornax admin
  create-runner NAME --labels … [--allow-untrusted]`, `fornax agent --server --token
  --work-dir [--capacity]`.
- **Job principal:** HTTP Basic `fornax-job:fnj_…` gives read-only git fetch on exactly the
  job's repository while the attempt runs; never push, never the REST API.
- **Orchestrator merge work:** three-way merge of `fornax-server` main.rs/Cargo.toml;
  applied E's githttp hook (`&user`) and re-enabled the real-git job-token test; one shared
  `QueueNotifier` wakes runners from HTTP and SSH pushes (`fornax_ci::Wakeup`).
- **Bug found by the end-to-end test:** `fornax-runner` decoded payloads with a private mirror
  of the format that rejected `"timeout_secs": null` written by `fornax-ci`, so every job
  failed with "invalid job payload". Fix: the runner now decodes with
  `fornax_ci::JobPayload` (single definition) — lesson for multi-agent work: shared wire
  formats must live in one crate both sides import, not be mirrored from a brief.
- **M2 acceptance (binary-level test `crates/fornax-server/tests/e2e.rs`):** `fornax serve` +
  `fornax agent`; `git push` of `.fornax/ci.yaml` with `build → test`; agent checks out via
  job token, runs both jobs, logs readable via API, job token masked, workspace cleaned.
- **Status:** accepted

## D-041 · 2026-11-21 · M2 review fixes, part 1 (S2, S3, S4, C6, S6) + Docker init
- **Review (fornax-reviewer agent)** found 3 high, 5 medium, several low issues in M2; each
  fix starts with a failing test (all 10 failed on the old code).
- **S2 YAML alias byte bomb:** alias expansion charged by bytes (scalar length + 16 B/node)
  against max(4 × file size, 4 MiB), checked before copying; node limit stays 100k.
- **S3 path-filter work:** ≤ 100 patterns per list, ≤ 512 chars per pattern (validation
  errors); > 1000 changed paths counts as "unknown" (path filters pass); spec loading and
  trigger/condition/payload evaluation run in `spawn_blocking`.
- **S4 job volume:** ≤ 256 jobs per pipeline after matrix expansion, ≤ 100 steps per job,
  ≤ 256 KiB encoded payload per job, ≤ 1000 jobs created per push; over-limit pipelines are
  reported as diagnostics and not created. Follow-up: a per-push payload-bytes cap.
- **C6:** `.fornax/` listing read capped at 4 MiB; past 200 files one "too many files" problem.
- **S6 (orchestrator):** `POST …/pipelines/{n}/cancel` now requires a JSON body (`{}`), so a
  simple cross-site POST can't trigger it with a session cookie (D-031 model); test asserts 415
  without a body.
- **Docker:** `compose.yaml` services run with `init: true` — the tester found two agent
  process-kill tests failing only in the container because PID 1 (`cargo test`) didn't reap
  killed grandchildren (zombies still answer `kill(pid, 0)`). A real agent in a container needs
  reaping too.
- **Status:** accepted

## D-042 · 2026-11-21 · UI shell inspired by TimeSeriesAgent ("TimeSense") (H-023)
- **Decision:** adopt TimeSense's navigation rail (floating glass sidebar that collapses to a
  52 px icon rail, persisted) and phone drawer; edge-lit glass with a pointer highlight (off on
  touch and reduced motion); theme menu System/Light/Dark + five accents (Ember default, Ocean,
  Violet, Emerald, Ink; stored in `fornax.theme`); navy dark mode; 4 px spacing, type scale and
  status tokens (ok/warning/danger/info) for the pipeline pages. Built on top of the
  liquid-glass design (D-035), not replacing it.
- **Licensing:** TimeSense has no licence file and another author, so only design patterns
  were taken; no code, fonts, icons or logos copied. Its Google Fonts / CDN icon font are also
  incompatible with Fornax's CSP.
- **Rejected:** TimeSense's flat blue style instead of glass; SVG lens refraction (Chromium-only).
- **Evidence:** svelte-check 0/0, build, header check, UI test; browser-checked light/dark/375 px
  (UI session, branch claude/hopeful-clarke-d4fd52).
- **Status:** accepted

## D-043 · 2026-11-21 · M2 review fixes, part 2 (S1, S5, S8, C1–C5) + agent token CLI + disk
- **S1 runner-token theft:** the agent makes itself non-dumpable at start
  (`PR_SET_DUMPABLE=0` via `nix`, no `unsafe`), so same-uid job steps can't read
  `/proc/<agent>/environ|mem` (test proves the canary is readable before, not after). The job
  token is revoked on the attempt's first step `Update` (checkout done). `fornax agent` no
  longer has a `--token` flag (argv is world-readable via `ps`): token from
  `FORNAX_RUNNER_TOKEN` or `--token-file`. Docs: runner and registration tokens grant
  instance-wide job access → host runners are for trusted, single-tenant workloads only.
  **Still open:** runner scoping to owner/repo; separate OS user for steps; the exec'd
  `git fetch` child holds the job token in its env during checkout (readable by a concurrent
  job when capacity > 1 — use capacity 1 on shared hosts).
- **S5:** logs accepted only while the attempt runs; 64 MiB per attempt (server
  `RESOURCE_EXHAUSTED`), agent truncates just below with one marker.
- **S8:** loud warning for `http://` to non-loopback servers.
- **C1/C4:** migration `0006_attempt_liveness.sql` (`job_attempts.seen_at`,
  `jobs.free_requeues`): an expired never-seen attempt, or a claim released after a transient
  dispatch error, is deleted and its job re-queued without consuming an attempt (≤ 5 times per
  job). Only an undecodable payload fails a job at dispatch. Residual: a partitioned runner
  could run a released job twice (its later RPCs get NotFound and it stops).
- **C2:** NotFound/PermissionDenied/FailedPrecondition/InvalidArgument/ResourceExhausted are
  permanent for Log/Update/Done: pending logs dropped, job stopped, Done sent, slot freed.
- **C3:** `extend_lease` and `touch_runner` take the CI write lock.
- **C5:** the agent sweeps all-digit workspace dirs in `work_dir` at start (one agent per
  `work_dir`); `PR_SET_PDEATHSIG` not used (needs `unsafe` `pre_exec`).
- **S7 (not fixed, accepted for now):** no server-side masking backstop; the agent masks. To
  revisit with secrets (C-010).
- **Disk:** D: was nearly full; agents' copy `target/` dirs (~49 GB) deleted after merging;
  `[profile.dev] debug = "line-tables-only"` to shrink debug builds.
- **Evidence:** 246 workspace tests incl. binary e2e; clippy/fmt/headers clean.
- **Status:** accepted
