# Optional comparisons: Woodpecker, Harness OSS (ex-Drone), Argo CD, Flux, Tekton

All five are **Apache-2.0** (`LICENSE` in each repo), so code reuse would be license-compatible
with attribution + NOTICE handling. Ideas only are planned.

| Repo | Commit (date) |
|---|---|
| `github.com/woodpecker-ci/woodpecker` | `f1f38e3` (2026-11-18) |
| `github.com/harness/drone` (now **Harness Open Source**, main branch) | `f38f54f` (2026-11-19) |
| `github.com/argoproj/argo-cd` | `232fa2d` (2026-11-19) |
| `github.com/fluxcd/flux2`, `source-controller`, `kustomize-controller` | `1380863`, `62314b9`, `e3ae793` (2026-11-17..19) |
| `github.com/tektoncd/pipeline` | `94305b7` (2026-11-18) |

---

## Woodpecker CI (community fork of Drone 0.8)

- **Architecture:** `server/` + `agent/` + `cli/`; server has `forge/` (GitHub, GitLab, Gitea, Forgejo, Bitbucket, Bitbucket DC + an **addon** mechanism for out-of-tree forges, `server/forge/addon/`), `queue/`, `scheduler/`, `store/` (xorm; SQLite default in tests, `server/store/datastore/engine_test.go:28`).
- **Queue:** an **in-memory FIFO** (`server/queue/fifo.go`) with lease-style deadlines; leases survive two missed renewals (`defaultTaskTimeout = 3 * TaskRenewInterval`, line 63); queue reshuffles every 100 ms. Persistence is a **wrapper** that writes tasks to the DB and reloads them on restart (`server/queue/persistent.go:38` `WithTaskStore`). → Single server only.
- **Agent protocol:** gRPC `service Woodpecker { Next, Init, Wait, Done, Extend, Update, Log, RegisterAgent, ReportHealth }` + `WoodpeckerAuth` (`rpc/proto/woodpecker.proto:26–38, 157`). `Next` = long-poll for work, `Extend` = lease renewal. Very clean, small surface — **closest model for Fornax's runner protocol**.
- **Backends:** `pipeline/backend/{docker,kubernetes,local}`. The docs flag the **local backend as running with no isolation** (`docs/docs/30-administration/10-configuration/11-backends/30-local.md`).
- **Security:** documented trust boundaries (`docs/docs/20-usage/17-security-boundaries.md`): forge data is untrusted input; pipeline approval; only admins can mark a repo *trusted* (unlocks `privileged`, volumes); Git credentials passed only to the clone step and trusted clone plugins.
- **Adopt:** gRPC verb set (Next/Init/Wait/Done/Extend/Update/Log), lease + extend, trusted-repo gate for privileged features, forge addon interface, a documented "security boundaries" page. **Avoid:** in-memory queue as source of truth (D-007 says DB).

## Harness Open Source (formerly Drone — `harness/drone` main branch)

- README: Drone was folded into **Harness Open Source**, which adds **code hosting**, pipelines, Gitspaces and artifact registries; Drone lives on in a `drone` branch (`README.md` "Where is Drone?"). **This is the closest existing analogue to Fornax's new scope (D-009)**: one Go binary, Git hosting + CI, SQLite default (`types/config.go:190–191`, driver `sqlite3`), Postgres optional.
- **Git hosting:** own `git/` layer that shells out to `git` (`git/command/command.go:149`), with merge, diff, blame, pre-receive processing, secret scanning on push (`git/scan_secrets.go`). Services include pull requests, merge queue, code owners, branch protection rules (`app/services/{pullreq,mergequeue,codeowners,protection,rules}`).
- **Pipelines:** `app/pipeline/{scheduler,runner,triggerer,resolver,converter}`; queue = pending stages in DB + in-memory waiters, guarded by a **global mutex** (`app/pipeline/scheduler/queue.go:43–55`, key `build_queue`) with a 1-minute rescan interval; execution reuses Drone runner libraries (`go.mod`: `drone/runner-go`, `drone-runners/drone-runner-docker`).
- **Adopt:** proof that "hosting + CI in one binary on SQLite" is viable; secret scanning in pre-receive; merge queue + code owners as later hosting features. **Avoid:** global mutex around the whole queue (fine on one node, a scalability ceiling later).

## Argo CD

- **Components** (`docs/operator-manual/architecture.md`): API server (gRPC/REST; app management, sync/rollback, repo + cluster credentials as K8s Secrets, SSO delegation, RBAC, webhook listener); **repo server** (Git cache + manifest generation from URL, revision, path, Helm/Kustomize params); **application controller** (continuously compares live vs desired state, detects `OutOfSync`, optional corrective action, PreSync/Sync/PostSync hooks).
- **Engine:** `gitops-engine/pkg/{cache,diff,health,sync}` — cluster cache, diff (incl. server-side diff strategies, `docs/user-guide/diff-strategies.md`), per-kind health checks (`health/health_deployment.go`, `health_statefulset.go`, …), sync with **phases, waves and hooks** (`sync/sync_phase.go`, `sync/syncwaves/`, `sync/doc.go`).
- **Reconcile loop:** default `timeout.reconciliation: 120s` with 60 s jitter (`docs/operator-manual/argocd-cm.yaml:397–404`) plus webhook-triggered refresh. `CompareAppState` in `controller/state.go:682`.
- **Rollback:** `status.history` + `revisionHistoryLimit` (`pkg/apis/application/v1alpha1/types.go:93–98, 1228`); rollback = sync to a past history entry.
- **Auto-sync:** `syncPolicy.automated` with `prune`, `selfHeal`, `allowEmpty` (`docs/user-guide/auto_sync.md`); prune is opt-in. Also sync windows, selective sync, sync options.
- **Scaling/HA:** controller sharding (`controller/sharding/`), `docs/operator-manual/high_availability.md`, `disaster_recovery.md`. State lives in K8s CRDs + Redis cache — no SQL DB.
- **Adopt:** desired/live/diff/health model; sync phases + waves + hooks; history-based rollback; prune + self-heal **opt-in**; webhook + periodic-with-jitter refresh; per-kind health assessment. **Avoid:** Kubernetes-only design (Fornax's first targets are host → Docker, D-008); credentials stored as cluster Secrets of the control plane (Fornax: least-privilege per environment).

## Flux (source-controller + kustomize-controller)

- **Split by responsibility:** *sources* produce versioned **artifacts** (tarballs addressable by revision/digest) from Git, OCI, Helm, buckets (`source-controller/README.md`; `api/v1/gitrepository_types.go`, `ocirepository_types.go`, `bucket_types.go`); *appliers* consume artifacts (`kustomize-controller`).
- **Kustomization spec** (`kustomize-controller/api/v1/kustomization_types.go`): `interval` (line 68), `retryInterval` (76), `prune` (100) with deletion policies (`MirrorPrune`, `Delete`, `WaitForTermination`, `Orphan`, lines 37–105), `dependsOn` (56), health checks / wait, suspend.
- **Inventory:** applied objects are recorded (`api/v1/inventory_types.go` `ResourceInventory.Entries`) → prune is computed as "in last inventory, not in new set" — safe garbage collection.
- **Verification:** Git commit/tag signature verification modes (`gitrepository_types.go:354–360` `VerifyHEAD`, `VerifyTag`); OCI cosign/notation verification (`ociverification_types.go`).
- **Adopt:** source → artifact → apply split; **inventory-based prune**; `dependsOn` between deploy units; signature verification of the source before apply; per-unit interval + retry interval. **Avoid:** being a pure K8s operator with no UI/history (Fornax must show history and support rollback in UI).

## Tekton Pipelines

- Kubernetes-native CRDs: `Task`, `TaskRun`, `Pipeline`, `PipelineRun`, plus params, results, workspaces, matrix, when-expressions, resolvers (`pkg/apis/pipeline/v1/*_types.go`; `docs/pipelines.md`, `tasks.md`, `workspaces.md`, `resolution.md`). Each TaskRun = a Pod; steps = containers.
- **Adopt:** typed *results* passed between tasks; *workspaces* as declared shared storage; *resolvers* (fetch task definitions from Git/bundles by reference, pinned). **Avoid:** K8s as a hard requirement and its verbosity; a separate trigger/dashboard project for basic UX.
- Tekton Chains (separate repo, not cloned) is the reference for SLSA provenance signing — **assumption**, to revisit for supply-chain design.
