Status: needs-triage

# The `RateLimit` headers are promised in prose and declared in no response

Found in the M1b whole-branch review (D3-F7, `final-d3-api.md`). Its sibling D3-F5 — five `422`
descriptions that omitted validations that fire — was fixed in the same pass, because that was
prose and this is structure.

## What the document says and does not say

`POST /segments/preview`, `GET /segments/counts`, `POST /contacts/selection` and
`POST /contacts/export` each say in prose that the `RateLimit` headers ride on the answer as well
as on the refusal, and the whole-branch pass made that true of export too
(`a_refused_export_keeps_the_rate_limit_headers_it_spent`, `tests/export.rs`).

What the document does not have:

- **no `headers` object on any response**, anywhere in `openapi.json`, so `ratelimit-limit`,
  `ratelimit-remaining` and `ratelimit-reset` are invisible to a generated client;
- **no mention of `Retry-After`** on any `429`, although `src/error.rs:122-123` sets it, and it is
  the one header a client actually has to read to back off correctly.

## Why it was not fixed with D3-F5

D3-F5 was four edited strings and a regeneration. This is a `headers(...)` clause on every
affected response of four operations — including the `200`s and every refusal, since the promise
is "whichever way it goes" — plus a decision about whether to declare them on the `429`s of
sign-in's limiter too, which is a different limiter with the same headers. That is a shape change
to the annotations, not a sentence, and it wants doing once across the whole API rather than on
the four routes this milestone happened to touch.

## What it costs

Today: a third-party client that wants to respect the limit has to discover three header names and
`Retry-After` by observation, and a generated client will not surface them at all. Our own client
does not read them, so nothing in NM is broken by this.

To fix: `responses(..., headers(("ratelimit-limit" = String, description = …), …))` on each
response of the four rate-limited operations, `Retry-After` on their `429`s, then regenerate. Worth
pairing with a sweep of the other `429`s so the API answers this question the same way everywhere.

## Comments
