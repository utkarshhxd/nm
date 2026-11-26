<!-- SPDX-License-Identifier: AGPL-3.0-only -->

Status: closed by M2c R2 (5ba5611). The unsubscribe link is the product-owned absolute `{NM_PUBLIC_URL}/u/{token}` in the system footer, in both parts (shape 1 above).

# A relative link is unusable in the plain-text part

Found by the review of Task R5 of M2b, reported rather than fixed because the template layer
cannot fix it.

## What happens

The plain-text part writes a link as `text (url)`, or `Label: URL` for a button, taking the URL
exactly as the operator wrote it. For an absolute URL that is correct. For a relative one it is
not:

```
href = "/u/{{ country }}"   ->   Unsubscribe: /u/UK
```

`/u/UK` is not something a recipient can click, copy, or paste anywhere. In the HTML part the
mail client resolves a relative URL against nothing useful either, but the link at least carries
whatever base the client assumes; in plain text there is no base at all and the string is simply
dead.

## Why R5 is not where this gets fixed

Making it usable means turning the path into an absolute URL, which needs the workspace's public
base URL. That is application configuration. `src/template/` has no access to it and should not:
the template modules take a block tree, a brand kit and a contact, and giving them a config
dependency to solve this would be the wrong shape.

## Why it is not academic

ADR 0005's relative-URL ruling (F8, from R3's fix round) exists *because* M2c's unsubscribe links
and M2d's tracking links are expected to be written relatively — `render.rs` was changed
specifically to accept `/path` and `path` rather than failing them closed. So the ordinary,
intended case for a relative URL is exactly the case that renders as unusable text.

Every commercial email must carry a working unsubscribe mechanism. A text-only recipient getting
`Unsubscribe: /u/UK` has no mechanism at all.

## Where it should be fixed

**R7 or M2c, at the point the URL is known to be absolute.** Two shapes worth weighing:

1. **Absolutise before the template layer sees it.** Whatever builds the unsubscribe or tracking
   link emits an absolute URL. The template layer stays ignorant, which keeps its boundary clean.
2. **Pass a base URL into the renderer** and resolve relative URLs against it in both parts. More
   general, and more surface.

Shape 1 is the smaller change and matches how the rest of this milestone is drawn. Decide before
M2c writes the first unsubscribe link, not after.

## See also

- `src/template/text.rs` — where the link is written.
- `src/template/render.rs` — `check_static_url` / `UrlCheck::Relative`, the verdict that lets a
  relative URL through untouched.
- `.superpowers/sdd/m2b/review-R5.md` — Minor 3, where this was found.
