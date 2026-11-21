Status: needs-triage — **read before adding a membership or consent writer**

# The lock order is written down, and nothing enforces it

Filed from M1d's C-1/C-4 fix round 3, by the implementer that wrote the order down.

## What exists now

`docs/adr/0005-one-lock-order-for-every-path.md` states one order for the whole product:

> advisory address lock → `lists` → `suppressions` → `list_memberships` → `consent_log`,
> by id within a class.

Three of those positions are forced by statements nothing can get inside — the rename trigger,
`delete_list`'s own row, and the erasure tombstone written before the contact dies — and the other
two follow from them. Ten paths were changed to obey it and **none had to be exempted**.

That ADR exists because three consecutive review rounds each fixed the deadlock pair they were
handed and had the next round find another pair. Six Importants in total, every one a race, every
one invisible to a fully green test suite. The pattern was not three unlucky bugs; it was that
correctness was being re-derived by hand, per pair, forever.

## The gap

**Nothing makes the next membership writer call `lists::lock_lists`.** The compiler cannot see the
omission, review catches it only if a reviewer thinks to look, and no test sees it until two paths
actually race — which is exactly the failure mode the ADR was written to end.

So the product has traded "no written order" for "a written order maintained by discipline". That
is a real improvement and it is not the same as a guarantee.

## What a fix looks like

**Route membership and consent writes through one function that takes the lock itself**, so the
lock is not something a writer remembers but something a writer cannot avoid. This branch has done
that shape repeatedly and it has worked every time:

- `Bucket` made a raw-address rate-limit key uncompilable.
- `Checked` made validation-after-lookup not compile.
- `scrub` moved inside `record` so a caller cannot forget it.

The same move here would make "wrote a membership without holding the list" a thing that does not
typecheck.

It is bigger than the milestone it was found in, it touches every writer at once, and it wants its
own review rather than a fix round. **If a seventh deadlock ever appears in this area, stop fixing
pairs and do this instead.**

## The table has been wrong twice, in the commits that wrote the rule

Not a hypothetical. Both were caught by review, not by a test:

- **`one_click`'s row said `A, L, S` while the code did `A, S, L`** — a path recorded as fixed that
  was not, in the very commit that wrote the rule it violates. Benign only by accident, because
  `unsubscribe_all` writes a row with a null `list_id` that neither the rename trigger nor
  `delete_list`'s pre-lock touches.
- **`delete_list` itself took position 3 before position 2**, which is the one order the ADR's own
  derivation argues is impossible. It deadlocked against a rename and against the public
  preference centre, and the control proved it was a regression introduced by the ADR commit.

**The table is hand-maintained prose about lock order, and lock order is the thing this subsystem
has been wrong about five rounds running.** Nothing derives the table from the code, so a row can
describe a path that does not exist. That is the argument for the refactor above, in its sharpest
form: it would make the order derivable rather than asserted.

## The method that found the `contacts` class, written down so it can be re-run

> For each table a writer references by foreign key: does the **writer** take the parent row last —
> implicitly, through the FK's `FOR KEY SHARE` at statement end — while some **deleting** path
> takes that same parent row first and exclusively, then reaches the child rows the writer holds?

That is how `contacts` was found, one round after the previous review had declared the order
complete. **Any new foreign key into a table a write path touches must be put through this
question**, and the answer recorded in ADR 0005 — including a "ruled out, because…" when the
answer is no, so the next person does not have to re-derive it.

## Why grepping for `40P01` would have missed most of this

Writing a deadlock test for each of the six remaining paths turned up a distinction nobody had
made. **Two of them fail as deadlocks (`40P01`) and four as foreign key violations (`23503`)**, and
it follows from one fact: a deletion's cascade can only block on a membership row it can **see**.

- A path that **updates** an existing membership row holds something the deletion needs, so the
  cycle closes and Postgres reports `40P01`.
- A path that **inserts** a new one holds nothing visible, so the deletion commits out from under
  it and the end-of-statement foreign key check finds no list — `23503`, surfacing as a `500` to an
  anonymous form submitter, a `500` on a bulk action, or a failed import.

Same missing lock, same fix, two different symptoms. **A reviewer grepping for `40P01` — which is
what three rounds did — would have called the other four safe.**

## A mapping that now depends on a lock added three rounds later

With its lock removed, `add_member` answers **404, not 500**. Its error mapping reads *"the list
exists in this workspace, so the only foreign key left to fail is the contact's"* — true **only
because of the lock this milestone added**. Without it the route tells an operator the contact does
not exist when what actually vanished was the list.

Not a defect today. It is a comment and a lock that are load-bearing for each other with nothing
connecting them, and whoever touches either should know about the other.

## The coverage is a star, not a mesh

All nine deadlock tests are some path against `delete_list`. That is the right antagonist for
position 2, but **nothing holds an import against a rename, or two membership writers against each
other.** Those are believed safe by the order rather than by test — which is the same sentence the
three previous rounds used before a reviewer found the pair they had missed.

## Two costs the same round introduced, recorded here so they are not rediscovered

1. **The importer now holds `for key share` on every list it names, for its whole run.** No
   deadlock — it obeys the order — but it is a long wait for an operator renaming or deleting one
   of those lists mid-import, and the import is the one transaction in this area with **no bound on
   its duration**. See `issues/22`.

2. **`delete_list` now scans and locks the list's `suppressions` rows before deleting**, because
   its cascade would otherwise reach them last and invert the order. Served by
   `suppressions_by_list` and the tables are small, but it is new work on an operator path that
   previously did none.

## Related

- `docs/adr/0005-one-lock-order-for-every-path.md` — the order, the path table, and the rule for
  new code.
- `issues/37` — the silent drop in the window the `lists` lock created.
- `issues/36` — the uniqueness redesign these paths lock around.
