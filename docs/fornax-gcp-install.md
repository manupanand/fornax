# Installing fornax-gcp (.deb / .rpm)

`fornax-gcp` ships as one static x86_64 binary in a `.deb` (Ubuntu/Debian) and an `.rpm`
(RHEL/Rocky/Alma). Design and commands: [`fornax-gcp.md`](fornax-gcp.md). Decisions: `LOG.md`
D-045.

## Supported systems

The binary is linked statically against musl (`x86_64-unknown-linux-musl`, static-pie): it
needs no glibc or other shared library, so the same file runs on any current x86_64 Linux.
Tested by installing the packages in clean containers:

| Package | Tested on | Expected to work on |
|---|---|---|
| `.deb` | Ubuntu 20.04, 22.04, 24.04; Debian 11, 12 | Ubuntu 20.04+, Debian 11+ |
| `.rpm` | Rocky Linux 8, Rocky Linux 9 | RHEL / Rocky / Alma 8 and 9 |

Only `x86_64` (`amd64`) is built. The agent unit needs systemd.

## What the package does — and does not do

Installs:

| Path | What |
|---|---|
| `/usr/bin/fornax-gcp` | the CLI and deploy agent |
| `/usr/lib/systemd/system/fornax-gcp-agent.service` | agent unit, **installed disabled** |
| `/usr/share/fornax-gcp/config.example.toml` | example config (= `fornax-gcp config example`) |
| `/usr/share/doc/fornax-gcp/` | this guide (`install.md`) and the design (`design.md`); minimized images (e.g. Ubuntu cloud/Docker) may skip `/usr/share/doc` |

During install it only runs `systemctl daemon-reload` (when systemd is running) and
`fornax-gcp doctor --mode server`, then prints what is missing and the next steps.

It does **not**:

- install gcloud, git, Docker or the Compose plugin (they are only *Suggests*, which apt and dnf
  do not install by default), and it never touches the network;
- fail because a tool is missing — the CLI itself stops with an error naming the missing tool
  when you use it;
- enable or start the agent (that is `sudo fornax-gcp server init`);
- create users, folders, `/etc/fornax-gcp/config.toml` or the gcloud login.

## Prerequisites (install them yourself)

| Tool | Where it is needed | Official install guide |
|---|---|---|
| Google Cloud CLI (`gcloud`) | admin machine and server | <https://cloud.google.com/sdk/docs/install> (apt: <https://cloud.google.com/sdk/docs/install#deb>, dnf: <https://cloud.google.com/sdk/docs/install#rpm>) |
| git | server | `sudo apt install git` / `sudo dnf install git` |
| Docker Engine + Compose v2 plugin | server | <https://docs.docker.com/engine/install/> (Ubuntu, Debian, RHEL; Rocky uses the CentOS repo: <https://docs.docker.com/engine/install/centos/>); plugin package `docker-compose-plugin` |

The deploy user (e.g. `blpdevops`) must be in the `docker` group. Check everything with:

```sh
fornax-gcp doctor --mode server    # on the server
fornax-gcp doctor --mode gcp       # on the admin machine (gcloud logged in as you)
```

## Install

Verify the download first (`SHA256SUMS` is published next to the packages):

```sh
sha256sum -c --ignore-missing SHA256SUMS
```

Ubuntu / Debian:

```sh
sudo apt install ./fornax-gcp_0.1.0_amd64.deb
```

RHEL / Rocky / Alma:

```sh
sudo dnf install ./fornax-gcp-0.1.0.x86_64.rpm
```

The packages are not signed yet; install them only from the build you made or trust.

## Next steps (printed after install)

1. On an **admin machine** with your own gcloud login (project owner / IAM admin):

   ```sh
   fornax-gcp gcp init                       # writes ./fornax-gcp.toml
   fornax-gcp gcp create-key --out /abs/path/key.json
   ```

   Copy `fornax-gcp.toml` and the key to the server; delete the local key copy.

2. On the **server**:

   ```sh
   sudo fornax-gcp server init --config /path/to/fornax-gcp.toml
   journalctl -u fornax-gcp-agent -f
   ```

   `server init` creates the folders, checks the key, logs the agent's gcloud in, installs
   `/etc/fornax-gcp/config.toml` and the systemd drop-in, and enables + starts the agent.

## The systemd unit

The packaged unit holds the settings that are the same everywhere (`ExecStart=/usr/bin/fornax-gcp
agent run`, `FORNAX_GCP_CONFIG=/etc/fornax-gcp/config.toml`, `Restart=always`, `KillMode=mixed`,
`TimeoutStopSec=600`, `NoNewPrivileges`, `PrivateTmp`) and does not start without
`/etc/fornax-gcp/config.toml`. The host-specific part is a drop-in written by `server init`:

```ini
# /etc/systemd/system/fornax-gcp-agent.service.d/override.conf
[Service]
User=blpdevops
Environment=CLOUDSDK_CONFIG=/data/blp/deploy/gcloud
```

Put your own changes in that drop-in (`sudo systemctl edit fornax-gcp-agent`), not in the
packaged file, which upgrades replace.

## Upgrade

Install the newer package the same way (`apt install ./…deb` / `dnf install ./…rpm`). Config,
drop-in and data are untouched. If the agent is running it is restarted with the new binary
(`systemctl try-restart`; a running deploy may finish first — `TimeoutStopSec=600`); a stopped
or disabled agent stays stopped.

## Remove

```sh
sudo apt remove fornax-gcp        # or: sudo apt purge fornax-gcp
sudo dnf remove fornax-gcp
```

Removal stops and disables the agent and deletes the packaged files. It **never deletes
configuration or data, not even on `apt purge`**: `/etc/fornax-gcp/`, the drop-in
`/etc/systemd/system/fornax-gcp-agent.service.d/`, and the deploy folders (`deploy.base_dir`,
`deploy.apps_dir`, holding the key, the agent's gcloud login, state and releases) were created by
`server init`, not by the package. The remove step lists what is left; delete it by hand if the
server is being retired. Running containers are not stopped.

## Building the packages

On the build server (never on a laptop):

```sh
scripts/remote-build.sh --dir fornax-dev -- scripts/package-fornax-gcp.sh
# -> target/packages/fornax-gcp_<ver>_amd64.deb, fornax-gcp-<ver>.x86_64.rpm, SHA256SUMS
```

The script installs pinned `cargo-deb` / `cargo-generate-rpm` (user-level) if missing, adds the
musl target, builds, strips, checks the binary is static and that the example config matches
`fornax-gcp config example`. No `rpmbuild`, no musl-tools, no sudo needed. A glibc build
(`FORNAX_GCP_TARGET=x86_64-unknown-linux-gnu`) is possible but then only runs on distros with a
glibc at least as new as the build host's.
