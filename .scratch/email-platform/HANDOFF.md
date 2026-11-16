# Handoff — where NM stands

Last updated 2026-11-16. Read this first, then the files it points to.

## What this is

**NM** is an open-source (AGPL-3.0), self-hosted email marketing platform in Rust and Postgres. It
replaces a self-hosted Interspire 6.2.0 that was audited at 330/1000. Its first production user
is a high-volume deployment sending through OCI Email Delivery.

Everything decided lives in the spec. The spec is the binding authority; a plan argues from it, and
where the two disagree the spec wins.

## Read these, in order

1. `.scratch/email-platform/spec.md` — the approved design.
2. `.scratch/email-platform/specs/` — the per-milestone specs that refine it.
3. `.scratch/email-platform/plans/` — one plan per milestone, task by task.
4. `.scratch/email-platform/m0-decisions.md`, `m1a-decisions.md`, `m1b-decisions.md` — what each
   milestone **decided and learned**, including the traps that cost real time. Read the one for the
   milestone you are entering; the decisions files are why later milestones lost fewer runs than
   earlier ones.
5. `GLOSSARY.md` and `docs/adr/` — the vocabulary, and the designs that were tried and rejected
   with the measurements that killed them.
6. `.scratch/email-platform/issues/` — the findings, numbered from `01` and still being added to.
   No count is carried here: the last one said "18 open findings" and was wrong about both halves,
   since nineteen files existed and sixteen of them were open. List the directory and read each
   file's `Status:` line. `AGENTS.md` explains the tracker and its triage labels.

## Where it stands

**M0 (foundations) and M1a (contact core) are complete and merged.** `main` is at `fb1afc6`.

**M1b (segments) is complete on the `m1b-segments` branch**, not pushed. All 14 tasks are
implemented and reviewed. At `26c324b` the branch was **54 commits, 90 files, 21,490 insertions
and 1,375 deletions** ahead of `main` — a measurement of that commit, not of your HEAD, and the
record pass kept adding to it afterwards. Recount rather than quote:
`git diff --shortstat fb1afc6..HEAD` and `git rev-list --count fb1afc6..HEAD`. The branch went
through a whole-branch review split into six dimensions, because one review seat across a diff
that size is a rubber stamp:

| Dimension | Scope | State |
|---|---|---|
| D1 | Tenancy, authorization, rate limiting | reported |
| D2 | SQL, migrations, query compilation | reported |
| D3 | API surface and generated drift | reported |
| D4 | Client | reported |
| D5 | The test suite as evidence (builds and runs) | reported, `final-d5-report.md` |
| D6 | Record accuracy | reported, `final-d6-report.md` |

A fix pass has landed every must-fix finding from D1–D4 (twelve commits, ending at `74b2539`) and
filed the deferred ones as issues `13`–`18`. All gates were green at that commit: `cargo test`,
`clippy -D warnings`, `fmt --check`, `sqlx prepare --check`, `check-spdx.sh`, `typecheck`, `build`,
`check:bundle`, and e2e at **127 passed, 0 failed**.

What D1–D4 found, in one line each, because it tells you what this codebase is like: a saved
segment naming a since-deleted tag compiled to a predicate **true for every contact**; an empty
segment told the user "No contacts yet" in a full workspace; a published doc comment described a
count's meaning **backwards**; and a snapshot's members table was unlogged while its parent was
not, so crash recovery could report a selection of 417,004 contacts with zero members.

## How to resume

1. Start Postgres: `docker compose -f docker-compose.dev.yml -p nm up -d`.
2. All six dimensions have reported; `final-d6-report.md` is the last of them. The dimension
   briefs and every report are in the review workspace under
   `.superpowers/sdd/2026-11-12-m1b-segments/` (git-ignored scratch — if you ran `git clean -fdx`
   it is gone, and `git log` is then your only recovery map).
3. Then open the PR and merge `m1b-segments` into `main`.
4. M1c follows. Its contract is already referenced by a comment in the CSV export's multi-select
   escaping — find it before you design M1c's import.

## Gotchas that have each cost somebody a run

- **This Windows machine cannot link Rust programs** (no MSVC C++ tools). Every `cargo`, `sqlx` and
  `cargo deny` command runs inside the Linux dev container: `bash scripts/dev.sh <command>`. Use
  `scripts/dev-serve.sh` to expose port 8080.
- **Never stop, modify or remove the `oci-email-postgres` or `marketing-automation` containers.**
  They belong to other projects. The compose project here is always `-p nm`. The e2e harness may
  only touch databases matching `nm_e2e_[0-9a-f]{12}`.
- **A long-lived `nm` dev database will refuse to migrate.** Migration `0017` changed checksum when
  `selection_snapshots` was made unlogged. Every gate is blind to this, because `sqlx::test` and
  the e2e harness both build fresh databases. Drop and recreate your dev database.
- **`cargo build --release` is a no-op after a web-only change.** rust-embed bakes `web/dist` in at
  compile time, so the binary looks rebuilt and is not. Order is `pnpm -C web build` →
  `cargo build --release --bin nm` → e2e, and before trusting an e2e run, delete the binary and
  grep the rebuilt one for a string you just added.
- **Never pipe a build to `/dev/null`.** A swallowed `tsc` failure leaves a stale `dist` and the
  next run looks like a result.
- **`web/e2e/audience-admin.spec.ts` sits at exactly 10 selections against
  `SELECTION_PER_MINUTE = 10`.** The file runs in 22 seconds, so all ten land in one window and
  refused requests count too. The next selection added anywhere in that file gets a `429` that
  reads exactly like a defect. A whole file has already been lost to this. Rate limits are keyed by
  workspace, so use a fresh workspace instead.
- **"Did my mutation land?" guards lie in two directions.** `grep -F` with a multi-line pattern
  matches any *one* line of it, so it reports success for an edit that never applied; and a
  scripted rewrite that normalises CRLF to LF makes `diff` report every line, so it reports failure
  for an edit that did. Compare against a backup, with `--strip-trailing-cr`.

## Repository

The canonical remote is `https://github.com/utkarshhxd/nm.git`. Nothing on this branch has been
pushed yet. An earlier remote under a different account is retired; do not push to it.
