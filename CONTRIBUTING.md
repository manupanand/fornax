# Contributing to Fornax

Thanks for your interest in Fornax. This guide covers how to set up, build, test and send
changes. The binding rules are in [`RULES.md`](RULES.md) — please read it first.

Fornax is **pre-alpha**. The most useful contributions right now are bug reports, design
feedback on [`docs/architecture.md`](docs/architecture.md), and work on open items in
[`TASKS.md`](TASKS.md).

## Ways to contribute

- **Report a bug** — open an issue with steps to reproduce, expected vs actual behaviour,
  your OS, `git --version` and `fornax --version`.
- **Propose a feature or design change** — open an issue first. Larger changes need a
  `LOG.md` entry (decision, alternatives, reasoning) agreed before code.
- **Pick up a task** — comment on the issue or task ID (e.g. `H-012`) so work isn't
  duplicated.
- **Security problems** — do **not** open a public issue. Contact the maintainer privately
  (a `SECURITY.md` with a reporting address will be added before the first release).

## Development setup

Fornax is developed on Linux and **WSL 2 (Ubuntu)** on Windows. Work inside the WSL file
system (e.g. `~/code/fornax`), not under `/mnt/c`, for build speed.

Requirements:

- Rust (stable, ≥ 1.88) via [rustup](https://rustup.rs)
- `git` ≥ 2.38
- Docker Engine or Docker Desktop with WSL integration (for container tests)
- `ssh`, `ssh-keygen` and `openssl` (used by the integration tests)
- Node.js 24 LTS for the web UI (`web/`)

```bash
git clone https://github.com/manupanand/fornax.git
cd fornax
cargo build --workspace
```

## Build, test, lint

```bash
cargo test --workspace                                   # unit + integration (real git over HTTP)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
scripts/license-headers.sh                               # every source file has the header
```

For UI changes also run, in `web/`:

```bash
npm ci
npm run check      # svelte-check, warnings fail
npm run build      # writes web/dist, embedded by the next cargo build
npm run dev        # hot-reload UI on :5173, proxying /api to a local fornax on :3000
```

All checks must pass before you open a pull request.

## Run locally

Natively:

```bash
cargo run -p fornax-server -- admin create-user alice --admin   # prints an access token
cargo run -p fornax-server -- serve --listen 127.0.0.1:3000
```

In Docker (WSL + Docker):

```bash
docker compose up -d --build                                    # http://localhost:3000
docker compose run --rm fornax admin create-user alice --admin  # prints an access token
docker compose --profile test run --rm test                     # full test suite in a container
docker compose down                                             # add -v to wipe data
```

Then try it:

```bash
curl -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
     -d '{"name":"hello"}' http://localhost:3000/api/v1/user/repos
git push http://alice:$TOKEN@localhost:3000/alice/hello.git main
```

## Making a change

1. Fork and create a branch: `git checkout -b feat/short-description`.
2. Keep changes focused — one task or fix per pull request.
3. Add the license header to new files (`scripts/license-headers.sh --fix`).
4. Write tests with the change. Code that touches Git is tested against real `git`.
5. Update docs that the change affects: `README.md`, `docs/architecture.md`, `TASKS.md`,
   and `LOG.md` for decisions.
6. Commit with a sign-off (see below) and open a pull request describing **what** changed,
   **why**, and **how it was tested**.

### Commit messages

Use [Conventional Commits](https://www.conventionalcommits.org/):

```
feat(api): add repository rename endpoint
fix(git): reject revisions that start with '-'
docs: describe .fornax/ layout
```

### Developer Certificate of Origin

Every commit must be signed off, certifying you wrote the change or have the right to submit
it under Apache-2.0 ([developercertificate.org](https://developercertificate.org)):

```bash
git commit -s -m "feat(api): add repository rename endpoint"
```

### Third-party code

Only copy code under an Apache-2.0-compatible license, keep its notice, and add it to
`THIRD_PARTY.md`. Never copy from GitLab EE or any proprietary source. See `RULES.md` §4.

## Code review

- A maintainer reviews every pull request; expect questions about design, tests and
  security.
- CI (once set up) runs build, tests, clippy, fmt and the header check.
- Review comments are about the code, not the person — see the community standards below.

## Repository layout

```
crates/fornax-core     validation and domain types (no I/O)
crates/fornax-db       SQLite persistence and migrations
crates/fornax-git      bare repositories via the git binary
crates/fornax-api      REST API, auth, Git smart HTTP
crates/fornax-ssh      built-in SSH server for git
crates/fornax-server   the `fornax` binary
web/                   Svelte web UI
docs/                  architecture and research notes
scripts/               developer scripts (license headers)
```

## Community standards

Be respectful and constructive. Harassment or discrimination of any kind is not tolerated.
A formal `CODE_OF_CONDUCT.md` will be added with the first public release.

## License

By contributing, you agree that your contributions are licensed under the
[Apache License 2.0](LICENSE).
