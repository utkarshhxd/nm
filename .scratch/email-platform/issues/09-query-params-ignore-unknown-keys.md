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
