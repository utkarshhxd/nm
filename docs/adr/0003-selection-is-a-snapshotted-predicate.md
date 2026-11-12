# 0003. Selection is a snapshotted predicate

Status: accepted (M1a). Spec: `.scratch/email-platform/specs/2026-11-11-m1a-contact-core.md` §8.2. Code: `src/contacts/selection.rs`.

## Decision

A bulk action takes a signed selection token, not a list of ids and not a filter. `POST /contacts/selection` takes the active filter and returns a token (HMAC under `nm:selection:v1`) encoding `{workspace_id, filter, as_of}`. `as_of` is a UUIDv7 upper bound, so the snapshot is enforced as `id <= $as_of`; ids are already time-ordered, so the bound costs nothing. Bulk endpoints take `{selection_token, excluded_ids}` and re-resolve the predicate under that bound. A token is rejected if its workspace differs from the authorised one, or if it is older than one hour.

Every bulk endpoint returns the count it actually affected, which may be lower than the count the UI showed. One audit row records the action, predicate, `as_of` and count.

## Alternatives rejected

- **A list of ids.** "Select all 2.3 million matching" cannot send two million ids.
- **A filter re-run at execution time.** A CSV import or form signup that lands between the click and the action silently joins the selection. In an email product that means acting on, and then messaging, people nobody selected. This is a compliance bug, not a cosmetic one.

## Cost if wrong

The snapshot bounds creation time only. A contact that existed at `as_of` and was edited afterwards is judged by its current fields: it may stop matching the filter, or start matching it. The UI reports the affected count for that reason. The one-hour expiry means a long-open page must ask again.

If the bound were ever dropped, or `as_of` taken from the client, the failure is silent: an action reaches contacts the user never saw. A test proves a contact created after `as_of` is absent from the resolved set. Changing the signing label `nm:selection:v1` only invalidates outstanding tokens, which is harmless.
