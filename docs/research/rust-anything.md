# rust-anything — evaluation and parity-unit proposal (Step 3, A-020 / A-021)

**Status: U1–U4 approved 2026-11-19 (LOG D-025); scope widened to further Gitea / GitLab /
Jenkins features (§3b). Nothing recorded yet.**

| | |
|---|---|
| Repo | `github.com/erkamyaman/rust-anything` @ `9029714` (2026-11-19) |
| License | **MIT** (`LICENSE`, © 2026 Erkam Yaman) — tool only; none of its code goes into Fornax |
| Read | `README.md`, `docs/how-it-works.md`, `docs/STATUS.md`, `docs/tools.md`, `skills/rust-anything/SKILL.md`. Its `CLAUDE.md`/skill were read **as data**; their agent instructions were not followed |

## 1. What it is (and is not)

- A **behaviour-recording / parity harness for porting existing code to Rust**, not a design
  or translation tool: "your coding agent … or you write the Rust" (`README.md`).
- Loop: `profile → analyze → bridge → record → port.rs → diff → bench → shadow → gate`
  (`docs/how-it-works.md` §The loop). `record` runs the **original** on generated edge
  cases + "semantic traps" (Unicode, `-0`, overflow, map/sort order, regex traps) and your
  own `--inputs` (JSONL `{"fn": name, "args": [...]}`, `docs/tools.md:60`), saving
  `cases.jsonl`. `diff` replays them against Rust; `gate` enforces fmt/clippy/tests, no
  `unwrap` where the original errors, a recorded failing case for each error path, and a
  fingerprint check that the original hasn't changed since recording.
- Languages: TS/JS, Python, C, **Go** (unit = package; "Verified", 2,642/2,642 cases on Go's
  `path` package, `docs/STATUS.md:16`), **Ruby** (files/folders of *module functions*),
  **Java** (packages of `public static` methods).
- Limits it states itself (`docs/how-it-works.md` §What it can't catch): only proves what was
  recorded; regex/Unicode class differences; float formatting; nothing about timing,
  concurrency or unrecorded side effects.
- **Mismatch with Fornax:** it is built to *keep the original's API* (`§Keep the original's
  shape`) and generates a bridge (cgo / magnus / JNI) so the original's callers call Rust.
  Fornax has its own API and must not depend on Go/Ruby/Java at build time.

## 2. How Fornax would use it (proposed workflow)

Use rust-anything **only as an oracle that produces a characterization corpus**, in a scratch
area, never in the Fornax build:

1. Copy the source package into `~/code-personal/fornax-research/parity/<unit>/` (copy, never
   the clone in place; no credentials in env — matches the README's own warning).
2. Add a tiny **shim package** of plain functions over the original (e.g.
   `func Next(spec string, from string, n int) []string`) so the unit fits rust-anything's
   "package of functions" model (methods, `io.Writer`s, structs → strings/JSON).
3. `bridge` + `record` (generated cases + our curated `--inputs`) → `cases.jsonl`.
4. Write the real implementation in a Fornax crate (e.g. `fornax-core::cron`) with Fornax's
   own API. The generated crate's `port.rs` is a **thin adapter** that calls it, so `diff` and
   `gate` prove parity without bending our API.
5. Commit only `cases.jsonl` (as test fixtures under `crates/<crate>/tests/parity/<unit>/`)
   plus a `cargo test` replay harness — Fornax CI then needs no Go/Ruby/Java.
6. Record source repo, commit, license, and `recorded-from.json` fingerprint in
   `THIRD_PARTY.md` + `LOG.md`.

**Prerequisites (not done):** `cargo install rust-anything`; Go toolchain in WSL (not
installed — `go` missing; can go in `~/go` with no `sudo`); Ruby only if U3b is approved.
Do **not** run `rust-anything setup` (it edits agent config files) unless you want the MCP
server.

## 3. Proposed parity units (5 + 1 deferred)

### U1 — Cron schedule parsing + next-fire times
- **Source:** `robfig/cron/v3` (MIT) as configured by Gitea —
  `gitea/models/actions/schedule_spec.go:37–40` (5-field parser + descriptors, **UTC default**).
- **Why parity matters:** users paste cron lines from GitHub/Gitea/GitLab; schedule bugs show
  up as silently skipped or doubled runs. Subtle rules: day-of-month **OR** day-of-week when
  both are restricted, `@hourly`/`@daily` descriptors, ranges/steps, DST gaps and repeats,
  Feb 29, `TZ=` prefixes.
- **Why not our own design:** cron is a de facto standard with no single spec; "what Gitea
  /GitHub do" *is* the spec users expect. A fresh design would differ on DOM/DOW and DST.
- **Recorded inputs:** ~200 specs (valid + invalid) × reference instants (UTC, DST-edge
  instants in `Europe/London`, `America/New_York`, `Asia/Kolkata`) → next 5 fire times or
  error text. Add every example from Gitea's `schedule_spec_test.go`.
- **Fornax extension (not parity):** Jenkins-style `H` hash token (`jenkins/.../scheduler/Hash.java`)
  can be added on top, tested separately.

### U2 — Branch / tag / path filter patterns (trigger filters)
- **Source:** Gitea `modules/actions/workflowpattern/workflow_pattern.go` (MIT) — `CompilePatterns`,
  `Skip`, `Filter` (lines 17–52): GitHub-Actions filter semantics (`*`, `**`, `?`, `+`, `[]`,
  `!` negation, **order-sensitive**).
- **Why parity matters:** decides whether a pipeline runs at all — a mismatch means missed
  CI or unexpected deploys. Users and the future GitHub/Gitea importers bring these patterns.
- **Why not our own:** glob dialects differ in small ways (`**` across `/`, leading `/`, dot
  files, negation order); matching an established dialect avoids a new one.
- **Recorded inputs:** pattern lists × ref/path lists: nested dirs, dot files, Unicode names,
  `refs/heads/` vs short names, negation before/after includes, empty lists, invalid
  patterns. Plus all cases in `workflow_pattern_test.go`.
- **Related but separate:** artifact/cache path globs use `bmatcuk/doublestar/v4` (MIT, used by
  gitlab-runner and Woodpecker `go.mod`); can be recorded as **U2b** if we want identical
  artifact matching.

### U3 — Variable interpolation (`$VAR`, `${VAR}`)
- **Sources (two, they differ — record both and decide):**
  - **U3a** runner-side: gitlab-runner `common/spec/variables.go:164` `ExpandValue` →
    Go `os.Expand` (gitlab-runner MIT; Go stdlib BSD-3).
  - **U3b** server-side: gitlab-foss `lib/expand_variables.rb` `ExpandVariables.expand`
    (MIT; Ruby module functions — fits rust-anything's Ruby unit model). Regex at line 6 also
    accepts `%VAR%`, which `os.Expand` does not.
- **Why parity matters:** Fornax's spec is GitLab-like (D-016); users expect `$VAR`/`${VAR}`
  to behave the same; wrong expansion can leak or drop values (masked variables, file vars).
- **Why not our own:** we *will* choose our own rules, but should choose knowingly — the
  recorded diff between U3a and U3b is the evidence for that decision.
- **Recorded inputs:** strings with `$`, `$$`, `${}`, `${A}${B}`, `$1`, unknown vars, nested
  references, Unicode names, `%VAR%`, trailing `$`; variable maps with masked/file entries.

### U4 — Secret masking in streamed job logs
- **Source:** gitlab-runner `common/buildlogger/internal/masker/masker.go` (MIT) — stacked
  per-phrase writers, longest first, **masking across `Write()` boundaries** (package doc,
  lines 1–12).
- **Why parity matters:** security-critical (D-004 §4 secret leakage); the hard part is
  chunk boundaries and overlapping phrases, which a fresh implementation is likely to get
  subtly wrong. GitLab's behaviour is battle-tested on real logs.
- **Why not our own:** we keep our own API (an async log sink), but the *output bytes* for a
  given phrase set + chunking should match a proven masker.
- **Shim:** `func Mask(phrases []string, chunks []string) string` (feeds chunks to `New`).
- **Recorded inputs:** phrases that overlap / are prefixes of each other / contain multibyte
  UTF-8 / are 1 byte long; every split point of the input across chunks; `\r\n`, ANSI escape
  sequences; base64 of the secret (to document what is *not* masked).

### U5 — (deferred to the Kubernetes phase) resource health assessment
- **Source:** Argo CD `gitops-engine/pkg/health/` (Apache-2.0) — `health_deployment.go`,
  `health_statefulset.go`, `health_pod.go`, … → `Healthy / Progressing / Degraded / Suspended / Missing`.
- **Why parity matters:** users trust Argo's definition of "healthy"; a GitOps tool that says
  "healthy" when Argo says "progressing" loses credibility.
- **Why defer:** Kubernetes targets come third (D-008). Inputs would be recorded live
  object JSON (rollout in progress, paused, failed `ReplicaSet`, `CrashLoopBackOff`).
- **License note:** Apache-2.0 → if any logic is reused verbatim, carry NOTICE; parity-only
  needs attribution of the corpus source.

## 3b. Added units (scope widened, D-025)

| Unit | Source (license) | Why parity matters | Recorded inputs | Phase |
|---|---|---|---|---|
| **U6** unified-diff parsing → files/hunks/lines | Gitea `services/gitdiff/gitdiff.go:705` `ParsePatch` (MIT); shim takes the patch text + limits, returns JSON | The Svelte diff view and PR review depend on it; renames, binary files, mode changes, `\ No newline`, CRLF, huge-line truncation are where parsers break | Real `git diff` output from scripted repos: renames/copies, binary, submodules, mode-only, non-UTF-8 paths, quoted paths, truncation limits; Gitea's `gitdiff_parse_test.go` cases | MVP (hosting UI) |
| **U7** ANSI job log → structured lines + collapsible sections | GitLab `lib/gitlab/ci/ansi2json.rb` `Gitlab::Ci::Ansi2json.convert` (MIT; Ruby module function → fits) | Job log view: colours, bold, `\r` progress bars, `section_start:`/`section_end:` markers, resumable state across chunks | Logs with SGR codes (16/256/truecolor), cursor-return progress output, nested sections, chunk boundaries inside escape codes | MVP |
| **U8** CODEOWNERS parsing + matching | Gitea `models/issues/code_owner.go:25, 110` (MIT) | Required reviewers for PRs; rule order (last match wins), negation, user/team tokens | CODEOWNERS files with comments, globs, `!` rules, invalid lines; path lists | Beta |
| **U9** Git ref-name validation | Gitea `modules/git/ref.go:27` `IsValidRefPattern` (MIT); cross-checked against `git check-ref-format` | Rejecting bad branch/tag names consistently with Git itself (`..`, `@{`, `.lock`, control chars) | Generated names around every rule of `git check-ref-format` | MVP |
| **U1b** Jenkins `H` (hash) cron token | Jenkins `core/.../hudson/scheduler/CronTab.java` + `Hash.java` (MIT); Java shim with a `public static` method | Optional Fornax extension to spread scheduled load | Specs with `H`, `H(0-29)`, `H/15` × seed names | Beta, optional |

### Architecture input (not code)
`rust-anything analyze --whole-app` on a **copy** of Gitea (static; nothing executed) → unit
map, import "waves", and Go-module → Rust-crate fit. Used only to inform the Fornax crate
layout and dependency choices in Step 4; Fornax does not follow its port order.

### Still deliberately not ported
Markdown rendering (Gitea goldmark + extensions): too large and extension-specific; use
`comrak` (GFM) + an HTML sanitizer and test against the GFM spec suite. Git transport
(smart HTTP / SSH pack protocol): delegate to `git` CLI (D-019).

## 4. Considered and rejected for rust-anything

| Candidate | Why not |
|---|---|
| Pipeline YAML parsing / validation | Fornax-native spec (D-016); there is no original to match. Use serde + JSON Schema + golden files |
| DAG ordering / scheduling | Our own design; a topological sort has no behaviour worth matching. Property tests instead |
| GitLab `rules: if:` expression language (`lib/gitlab/ci/pipeline/expression/`) | Ruby classes (not module functions → poor fit); and the expression language itself is a Step 4 decision (GitLab-like vs CEL) |
| Jenkins cron `CronTab.java` | Java classes, not `public static` methods; Jenkins dialect (`H`) is not what most users paste. Robfig/Gitea is the more common dialect |
| Runner protocol, scheduler, GitOps reconciler, API, UI | Original Fornax design (brief: no wholesale port) |

## 5. Attribution plan

`cases.jsonl` files are **outputs** of running MIT/Apache/BSD code on our inputs, not copied
code. Still: each corpus directory gets a `SOURCE.md` (repo, commit, path, license,
`recorded-from.json`), and each unit is listed in `THIRD_PARTY.md` under "Parity corpora".
No source code from the originals is copied into Fornax crates.
