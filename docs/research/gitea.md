# Gitea (hosting + Actions) + act_runner

| | |
|---|---|
| Repos | `github.com/go-gitea/gitea` @ `d4101fd` (2026-11-19); `gitea.com/gitea/act_runner` @ `bcc0b71` (2026-11-18) |
| License | Gitea **MIT** (`LICENSE`, Gitea + Gogs authors). act_runner **MIT**; its embedded `act/` engine (fork of nektos/act) **MIT** (`act_runner/act/LICENSE`) |
| Reuse rule | Ideas only; MIT is Apache-compatible with attribution. |
| Why it matters most | Closest existing product to the new Fornax scope (D-009): **single Go binary with Git hosting + CI**, SQLite → Postgres. |

## 1. Architecture

- **Single binary** (`main.go`, `cmd/`): web UI, REST API, Git HTTP (`routers/web/repo/githttp.go`), **built-in SSH server** (`modules/ssh/ssh.go`, `Listen` at line 251), background queues, Actions orchestration — all in one process.
- **Layering:** `routers/` (HTTP) → `services/` (business logic) → `models/` (xorm DB models) → `modules/` (infrastructure).
- **DB:** xorm with MySQL, Postgres, MSSQL, and SQLite (build-tag controlled) (`modules/setting/database.go` lines 14–17, 96).
- **Git access:** shells out to the **`git` CLI** (`modules/git/gitcmd/env.go:30` `exec.LookPath(GitExecutable)`, `gitcmd/command.go`), with `cat-file --batch` long-lived processes for fast object reads (`modules/git/catfile_batch*.go`).
- **Internal queues:** pluggable backends — in-memory channel, LevelDB, Redis (`modules/queue/base_channel.go`, `base_levelqueue.go`, `base_redis.go`). Used for indexing, notifications, PR checks — not for Actions jobs (those are DB rows).
- **Hosting model:** pull requests (`models/issues/pull.go`), protected branches (`models/git/protected_branch.go`), merge strategies merge / rebase / squash / fast-forward-only (`services/pull/merge_*.go`), reviews (`services/pull/review.go`), LFS (`services/pull/lfs.go`).

### Actions data model (`models/actions/`)
`ActionRun` (workflow run, `run.go`) → `ActionRunAttempt` → `ActionRunJob` (`run_job.go`) → `ActionTask` (one execution of a job on a runner, `task.go`) → `ActionTaskStep`; plus `ActionRunner`, `ActionRunnerToken`, `ActionSchedule`/`ScheduleSpec`, `ActionArtifact`, `ActionVariable`, and **`ActionTasksVersion`** (`tasks_version.go`).

### Queue / scheduler — DB-backed (directly relevant to D-007)
- Waiting jobs are rows with `status = waiting, task_id = 0`. A runner's fetch calls `CreateTaskForRunner` (`models/actions/task.go:266`), which pages through waiting jobs with a cursor (lines ~277–300), matches labels in Go code (there's a TODO about filtering labels more efficiently, ~line 271), inserts the task + steps, then **claims the job with a conditional UPDATE** `WHERE task_id = 0 AND status = waiting` (~line 393). If 0 rows changed, another runner won → move on. No `SELECT … FOR UPDATE`; works the same on SQLite and Postgres.
- **Cheap polling via a version counter:** `ActionTasksVersion` per (owner, repo) scope is bumped when jobs become ready. `FetchTask` (`routers/api/actions/runner/runner.go:168`) compares the runner's last-seen version with the DB; only if it differs does it try to pick a task (lines ~175–210). Concurrent picks are throttled (`MAX_CONCURRENT_TASK_PICKS`).
- Liveness: `ZOMBIE_TASK_TIMEOUT` 10 min, `ENDLESS_TASK_TIMEOUT` 3 h, `ABANDONED_JOB_TIMEOUT` 24 h (`modules/setting/actions.go:133–135`), cleaned in `services/actions/cleanup.go`.

### Runner protocol
- **Connect RPC (gRPC-compatible, protobuf over HTTP)**, service `runner.v1.RunnerService` from `gitea.dev/actionslib` (`routers/api/actions/runner/runner.go:13–35`): `Register`, `Declare` (labels/version), `FetchTask`, `UpdateTask`, `UpdateLog` (lines 44–288).
- Runner polls: `fetch_interval` 2 s with **exponential backoff when idle**, `fetch_timeout` 5 s, `capacity` concurrent tasks (`act_runner/internal/pkg/config/config.example.yaml` lines 25–60).
- Labels choose executor: `name:host`, `name:docker://image`, `name:kubernetes://…` (`act_runner/internal/pkg/labels/labels.go:13–15`).
- Artifacts via separate twirp/protobuf API (`routers/api/actions/artifact.proto`, `artifactsv4.go`).

## 2. Pipeline definition

- **GitHub Actions–compatible YAML** in `.gitea/workflows/` / `.github/workflows/`; parsed and executed using the act engine (`act_runner/act/`).
- Jobs with `needs:` (DAG), `strategy.matrix` (`services/actions/matrix.go`), `max-parallel` (`max_parallel.go`), `concurrency` groups (`concurrency.go`), reusable workflows (`reusable_workflow.go`), schedules (`schedule_tasks.go`), `workflow_dispatch`.
- Actions are fetched from Git repos (default GitHub mirror) at runtime → supply-chain dependency on third-party action code.
- Secrets/variables at owner and repo scope (`models/secret/secret.go`, `models/actions/variable.go`).

## 3. GitOps / CD

- None. Deploys are workflow steps. No environments/promotion model comparable to GitLab; no reconciler.

## 4. Scaling, HA, storage, upgrades

- Designed for single instance; HA is possible with a shared DB, shared storage, Redis queues and an external indexer (**assumption**, from general knowledge of the docs — not re-verified).
- Storage: repos on disk; LFS/attachments/artifacts/packages via local or MinIO/S3 storage (`modules/storage/` — **assumption**, dir not inspected).
- Upgrades: binary swap; DB migrations run automatically at start (`models/migrations/`, `modelmigration/`).

## 5. Security model & known weaknesses

- **Fork PRs get no secrets** except the auto-generated job token, unless the event is `pull_request_target` (`models/secret/secret.go:155–166`).
- **Fork-PR approval gate**: runs from forks may need approval by a maintainer (`services/actions/notifier_helper.go:394–560`, `approve.go`).
- **Job token permission clamping**: a workflow may request permissions, but they're always capped by a repo/org ceiling (`services/actions/token_permission_design.md`).
- Weaknesses: `host` label executes directly on the runner machine, no isolation; Docker mode typically mounts the Docker socket (**assumption**); GitHub-Actions compatibility imports the `pull_request_target` footgun (secrets + untrusted code if a workflow checks out the PR head).

## 6. Adopt / avoid / do differently in Rust

**Adopt**
- **Single-binary layering** (routers → services → models → modules) — maps to Fornax crates.
- **Conditional-UPDATE job claim** (`WHERE task_id = 0 AND status = 'waiting'`) — portable across SQLite and Postgres; fits D-007 directly.
- **Version counter** to make idle polls cheap without Redis.
- Separation of *job* (what to run) and *task* (one attempt on one runner) → clean retries and history.
- Fork PR → no secrets, maintainer approval gate; clamped job-token permissions.
- Executor chosen by runner label (`host` / `docker` / `kubernetes`) — matches D-008 order.
- Zombie / endless / abandoned timeouts as explicit, configurable settings.

**Avoid**
- Label matching in application code over pages of rows — Fornax should index labels/capabilities.
- Runtime download of third-party actions from the internet (breaks air-gapped installs, supply chain).
- `pull_request_target`-style semantics.

**Do differently**
- Rust Git: evaluate **gitoxide** for reads and keep `git` CLI for complex writes (merge/rebase) behind a trait (D-009 open question).
- Long-poll / server-streaming fetch rather than fixed-interval polling (lower latency; still cheap with an in-process notifier).
- Fornax-native pipeline spec rather than GitHub Actions compatibility (decision for Step 4; compatibility could be an importer later).
