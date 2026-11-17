Status: needs-triage

# A GDPR erasure that commits during an import's last milliseconds leaves the contact re-created

Filed from M1c Task 5 (`upsert_from_staging`, `src/contacts/import/staging.rs`), where the
implementer found the window rather than leaving it to be discovered. It is a **real product
defect on a compliance rule**, not a test fragility: the outcome is a contact row for an address
whose owner asked to be erased. It is also rare, bounded, and recoverable (the tombstone survives,
so the next import, send or erasure re-check still refuses the address; only the contact row
itself is wrongly present).

## The rule

Spec decision 1: an address with a `gdpr_erasure` tombstone is never re-created by an import. The
importer enforces it twice inside the caller's transaction:

1. An **anti-join** in the `insert … select`, against `suppressions` on the per-row
   `email_hmac` the `COPY` stage already computed. Matched rows never reach the insert, are
   counted in `failed`, and get an `import_errors` row.
2. The **last statement before `COMMIT`** re-runs the same anti-join over the ids this import
   inserted (`contacts.id = staging.id` matches only rows the insert created) and deletes any that
   now match, counting them in `failed` too.

Step 2 exists because step 1 reads a snapshot. Under `read committed`, each statement takes a
fresh one, so step 2 sees erasures that committed after step 1 — which is most of the window.

## The window that is left

Between **step 2's snapshot** and the import transaction's **`COMMIT`**.

```
import tx:    … step 1 … step 2 (snapshot S2) ──────────────────► COMMIT
erasure tx:                         BEGIN ─ delete contacts ─ write tombstone ─ COMMIT
                                            (this import's rows are not visible to it)
```

Neither transaction can see the other. The erasure deletes the contacts it can see, and this
import's rows are still uncommitted, so they are not among them. Step 2 has already run, so the
tombstone the erasure writes is not among what it saw. Both commit, and the contact survives its
own erasure.

The window is the time from the last statement to `COMMIT` — milliseconds — and it needs an
erasure for one of the very addresses in the file, committing inside it. It is not reachable from
a test without a concurrency hook, which is why there is no red test for it here;
`a_contact_erased_after_it_was_inserted_is_deleted_before_commit` stages the shape of it inside
one transaction and proves step 2 fires, which is the closest one transaction can get.

The same window applies, slightly wider, to the `update existing` path: step 2 only covers rows
this import **inserted**, because an updated contact kept its own id. An erasure racing an update
is a narrower case still — an erasure deletes the contact, so the row being updated would have to
have been re-created between the two — and the fix for the insert window closes it as well.

## Two ways to close it

1. **`serializable` isolation for the import transaction.** The erasure reads `contacts` to delete
   them and this import writes `contacts`; at `serializable` that is a read-write dependency and
   one of the two is aborted with `40001`. The import is the one that should retry, because it is
   the cheaper side to re-run and the erasure is the request with a deadline on it. Cost: the
   import is a long transaction holding a big write set, so serialization failures from unrelated
   writers become a thing imports have to retry; that retry logic does not exist yet, and a 1M-row
   import retried from the top is not free.
2. **Serialise imports and erasures through the worker's queue**, when the background worker lands
   (M1c Task 9 / the queue in the M1d plan). If erasures and imports for one workspace are
   processed by the same queue, this pair never runs concurrently and no isolation change or retry
   loop is needed. This is the cheaper fix and the one that fits the direction of travel; it just
   does not exist yet.

Whichever is chosen, step 2 should stay: it closes the much larger window between step 1 and the
end of the import, which is seconds to minutes on a large file rather than milliseconds.

## What to decide at triage

- Whether the residual risk is acceptable until the worker queue lands (recommended: yes — the
  window is milliseconds, the tombstone is not lost, and the wrong state is a contact row that the
  next erasure re-check removes).
- If not, whether to pay for `serializable` plus an import retry loop now.
