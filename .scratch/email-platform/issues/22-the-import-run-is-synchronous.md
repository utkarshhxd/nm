Status: needs-triage

# The import run happens inside the HTTP request, so `running` can never be observed

Filed from M1c's whole-branch review, where three reviewers reached it from three directions.

`POST /imports/{id}/start` opens one transaction at `src/contacts/import/routes.rs:871` and
commits it at `:1108`. The whole run — `COPY` into staging, the contacts upsert, memberships and
tags, the erasure sweep — happens inside it. `status = running` is written in that transaction, so
**no other connection can ever see it**: from any observer's point of view the import goes
straight from `pending` to `complete`.

The code says as much itself at `routes.rs:874-877`. This issue is not a discovery that the code
is doing something unintended; it is a record that the intended design has three consequences
that were not written down.

## What follows from it

**1. The UI's running screen and its 3-second poll are dead code.** They were written against a
state the server cannot publish. M1c removed `running` from `ImportReport::status`'s published
description so clients stop being told it is reachable, but the screen remains.

**2. The advertised maximum cannot complete behind a default proxy.** This does not depend on any
measurement. `spec.md` §9 budgets **120 s** for a 1,000,000-row import. nginx's
`proxy_read_timeout` and an AWS ALB's idle timeout both default to **60 s**. So a spec-conformant
run of the largest file the product accepts is cut by a default deployment, and the operator sees
a dropped connection rather than a result. M1c documented this ceiling on `MAX_IMPORT_ROWS`; it
did not remove it.

**3. A cut connection strands the operator, and the refusal cannot name the import.** When the
client disappears mid-run the transaction is still live, so it still holds its advisory lock.
Pressing Run again takes the `!guard.held` branch and returns 409 `import-in-progress`. That arm
**cannot name the import**, because it never read one — so the operator is blocked by their own
run, with no way to see it or reach it. M1c changed the copy to say the run may be the caller's
own. The underlying condition is unchanged.

## What a fix looks like

Move the run off the request: `start` commits `status = running` and returns, a worker picks the
import up, and the existing poll becomes live rather than decorative. The code already names its
successor at `routes.rs:874-877`.

Doing that well means deciding several things this issue deliberately does not: how a worker is
supervised in a single-binary deployment, what happens to a `running` import when the process
restarts, and whether the advisory lock survives the move or is replaced by a row-level claim.
Those are design questions, not a refactor, which is why M1c ticketed this rather than attempting
it inside an unrelated milestone.

**Until it is fixed, `imports.status` should stay a `String` at the API boundary.** A typed enum
would make clients write an exhaustive match including a `running` arm that cannot execute. The
right moment to type it is the change that makes `running` observable.
