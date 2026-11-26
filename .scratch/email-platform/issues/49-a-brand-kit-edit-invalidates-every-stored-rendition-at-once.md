# 49 — A brand-kit edit invalidates every stored rendition at once, and nothing recompiles them

Status: closed by M2c R2 (5ba5611). Recompile on read, in memory, never written back (direction 2 without the write): a campaign freezes a fresh compile at launch, and a stored-template preview recompiles a `Stale` or `MarkerFormat` rendition. Field drift still refuses, and `Stored::ready` answers it before staleness.

**Found by:** the M2b task R7 review (commit `bf5a344`), executed, not reasoned.
**Blocks:** M2c sign-off. Not M2b.

## What happens

M2b's pipeline ruling is that `mrml` runs **once, at save**. The marked HTML and marked
plain text are stored, and the send path substitutes per-contact values into those stored
renditions. The document that was classified is the document that is sent. This is
correctness first — it also removes a full MJML render from a path that walks millions of
rows.

A stored rendition is usable only when its fingerprint still matches. The fingerprint
covers the authored document, **the brand kit as the emitter sees it**, the marker format
version, and the `mrml` and `url` crate versions.

So a single admin PUT to `/api/v1/workspaces/{id}/brand-kit` changes the brand-kit
component of the fingerprint for **every template in that workspace at the same moment**.
Each one's stored rendition stops being usable. The API answers `409 template-stale`, and
the detail says the cure is to save it again.

Nothing saves them again. There is no recompile path — not on write, not in the
background, not on demand.

## Why it matters at send time

An operator who changes their footer text or their accent colour has, in that one request,
made every template in the workspace unusable until a human opens each one and re-saves
it. A workspace with fifty templates needs fifty manual saves. Nobody is told this is
required; they find out when a send fails or a preview 409s.

M2c owns the send path, so M2c is where this lands. The obvious fix is the one the
pipeline ruling forbids: recompile at send. That would put `mrml` back on the
millions-of-rows path and, worse, would send a document that was never classified — the
exact defect the ruling exists to prevent. **Do not take that route.**

## The shape of an answer, for whoever picks this up

Three directions, none of them chosen here:

1. **Recompile on the brand-kit write.** The PUT compiles every template in the workspace
   against the new kit inside the same transaction, or enqueues that work. Bounded by the
   template count; slow for a large workspace, but the staleness window is zero and
   nothing is ever sent unclassified.
2. **Recompile on read, write back.** The first request that finds a stale fingerprint
   recompiles, classifies and stores, then serves. The send path still refuses to render a
   stale rendition — the recompile happens before it, not inside it — so the invariant
   holds. Cost is a latency spike on the first touch after an edit.
3. **Narrow the fingerprint.** Only the brand-kit keys a given template's emitter actually
   read go into its fingerprint. A footer-only edit then leaves templates that never
   reference the footer alone. This shrinks the blast radius; it does not remove it, and it
   is only sound if the emitter's key reads are captured exactly.

Whichever is taken needs a test that plants a brand-kit edit and proves a send still
produces a classified document — red first, against the planted defect.

## Where it was proved

`.superpowers/sdd/m2b/review-R7.md`, Important finding 3. The review also found, and R7
fixed under the same round, a related dead end: a template naming an archived custom field
could neither be previewed (409, "save it again") nor saved (422, "that field has been
archived"). That one is closed. This one is not.
