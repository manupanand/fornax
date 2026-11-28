# CLAUDE.md — Fornax

Working memory for contributors and AI assistants. **Re-read this file, `TASKS.md`
and the latest entries in `LOG.md` at the start of every session.**

## Project brief

Fornax is an open-source (Apache-2.0) **Git hosting + CI/CD + GitOps** platform
written in Rust. Target feel: GitLab (hosting + CI) + Argo CD/Spinnaker-style
delivery, shipped as a single fast binary, self-hosted first. It also integrates with
external Git hosts (first: GCP Cloud Source Repositories). A paid Enterprise Edition
follows later (open core, like GitLab CE/EE).

**Current phase: M1 done (2026-11-20) → next M2 CI.** Follow-ups H-018 … H-021 in TASKS.md
(coding started 2026-11-19 on user instruction, D-028). Architecture reference: `docs/architecture.md`. Backend is **Rust only** (D-026);
only the Svelte frontend may use another language.

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
| UI | Svelte web UI (incl. Git hosting UI), assets embedded in the binary (D-023) |
| Pipeline config | **YAML only** (no Groovy / server-side scripting), read from **`.fornax/`** at repo root, multiple files like `.github/workflows/` (D-024) |
| Parity | rust-anything records behaviour of selected Gitea/GitLab/Jenkins units; Fornax writes its own Rust and proves parity (D-021, D-025). No wholesale ports |

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
| Web UI | Svelte (SvelteKit or Svelte + Vite — open) | accepted (D-023) |
| Parity tooling | rust-anything (dev-only, never a build dependency) | accepted (D-025) |

Decisions move to "accepted" only via a `LOG.md` entry.

## Repo layout (current / planned)

```
README.md          project overview
CLAUDE.md          this file
TASKS.md           phased backlog
LOG.md             append-only decision log
THIRD_PARTY.md     attributions
docs/
  architecture.md  architecture (v0.1)
  research/        per-tool research notes, rust-anything parity plan
crates/
  fornax-core      validation + domain types, no I/O
  fornax-db        sqlx + SQLite migrations (migrations/sqlite/)
  fornax-git       RepoStore over the git binary
  fornax-api       axum: REST /api/v1, auth, Git smart HTTP
  fornax-ssh       built-in SSH server for git (russh)
  fornax-server    the `fornax` binary (serve, admin create-user/create-token/backup/restore)
web/               Svelte 5 + Vite UI, built to web/dist and embedded (rust-embed)
scripts/           license-headers.sh
Dockerfile, compose.yaml   local testing in Docker (WSL)
```

Boundaries: only `fornax-db` runs SQL, only `fornax-git` spawns `git` (the SSH crate asks
`fornax-git` for the command), `fornax-core` does no
I/O (architecture §4).

## How to run / test

Requires Rust ≥ 1.88, `git` ≥ 2.38, and for tests `ssh`/`ssh-keygen` and `openssl` on `PATH`.
The UI needs Node 24 (`~/.local/node/bin` in WSL). **Read `RULES.md`** — every source file
carries the license header.

```bash
cargo build --workspace
cargo test --workspace                       # includes real git clone/push over HTTP
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check

cargo run -p fornax-server -- admin create-user alice --admin   # prints a token
cargo run -p fornax-server -- serve --listen 127.0.0.1:3000 --ssh-listen 127.0.0.1:2222
scripts/license-headers.sh                   # header check (--fix to add)
(cd web && npm ci && npm run check && npm run build)   # UI → web/dist, embedded on next cargo build
docker compose up -d --build                 # local stack; --profile test run --rm test for tests
```

Data lives in `./data` (`--data-dir` / `FORNAX_DATA_DIR`): `fornax.db`, `repos/`,
`ssh_host_ed25519_key`. OIDC via `--oidc-issuer/--oidc-client-id/--oidc-client-secret`
(or `FORNAX_OIDC_*`).

## Where to build (user rule)

**Do not build Rust on the laptop** (target dirs are tens of GB). Edit here, then build and
test on the server `ansiblenode` (Ubuntu 22.04, Rust in `~/.cargo`): `scripts/remote-build.sh
[--dir NAME] -- cargo test -p <crate>` syncs this tree to `~/DevOps/code-pe/fornax-dev` there.
The user's git clone on ansiblenode is `/home/blpdevops/DevOps/code-pe/fornax`; it is updated by
pull after the user approves commit + push. Ask before committing or pushing.

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

## Multi-agent workflow (D-032)

When work can be split, **the main session acts as orchestrator and deploys subagents**
(Claude Code `Agent` tool) instead of doing everything serially. Project subagents live in
`.claude/agents/`:

| Agent | Writes code? | Use for |
|---|---|---|
| `fornax-backend` | yes (Rust, `crates/*`) | one task ID on one crate or a disjoint set of crates |
| `fornax-frontend` | yes (`web/` only) | UI task IDs, once the API it needs exists or is fixed in the brief |
| `fornax-parity` | yes (corpora + replay tests) | one rust-anything parity unit (U1–U9) each |
| `fornax-researcher` | no | prior art in one FOSS tool / one question each |
| `fornax-reviewer` | no | security + RULES.md review of a diff |
| `fornax-tester` | no | full verification matrix (fmt, clippy, tests, headers, UI, Docker) |

**Fan out when** there are ≥ 2 pieces of work that are independent: different crates or
`web/` vs backend, separate parity units, research across several tools, or review + test
after a change. **Stay single-agent when** the steps depend on each other, touch the same
files, or the whole thing is a few edits.

**Orchestrator rules**
1. Plan first: list the pieces, mark which are independent, and give each a task ID from
   `TASKS.md`, the files/crates it owns, and its definition of done.
2. **One writer per file set.** Writing agents get disjoint paths; run them with
   `isolation: "worktree"` when they share the repository, then merge their branches.
3. The orchestrator alone edits shared files: `LOG.md`, `TASKS.md`, `CLAUDE.md`, root
   `Cargo.toml`, `Cargo.lock`, migration numbering. Agents report needed changes instead.
4. Briefs are self-contained (agents start cold): task ID, goal, files owned, interfaces
   they may rely on, constraints from `RULES.md`, commands that must pass.
5. Agents' reports are claims, not proof: after they finish, run `fornax-tester` (and
   `fornax-reviewer` for security-relevant changes), check the diff, then record decisions
   in `LOG.md` and status in `TASKS.md`.
6. Prefer `model: sonnet` agents for read-only research/testing; keep `inherit` for design-
   heavy implementation.

Example (M2): `fornax-backend` ×3 in parallel — C-001 `fornax-spec` (new crate),
C-005 `fornax-proto` (new crate), C-009 trigger filters in `fornax-core` — plus
`fornax-parity` for U1 and U2; then the orchestrator wires C-003/C-004, and
`fornax-tester` + `fornax-reviewer` verify.

## Phase plan (from the kickoff brief)

- **Step 0** — working memory files (this, `TASKS.md`, `LOG.md`).
- **Step 1** — DevOps plan in `LOG.md` (users, day-2, failure modes, threat model,
  metrics) + up to 5 clarifying questions. ✅ done — answers in LOG D-005 … D-011.
- **Step 2** — research gitlab-foss, Jenkins, Gitea (+ optional Woodpecker, Drone,
  Tekton, Argo CD, Flux) into `docs/research/<tool>.md` + comparison matrix.
  Hosting side of gitlab-foss and Gitea is in scope; also GCP CSR / Secure Source
  Manager integration surface. ✅ done.
- **Step 3** ✅ approved — parity units U1–U4 (+U6–U9, U1b; U5 deferred).
- **Step 3** — evaluate `erkamyaman/rust-anything`; propose 3–6 bounded parity units
  (propose only). ⏸ wait.
- **Step 4** — `docs/architecture.md` + phased roadmap in `TASKS.md`. ⏸ wait.
