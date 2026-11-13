Status: needs-triage

# Query-string params ignore unknown keys

Found in M1a task 17 review. Corrected after the PR gate review, which found the original framing
("while every JSON body denies them") false API-wide: twelve M0 bodies ignore unknown fields too,
and that is now issue 11.

## The drift

Every JSON request body added in **M1a** carries `#[serde(deny_unknown_fields)]` -- all of them,
plus the selection token payload. Twelve M0 bodies do not, which is issue 11 and a larger drift
than this one; what follows is about the query side only. Two whole-branch fixes in this milestone
existed precisely because a
misspelt field that is silently ignored changes what an action does: a misspelt `excluded_ids`
defaulted to empty and ran the action on exactly the contacts the user meant to spare.

Both of those were the same gap on the body side, and twice it was a bug that would have destroyed
every contact in a workspace: once on the bulk bodies, where a dropped `excluded_ids` turns an
unfiltered `bulk/delete` into "delete everything", and once on the signed selection payload, where
a key from another version of the struct parsing as a default would have resolved a selection
wider than the version that issued it meant. Both are now fail-closed. The query side is the same
drift, untouched.

The three query-string structs carry no such attribute: `SortedPageParams`
(`src/pagination/sorted.rs`), `PageParams` (`src/pagination/mod.rs`) and `SuppressionParams`
(`src/suppressions/routes.rs`) accept and discard any key they do not know.

## No present exploit

The UI builds query params from typed state, so it cannot misspell one, and none of the ignored
keys changes a result today -- a stray `?limt=5` yields the default page size rather than a wrong
scope. The exposure is the same shape as the body one, not the same severity: a client that
believes it is narrowing a listing and is not.

## Scope

`#[serde(deny_unknown_fields)]` on the three structs, plus a decision about what that breaks.
Unlike bodies, query strings attract extras from outside the application -- analytics parameters
(`utm_*`, `gclid`) and anything a proxy or link-rewriter appends. A flat deny would turn a link
someone shared through a tracker into a 422. Either strip known-benign prefixes first, or deny
only on routes a browser never navigates to.

That decision is the work. The attribute is one line.

## Comments

**2026-11-14 — updated after M1b. Status unchanged; the exposure is not.** Two things moved.

**1. "No present exploit" is now too strong, on the one screen where it matters.**
`SortedPageParams` gained `snapshot_id` (`src/pagination/sorted.rs:63-70`), which is how the
contacts grid shows the **frozen set** of a selection rather than the live filter. The field is
`Option<Uuid>`, and because the struct discards keys it does not know, a client that misspells it
is served the live-filter page with a `200` and no indication that the narrowing did not happen.
That is exactly this issue's sentence -- "a client that believes it is narrowing a listing and is
not" -- except that here the narrowing is a safety property: the frozen set is what makes a bulk
action act on the contacts a person was shown. Note the server is still correct about a
*well-spelled* snapshot it cannot honour: an expired, swept, superseded or foreign `snapshot_id`
is `422 selection-expired`, never an empty page. It is only the misspelling that is silent.

**2. The convention now exists on this side of the API too.** `CountParams`
(`src/segments/routes.rs:507-513`, `?ids=` for the batch segment count) carries
`#[serde(deny_unknown_fields)]`. So the count of query structs that ignore unknown keys is still
three, but they are now the exception rather than the whole population, and the scope note's worry
about `utm_*` and link-rewriters does not apply to any of the four equally -- `?ids=` is posted to
by one screen and navigated to by nobody, which is precisely the "deny only on routes a browser
never navigates to" line this issue proposed. The three M1a structs are unchanged:
`SortedPageParams` (`src/pagination/sorted.rs`), `PageParams` (`src/pagination/mod.rs`),
`SuppressionParams` (`src/suppressions/routes.rs`).
