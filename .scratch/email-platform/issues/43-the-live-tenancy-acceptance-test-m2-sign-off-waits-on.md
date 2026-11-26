Status: needs-triage

# The live-tenancy acceptance test M2 sign-off waits on, and nothing local can answer

M2a's spec §8.1 asks six questions that only a real OCI tenancy can answer, and M2c adds a seventh
(below). They lived in the spec's open questions, which is the right place to *record* a question
and the wrong place to track work: a spec is read once per milestone and a worklist is read before
sign-off. Merging the M2a branch is fine; **declaring M2 accepted is not, until these seven have
answers.**

Question 6 is the one with teeth. Invariant 5 — "an accepted suppression reply is recorded as a
failure" — rests on a string literal in `src/relay/transport.rs`
(`SUPPRESSION_NOTICE = "suppression for user"`), matched against a reply nobody on this side has
ever seen. Everything else here decides a default; that one decides whether the product lies.

## The test

Send a seed message through the real OCI tenancy to a Gmail address and a Yahoo address, read the
raw headers on arrival, and record the answers verbatim rather than in summary. Then send one more
to an address already on OCI's suppression list.

## The seven questions, what each decides, and what the bad answer costs

1. **Does OCI's `DKIM-Signature` `h=` include `List-Unsubscribe` and `List-Unsubscribe-Post`?**
   Decides whether relay signing is enough for one-click unsubscribe. If it does not, a receiver
   cannot trust the headers RFC 8058 requires it to act on, and relay signing stops being a
   supported mode for bulk mail through this relay.
2. **Are client-supplied headers signed at all?** The general form of 1. If the answer is no, the
   `Feedback-ID` and `Message-ID` the product sets are outside the signature too, and every
   deliverability signal the product puts on a message is unauthenticated.
3. **Does OCI keep our HTTPS `List-Unsubscribe` pair, or replace it?** Test a `mailto:`-only
   header as well — Oracle's own documentation contradicts itself here. If OCI replaces the pair,
   a person clicking unsubscribe is recorded in Oracle's list and in nothing of ours: no
   `consent_log` row, nothing a preference centre or a GDPR export can see, and the product's
   unsubscribe record is quietly wrong. `src/worker/confirmation.rs` already names this as the
   failure it is built to avoid.
4. **Does a signature we apply before submission survive OCI's handling?** Decides whether
   `Signing::App` works at all through this relay. If OCI mutates headers after accepting, our
   signature breaks on every message and the planned workaround for 1 and 2 does not exist — which
   would leave no mode that both signs and keeps the unsubscribe headers.
5. **Which `d=` appears when customer DKIM is configured** — ours, Oracle's, or both. Decides what
   the sending-domain screen should tell an operator to expect, and whether DMARC alignment runs
   off the customer's domain or Oracle's. A wrong answer here is a domain that passes our DNS
   checks and still fails the receiver's DMARC.
6. **The exact text of a suppression reply, recorded verbatim.** Send to an address already on
   OCI's suppression list. The documented form is `254 4.7.1 ... Suppression for user ...`, and
   lettre parses `254` as a success, so the phrase match in `src/relay/transport.rs` is the only
   thing between that reply and a `Sent` row. **If the real reply omits the phrase, the product
   tells an operator that someone received mail who did not** — spec §4's anti-goal, reached
   silently, with no failed row and nothing to look at. Record the full reply line; a paraphrase
   is not an answer, because what is being verified is a substring.

7. **Does a Gmail one-click unsubscribe on a real campaign message land in `consent_log` with
   `source_ref` = the campaign id?** Added by M2c. Every campaign message carries an https
   `List-Unsubscribe` pair pointing at `{NM_PUBLIC_URL}/u/{token}`, and the token's `ref_id` is the
   campaign id, so the row that a one-click writes should name the campaign (and `token_jti` the
   message). The in-process tests prove that for a POST of the exact URL a message carries; they
   cannot prove that Gmail's own button POSTs that URL, to that host, with that body, through
   whatever it does to the header on the way. Send a campaign of one to a Gmail address, press
   Gmail's "Unsubscribe", and read the `consent_log` row. A row with no `source_ref`, or no row,
   means one of Q3's failure modes has reached campaigns, and the campaign's own unsubscribe count
   is wrong with nothing to say so.

**M2c merged without these. M2 sign-off waits on all seven.**

## What a bad answer changes in code

- 1 or 2 no: `Signing::App` becomes the default and the documented mode for OCI, not a switch.
- 3 replaced: either the relay is unusable for list mail, or the unsubscribe record has to be
  reconciled from Oracle's side, which the product has no path for today.
- 4 no: 1 and 2's workaround is gone; escalate to Oracle before M2b.
- 6 different text: change the constant to the real phrase **and** add the reply verbatim to
  `tests/worker_confirmation.rs`'s suppression test, which today asserts against the documented
  wording rather than an observed one.
- 7 no row: Gmail's button did not reach `/u/{token}`, which is 3's failure reached campaigns, and
  3's answer decides what changes. A row without `source_ref`: the token's `ref_id` (the campaign
  id, set where `src/campaign/send.rs` issues the link) was lost on the way to
  `src/public/unsubscribe.rs`, which copies it to `source_ref`; fix that path, and add the request
  Gmail actually sent to `tests/campaign_send.rs`'s one-click test.

## Related

- M2a spec §8.1 (the source of the six), §4 (the anti-goal question 6 guards), §7 invariant 5.
- `.scratch/email-platform/spec.md` open question 4, which records that this is open. This issue
  is where the doing lives; that stays as the record.
- `src/relay/transport.rs` — `SUPPRESSION_NOTICE`, and the `254` classification around it.
