Status: needs-triage — **read before setting up logical replication or restoring a dump**

# `consent_log`'s `enable always` triggers will stall a logical replica

Filed from M1d's final fix wave, where the implementer made the trade deliberately and flagged its
cost rather than presenting it as free.

## What was done and why

`consent_log`'s append-only triggers are now `enable always`. Without that, a trigger does not fire
under `session_replication_role = 'replica'` — so the append-only guarantee was absent on a logical
replica and during `pg_restore --disable-triggers`, while the table comment and the admin UI both
told operators that nothing in the product can edit or delete a row.

A guarantee one session variable away from being bypassed is not a guarantee, so the triggers fire
always.

## The cost, stated plainly

**On a logical subscriber, the row changes that a list deletion or a contact erasure produces
arrive at trigger depth 1, not depth 2.** The triggers distinguish a direct write from a
referential action by `pg_trigger_depth()`, so on the subscriber those replicated changes look
direct — and are refused. **Replication stalls** until somebody runs
`alter table consent_log disable trigger <name>` on the subscriber.

The same applies to `pg_restore --disable-triggers`, which is exactly what that flag exists to
avoid and exactly what `enable always` overrides.

## Why it was still the right call for now

- No replica exists. The product has not shipped.
- The failure is **loud and reversible** — replication stops with an error naming the trigger, and
  one statement on the subscriber clears it.
- The alternative failure is silent: a consent row deleted on a replica, or during a restore, with
  nothing noticing and the product still claiming the log is append-only.

A loud failure in a situation nobody is in yet beats a silent one in a situation everybody ends up
in.

## What whoever sets up replication needs to do

Decide, before the first subscriber exists, which of these the product wants:

1. **Disable the triggers on subscribers** as part of standing up replication, and document that
   the append-only guarantee on a subscriber rests on nothing but the subscriber being read-only.
2. **Make the triggers depth-aware on a subscriber** — detect `session_replication_role` inside the
   trigger and admit replicated changes while still refusing local ones. More code, and it has to
   be right.
3. **Replicate `consent_log` by other means** (or not at all), treating the primary as the only
   place the evidence lives.

Option 1 is the usual answer and is fine as long as it is written down rather than discovered at
two in the morning.

## A deliberate asymmetry worth not "fixing"

`jobs`'s retention trigger is **not** `enable always`, and that is intentional: `jobs` is
operational, not evidence. Refusing a replicated delete there would stall replication to protect a
copy of a queue. The reasoning is in `0026`.

## Related

- `issues/24` — the other `consent_log` schema note; its prescribed remedy was corrected on
  2026-11-20 and does not apply as written.
- `issues/30` — the log loses which list once that list is deleted.
