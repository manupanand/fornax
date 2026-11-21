# GitLab CE (gitlab-foss) + GitLab Runner

| | |
|---|---|
| Repos | `gitlab.com/gitlab-org/gitlab-foss` @ `0c0dfd35e` (2026-11-19); `gitlab.com/gitlab-org/gitlab-runner` @ `87a75c8` (2026-11-19) |
| License | gitlab-foss: **MIT Expat** outside `ee/`, `jh/`, `domains/<x>/ee/`; `doc/` is **CC BY-SA 4.0** (`LICENSE`). gitlab-runner: **MIT** (`LICENSE`) |
| Reuse rule for Fornax | Ideas only. MIT code could be reused with attribution but none is planned. Doc text is CC BY-SA (share-alike) → **never copy doc prose**. The clone contains no `ee/` or `jh/`; one fixture dir named `fixtures/lib/gitlab/graphql/queries/ee` exists and was **not read**. |

## 1. Architecture

**Components** (evidence: root `*_VERSION` files: `GITALY_SERVER_VERSION`, `GITLAB_SHELL_VERSION`, `GITLAB_WORKHORSE_VERSION`, `GITLAB_KAS_VERSION`, `GITLAB_PAGES_VERSION`):
- **Rails monolith** (Puma web + Sidekiq background workers) — UI, REST/GraphQL API, CI orchestration (`app/`, `lib/`).
- **Workhorse** (Go reverse proxy, `workhorse/`) — offloads uploads/downloads, Git HTTP, and **long-polling for runner job requests** (`workhorse/internal/builds/register.go`).
- **Gitaly** (Go gRPC service) — the only component touching Git repos on disk (`doc/administration/gitaly/_index.md`). Rails uses it through `lib/gitlab/git/*` wrappers.
- **gitlab-shell** — SSH Git access.
- **KAS** — server side of the GitLab agent for Kubernetes.
- **PostgreSQL** (only supported DB, `doc/install/requirements.md` §PostgreSQL) + **Redis** (cache, queues, live traces, runner watch keys).
- **GitLab Runner** — separate Go binary polling the API.

**Process model / cost:** 16 GB RAM baseline for a single node, 8 GB minimum in constrained mode (`doc/install/requirements.md` §Memory). This is the footprint Fornax is explicitly trying to beat.

**Data model (CI):** `app/models/ci/` — `Pipeline` → `Stage` → `Build` (job) / `Bridge` (trigger job); `BuildNeed` (DAG edges), `JobArtifact`, `BuildTraceChunk`, `Runner`, `RunnerManager`, `PendingBuild`, `RunningBuild`, `PipelineSchedule`, `ResourceGroup`, `FreezePeriod`, variable tables at instance/group/project/pipeline/job level. Tables are partitioned (`app/models/ci/partition.rb`, `partitionable` in `pending_build.rb`).

**Status machine:** `app/models/concerns/ci/has_status.rb` defines `created, waiting_for_resource, preparing, waiting_for_callback, pending, running, success, failed, canceled, canceling, skipped, manual, scheduled`; groupings `ACTIVE_STATUSES`, `COMPLETED_STATUSES` (lines 25–28). Transitions in `app/models/ci/build.rb:322+` (`state_machine :status`).

**Queue / scheduler:**
- Jobs ready to run are denormalized into a dedicated **`ci_pending_builds`** table (`app/models/ci/pending_build.rb`) carrying the matching attributes (protected, tags, instance-runner enabled, namespace) so the queue query is cheap (`app/services/ci/queue/build_queue_service.rb`, `pending_builds_strategy.rb`).
- Assignment happens **on the runner's request**: `Ci::RegisterJobService` (`app/services/ci/register_job_service.rb`) loads up to `MAX_QUEUE_DEPTH` candidates, takes a short Redis lock per build (`acquire_temporary_lock`, line 483, key `build/register/<id>`), then transitions the build with **optimistic locking**; `StaleObjectError` = another runner won → try next (lines ~255–275). If every candidate conflicts the API returns **409** so the runner retries.
- Pipeline advancement: `Ci::PipelineProcessing::AtomicProcessingService` takes an **exclusive lease per pipeline** (line 23) and updates job statuses with `lock_version` retries (lines 126–150).

**Runner protocol** (HTTP/JSON, pull-based):
- `POST /api/v4/jobs/request` (`lib/api/ci/runner.rb` ~line 205): 201 = job payload, 204 = nothing, 409 = conflict, 422 orphaned, 429 rate-limited. Runner side: `gitlab-runner/network/gitlab.go:664 RequestJob`.
- **Long-poll**: Workhorse holds the request and watches a Redis key per runner token (`runner_queue` value / `last_update`), releasing it only when the queue changes or the timeout hits (`workhorse/internal/builds/register.go:113–190`).
- `PUT /jobs/:id` status updates (`runner.rb:273`), `PATCH /jobs/:id/trace` incremental log chunks with byte ranges (`runner.rb:334`; client `gitlab.go:836 PatchTrace`), artifact upload (`gitlab.go:1084`).
- Logs: 128 KB chunks (`build_trace_chunk.rb:25`), live in Redis, then migrated to DB / object storage (`DATA_STORES` lines 33–36).
- Auth: runner authentication token; per-job **CI_JOB_TOKEN** scoped to the job, with a project allowlist (`doc/ci/jobs/ci_job_token.md`).

**Executors** (`gitlab-runner/executors/`): `shell`, `ssh`, `docker`, `kubernetes`, `custom`, `instance`, `virtualbox`, `parallels`, `vm`.

## 2. Pipeline definition

- `.gitlab-ci.yml`, parsed by entry classes in `lib/gitlab/ci/config/entry/` (one Ruby class per keyword: `job.rb`, `needs.rb`, `rules.rb`, `cache.rb`, `artifacts.rb`, `environment.rb`, `include.rb`, `workflow.rb`, `trigger.rb`, `retry.rb`, `timeout.rb`, `services.rb`, `id_token.rb` …).
- **Stages + DAG**: `stages:` ordering plus `needs:` edges (`need.rb`, `needs.rb`; persisted as `Ci::BuildNeed`).
- **Triggers**: push, MR, tag, schedule (`Ci::PipelineSchedule`), API/trigger tokens (`Ci::Trigger`), downstream/child pipelines (`bridge.rb`, `trigger.rb`).
- **Rules**: `rules:` with `if`/`changes`/`exists` (`entry/rules/`), `workflow:` (`workflow.rb`).
- **Matrix**: `parallel: matrix` (under `entry/product/`).
- **Reuse**: `include:` (local/remote/template/component — `entry/include/`), `extends`, hidden jobs (`hidden.rb`), `default:` + `inherit:`.
- **Caching / artifacts**: `cache.rb`, `caches.rb`, `artifacts.rb`, `reports.rb`.
- **Secrets**: CI variables with visibility Visible / Masked (default since 18.3) / Masked-and-hidden, and a *protected* flag that limits them to protected branches/tags (`doc/ci/variables/_index.md` ~lines 127–184); OIDC `id_tokens:` (`id_token.rb`) for keyless cloud auth.
- **Environments**: `environment:` keyword (`environment.rb`), `resource_group` for serialized deploys (`Ci::ResourceGroup`), `FreezePeriod` for deploy freezes.

## 3. GitOps / CD behaviour

- Deployments are recorded as `Environment` + `Deployment` rows (`app/models/environment.rb`, `deployment.rb`) created by CI jobs — **push-based CD**, not reconciliation.
- Real GitOps is delegated to **Flux**: the docs state GitLab integrates Flux; the GitLab agent (KAS) adds cluster connectivity and observability; drift remediation comes from Flux / server-side apply (`doc/user/clusters/agent/gitops.md`).
- Rollback = re-run an older deployment job. Approvals / protected environments for deploy gating are largely paid-tier (**assumption**: not verified in CE code; EE dirs deliberately not read).

## 4. Scaling, HA, storage, upgrades

- Horizontal scale by role (Puma, Sidekiq, Gitaly/Praefect, Postgres, Redis); reference architectures in `doc/administration/reference_architectures/`.
- Git storage: Gitaly nodes; Gitaly Cluster (Praefect) for replication.
- Artifacts, logs, LFS, packages → object storage.
- DB partitioning of CI tables is required at GitLab.com scale (`app/models/ci/partition.rb`) — signal that a naive single `jobs` table will hurt over time.
- Upgrades: monthly releases, background migrations, required upgrade stops (**assumption** from general knowledge; not re-verified here).

## 5. Security model & known weaknesses

- Fork MR pipelines run **in the fork project** with the fork's config and variables, so parent secrets aren't exposed by default; the parent can optionally run them in its own context (`doc/ci/pipelines/merge_request_pipelines.md` §Use with forked projects, ~line 137).
- Protected variables + protected branches/tags; masked/hidden values; job-token allowlist.
- Runner security guide (`gitlab-runner/docs/security/_index.md`): the **shell executor is high risk** — jobs run as the runner user and can read other projects' code; Docker is reasonably safe only **without `--privileged`**; privileged mode effectively hands root on the host; recommends ephemeral VMs for privileged jobs, dedicated runners for protected branches, network segmentation, and cleaning `.git` between builds (§§ 32–189).
- Weaknesses: huge attack surface (Rails monolith + many services); historical CVEs in Rails/markdown/import paths (**assumption**, general knowledge); Docker-in-Docker commonly needs privileged mode.

## 6. Adopt / avoid / do differently in Rust

**Adopt**
- Pull-based runner protocol with **request → 201/204/409**, long-poll, incremental log PATCH with byte offsets. Simple, firewall-friendly, proven.
- A dedicated **pending-jobs table** with denormalized match attributes, separate from the job history table (D-007).
- Optimistic locking (`lock_version`) on job state + exclusive per-pipeline processing.
- Per-job, short-lived **job token** with explicit scope; protected variables bound to protected refs.
- Fork-PR pipelines run without parent secrets by default.
- Log chunking: hot chunks → DB/disk, cold → object storage.
- Rich status set including `waiting_for_resource`, `manual`, `scheduled`, `canceling`.

**Avoid**
- Multi-service sprawl (Rails + Sidekiq + Workhorse + Gitaly + Redis + Postgres) — Fornax collapses these into one process with in-process modules.
- Redis as a required dependency; use DB `LISTEN/NOTIFY` (Postgres) or in-process notify (single node) for long-poll wake-ups.
- A sprawling YAML keyword set grown over 10+ years; keep the Fornax spec small and versioned.

**Do differently**
- Long-poll waiting via `tokio::sync::Notify` / broadcast channels in-process instead of Redis watch keys.
- Strongly typed pipeline spec (serde + a JSON Schema export) instead of one class per keyword.
- Git access via an in-process library (gitoxide) or a `git` subprocess behind a `RepoStore` trait — a Gitaly-like boundary as a **trait**, not a service, so it can be split out later.
- GitOps reconciliation built in (GitLab delegates to Flux).
