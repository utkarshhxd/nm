# 0003. Selection is a snapshotted predicate

Status: accepted (M1a). Spec: `.scratch/email-platform/specs/2026-11-11-m1a-contact-core.md` §8.2. Code: `src/contacts/selection.rs`.

## Decision

A bulk action takes a signed selection token, not a list of ids and not a filter. `POST /contacts/selection` takes the active filter and returns a token (HMAC under `nm:selection:v2`) encoding `{workspace_id, filter, as_of, issued}`. `as_of` is a UUIDv7 upper bound, so the snapshot is enforced as `id <= $as_of`; ids are already time-ordered, so the bound costs nothing. Bulk endpoints take `{selection_token, excluded_ids}` and re-resolve the predicate under that bound. A token is rejected if its workspace differs from the authorised one, or if it is older than one hour.

`issued` is the live count at issue time, signed into the token. **`bulk/delete` is the only endpoint that compares it**: a delete whose predicate resolves to more is refused with `409 selection-widened`, inside the transaction, so nothing is deleted. The reversible kinds (tag, untag, subscribe, unsubscribe) do not refuse; they return `issued` beside `selected` and the browser compares, so the result says the view widened.

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

The adjusted comparison over-refuses in one case: an `excluded_ids` entry that was **never** in the match (an id that was in it and has since left cancels out on both sides). Only an API client can produce that; the UI excludes rows drawn from the matching view. Over-refusing is the safe direction on the one action that cannot be undone.

The one-hour expiry means a long-open page must ask again.

If the bound were ever dropped, or `as_of` taken from the client, the failure is silent: an action reaches contacts the user never saw. A test proves a contact created after `as_of` is absent from the resolved set. Changing the signing label only invalidates outstanding tokens, which is harmless — `v1` to `v2` was exactly that, chosen over accepting uncounted tokens so that no token can skip the widening check.
