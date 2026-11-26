# 52 — Gmail clips a large email, and the clip hides the footer

**Found by:** the M2c task R2 review.
**Blocks:** nothing. The `List-Unsubscribe` and `List-Unsubscribe-Post` headers keep one-click
unsubscribe working whatever the body shows.

## What is true

Gmail clips any message whose HTML is larger than about 102 KB. The reader sees a
"[Message clipped] View entire message" link instead of the rest of the body.

M2c puts the system footer at the real end of the body. That footer holds the postal address
and the visible unsubscribe link. In a large email, the footer is therefore exactly what gets
clipped. The message still carries the address, so it is compliant, but the reader cannot see
it without clicking through.

## What to do, later

- Add a lint **warning** to the preview: "This email is over 102 KB of HTML. Gmail will clip
  it, and the footer with your unsubscribe link and postal address will be hidden."
- Count the size on the frozen send rendition, because that is what is sent, using the longest
  realistic substitution.
- Do not refuse to save or send. Large emails are legitimate, and the header unsubscribe still
  works.
