Status: needs-triage

# The export-saturation tests stack a 20-connection pool on top of the test's own pool

Filed from the final M1b fix round (`final-fix-2-report.md`), where the implementer named it as the
one risk in its own work rather than leaving it to be discovered. It is a **test-infrastructure**
fragility, not a product defect: nothing a user can reach is affected.

## What it is

`d790d59` added two server tests that prove the export concurrency bound — most importantly
`four_workspaces_inside_their_quotas_cannot_take_readiness_down`, which is the only test that reds
when `MAX_CONCURRENT_EXPORTS` is raised above the pool size, and therefore the only thing standing
between this branch and the defect D5 measured (four tenants inside their own quotas taking
`/readyz` to `503`).

To be that test, it has to be honest about the pool: it builds a **real
`max_connections(20)` pool**, because a bound proved against a toy pool proves nothing about the
one in `src/db.rs`. But each test already runs on a pool of its own, supplied by `#[sqlx::test]`.
So at peak, `cargo test` now asks Postgres for meaningfully more connections than it did before.

## Why it is filed rather than fixed

It passed here, repeatedly, and the alternative designs are each worse:

- A smaller pool in the test would make the proof vacuous — the exact defect class this milestone
  spent itself finding.
- Serialising the test against the rest of the suite costs more than the problem.
- Raising Postgres's `max_connections` fixes one machine, not the next one.

## How it will present, which is the reason to write it down

**It will look like a flake, not like a defect.** On a machine with a lower `max_connections` than
this one, the failure arrives as a connection error inside an unrelated test that happened to be
running at the same moment — not as a failure of the saturation test itself. Somebody will rerun
`cargo test`, watch it pass, and move on.

If that happens: check Postgres's `max_connections` against the suite's peak demand before
suspecting the test that failed. The cause and the symptom are in different files.

## Comments

Deliberately left `needs-triage`: the right fix depends on the CI runner's Postgres configuration,
which M1b never had to care about and which the next milestone will. The decision is whether to pin
`max_connections` in the CI service definition (`.github/workflows/ci.yml` already runs
`postgres:16` as a service) or to give the saturation tests a pool size they derive from the
server's own setting.
