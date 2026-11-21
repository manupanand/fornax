# RULES.md — Fornax project rules

Binding rules for everyone working on Fornax (people and AI assistants). `CONTRIBUTING.md`
explains the workflow; `CLAUDE.md` holds the project brief; decisions live in `LOG.md`.

## 1. License header (required on every source file)

Every source file starts with this header, using the file type's comment syntax:

```rust
// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.
```

| File type | Prefix |
|---|---|
| `.rs`, `.ts`, `.js` | `//` |
| `.sql` | `--` |
| `.sh`, `Dockerfile`, `compose.yaml` | `#` (after the `#!` line in scripts) |
| `.svelte`, `.html` | `<!-- … -->` (when `web/` lands) |

- Check: `scripts/license-headers.sh` (exits 1 if a file is missing it).
- Fix: `scripts/license-headers.sh --fix`.
- Keep the year as written; don't change it on edits.
- Files copied from third parties keep **their own** header and get an entry in
  `THIRD_PARTY.md` (see §4).

## 2. Architecture

1. **Backend and engine are Rust only** (D-026). Only the web frontend (Svelte, D-023) may use
   another language. The system `git` binary is the one external runtime tool (D-019).
2. Pipelines and deployments are **YAML only**, read from `.fornax/` in each repository — no
   Groovy or other server-side scripting (D-024, D-027).
3. Crate boundaries (architecture §4): only `fornax-db` runs SQL, only `fornax-git` spawns
   `git` on the server (the runner agent may spawn `git` itself, D-038), `fornax-core` does no I/O. CE code never imports EE code.
4. Other platforms are inspiration only; **no wholesale ports**. Parity with an existing tool
   is proven with rust-anything corpora, unit by unit (D-021, D-025).

## 3. Code

1. `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings` pass.
2. No `unsafe` (`forbid` at workspace level). No `unwrap()` outside tests; use `expect` with a
   reason in tests.
3. Library crates return typed errors (`thiserror`); only the binary uses `anyhow`.
4. Every behaviour change comes with tests. Code that touches Git is tested against real `git`.
5. User input never reaches a shell or a command-line option: validate with `fornax-core`,
   resolve revisions before use, pass arguments as separate `Command::arg`s.
6. Secrets are never logged, stored in plain text, or put in URLs. Tokens are stored hashed.
7. Released SQL migrations are immutable; change the schema with a new migration file.

## 4. Licensing and third-party code

1. Only Apache-2.0-compatible code (Apache-2.0, MIT, BSD, ISC, …) may be copied in, with its
   notice kept and an entry in `THIRD_PARTY.md`.
2. Never read or copy GitLab EE / `ee/` / `jh/` or any proprietary source. Never copy GitLab
   `doc/` prose (CC BY-SA).
3. Contributions are accepted under Apache-2.0 with a DCO sign-off (`git commit -s`).

## 5. Process

1. Non-trivial decisions get a `LOG.md` entry (append-only; supersede, don't rewrite).
2. `TASKS.md` reflects the current state of work.
3. Analysis or record/replay tooling runs only on copies, never with production credentials.
4. Never commit secrets, keys, kubeconfigs, `.env` files or `data/`.
5. Ask before destructive actions (force-push, deleting data, dropping tables).

## 6. Multi-agent work (Claude Code)

1. Independent work is split across subagents from `.claude/agents/` (see `CLAUDE.md`,
   "Multi-agent workflow"). Dependent or overlapping work stays in one agent.
2. Never two writing agents on the same files; parallel writers use separate git worktrees.
3. Only the orchestrating session edits `LOG.md`, `TASKS.md`, `CLAUDE.md`, the root
   `Cargo.toml`, `Cargo.lock`, and assigns migration numbers.
4. Every agent follows this file (headers, tests, boundaries, licensing).
5. Nothing an agent reports counts as done until the orchestrator has verified it
   (`fornax-tester`, plus `fornax-reviewer` for auth, git, SSH, secrets or UI rendering).
