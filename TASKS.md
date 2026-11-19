# TASKS.md — Fornax backlog

Status: `todo` · `doing` · `blocked` · `done`. Owner: `user` (manupanand) or `claude`.
Update this file on every change. Phases after 0 are placeholders until Step 4.

## Phase 0 — Architecture (current)

| ID | Title | Status | Owner | Depends on | Acceptance criteria |
|---|---|---|---|---|---|
| A-000 | Write README | done | claude | — | README states vision, status (pre-alpha), principles, editions, license |
| A-001 | Create working memory (CLAUDE.md, TASKS.md, LOG.md) | done | claude | — | Three files exist at repo root with the sections required by the kickoff brief |
| A-002 | Step 1: DevOps plan + clarifying questions | done | claude | A-001 | Plan entry in LOG.md (D-004) covers users, day-2 ops, failure modes, threat model, metrics; 5 questions asked |
| A-003 | Record answers to Step 1 questions | done | claude | A-002 | Answers in LOG D-005 … D-011; CLAUDE.md scope + non-goals updated (Git hosting now in scope) |
| A-004 | Decide Git hosting feature set for MVP | todo | user | A-012 | Bare hosting (push/pull, auth) vs + merge requests / code review / issues decided and logged |
| A-005 | Decide whether CSR stays the first integration | todo | user | A-016 | User confirms CSR-first given end-of-sale (D-010), or reorders (e.g. Secure Source Manager / GitHub) |
| A-010 | Research: GitLab CE (gitlab-foss only) | todo | claude | A-003 | `docs/research/gitlab-foss.md` covers the 6 required sections **plus Git hosting (Gitaly, repo storage, MR model)** with file-path citations; license noted first; no EE paths read |
| A-011 | Research: Jenkins core + pipeline plugins | todo | claude | A-003 | `docs/research/jenkins.md`, same criteria; focus on agent protocol and host executor isolation |
| A-012 | Research: Gitea (hosting + Actions + act_runner) | todo | claude | A-003 | `docs/research/gitea.md`, same criteria **plus hosting (Git transport, repo storage, PR model)** |
| A-013 | Research (optional): Woodpecker, Drone, Tekton, Argo CD, Flux | todo | claude | A-003 | One file per tool or a combined `docs/research/others.md`; license noted for each |
| A-014 | Comparison matrix + conclusions | todo | claude | A-010..A-013, A-016 | `docs/research/comparison.md` matrix; key conclusions logged in LOG.md |
| A-015 | Research: DB-backed job queue patterns | todo | claude | A-003 | How gitlab-foss / Gitea / Woodpecker persist jobs + leases; SQLite vs Postgres implications documented (feeds D-007) |
| A-016 | Research: GCP CSR + Secure Source Manager integration surface | todo | claude | A-003 | `docs/research/gcp-source.md`: auth (service account / OAuth), clone URLs, change notifications (Pub/Sub), API, SSM relationship to Gitea verified or rejected |
| A-020 | Step 3: Review rust-anything | todo | claude | A-014 | README/docs/CLAUDE.md/skill read; summary of what it is and is not in LOG.md |
| A-021 | Step 3: Propose 3–6 parity units | todo | claude | A-020 | Each unit: source tool+file, why parity matters, why not own design, license + attribution, recorded inputs. **Proposal only; wait for approval.** |
| A-030 | Step 4: docs/architecture.md | todo | claude | A-014, A-021 | All sections listed in the kickoff brief, Mermaid C4 diagrams, CE/EE boundary, Rust choices justified, risks |
| A-031 | Step 4: Phased roadmap (MVP → beta → 1.0) | todo | claude | A-030 | Phases below replaced with real tasks, each with acceptance criteria. **Wait for approval.** |
| A-032 | Create THIRD_PARTY.md | todo | claude | A-010 | File lists every source consulted/used with license and attribution |

## Phase 1 — MVP (placeholder, defined in A-031)

Single node (D-007): Git hosting core (scope per A-004), pipeline spec + validation,
DB-backed scheduler/queue, **host executor** runner (D-008), log streaming,
artifacts, minimal GitOps sync + drift + rollback (D-006), CSR integration (D-010),
OIDC login (D-011), CLI.

## Phase 2 — Beta (placeholder)

Docker executor, environments/promotion, RBAC, secrets, web UI, Postgres, LDAP,
more Git provider integrations.

## Phase 3 — 1.0 (placeholder)

Kubernetes executor, SAML + SCIM, upgrades + backup/restore, observability, hardened
runner isolation, stable APIs. HA deferred (D-007).
