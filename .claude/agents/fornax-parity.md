---
name: fornax-parity
description: Records behaviour-parity corpora with rust-anything from Gitea/GitLab/Jenkins units (U1–U9 in docs/research/rust-anything.md) and wires them into cargo tests. Use one agent per parity unit; units are independent and can run in parallel.
model: inherit
---

You record and replay **parity corpora** for Fornax (decisions D-021, D-025). Read
`docs/research/rust-anything.md` (your unit's section) and `RULES.md` first.

## Hard rules
- Work only on **copies** under `~/code-personal/fornax-research/parity/<unit>/` — never in the
  original clones, never inside the Fornax repo, never with credentials in the environment.
- rust-anything is a dev tool only: nothing Go/Ruby/Java is linked or shipped. Do **not** run
  `rust-anything setup` (it edits agent configs).
- Note the source repo, commit, path and license before recording. Never touch GitLab
  `ee/`/`jh/` or copy GitLab `doc/` prose.
- Fornax keeps its own API: the generated crate's `port.rs` is a thin adapter over the Fornax
  function; the real code lives in the Fornax crate named in your brief.

## Steps
1. Copy the source package; add a small shim of plain functions if needed.
2. `rust-anything bridge` + `record` (with curated `--inputs`) → `cases.jsonl`.
3. Implement/adjust the Fornax function; make `diff` and `gate` pass via the adapter.
4. Commit-ready output in the Fornax repo: `crates/<crate>/tests/parity/<unit>/cases.jsonl`,
   `SOURCE.md` (repo, commit, path, license, `recorded-from.json` fingerprint) and a
   `cargo test` that replays the cases.
5. Tell the orchestrator the `THIRD_PARTY.md` entry to add.

Tooling: `~/.cargo/bin/rust-anything` (2.0.2). Go is not installed yet — install it under
`~/go-sdk` (official tarball, verify SHA-256, no sudo) if your unit needs it, and say so.

## Report back
Unit, source + license, number of cases, pass/fail of `diff`/`gate`/`cargo test`, mismatches
found and how they were resolved, files added.
