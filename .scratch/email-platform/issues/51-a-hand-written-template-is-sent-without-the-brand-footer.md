# 51 — A hand-written template would be sent without the brand footer and its postal address

Status: closed by M2c R2 (5ba5611). Option 1: the system footer, with the postal address for raw kinds and the unsubscribe link for every kind, goes in after `mrml` and before classification, at the real end of body; a document whose end is not markup fails with `FooterHidden`.

**Found by:** the M2b final whole-branch review (finding M9).
**Blocks:** M2c sign-off. It is a compliance gap on the send path, not an M2b defect.

## What is true

The brand kit's footer carries the sender's postal address. Contract §9 requires that address,
and commercial-email law expects it in every marketing message (CAN-SPAM requires a valid
physical postal address).

A `blocks` template gets the footer, because the MJML emitter appends it. A hand-written
`mjml` or `html` template does not: the operator's source is compiled as written
(`src/template/mjml.rs`, the raw-kind arm), and the brand footer is never appended.

So a hand-written template can be saved, previewed and, once M2c sends, delivered with no
postal address in it.

## What M2c must decide

Pick one, and test it red-first against a hand-written template with no address:

1. **Append the footer to raw kinds too.**
   - For `mjml`, insert it as a final section.
   - For `html`, insert it before `</body>`.
   - The cost is that the operator's layout can be disturbed.
2. **Refuse to send a raw template that lacks the address.**
   - Check the compiled text part for the brand kit's postal address and block the send
     with a clear reason.
   - The cost is false refusals when the address is formatted differently.
3. **Require an explicit footer merge tag** in raw templates (`{{ footer }}`), and refuse to
   save without it.

Whichever is chosen, the send path must not deliver a marketing message without the postal
address. That is a legal requirement, not polish.
