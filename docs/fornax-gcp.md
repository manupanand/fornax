# fornax-gcp — interactive CLI + Rust deploy agent for GCP-connected on-prem servers

Status: **in development on branch `feat/fornax-gcp`.** Decisions: `LOG.md` D-045.

## Problem

The BLP/PPE setup (docs in `srf-blp-infra-configuration/docs`, agent in `pub-sub-agent/`) works
but needs many **manual steps**: create the Pub/Sub topic and pull subscription, the service
account, IAM bindings (subscriber, Artifact Registry reader, `source.reader` per repo), attach the
topic to every repo, create the key, copy it to the server, create folders with the right
permissions, log the agent's gcloud in, write `agent.env`, install the systemd unit, copy
`agent.py`/`deploy.sh`. `fornax-gcp` makes every step a CLI command that **asks the DevOps
engineer** for the values that are theirs to decide, and replaces `agent.py` + `deploy.sh` with
a Rust agent that behaves the same.

## Decisions (user, 2026-11-28)

| Topic | Decision |
|---|---|
| Scope | Setup **and** a Rust agent replacing `agent.py`/`deploy.sh` (same behaviour) |
| Binary / package | Separate `fornax-gcp` binary and `.deb` / `.rpm` packages |
| Where it runs | Two modes: `fornax-gcp gcp …` on an admin machine with your own gcloud login; `sudo fornax-gcp server …` on the on-prem server |
| Service-account key | The engineer gives an **absolute path**; the CLI asks for it and reads it from there |
| Deployment folder in repos | Asked per installation; the CLI **suggests `.fornax`**, BLP uses `.deployment` |
| Dependencies | `gcloud`, `git`, `docker` + compose v2 are **not installed by the package**; the CLI stops with a clear error naming what is missing |
| Build / test | On `ansiblenode` (not the laptop), Rust on the host; tests use a fake `gcloud`/`docker` on `PATH` — no real GCP changes |

## Commands

```
fornax-gcp doctor [--mode gcp|server|agent]     check gcloud, git, docker, compose, auth; exit 1 if missing
fornax-gcp config show|validate [--config F]

# admin machine (your own gcloud login, project owner/IAM admin)
fornax-gcp gcp init      [--config ./fornax-gcp.toml] [--dry-run] [--yes]
fornax-gcp gcp add-repo NAME [--config F] [--dry-run]
fornax-gcp gcp create-key --out /abs/path/key.json [--config F]

# on-prem server (sudo)
fornax-gcp server init   [--config /etc/fornax-gcp/config.toml] [--dry-run] [--yes]

# on-prem server (systemd runs this as the deploy user)
fornax-gcp agent run [--once] [--config F]
fornax-gcp deploy NAME TAG [--restart] [--config F]      same exit codes as deploy.sh
fornax-gcp status [--config F]
```

`--dry-run` prints every command it would run (gcloud, git, docker, systemctl) and changes
nothing. Every step is **idempotent**: it checks what exists before creating it, so `init` can be
re-run after a failure.

## Configuration file (TOML)

`gcp init` writes `./fornax-gcp.toml`; copy it to the server; `server init` completes the server
part and installs it as `/etc/fornax-gcp/config.toml` (root-owned, mode 0644, **no secrets**).

```toml
[gcp]
project = "visual-analytics-1"
project_number = "121549680248"          # looked up by `gcp init`
topic = "srf-source-repo-events"
subscription = "srf-onprem-pull-subscription"
service_account = "srf-blp-deploy-agent"   # account id; email is derived
artifact_location = "asia-south1"          # optional: grant Artifact Registry read
artifact_repository = "srf-vision-ai"      # optional
repo_base_url = "https://source.developers.google.com/p/visual-analytics-1/r"

# App repositories, in deploy order. Tags vX.Y.Z; config in <repo>/<deploy.deployment_dir>/
repos = ["srf-blp-kafka-init", "srf-blp-db-migrate", "srf-blp-api", "srf-blp-ui"]

# Optional: shared services kept in one infra repo, tags <app>-vX.Y.Z, deployed before repos
[shared]
repo = "srf-blp-infra-configuration"
apps = ["platform", "testcameras"]
apps_dir = "apps"                # folder in the infra repo: apps/<app>/
project_prefix = "srf-blp-"      # compose project = <prefix><app>

[deploy]
deployment_dir = ".deployment"   # folder in each app repo (suggested default: .fornax)
user = "blpdevops"               # runs the agent; must be in the docker group
base_dir = "/data/blp/deploy"    # repos, releases, state, locks
apps_dir = "/data/blp/apps"      # code checked out per repo, mounted by compose as ${APP_DIR}
secrets_dir = "/data/blp/deploy/secrets"
shared_env_file = "/data/blp/deploy/secrets/ppe.env"     # optional, passed as --env-file + ${SHARED_ENV_FILE}
service_account_key = "/data/blp/deploy/secrets/srf-blp-deploy-agent-key.json"  # absolute, asked
gcloud_config_dir = "/data/blp/deploy/gcloud"            # CLOUDSDK_CONFIG of the agent only

[agent]
reconcile_interval_secs = 300
pull_timeout_secs = 120
deploy_timeout_secs = 1800
health_timeout_secs = 120
keep_releases = 5
notify_cmd = ""                  # receives "subject\n\nbody" on stdin
```

## What each setup command does (all steps idempotent, all printed in `--dry-run`)

**`gcp init`** (asks for project, names, repos …, suggests BLP defaults):
1. `doctor --mode gcp` (gcloud present and logged in with a user account).
2. Enable APIs: `pubsub`, `sourcerepo`, `iam` (+ `artifactregistry` if used).
3. Look up the project number.
4. Create the topic and the pull subscription (ack deadline 60 s, retention 7 d, never expires).
5. Create the service account.
6. Bind `roles/pubsub.subscriber` on the subscription; `roles/artifactregistry.reader` on the AR repo.
7. Bind `roles/pubsub.publisher` on the topic for the CSR publisher (`<number>-compute@developer.gserviceaccount.com`).
8. For every repo: **merge** `roles/source.reader` for the SA into the repo policy (never replace
   existing bindings) and attach the topic (`--add-topic … --message-format=json`).
9. Write `./fornax-gcp.toml`.
10. Offer `gcp create-key`, which asks for an absolute output path, creates the key with mode
    0600, and reminds to copy it to the server and delete the local copy.

**`server init`** (root; asks for paths, user, key path …):
1. `doctor --mode server` (git, docker, compose v2, gcloud; user exists and is in `docker` group).
2. Create `base_dir/{bin,state,locks,releases,repos,secrets,gcloud}` owned by the deploy user,
   mode 0700 (no group/other access; ACLs cleared when `setfacl` exists), and `apps_dir` 0700.
3. Check the key file: exists, absolute path, regular file, owner = deploy user or root, mode ≤ 0600.
4. `gcloud auth activate-service-account --key-file <key>` with `CLOUDSDK_CONFIG=gcloud_config_dir`,
   as the deploy user; `gcloud auth configure-docker <location>-docker.pkg.dev` if AR is set.
5. Create `shared_env_file` (0600, empty) if configured and missing — never overwrite.
6. Install `/etc/fornax-gcp/config.toml` and the systemd unit `fornax-gcp-agent.service`
   (User = deploy user, `CLOUDSDK_CONFIG`, `KillMode=mixed`, `TimeoutStopSec=600`), `daemon-reload`,
   `enable --now`.
7. Run `agent run --once` as a smoke test and print `journalctl -u fornax-gcp-agent -f`.

## Agent behaviour (port of agent.py + deploy.sh — same semantics)

- Targets: shared apps first (infra repo, tags `<app>-vX.Y.Z`, config `apps_dir/<app>/`, compose
  project `<project_prefix><app>`, no code sync), then repos in order (tags `vX.Y.Z`, config
  `<deployment_dir>/`, code synced to `apps_dir/<repo>`, project `<repo>`). Names `^[a-z0-9][a-z0-9-]*$`.
- Loop: `gcloud pubsub subscriptions pull --auto-ack` (message = wake-up only) or every
  `reconcile_interval_secs`; reconcile at start; exponential backoff on pull errors; SIGTERM
  finishes the current step.
- Reconcile: bare clone / `fetch --prune --force origin +refs/tags/*:refs/tags/*` with the gcloud
  credential helper (fetch the infra repo once per cycle); newest tag by version sort matching
  `^<prefix>v[0-9]+(\.[0-9]+){0,3}$`; skip if `state/<name>.deployed` == tag or `.failed` == tag.
- Deploy (in-process, also `fornax-gcp deploy`): per-app `flock` (busy → exit 2); extract the
  config of the tag into `releases/<name>/<tag>/`; `docker compose config -q`; WATCH_PATHS exist
  in the tag; `pull --policy missing`; sync code (`git checkout -f --detach` into `apps_dir/<name>`);
  `JOB_SERVICE` runs once (`run --rm -T`, never rolled back); restart containers that were not
  recreated when watched code changed; `up -d --pull never --remove-orphans --wait
  --wait-timeout <HEALTH_TIMEOUT>`; success → `state/<name>.{deployed,commit}`, clear `.failed`,
  `current` symlink, prune to `keep_releases` (never live or previous); failure → `.failed`, roll
  back to the previous release; exit codes 0 / 2 / 3 / 4 / 64 and the same NOTIFY messages.
- `--env-file` order: `release.vars`, `shared_env_file`, `secrets_dir/<name>.env`; env
  `APP_DIR`, `SHARED_ENV_FILE`; `release.vars` keys `IMAGE`, `WATCH_PATHS`, `HEALTH_TIMEOUT`,
  `JOB_SERVICE` (last assignment wins, surrounding quotes removed, CR stripped).
- No shell anywhere; every name and tag validated before it reaches a command line.

## Packaging

- `.deb` (cargo-deb) and `.rpm` (cargo-generate-rpm) of the single binary + systemd unit file +
  example config + docs. Depends: none beyond libc (gcloud/docker/git are checked at run time by
  `doctor`, per the decision above). `postinst` only prints next steps (`fornax-gcp doctor`,
  `gcp init` / `server init`); it never contacts the network.
- Built on `ansiblenode` by `scripts/package-fornax-gcp.sh`.
