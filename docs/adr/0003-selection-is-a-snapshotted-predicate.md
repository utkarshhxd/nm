# 0003. Selection is a snapshotted predicate

Status: **superseded by [ADR 0004](0004-selection-is-a-materialised-set.md)** (M1b). Accepted in M1a. Spec: `.scratch/email-platform/specs/2026-11-11-m1a-contact-core.md` §8.2. Code at the time: `src/contacts/selection.rs`.

Superseded, not edited, for the reason the milestone applies to old audit strings: rewriting a record makes it a worse record than it was. Everything below describes the M1a design — a token carrying `{workspace_id, filter, as_of, issued}`, re-resolved when the action ran — and **none of it describes the code any more**. The predicate has left the token, the widening comparison is no longer the guarantee and no longer adds the exclusions back, and the alternative this document explicitly rejected ("the materialised set of ids the filter matched at issue time") is what M1b builds. ADR 0004 carries the measurements that killed the bounded-timestamp repair this one proposed.

## Decision

A bulk action takes a signed selection token, not a list of ids and not a filter. `POST /contacts/selection` takes the active filter and returns a token (HMAC under `nm:selection:v2`) encoding `{workspace_id, filter, as_of, issued}`. `as_of` is a UUIDv7 upper bound, so the snapshot is enforced as `id <= $as_of`; ids are already time-ordered, so the bound costs nothing. Bulk endpoints take `{selection_token, excluded_ids}` and re-resolve the predicate under that bound. A token is rejected if its workspace differs from the authorised one, or if it is older than one hour.

`issued` is the live count at issue time, signed into the token. **`bulk/delete` and `bulk/subscribe` compare it** and refuse with `409 selection-widened`, inside the transaction, so nothing is written. `tag`, `untag` and `unsubscribe` do not refuse: they return `issued` beside `selected` and the browser compares, so the result says the view widened.

The split is **whether the action manufactures consent that cannot be obtained retroactively**, not whether it is reversible. A delete cannot be undone at all. A subscribe is perfectly reversible as a row and not reversible as a consequence: being on a list is what makes someone mailable, and unsubscribing afterwards does not un-send — the send is the thing nobody agreed to, and a widened subscribe between a selection and a campaign is the failure this ADR opens by naming. Tagging, untagging and unsubscribing let nobody be mailed who could not be mailed already, so they report rather than refuse.

`tag` sits on the reporting side only because nothing mails a tag today: a campaign targets a list. The moment M1b lets one target a tag or a segment expression, tagging manufactures consent by exactly this argument and the split has to be revisited. Decide that with the segment work, not after it.

The comparison adds the exclusions back: `issued` is the whole match at issue time and `selected` is the match at run time *less* `excluded_ids`, so it refuses when `selected + excluded_ids.len() > issued`. Comparing the two raw numbers let a widening of up to `MAX_EXCLUDED` (10 000) through.

Every bulk endpoint returns the count it actually affected, which may be lower than the count the UI showed. One audit row records the action, predicate, `as_of`, `issued` and both counts.

## Alternatives rejected

- **A list of ids.** "Select all 2.3 million matching" cannot send two million ids.
- **A filter re-run at execution time.** A CSV import or form signup that lands between the click and the action silently joins the selection. In an email product that means acting on, and then messaging, people nobody selected. This is a compliance bug, not a cosmetic one.

## Cost if wrong

The snapshot bounds creation time only. A contact that existed at `as_of` and was edited afterwards is judged by its current fields: it may stop matching the filter, or start matching it. `push_filter` reads list memberships, tags and names as they are when the action runs, not as they were.

An earlier version of this ADR said the UI reporting the affected count was the mitigation. It was not: the UI only warned when fewer contacts were affected than confirmed, so a filter that *widened* was reported as a plain success. A selection matching one contact, widened to four by a second editor applying a tag, deleted all four under a button reading "Delete 1 contact". That is why `issued` is now in the token and compared server-side.

What the comparison does not catch is a **same-count swap**: one contact leaving the match while another joins it leaves the count unchanged, so an equal count is not proof the same contacts matched. Catching that needs the materialised set of ids the filter matched at issue time — which is exactly what this decision exists not to carry, because "all 2.3 million matching" cannot be a list.

So what the check does buy is one sentence and no more: **no more contacts are acted on than the person was shown.** It buys nothing about *which* ones. The number that can be substituted is bounded by how many contacts left the match inside the one-hour window, and nothing makes that small — an import that retags a segment could swap all of them. The TTL bounds how long the window is open, not how much can move inside it. The same limit is recorded at `Selection::widened` in `src/contacts/selection.rs`.

That limit applies to `bulk/subscribe` as much as to `bulk/delete`, so be precise about what refusing a widened subscribe achieves: it makes the action **no-worse-than-shown, not safe**. A same-count swap still puts contacts on a list who were never selected, with no widening to refuse and nothing in the response to notice it, and the next campaign mails them. The guard counts; it does not identify. `.scratch/email-platform/issues/04-same-count-swap-is-unbounded.md` is the real blocker for M1b's importer — an import that retags a segment is exactly the write that performs the swap — and this refusal is not a substitute for fixing it.

State what the comparison enforces the stronger way round, because `|M1| ≤ |M0|` is only its corollary: **no action touches more contacts than `issued - excluded_ids.len()`** — the number the confirmation dialog showed. The raw comparison already gave `selected ≤ issued`; adding the exclusions back is what makes the dialog's number the bound.

It over-refuses whenever an excluded id is not in the run-time match, and **the UI can produce that**. An earlier version of this ADR said only an API client could, which was wrong, and it is recorded here rather than quietly corrected because this document has three times claimed a property the code did not have. Two UI paths, neither of which widened anything:

- The contacts grid applies the filter but **not** the `as_of` bound, which lives only in `push_selected`. The query client's `refetchOnWindowFocus` is on, so a contact created after the token and matching the filter is drawn as a ticked row when the tab regains focus. Unticking it excludes an id the match never held, and a selection of two is refused with "matches up to 3 now" — a number no selection ever stood for.
- An excluded id that has left the match cancels out on both sides only when nothing joined the match in the same window. A same-count swap with an exclusion is refused too, although `|M1| = |M0|`.

Both over-refuse, which is the safe direction on a delete that cannot be undone and a subscribe that makes people mailable; the cost is that the user must select again. `tests/bulk.rs` pins the isolated leave-the-match case, so nothing in the suite contradicts this — or covers either path above.

The one-hour expiry means a long-open page must ask again.

If the bound were ever dropped, or `as_of` taken from the client, the failure is silent: an action reaches contacts the user never saw. A test proves a contact created after `as_of` is absent from the resolved set. Changing the signing label only invalidates outstanding tokens, which is harmless — `v1` to `v2` was exactly that, chosen over accepting uncounted tokens so that no token can skip the widening check.
