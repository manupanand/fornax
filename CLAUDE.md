# CLAUDE.md — Fornax

Working memory for contributors and AI assistants. **Re-read this file, `TASKS.md`
and the latest entries in `LOG.md` at the start of every session.**

## Project brief

Fornax is an open-source (Apache-2.0) **Git hosting + CI/CD + GitOps** platform
written in Rust. Target feel: GitLab (hosting + CI) + Argo CD/Spinnaker-style
delivery, shipped as a single fast binary, self-hosted first. It also integrates with
external Git hosts (first: GCP Cloud Source Repositories). A paid Enterprise Edition
follows later (open core, like GitLab CE/EE).

**Current phase: 0 — Architecture.** No product code until the architecture is
approved (see `TASKS.md`, phase 0).

## Goals

- One system for CI (pipelines, runners, artifacts, caches) and CD (GitOps
  reconciliation, environments, promotion, rollback).
- Single binary (server / agent / CLI) that runs on a laptop, one VM, or a cluster.
- Secure by default: isolated runners, scoped secrets, no secrets for untrusted refs,
  full audit trail.
- Day-2 operations designed in: upgrades, backup/restore, HA path, observability,
  air-gapped installs.
- Git is the source of truth for pipelines, environments and desired state.
- Built-in Git hosting (feature set for MVP still open — LOG D-009).

## Scope decisions (Step 1 answers — LOG D-005 … D-011)

| Topic | Decision |
|---|---|
| MVP | CI **and** minimal GitOps delivery (D-006) |
| Topology | Single node for now; CI + runner jobs persisted in the DB as a durable queue (D-007) |
| Executors | Host (shell) → Docker → Kubernetes (D-008). Host runners never run untrusted PR code by default |
| Git | Own hosting + external integrations; first integration GCP Cloud Source Repositories, behind a provider abstraction (D-009, D-010). CSR is closed to new customers since 2024-06 |
| Auth | OIDC → LDAP → SAML → SCIM, plus break-glass local admin (D-011). CE/EE split open |

## Non-goals (for now)

- HA / multi-node server (deferred; keep the design HA-ready — D-007).
- Not a general workflow engine or a Kubernetes distribution.
- No wholesale port of any existing project.
- No plugin marketplace in 1.0; a narrow, explicit extension surface instead.
- SaaS/multi-region hosted offering is out of scope until after 1.0.

## Constraints

- License: Apache-2.0 for CE. Only Apache-2.0-compatible third-party code may be
  used, with attribution in `THIRD_PARTY.md` and `LOG.md`.
- **Never read or copy GitLab EE / proprietary code** (`ee/`, `jh/` directories or
  any non-FOSS repo). Use `gitlab-foss` only.
- EE features must live in clearly separated crates/dirs; CE must build and work alone.
- Evidence over memory: cite repo file paths or docs for claims; mark the rest as
  **assumption**.
- Record/replay or analysis tooling runs only on copies, never with production
  credentials.

## Tech stack (proposed — not yet accepted)

| Concern | Candidate | Status |
|---|---|---|
| Language | Rust (stable, edition 2024) | accepted |
| Async runtime | tokio | proposed |
| HTTP API | axum | proposed |
| Runner protocol | gRPC (tonic) | proposed |
| DB | SQLite → Postgres via sqlx | proposed |
| Serialization | serde (YAML / JSON) | proposed |
| Errors | thiserror (libs), anyhow (bins) | proposed |
| Telemetry | tracing + OpenTelemetry, Prometheus metrics | proposed |

Decisions move to "accepted" only via a `LOG.md` entry.

## Repo layout (current / planned)

```
README.md          project overview
CLAUDE.md          this file
TASKS.md           phased backlog
LOG.md             append-only decision log
THIRD_PARTY.md     attributions (planned)
docs/
  architecture.md  architecture (planned, Step 4)
  research/        per-tool research notes (planned, Step 2)
crates/            Rust workspace (planned, after approval)
```

## How to run / test

Not applicable yet. Once the workspace exists: `cargo build`, `cargo test`,
`cargo clippy -- -D warnings`, `cargo fmt --check`.

## Coding conventions (to apply once code starts)

- `cargo fmt` + `clippy -D warnings` clean; no `unsafe` without a justified comment.
- Library crates return typed errors (`thiserror`); no `unwrap()` outside tests.
- Public types documented; every crate has a short README stating its boundary.
- Tests next to code; integration tests in `tests/`; golden files for spec parsing.
- Conventional commits; commits end with
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` when AI-assisted.

## Session workflow rules

1. **Plan first, then act.** Stop and wait for approval at the end of Steps 1, 3 and 4.
2. Record every non-trivial decision in `LOG.md` (incl. rejected options and open
   questions). `LOG.md` is append-only: supersede entries, never rewrite them.
3. Update `TASKS.md` whenever work starts, finishes, gets blocked or changes.
4. Respect licenses; note the license of each repo **before** reading it deeply.
5. Keep outputs concise and structured. Ask before any destructive action.
6. Environment: work in WSL; repo at `/home/manupanand/code-personal/fornax`.

## Phase plan (from the kickoff brief)

- **Step 0** — working memory files (this, `TASKS.md`, `LOG.md`).
- **Step 1** — DevOps plan in `LOG.md` (users, day-2, failure modes, threat model,
  metrics) + up to 5 clarifying questions. ✅ done — answers in LOG D-005 … D-011.
- **Step 2** — research gitlab-foss, Jenkins, Gitea (+ optional Woodpecker, Drone,
  Tekton, Argo CD, Flux) into `docs/research/<tool>.md` + comparison matrix.
  Hosting side of gitlab-foss and Gitea is in scope; also GCP CSR / Secure Source
  Manager integration surface.
- **Step 3** — evaluate `erkamyaman/rust-anything`; propose 3–6 bounded parity units
  (propose only). ⏸ wait.
- **Step 4** — `docs/architecture.md` + phased roadmap in `TASKS.md`. ⏸ wait.
