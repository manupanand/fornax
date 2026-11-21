# Comparison matrix (Step 2 — A-014)

Per-tool details and file citations: `gitlab-foss.md`, `jenkins.md`, `gitea.md`, `others.md`,
`job-queues.md`, `gcp-source.md`. "—" = not provided. *Italic* = assumption, not verified in code.

| Dimension | GitLab CE | Jenkins | Gitea | Harness OSS | Woodpecker | Argo CD | Flux | Tekton |
|---|---|---|---|---|---|---|---|---|
| License | MIT (+CC BY-SA docs; EE separate) | MIT | MIT | Apache-2.0 | Apache-2.0 | Apache-2.0 | Apache-2.0 | Apache-2.0 |
| Language | Ruby + Go | Java | Go | Go | Go | Go | Go | Go |
| Git hosting | ✅ (Gitaly service) | — | ✅ (git CLI, in-process) | ✅ (git CLI) | — | — | — | — |
| CI | ✅ | ✅ | ✅ (GH Actions compat) | ✅ | ✅ | — | — | ✅ (K8s) |
| GitOps reconcile | via Flux | — | — | — | — | ✅ | ✅ | — |
| Deploy unit | single node → many services | one JVM + agents | **single binary** | **single binary** | server + agents | K8s controllers | K8s controllers | K8s controllers |
| DB | Postgres + Redis | filesystem XML | SQLite/PG/MySQL/MSSQL | SQLite default / PG | SQLite/PG/MySQL | K8s CRDs + Redis | K8s CRDs | K8s CRDs |
| Min footprint | 8–16 GB RAM | *~1–2 GB* | *small* | *small* | *small* | *K8s cluster* | *K8s cluster* | *K8s cluster* |
| Queue truth | DB (+Redis locks) | memory (+XML) | **DB** | DB + global mutex | memory (+DB mirror) | n/a | n/a | K8s API |
| Job claim | optimistic lock + 409 | push | **conditional UPDATE** | mutex | in-process | n/a | n/a | n/a |
| Agent protocol | REST long-poll | Remoting (Java serialization) | Connect RPC poll + version | Drone runner API | **gRPC Next/Extend/Done** | n/a | n/a | n/a |
| Executors | shell, ssh, docker, k8s, VM… | agents (any) | host, docker, k8s | docker (+others) | local, docker, k8s | n/a | n/a | K8s pods |
| Pipeline syntax | YAML, stages + `needs` | Groovy (scripted/declarative) | GH Actions YAML | YAML | YAML | n/a | n/a | CRDs |
| DAG | ✅ `needs` | `parallel` only | ✅ `needs` | ✅ | ✅ `depends_on` | sync waves | `dependsOn` | ✅ `runAfter` |
| Matrix | ✅ | ✅ declarative | ✅ | ✅ | ✅ | — | — | ✅ |
| Fork-PR secrets | not exposed (runs in fork) | plugin-dependent | **not exposed + approval gate** | *?* | approval + trusted repos | n/a | n/a | n/a |
| Job token scoping | allowlist | — | **permission clamping** | *?* | — | n/a | n/a | n/a |
| Drift detection | via Flux | — | — | — | — | ✅ diff + health | ✅ | — |
| Rollback | rerun old deploy job | rerun build | — | — | — | **history-based** | revert in Git | — |
| Prune safety | — | — | — | — | — | opt-in prune | **inventory-based** | — |
| HA | ✅ (complex) | ❌ | *possible (shared DB+Redis)* | *?* | ❌ | ✅ sharding | ✅ | ✅ |

## Key conclusions (logged as D-012 … D-018)

1. **Gitea and Harness OSS prove the target shape.** Git hosting + CI in one binary on SQLite → Postgres is viable; Fornax's differentiators are **Rust**, **built-in GitOps reconciliation**, and **secure-by-default runner/trust model**.
2. **Queue:** DB is the source of truth with a **conditional-UPDATE claim** (Gitea) + **leases with Extend** (Woodpecker) + **in-process notify / long-poll** (not Redis). Portable across SQLite and Postgres; HA-ready later.
3. **Runner protocol:** small gRPC (tonic) service modeled on Woodpecker's verbs (`Register, Next, Init, Extend, Update, Log, Done`), outbound-only from agent over the HTTPS port, versioned protobuf — **no** remote code shipping (Jenkins anti-pattern).
4. **Pipeline spec:** Fornax-native typed YAML (GitLab-like stages + `needs` DAG, rules, matrix, artifacts, cache, environments). Logic never runs on the server (avoid Jenkins CPS). GitHub-Actions compatibility = not in MVP.
5. **GitOps:** adopt Argo CD's desired/live/diff/health/history model + sync phases/hooks, and Flux's source → artifact → apply split, **inventory-based prune**, `dependsOn`, and source signature verification. Must work for **host/Docker targets first** (D-008), not only Kubernetes.
6. **Security defaults:** fork/untrusted PRs → no secrets + approval gate (Gitea/GitLab); job-token permission clamping (Gitea); "trusted project" gate for privileged features (Woodpecker); host executor flagged as no-isolation and restricted to trusted refs (Woodpecker/GitLab Runner docs agree).
7. **Git layer:** both Go hosting products shell out to `git`; Fornax should define a `RepoStore` trait with a `git`-CLI implementation first and gitoxide for hot read paths — a Gitaly-like boundary as a trait, not a service.
