Status: needs-triage

# Twelve M0 JSON bodies ignore unknown fields

Found by the PR gate review of `351fd62..80f2936`, while checking a claim in issue 09. Split out
of that issue on purpose: twelve auth, invite, workspace and API-key bodies accepting fields they
do not understand is a larger drift than the query-string one, and it stays buried if it is a
paragraph inside an issue about something else.

## The drift

Of the `ApiJson<T>` body types in `src/`, twelve carry no `#[serde(deny_unknown_fields)]`:

| Type | Where |
| --- | --- |
| `LoginRequest` | `src/auth/login.rs:36` |
| `ChangePasswordRequest` | `src/auth/login.rs:81` |
| `ConfirmRequest` | `src/auth/totp.rs:204` |
| `DisableRequest` | `src/auth/totp.rs:215` |
| `SecondFactorRequest` | `src/auth/totp.rs:221` |
| `NewPasskey` | `src/auth/passkeys.rs:186` |
| `FinishRequest` | `src/auth/passkeys.rs:191` |
| `NewApiKey` | `src/auth/api_keys.rs:115` |
| `NewInvite` | `src/workspaces/invites.rs:45` |
| `AcceptInvite` | `src/workspaces/invites.rs:82` |
| `ChangeRole` | `src/workspaces/members.rs:45` |
| `NewWorkspace` | `src/workspaces/mod.rs:56` |

All twelve are M0. Every body added in M1a has the attribute, as does the signed selection
payload, so the convention exists and these predate it rather than contradict it.

The split is not even consistent within a file: `UpdateWorkspace` (`src/workspaces/mod.rs:66`)
denies unknown fields and carries a comment explaining why, while `NewWorkspace` ten lines above
does not.

## Why it matters here more than on the query side

Issue 09 is about query strings, where the realistic failure is a listing that is not narrowed the
way a client believes. These are the bodies that create sessions, accept invitations, mint API
keys and set roles. The M1a precedent is specific: a field that is silently ignored changes what
an action does, and on the bulk bodies a dropped `excluded_ids` would have turned an unfiltered
`bulk/delete` into "delete everything". Nothing of that shape is reachable through these twelve
today -- none of them has an optional field whose absence widens anything -- but that is a
property of today's field lists, not of the types, and it is exactly what nobody re-checks when a
field is added.

`ChangeRole` is the one to look at first: it is the body that decides who is an owner.

## Scope

The attribute on twelve structs, plus one decision per struct about what it breaks: a client
sending a field this build does not know starts getting a 422 instead of being quietly tolerated.
That matters most for `LoginRequest` and `FinishRequest`, which a browser posts and a future
client library may extend. Check the e2e and the Rust tests for bodies that send extra keys before
flipping any of them.

Unlike the query-string case there is no analytics-parameter problem here: nothing appends keys to
a JSON body in transit.

## Comments

**2026-11-14 — re-checked after M1b. Nothing changed, and that is the finding.** All twelve
structs still lack the attribute, at the same twelve line numbers this issue records; none of the
files was touched by this milestone.

What did change is the strength of the argument against them. Every body added in M1b denies
unknown fields -- `NewSegment`, `UpdateSegment`, `PreviewRequest`, `NewSelection`, `BulkTarget`,
`BulkTag`, `BulkList` -- and `NewSelection` carries a doc comment saying explicitly not to factor
the bulk bodies back into one `serde(flatten)` struct, because flatten cannot deny unknown fields.
So the convention now holds across two milestones and twenty-odd bodies, and these twelve are the
only exceptions in the codebase rather than one side of a split.

M1b also supplies the sharpest precedent yet for the sentence "it is a property of today's field
lists, not of the types". `confirmed_count` is a **new required field on five existing bodies**
(issue `03`), added precisely because a number that may be omitted is a number nothing checks --
and it is enforced by an explicit `is_none()` check rather than by serde, since the field is
optional on the wire for the `ids` case. A body that silently tolerates unknown keys cannot have a
field added to it that way at all: the next build's new field is indistinguishable, to the
previous build, from a typo. `ChangeRole` is still the one to look at first.
