<!-- SPDX-License-Identifier: AGPL-3.0-only -->

Status: open

# A block template cannot express dark-mode styling at all

Raised by Task R4 of M2b, which observed that `to_mjml` emits no dark-mode CSS because the brand
kit's palette has no dark colours.

## What exists and what does not

M2b ships three dark-mode things, and they are all correct on their own terms:

- A **dark preview** (W4) that forces `@media (prefers-color-scheme: dark)` on, labelled with the
  three client behaviours it can and cannot show.
- A **lint** (R6) for the silent `mrml` failure where an at-rule inside `<mj-style inline="inline">`
  is discarded.
- **Raw-mode templates**, where an operator writes their own `<mj-style>` and can put whatever
  dark CSS they like in it.

What is missing is the middle: a **block** template has no way to express dark styling. The
operator does not write CSS, the brand kit holds one palette with no dark variants, and `to_mjml`
therefore emits no `@media (prefers-color-scheme: dark)` block.

## Why this is not a bug in M2b

Nothing is wrong. The preview will honestly report "this email has no dark styles" for every block
template, which is true. The spec's editor goal is "dark-mode preview", and the preview exists.

But the honest report is going to be the *same* report every time, for every block template any
operator ever builds, and an operator who wants dark styling has no action available to them
except switching to raw mode and hand-writing MJML — which is the thing the block editor exists to
avoid.

## What picking this up looks like

Most likely a second set of palette colours on the brand kit (`palette_dark`, same keys, same
frozen `#rgb`/`#rrggbb` format per contract §8), with `to_mjml` emitting a plain `<mj-style>` —
**not** `inline="inline"`, which is where mrml silently discards at-rules — containing a
`@media (prefers-color-scheme: dark)` block that swaps them.

Three things that must come with it:

- The brand-kit screen needs a dark palette editor, and the contrast warning must check the dark
  pair as well as the light one.
- W4's preview already forces the media query on, so it would start showing something real; its
  "no dark styles" notice must stop firing for templates that now have them.
- The emitted CSS must survive `mrml`'s inliner — see M2b spec §5.8, which is why the plain
  `<mj-style>` is load-bearing rather than stylistic.

## Related

- M2b spec §5.7 (what a preview may honestly claim), §5.8 (the two silent mrml failures).
- `.superpowers/sdd/m2b/contract-block-tree.md` §8 — the palette's frozen keys and colour format.
- `.scratch/email-platform/research/m2b-editor-and-mjml.md` §3 — the three client behaviours, and
  §3.3, which records that MJML's own `--dark` attributes shipped in 6.0.0-alpha.1 and that mrml
  6.0.1 targets MJML 4.18 and has none of them.
