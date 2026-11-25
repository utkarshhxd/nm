<!-- SPDX-License-Identifier: AGPL-3.0-only -->

Status: open

# URL handling in the renderer is both stricter and wider than "check the scheme"

Both raised by Task R3 of M2b during its fix round, reported rather than fixed because neither
blocks the milestone and both deserve a decision rather than a patch.

## 1. Component encoding is stricter than a URL strictly needs

A merge tag substituted into a **path** position keeps only `-._~!$&'()*+,;=:@`, so a field holding
a multi-segment path is encoded to `%2F` between segments. `{{ path }}` with a value of `a/b`
renders `a%2Fb`.

That is correct and safe — it is what stops a value escaping its component, which is the whole
point of positional encoding — but it means a field genuinely holding a relative path cannot be
used as one. The operator gets a link that 404s rather than an error.

A decision is needed, not a loosening: either this is the documented behaviour and the editor says
so where a tag is inserted into a URL, or there is a separate tag form for "this field is a path
segment list". Do not simply widen the allowed set — that re-opens the path-escape case R3's review
demonstrated (`../../admin` normalising past the operator's prefix).

## 2. `static_url` re-serialises URLs that `mrml` itself wrote

The static-URL check added in R3's fix round runs **every** URL-position attribute in the document
through the `url` crate, including URLs no operator typed and no merge tag touched — ones `mrml`
generated. Re-serialising can normalise a bare host to a trailing slash and may differ from the
input in other ways not yet found.

No test caught a difference, and the safety argument for checking static URLs stands (see the
hosted view-in-browser page: a `javascript:` href inert in an inbox is stored XSS on our own
origin). But the blast radius is wider than "check the scheme", and a normalisation that changes a
URL `mrml` wrote is a change nobody asked for.

Worth a pass that compares input and output for every URL in a corpus of rendered templates, and
either a note saying normalisation is intended or a path that checks without re-serialising.

## Related

- `src/template/render.rs` — `safe_url`, `static_url`, the positional component encoders.
- M2b spec §5.4, and the queued ruling that static non-tag URLs must get allowlist treatment.
