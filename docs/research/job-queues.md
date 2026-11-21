# DB-backed job queue patterns (A-015, feeds D-007)

How the studied tools persist and claim jobs. File references point into the repos listed in
each tool's research note.

| Tool | Source of truth | Claim mechanism | Wake-up / idle cost | Lease / liveness |
|---|---|---|---|---|
| GitLab | Postgres `ci_pending_builds` (denormalized match attrs) + `ci_builds` | Redis temp lock per build + optimistic `lock_version` transition; conflict → next candidate, all conflict → HTTP 409 (`app/services/ci/register_job_service.rb`) | Workhorse long-poll on a Redis key per runner (`workhorse/internal/builds/register.go`) | Stuck/timed-out build services (`app/services/ci/stuck_builds/`, `timed_out_builds/`) |
| Gitea | DB rows `action_run_job` (`status=waiting, task_id=0`) | **Conditional UPDATE** `WHERE task_id=0 AND status=waiting`; 0 rows → lost race (`models/actions/task.go:266–400`) | Per-scope **version counter**; runner polls with backoff, server only searches when version changed (`routers/api/actions/runner/runner.go:168`, `models/actions/tasks_version.go`) | Zombie 10 min / endless 3 h / abandoned 24 h (`modules/setting/actions.go:133–135`) |
| Woodpecker | **In-memory FIFO**, mirrored to DB for restart (`server/queue/fifo.go`, `persistent.go`) | In-process; single server | gRPC `Next` long-poll | Lease extended via `Extend`; timeout = 3 × renew interval (`fifo.go:63`) |
| Harness OSS | DB pending stages | Global mutex `build_queue` + in-memory waiters (`app/pipeline/scheduler/queue.go:43–55`) | Signal to waiters; 1-min rescan | — |
| Jenkins | In-memory `Queue`, persisted to `queue.xml` | Controller pushes to executors (`hudson/model/Queue.java`, `LoadBalancer.java`) | n/a (push) | Agent disconnect → durable task re-attach |

## Recommended Fornax pattern (proposed, to confirm in Step 4)

1. **Tables:** `jobs` (history, immutable-ish) + `job_queue` (only runnable jobs, with indexed match columns: labels/capabilities, protected flag, trust level, priority, `available_at`) + `job_attempts` (one row per execution, like Gitea's task) + `job_leases` (attempt id, agent id, `expires_at`).
2. **Claim:** single transaction, portable to SQLite and Postgres:
   `UPDATE job_queue SET claimed_by=?, claimed_at=now WHERE id = (SELECT id … WHERE claimed_by IS NULL AND <match> ORDER BY priority, id LIMIT 1) AND claimed_by IS NULL RETURNING id` — Gitea-style conditional update; on Postgres optionally `FOR UPDATE SKIP LOCKED` for throughput.
3. **Wake-up:** in-process `tokio::sync::Notify` per label set (single node); later Postgres `LISTEN/NOTIFY` for multi-server. Agents use **long-poll** (`Next`), not fixed-interval polling.
4. **Leases:** attempt lease renewed by agent heartbeats (`Extend`), expiry = 3 × interval (Woodpecker); expired → attempt `lost`, job re-queued per retry policy. Server time only (D-004 §3 clock skew).
5. **Explicit blocked reasons** (Jenkins-style): `waiting_for_dependency`, `waiting_for_resource`, `waiting_for_approval`, `no_matching_runner`.
6. **SQLite note:** single writer — keep claim transactions short; WAL mode; this is acceptable for single node (D-007).
