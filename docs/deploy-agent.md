# Pull-based deploy to client DMZ servers (M3a, minimum)

Status: **design for v1 — config only, not implemented yet.** Decisions: `LOG.md` D-044.

## The problem

- Our applications run on **clients' on-prem servers in a DMZ** with **limited internet**.
- Today every release is manual: build, copy the code and the **container images** to the
  client's DMZ server, then copy again to the production server, then `docker compose up`.
- We want this **automated** and the TL wants **auto-deploy** (every new release goes out
  without a person copying files).
- Clients won't open inbound access, so it must be **pull-based** (like Argo CD): a small
  agent on the client server asks Fornax "what should be running?" and makes it so.
- v1 target is **Docker Compose**. Kubernetes and proper workload identity (SPIFFE /
  service account) come later.

## v1 in one picture

```mermaid
sequenceDiagram
  participant Dev as Developer
  participant F as Fornax (our side)
  participant A as fornax deploy-agent (client DMZ server)
  participant D as Docker on that server
  Dev->>F: git push (app code + deploy/client-a/docker-compose.yml)
  F->>F: CI builds images, saves them as an image bundle (docker save)
  loop every poll_interval (outbound HTTPS only)
    A->>F: what is desired for environment client-a-prod?
    F-->>A: revision (commit), compose bundle URL + sha256, image bundle URL + sha256
    A->>A: compare with last applied (state file)
    alt changed
      A->>F: download compose bundle + image bundle (verify sha256)
      A->>D: docker load; docker compose -p client-a up -d
      A->>D: wait until services are running/healthy
      A->>F: report: applied revision, status, service health
    else unchanged
      A->>D: docker compose ps (detect drift)
      A->>F: report: in sync / drifted
    end
  end
```

Only one network rule is needed on the client side: **outbound HTTPS from the agent to the
Fornax server** (optionally through their proxy). No inbound ports, no registry access, no
SSH from us.

## Minimum config

### 1. In the application's repository (our side)

```
.fornax/deployments/client-a-prod.yaml   what to deploy where
deploy/client-a/docker-compose.yml       the compose project (images pinned by tag/digest)
deploy/client-a/.env                     non-secret settings only
```

`.fornax/deployments/client-a-prod.yaml` — uses the existing `kind: Deployment` format
(already parsed and validated by `fornax-spec`):

```yaml
kind: Deployment
version: 1
environment: client-a-prod        # the agent on the client server asks for this name
source:
  path: deploy/client-a           # folder with docker-compose.yml (+ non-secret .env)
target:
  type: docker-compose
  project: client-a               # compose project name on the server
sync:
  auto: true                      # TL: deploy every new commit to main automatically
  prune: false                    # never delete services/volumes automatically (v1)
  self-heal: false                # report manual changes on the server, don't overwrite them
```

### 2. On the client's server (their side)

`/etc/fornax/deploy-agent.yaml`:

```yaml
server: https://fornax.example.com      # the only endpoint the server must reach
environment: client-a-prod              # which deployment(s) this agent applies
token_file: /etc/fornax/agent.token     # temporary shared token; SPIFFE/SA identity later
work_dir: /var/lib/fornax-deploy        # releases/<commit>/ + state.json (last applied)
poll_interval: 60s
images: bundle                          # bundle = get images from Fornax (docker load)
                                        # registry = docker pull (if their network allows)
# proxy: http://proxy.client.local:3128 # optional outbound proxy
```

Run it as a systemd service (or a container with the Docker socket) next to Docker:
`fornax deploy-agent --config /etc/fornax/deploy-agent.yaml`.

## What the agent does (v1 behaviour)

1. **Poll** `GET /api/v1/deploy/{environment}/desired` (outbound HTTPS, token auth for now).
2. **Compare** with `work_dir/state.json` (last applied commit, compose sha256, image digests).
3. If different:
   - download the compose bundle and image bundle, **verify sha256**;
   - `docker load` the images (no registry needed);
   - unpack to `work_dir/releases/<commit>/` and run `docker compose -p <project> up -d`;
   - wait for services to be running/healthy (timeout), then report success or failure;
   - keep the last 3 releases — **rollback** = `up -d` of the previous release folder.
4. If unchanged: `docker compose ps` and report *in sync* or *drift* (self-heal off).
5. Report every result to Fornax so the UI shows: environment → commit → status.

## Not in v1 (later)

| Later | Why deferred |
|---|---|
| SPIFFE/SPIRE or service-account identity, per-environment authorisation | TL: auth later; v1 uses a per-agent token file |
| Production server behind the DMZ server (two hops) | v1 agent runs where Docker runs; a relay mode (agent on DMZ serves a second agent inside) comes next if production has no route out |
| Approvals / promotion PRs, deploy windows | v1 is auto-deploy as the TL asked |
| Secrets delivery | v1: secrets stay on the client server (`.env` not in Git); proper secrets with C-010 |
| Prune and self-heal | off for safety |
| Kubernetes target | after compose works |
| Image signing (cosign) | sha256 verification first |

## Assumptions to confirm

1. Each client DMZ server can reach **one** Fornax URL over HTTPS (directly or via proxy).
2. Docker + Compose v2 are installed on the client server, and the agent may use the Docker
   socket.
3. "main" is the release branch: a push to `main` that passes CI is what auto-deploys.
4. One Fornax environment per client server (e.g. `client-a-prod`).
