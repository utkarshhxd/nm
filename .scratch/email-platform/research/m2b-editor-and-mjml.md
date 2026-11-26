# M2b research — the editor, MJML through mrml, and personalisation

Research for M2b (templates, the editor, personalisation), before its spec is written. Branch
`m2b-templates-and-editor`. Scope from `.scratch/email-platform/m2-decomposition.md`; budgets from
`.scratch/email-platform/spec.md` §9.

Nothing in `src/`, `web/src/`, `Cargo.toml` or `web/package.json` was changed. Measurements were
taken in a throwaway directory that has been deleted; `deny.toml` was edited twice to test a gate
and reverted both times. `git status` at the end of this work shows only the `Cargo.lock` and
`Cargo.toml` modifications that were already present when it started (the `mrml` dependency).

Everything in §1 is a number this machine produced. §2–§6 lean on upstream sources, cited inline.

---

## 1. Can a block editor fit in 50 KB gzipped?

**Yes — if we write it. No — if we take any of the five editor frameworks.** The gap is not
marginal and no configuration closes it.

### 1.1 The budget, measured today

`pnpm --dir web check:bundle` on the current `dist`:

| | gzipped bytes | limit |
|---|---|---|
| Initial JS (one chunk, `assets/index-*.js`) | **109,724** | 153,600 |
| Largest lazy route (`contacts`) | **19,344** | 51,200 |
| All JS in `dist/assets`, summed | 220,847 | — |

Shell headroom is 43,876 bytes, and the editor is not allowed to use it: spec §9 names the editor
as one of the two things loaded on demand. The editor's budget is the per-chunk one: **51,200
bytes**.

### 1.2 What the foundations cost

Method: each candidate imported at the surface a block editor would actually use, bundled with
`esbuild 0.28.2` (`--bundle --minify --format=esm --target=es2022`), `process.env.NODE_ENV` defined
as `production`, `react`/`react-dom`/`react/jsx-runtime` marked external because the app already
ships them, then `gzip -9`. Re-exporting every imported binding keeps tree-shaking honest — nothing
is dropped as unused that the real editor would use.

Lexical publishes separate development and production builds behind export conditions, so it was
measured both ways. The production column is the fair one; the development column is what a build
that forgets `--conditions=production` actually ships.

| Foundation | version | gzipped | vs 51,200 |
|---|---|---|---|
| `prosemirror-model` alone | 1.25.13 | 14,669 | 29% of budget |
| `+ prosemirror-state` | 1.4.4 | 22,592 | 44% |
| `+ prosemirror-view` | 1.43.0 | **57,413** | **112% — over, with no product code** |
| ProseMirror, practical set (`+keymap +commands +history +schema-basic`) | — | **65,821** | 129% |
| Slate (`slate` + `slate-react` + `slate-history`) | 0.147.4 / 0.147.6 / 0.144.0 | **71,466** | 140% |
| Lexical core only (prod) | 0.53.0 | **61,354** | 120% |
| Lexical + rich-text + history + link (prod) | 0.53.0 | **81,373** | 159% |
| Lexical + `@lexical/react` + html (prod) | 0.53.0 | **86,287** | 169% |
| Lexical, same imports, **dev** build | 0.53.0 | 124,279 | 243% |
| TipTap `@tiptap/react` + `starter-kit` + `link` | 3.35.0 | **131,094** | 256% |
| TipTap, minimal hand-picked extensions (7) | 3.35.0 | 119,113 | 233% |
| `@dnd-kit` core + sortable + utilities | 6.3.1 / 10.0.0 / 3.2.2 | 14,785 | 29% |
| **Hand-rolled (measured, see §1.3)** | — | **4,467** | **9%** |

Three things to take from the table:

- **`prosemirror-view` is the whole problem.** Model plus state is 22,592 bytes and would fit
  comfortably. The `contenteditable` abstraction on top is another 34,821 bytes on its own and puts
  the stack over budget before a single NM component exists. Every framework that fits our needs
  carries an equivalent layer, which is why the four numbers cluster between 57 KB and 86 KB.
- **TipTap is ProseMirror plus a plugin architecture plus a React adapter**, so it can only be
  larger than ProseMirror. It is. Picking seven individual extensions instead of `StarterKit` saved
  11,981 bytes and left it 133% over.
- **Lexical's reputation for being small does not survive 0.53.** Its core alone, production build,
  is 61,354 bytes — over the whole chunk budget by itself. The often-quoted ~22 KB figure is from
  earlier versions. The dev/prod gap (61,354 vs 75,551 for core) is also a live footgun: a Vite
  build that does not resolve the `production` condition silently ships 40% more.

**There is no configuration that rescues a framework here.** The budget is 51,200 and the cheapest
usable framework stack is 57,413. Dropping to a code-split sub-chunk would be gaming the gate rather
than meeting it — see §1.5.

### 1.3 What hand-rolling costs: a number, not an estimate

A representative editor was written and measured. **639 lines, 24,545 source bytes of TSX** covering:

- six block types (text, heading, image, button, divider, spacer) with their full property shapes
- a reducer-based document with insert / remove / duplicate / move / patch and a 50-step undo-redo
  stack bound to the usual keys
- HTML5 drag-and-drop reordering with drop-target state, plus keyboard move-up / move-down
- a property panel with per-kind fieldsets — number, colour, select and URL fields, plus shared
  spacing and background controls
- inline bold / italic / underline / link over the Selection API: `surroundContents` with an
  `extractContents` fallback, an unwrap path, active-mark detection driven by `selectionchange`, a
  scheme allowlist on link insertion, and plain-text-only paste
- the MJML emitter: every block to its `mj-*` element, plus the `mj-head` / `mj-attributes` /
  `mj-body` envelope from the brand kit

Result, bundled exactly as the framework candidates were:

```
639 lines   24,545 source bytes
13,625 bytes minified
 4,467 bytes gzipped
```

**4,467 bytes.** The cheapest framework foundation is 12.9× that, and is over budget by itself.

### 1.4 How much editor fits in 51,200 bytes

The probe's compression ratio is **0.182 gzipped bytes per source byte**. That is not a one-off:
the same ratio falls out of this repository's own shipped routes.

| route source | source bytes | its chunk, gzipped | ratio |
|---|---|---|---|
| `segments.tsx` + `rule-builder.tsx` | 49,414 | 6,508 | 0.132 |
| `contact-detail.tsx` | 36,340 | 5,853 | 0.161 |
| `import-detail.tsx` | 37,902 | 6,893 | 0.182 |
| `lists.tsx` | 21,677 | 4,125 | 0.190 |
| `sending-domains.tsx` | 25,646 | 5,046 | 0.197 |
| `relays.tsx` | 16,025 | 3,545 | 0.221 |
| `contacts.tsx` | 51,366 | 19,344 | 0.377 |

`contacts` is the outlier because its chunk also carries `@tanstack/react-virtual`; the rest sit
between 0.13 and 0.22, bracketing the probe. Across all of `web/src`, 468,466 hand-written source
bytes over 12,071 lines average 38.8 bytes per line.

At 0.182, the 51,200-byte chunk budget buys roughly **281,000 source bytes — about 7,300 lines of
TSX**. At the pessimistic 0.22 it is still about 6,000 lines. The probe used 639 of them for the
core mechanics, and M2b's full scope (merge-tag picker, conditional blocks, raw HTML/MJML mode with
a textarea, preview pane with the dark toggle, link-checker results, brand kit form, template
gallery) is plausibly 2,000–2,500 lines in total — **roughly 17,000–21,000 gzipped bytes, a third
of the budget.** That leaves real headroom for the route to grow, which a framework-based route
would never have had.

### 1.5 One loophole in the gate, named so nobody uses it by accident

`web/scripts/check-bundle.mjs` measures **each chunk individually**, not a route's transitive
closure. A `manualChunks` split could put ProseMirror into two 30 KB chunks and the gate would pass
while the operator downloads 60 KB. The gate's own comment says the per-chunk limit exists "so the
contacts route cannot quietly grow without bound", which is the intent — but the mechanism does not
enforce it for a route that deliberately fans out. **If a framework is ever reconsidered, the gate
must be changed first to measure a route's closure, not its chunks.** Do not take the framework and
split around the number.

### 1.6 What we give up, honestly

Hand-rolling a rich-text layer is a known trap, and the measured number is only the admission fee.
What the frameworks are actually selling is correctness in the places `contenteditable` is hostile:
IME and composition events, selection normalisation across block boundaries, undo granularity
inside the browser's own editing stack, cross-browser paste, and a document model that cannot drift
from the DOM.

The design that makes the trade survivable, and that the probe is built around:

- **The document lives in React state, never in the DOM.** Block order, block kinds and every
  property are reducer state. The DOM is a projection.
- **`contenteditable` is never applied to the document.** It is applied per text block, and inside
  one it may only hold inline runs — `b`, `i`, `u`, `a`, text. No block creation, no splitting, no
  merging, no lists inside the editable region.
- Everything structural — add, delete, duplicate, reorder, nest — is a button or a drag handle
  outside the editable region, acting on state.

That boundary is what keeps the hard parts out. Multi-block selection, cross-block drag-select and
rich paste that creates blocks are the features we are declining, and the spec should decline them
in writing rather than discover them later. If M2b or a later milestone needs a document-wide
editing surface, that is the moment to revisit — and the moment to revisit the budget with it.

`@dnd-kit` at 14,785 bytes is affordable but was not needed: native HTML5 drag events plus keyboard
move buttons cost nothing and are more accessible than a pointer-only sortable. Take it only if
drag-between-columns arrives and the native API genuinely fails.

### 1.7 Recommendation

**Hand-roll it. No editor framework.** The deciding number: **`prosemirror-view`, the cheapest
usable editing layer in the field, is 57,413 gzipped bytes — 6,213 bytes over the entire route
budget before NM writes a line.** The hand-rolled equivalent measured 4,467.

---

## 2. mrml — is MJML through it safe to build on?

Yes, with two silent failure modes that the editor must lint for. `mrml` is a Rust reimplementation
of MJML, not a binding, and its fidelity is better than its documentation suggests.

Repository is **https://github.com/jdrouet/mrml** (`jolimail/mrml-core` is dead; only stale badge
URLs still point there). This branch pins `mrml 6.0.1`, released 2026-04-25, at
`Cargo.toml:38`, with `css-inline 0.20.2` in the lockfile.

### 2.1 Component coverage

Derived from the source tree at tag `mrml-v6.0.1`, not from a project statement — **mrml publishes
no compatibility matrix.** Every standard MJML component is present except **`mj-html-attributes`**
(and its `mj-selector` / `mj-html-attribute` children). That gap is open issue
[#426](https://github.com/jdrouet/mrml/issues/426) since 2024-05; the attempted PR was never merged
because mjml implements the feature by query-selecting its own rendered HTML, an approach mrml
rejects on performance grounds. It is unlikely to land. M2b should not design anything that needs
it.

The readme's "Missing implementations" section is **wrong at 6.0.1** — it claims `mj-style[inline]`
and `mj-include` are unimplemented, and both exist in the source at that tag. Treat the readme as
unreliable and the source as authority.

**Which MJML does it track?** Issue [#587](https://github.com/jdrouet/mrml/issues/587): v4. The
golden files at this tag were generated against mjml `^4.18.0`. So mrml 6.0.1 is **MJML 4.18
semantics**, and MJML 6.0.0-alpha's dark-mode feature (§3.3) does not exist for us.

### 2.2 There is a real conformance suite

`packages/mrml-core/resources/compare/success/` holds **157 `.mjml`/`.html` pairs**, and
`scripts/update-resources.js` generates each `.html` by running the **official `mjml` npm package**
over the input. These are genuine golden files, compared with an in-tree `html-compare` crate that
ignores whitespace, CSS property ordering and class ordering.

That generator script is also the only honest divergence list the project has. It normalises, before
comparing:

- `33.333333333333336` → `33.333332` — mrml rounds column-width percentages differently.
- generated element ids (accordion/navbar `for=`/`id=`, `mj-carousel-<id>`) → `00000000`; mrml
  removed randomness from id generation, mjml did not.
- empty `<style type="text/css"></style>`, empty `style=""`, empty `<div></div>` stripped.

None of these change rendering. They do mean byte-identical output with mjml is not a thing to test
for.

### 2.3 The biggest divergence: input mjml.io accepts that mrml rejects

This is the finding that matters for an editor, because operators will paste MJML they tested
elsewhere.

| issue | what fails |
|---|---|
| [#318](https://github.com/jdrouet/mrml/issues/318) | **HTML boolean and unquoted attributes fail to parse.** `<ol reversed>` needs `reversed=""`; `padding=0` needs quotes. Maintainer calls it "inconsistent with the mjml.io implementation" — caused by the XML-ish parser underneath. |
| [#499](https://github.com/jdrouet/mrml/issues/499) | **HTML inside `<mj-preview>` errors.** `<mj-preview><b>x</b></mj-preview>` works in mjml, returns `unexpected token at position 42..44` in mrml. |
| [#252](https://github.com/jdrouet/mrml/issues/252) | **`mj-raw` only accepts well-formed HTML.** `{% if foo < 5 %}` fails. Labelled **wontfix** — relevant because templating syntax inside `mj-raw` is exactly what an operator might try. |
| [#50](https://github.com/jdrouet/mrml/issues/50) | Unknown tags are handled poorly rather than failing cleanly. |

**Consequence for the spec:** raw MJML mode must surface mrml's parse errors verbatim, with
position, and "it previews fine on mjml.io" is explicitly not a support claim we make.

### 2.4 Two silent failures the editor must lint for

Both produce wrong output with no error, which is worse than a parse failure.

1. **`<mj-style>` inlining only triggers on the exact spelling `inline="inline"`.**
   `mj_style/mod.rs` at this tag is literally
   `matches!(self.attributes.inline.as_deref(), Some("inline"))`. `scope="inline"` or
   `inline="true"` falls through to a plain `<style>` tag — no inlining, no warning. Reported as
   [#668](https://github.com/jdrouet/mrml/issues/668).
2. **At-rules inside `<mj-style inline="inline">` are discarded outright.** mrml builds its inliner
   with `keep_at_rules` left at the `css-inline` default, which is `false`. Any `@media`,
   `@font-face` or `prefers-color-scheme` block an operator writes there is neither inlined nor
   re-emitted. It vanishes. **This one is load-bearing for dark mode**, which is written as an
   `@media` block.

### 2.5 Is `css-inline` enough?

The integration, `packages/mrml-core/src/mjml/render.rs:71-92`:

```rust
let inliner = css_inline::CSSInliner::options()
    .inline_style_tags(false)
    .keep_link_tags(true)
    .keep_style_tags(true)
    .load_remote_stylesheets(false)
    .extra_css(Some(Cow::Owned(inline_styles)))
    .build();
```

Four of those are the right calls and one is a gap:

- The inliner **runs only when the template contains an `mj-style[inline]`**. Otherwise zero cost
  and no HTML round-trip. Good default.
- `inline_style_tags(false)` + `keep_style_tags(true)` → MJML's own generated `<style>` blocks (the
  responsive `@media` rules) are not consumed and not removed. **Responsive behaviour survives
  inlining.** This is the thing that could have broken and does not.
- `load_remote_stylesheets(false)` → no network fetch from `<link>`. Correct for operator content.
- `apply_width_attributes` / `apply_height_attributes` — `css-inline` options that mirror CSS
  width/height onto `table`/`td`/`img` HTML attributes **specifically for Outlook** — are **not
  enabled** by mrml. MJML's own ghost tables already carry pixel widths, so this is probably
  redundant; it is not verified.

`css-inline`'s documented limits: `@media` blocks are dropped (a `style` attribute cannot hold an
at-rule), pseudo-classes like `p:hover` are never applied, invalid selectors are skipped silently,
and a stylesheet that fails to load aborts the whole inlining with an error.

**So: inlining is necessary and not sufficient, and MJML already knows that.** Media queries,
`@font-face`, `@import`, `:hover` and `prefers-color-scheme` cannot be inlined by anyone and must
stay in `<style>`.

### 2.6 What happens when a client drops the `<style>` block

**The premise that Gmail strips `<style>` needs correcting.** Gmail webmail has supported `<style>`
in head and media queries since
[September 2016](https://workspaceupdates.googleblog.com/2016/09/emails-optimized-for-every-screen.html).
Current state per [caniemail](https://www.caniemail.com/features/html-style/) (last tested
2023-07-27): Gmail desktop webmail **partial** — not supported inside `<body>`, and **the `<style>`
tag is capped at 16 KB**; Gmail iOS/Android **partial** — "not supported with non Google accounts"
(the GANGA case: the Gmail app reading a non-Google account over POP/IMAP); Gmail **mobile webmail
— not supported at all**.

The good news, verified from mjml's own golden output: **MJML degrades gracefully.** It is
mobile-first. Each column `<div>` carries `width:100%` *inline*, and the `@media (min-width:480px)`
rule *widens* it to 50% / 25%. Lose the style block and you get single-column, full-width, stacked,
readable content — no overflow, no invisible text. Classic Outlook is unaffected either way because
its ghost tables carry hard pixel widths.

What is actually lost without `<style>`: multi-column desktop layout, webfonts, hover states, and
all `prefers-color-scheme` theming.

### 2.7 Outlook on Windows, and the one gap MJML does not cover

Outlook 2007 onward uses Word's HTML engine for display. "New Outlook for Windows" uses WebView2
(Chromium) and renders like Outlook.com — so **"Outlook" is now two unrelated renderers under one
name**, and any preview UI that offers "Outlook" as a single option is lying.

Word-engine gaps, per caniemail: no `max-width` (2019 partial, `<table>` only), no
`background-image`, no `float`, no `border-radius`, no `box-shadow`, `@font-face` declared but
distant fonts ignored (fallback is Times New Roman unless `mso-generic-font-family` is set),
`<style>` must be declared before its rules are used.

mrml 6.0.1 reproduces mjml's countermeasures exactly — MSO ghost tables with hard pixel widths,
`<v:rect>`/`<v:fill>`/`<v:textbox>` for section backgrounds, `<v:image>` for hero backgrounds,
`<o:AllowPNG/>` and `<o:PixelsPerInch>96`, and the `.mj-outlook-group-fix` rule under
`[if lte mso 11]`.

**The gap is buttons.** MJML emits `border-radius` on a plain `<td>` and `mso-padding-alt` — it
never uses VML `roundrect`. **In classic Outlook every MJML button is a square rectangle**, and a
background image on a button never renders. That is a permanent, by-design divergence from what the
author sees in the browser preview, and it is the single most useful thing for the editor to say
out loud next to the button block.

---

## 3. Dark mode: what a preview may honestly claim

### 3.1 Three different behaviours, only one of which a preview can reproduce

1. **Respects `prefers-color-scheme`** — Apple Mail (macOS 12.4+, iOS 13+), Outlook macOS,
   Outlook.com, Outlook iOS/Android, Thunderbird 68.4, Samsung Email, Fastmail.
2. **Forcibly inverts, regardless of author CSS.** Per Litmus's chart (February 2025): *partial
   invert* — Outlook.com, Outlook iOS/Android, Office 365 Mac, Gmail app Android; *full invert* —
   Gmail app iOS, Outlook 2021 Windows, Office 365 Windows, Windows Mail.
3. **Leaves the email alone** — Gmail desktop webmail, AOL, Yahoo, and classic Outlook
   2007–2019 as far as the query is concerned.

`prefers-color-scheme` is **not supported on any Gmail surface**, nor on Outlook Windows 2003–2019,
Yahoo, AOL or ProtonMail
([caniemail](https://www.caniemail.com/features/css-at-media-prefers-color-scheme/), last tested
2023-03-08). Several of those clients do not merely ignore the query, they **rewrite it**:
`@media (prefers-color-scheme)` becomes `@media ( _filtered_a )` on AOL and Yahoo, `@media (false)`
on HEY, `@media ()` on Yahoo 2022-12.

Vendor tables disagree on specific clients — Email on Acid puts Apple Mail in the auto-inverting
column, contradicting Litmus. Treat any single table as approximate.

### 3.2 `<meta name="color-scheme">`

Per [caniemail](https://www.caniemail.com/features/html-meta-color-scheme/), **last tested
2026-09-16** — the freshest data point found anywhere here: Apple Mail macOS 16 **yes**, Thunderbird
**yes**, Gmail iOS/Android **partial, `light only` is the only value supported**, Gmail webmail
**no**, and **every Outlook: no**. The meta tags buy Apple Mail, Thunderbird and a `light only`
escape hatch on the Gmail apps. They buy nothing in any Outlook.

### 3.3 MJML's own dark-mode support does not reach us

mjml [PR #3068](https://github.com/mjmlio/mjml/pull/3068) added `--dark` modifier attributes across
the component set, merged 2026-09-03, shipped in **MJML v6.0.0-alpha.1** (2026-10-09). mrml 6.0.1
targets MJML 4.18 and has none of it, with no issue tracking it. **In our stack, dark mode is
entirely whatever the operator hand-writes into `<mj-style>`** — which also means §2.4's at-rule
discard bug will eat it if they write it inside `inline="inline"`.

### 3.4 What makes a preview actively misleading

A web preview can toggle `prefers-color-scheme: dark` and re-render. That reproduces exactly one of
the three behaviours, and it is the one **Gmail (every surface) and classic Outlook do not have**.

In order of harm:

1. **False green light.** The operator writes a `prefers-color-scheme: dark` block, the preview
   looks perfect, and Gmail iOS full-inverts the light-mode HTML with a different algorithm. The
   preview said "handled".
2. **False red light.** A template with no dark CSS renders identically in the preview, so the
   preview implies nothing happens — in precisely the clients that rewrite every colour.
3. **The inversion algorithms are not CSS and are not documented.** Outlook.com runs a
   `fixContrast()` pass and stashes originals in `data-ogsc`/`data-ogsb` attributes; Gmail's has
   never been published and has changed over time, and treats `background-color` and
   `background-image` differently. A `filter: invert()` approximation is a guess that diverges worst
   on logos, buttons and near-white surfaces — the things people actually care about.

**The honest design.** Label the toggle with what it is, naming both lists:

> Shows how your own `prefers-color-scheme: dark` CSS renders — matching Apple Mail, iOS Mail,
> Thunderbird and Outlook.com. It does **not** simulate the forced colour inversion in Gmail or
> Outlook for Windows, which rewrite colours with undocumented algorithms.

And prefer **static lint over simulation**, because the lint is the part that is actually true:
flag near-white and near-black backgrounds, text colours with no explicit background behind them,
transparent PNG logos with dark artwork, `background-image` on sections, buttons whose contrast
survives neither inversion, and `@media` written inside `<mj-style inline="inline">` (§2.4). Those
predict the real failures and need no algorithm. Act-On calls its own in-app dark preview "a helpful
approximation" and points users at Litmus; that is the framing to copy. Litmus and Email on Acid
sell dark-mode testing as screenshots from real clients, which is a different product from what we
can build.

---

## 4. Merge tags: syntax, fallbacks, and the injection answer

### 4.1 What the field uses

| Platform | Engine | Tag with fallback | Conditional |
|---|---|---|---|
| Mailchimp | own `*|…|*` macros | **no inline fallback exists** — default lives in the field definition | `*|IF:AGE >= 21|*` … `*|ELSE:|*` … `*|END:IF|*`; no AND/OR |
| listmonk | Go `html/template` + Sprig | `{{ .Subscriber.FirstName }}`; no documented default helper | `{{ if eq … }}…{{ end }}` |
| Mautic | own `{…}` tokens | `{contactfield=firstname|there}` | separate "Dynamic Content" items |
| Klaviyo | Django subset | `{{ first_name|default:'there' }}` | `{% if … %}…{% endif %}` |
| Customer.io / Braze | Liquid | `{{ customer.first_name | default: "there" }}` | `{% if %}…{% endif %}` |
| SendGrid | Handlebars | no `default` helper; `{{#if x}}{{x}}{{else}}there{{/if}}` | `{{#if}}/{{else if}}/{{#unless}}/{{#each}}` |
| Postmark | Mustachio | HTML-encodes by default, `{{{ }}}` raw | **none** — section-skipping only |
| Brevo | Django-ish | `{{ contact.FIRSTNAME|default:"there" }}` | `{% if %}/{% elif %}/{% endif %}` |

The design signal is louder than the syntax: **the two most conservative vendors deliberately avoid
a general template language.** Mailchimp has no inline fallback and pushes defaults into the data
layer. Postmark ships a language with no conditionals at all. Every platform that shipped a general
engine has a CVE below.

### 4.2 The CVEs

- **Mautic CVE-2026-9558**, CVSS **9.9** — SSTI in the theme engine; uploaded Twig templates
  rendered with no sandbox → RCE. Affects 1.3.0–4.4.20, 5.0.0–5.2.10, 6.0.0–6.0.8, 7.x. The
  advisory's mitigation is "restrict theme creation to highly trusted admins" — they gated it rather
  than fixed it.
- **listmonk CVE-2025-49136** (GHSA-jc7g-x28f-3v3h), ~9.0 — **template injection in campaign
  preview**. Sprig's `env` and `expandenv` enabled by default; a user with only `campaigns:get`
  extracts DB credentials, SMTP passwords and admin credentials. Fixed 5.0.2. This is the exact
  self-hosted-mailer failure mode.
- **Mailchimp for WordPress CVE-2024-8850** — the `{email}` merge-tag placeholder did not escape
  HTML; arbitrary HTML/JS rendered in a visitor's browser. The fix was literally "strip and escape
  all HTML from `{email}` tag replacements before output". The canonical merge-tag-becomes-XSS.
- **Handlebars CVE-2019-19919**, CVSS 9.8 — prototype pollution via `__proto__` in templates → RCE.
- **Uptime Kuma GHSA-vmmq-p5r9-qm38** — Liquid `{% render %}` SSTI in notification templates.
- listmonk CVE-2026-21483 — stored XSS → admin takeover, fixed 6.0.0.

The listmonk case separates two problems that get conflated: listmonk **did** have Go's contextual
auto-escaping on the HTML body. Its bug was the **function vocabulary**, not the escaping. Fixing
one does not fix the other, and both have to be designed for.

### 4.3 What actually stops a merge tag becoming an injection vector

**Two mechanisms, and both are needed.**

**(a) A closed vocabulary, so there is nothing to inject into.** Not Liquid, not Handlebars, not Go
templates, not Twig. A non-Turing-complete tag language we parse ourselves:

```
{{ first_name }}                        lookup against a fixed registry
{{ first_name | "there" }}              fallback is a LITERAL ONLY, never an expression
{{# first_name }} … {{/ first_name }}   present-block
{{^ first_name }} … {{/ first_name }}   absent-block
{{# country = "US" }} … {{/ country }}  equality against a literal
```

- The registry is system fields plus *this workspace's declared* custom fields. NM already has
  operator-controlled custom fields of several types, so the registry already exists. **An unknown
  name is a compile error at save time**, not a render-time surprise.
- A small fixed filter list at most (`upper`, `lower`, `title`, `date:<fmt>`). No `env`, no
  `include`, no `render`, no HTTP, no loops, no arbitrary expressions.
- Parse once at save, store the compiled AST, render the AST at send. **Never re-parse operator text
  at send time.**

Why this and not a sandbox: there is no function vocabulary to audit and none a dependency bump can
silently widen (listmonk's bug arrived through Sprig). No loops means no resource exhaustion, so no
CPU or instruction budgets, no render timeouts, no process isolation. No sandbox means no sandbox
escape. Render becomes a pure function of (compiled AST, one contact row) — trivially fuzzable and
trivially provable as workspace-isolated. The cost is that `{% for item in cart %}` is unavailable;
Mailchimp ships no AND/OR and Postmark ships no conditionals, so that cost is survivable.

**(b) Context-determined escaping, decided by the parser and never by the author.** The model to
copy is Go's `html/template`, which parses the template, works out each hole's context, and injects
the matching escaper — so `<a href="/s?q={{.}}">{{.}}</a>` gets URL encoding in the attribute and
HTML encoding in the link text, from one template.

Concretely, for NM:

- Parse the HTML **with a real HTML5 parser, together with the tags**, and classify every tag site:
  HTML text / quoted attribute value / URL in `href|src|action` / CSS / JS / comment / tag name /
  attribute name.
- Encode per context — entity encoding, attribute encoding with forced double quotes,
  percent-encoding for URL parameter positions, CSS hex.
- For OWASP's "dangerous contexts" — inside `<script>`, inside an HTML comment, in a **tag name** or
  an **attribute name** — **refuse to compile a tag there at all.** Do not escape; reject. Encoding
  does not save those positions.
- URLs: substitute, then parse with a real URL parser, canonicalise, check the scheme against an
  **allowlist** of `{http, https, mailto, tel}`, re-serialise. On failure render a dead `#` and log
  it, the way Go renders `#ZgotmplZ`. Never a blocklist and never a regex for the string
  `javascript:` — that loses to `java\tscript:`, `&#106;avascript:`, case changes, NUL bytes and
  leading newlines.
- **One renderer for the email HTML and the view-in-browser page.** The hosted page is a real
  browser on our real origin with no email-client sanitiser in front of it; a second code path there
  is where a merge-tag XSS turns into operator session theft.
- No typed-string escape hatch exposed to operators. A field that genuinely needs raw HTML (a
  signature block) should be a separately named field **type** run through an element and attribute
  allowlist — not an unescaped merge tag.
- **The plain-text part needs its own encoder.** listmonk runs its alt body through `html/template`,
  which delivers `&amp;` and `&#39;` to plain-text readers. Do not copy that.

**What the residual risk in email actually is.** Scripts are mostly dead in the inbox, so
XSS-in-the-mail-client is not the main threat. The real ones:

1. **Phishing with our reputation.** A merge tag that can fill an `href` lets whoever controls that
   attribute point the link anywhere, carrying our sending domain and our DKIM signature.
2. **Open redirect through our own click redirector.** This is the documented, in-the-wild abuse —
   Microsoft published a credential-phishing campaign built on open-redirector links, and
   Trustwave/LevelBlue names abused click-tracking domains belonging to email vendors. **If a merge
   tag can reach the target parameter of M2d's `/t/c/{token}` redirector, we have shipped an open
   redirect on a warmed, reputable domain.** M2b defines the tags; this is the constraint M2b owes
   M2c and M2d, and it belongs in M2b's spec rather than being discovered in M2d.
3. **HTML structure break-out.** An unescaped `"` closes an attribute; the payload in email is not
   script but layout — an overlay, a new `<a>`, a `style` that covers the real content.

### 4.4 Conditional blocks

The closed-vocabulary present/absent/equals form above covers what Mailchimp's `*|IF:|*` covers,
which is the bar operators migrating from Interspire will measure us against. Design note: a
conditional's *branches* are rendered content, so the escaping rules apply inside them unchanged —
a conditional must not become a way to smuggle a tag into a dangerous context.

---

## 5. The plain-text part

### 5.1 Where it plugs in

`src/mail/mod.rs:151` already does the right MIME thing:

```rust
Some(html) => builder.multipart(MultiPart::alternative_plain_html(o.text.to_owned(), html.to_owned()))?,
None       => builder.singlepart(SinglePart::plain(o.text.to_owned()))?,
```

`Outgoing.text` is caller-supplied, and `src/mail/mod.rs:292` already pins plain-first ordering with
a test. **M2b does not touch the builder; it has to produce that `text` string from the template.**
Plain-first is also what RFC 2046 §5.1.4 requires — "the plainest format first and the richest
format last".

### 5.2 Generate from the block tree, not from the rendered HTML

MJML's output is *all* layout tables. An HTML-side converter must guess which tables are layout and
which are data, and that guess is what produces garbage — Adobe's own documentation concedes the
guess fails and tells users to hand-write the text version.

From the block tree we have semantics for free: heading, paragraph, button (label *and* URL), image
(alt), divider, spacer, footer. No parsing, no heuristics, no tracking-pixel problem, no
escape/unescape round trip, and **merge tags survive as tags**, so they resolve per recipient in
both parts from the same substitution pass. Going via rendered HTML means the tags have already been
HTML-escaped once.

Stated honestly: **every tool surveyed generates from HTML** — Mailchimp, Premailer, Maizzle,
phpList — because they only have the HTML. Maizzle is the near-miss; it owns the source and still
runs `string-strip-html` over the compiled output. No source was found arguing explicitly for
source-side generation; the argument above is ours, built from the MJML-tables problem.

**Both paths are needed**, because raw HTML/MJML mode has no block tree. For that path, the rule
every working converter uses is **do not treat `<table>` as a table** — npm `html-to-text` ships a
default `table` formatter documented as "equivalent to `block`", which is exactly why it survives
MJML. Never default to a mode that draws ASCII borders or Markdown pipes.

### 5.3 Images, buttons, links

- **Images**: alt text when present, drop otherwise. **Never emit the `src`.** MJML emits spacers
  and tracking pixels with no alt, and the absence of alt is the drop signal.
- **Buttons**: label *and* URL — `Label: https://…`. From rendered HTML a button is indistinguishable
  from a link in a table cell; from the block tree you know. Another argument for source-side.
- **Links**: inline in brackets, the industry default (Premailer `Link ( url )`, `html-to-text`
  `text [url]`). **Not numbered footnotes** — the text part is read linearly, and forwards and
  quote-replies truncate, which separates a footnote list from its markers. Suppress the URL when
  the link text already is the href.

### 5.4 Deliverability: the commonly-quoted number is stale

Current SpamAssassin trunk `50_scores.cf`:

```
score MIME_HTML_ONLY        0.1
score MPART_ALT_DIFF        2.246 0.724 0.595 0.790
score MPART_ALT_DIFF_COUNT  2.799 1.483 1.199 1.112
```

**The widely-quoted 2.199 for `MIME_HTML_ONLY` is from 3.3.x and is no longer true — it is ~0.1.**
Do not build an argument on it.

`MPART_ALT_DIFF` fires when the text part shares **essentially no words** with the HTML part
(`madiff > 99`), the classic spam decoy. So:

- An auto-generated text part derived from the same content is **better than none** and does not
  risk `MPART_ALT_DIFF` — high word overlap puts the metric near zero.
- The genuinely dangerous artefact is a **boilerplate stub** — "This email is best viewed in HTML,
  click here" — which trips `MPART_ALT_DIFF` at 2.246.
- **Rule: generate the text part from the content, or ship HTML-only. Never ship a decoy stub.**

---

## 6. The link checker, and the SSRF it must not become

### 6.1 Everything worth checking is decidable offline

Per link, from the parsed document after one render pass with test data:

1. `<a>` with no `href`, empty `href`, `href="#"`, or an `href` that is entirely a merge tag.
2. **Unresolved merge tags** after a test render — `{{`, `}}`, `*|`, `|*` residue — in body, subject
   **and preheader**.
3. **Merge tags inside URLs that are not URL-encoded** — a value containing a space or `&` silently
   breaks the query string. A merge tag in the **host** part is always a bug and a phishing vector:
   reject at compile time, per §4.3.
4. Scheme in `{http, https, mailto, tel}`; bare `http` is a warning, anything else an error.
5. Host is not `localhost`, `*.local`, a dotless hostname, an IP literal, or private / loopback /
   link-local / CGNAT / metadata space — **checked statically, without resolving**.
6. `mailto:` addressee validity, `tel:` E.164 shape.
7. Leading or trailing whitespace inside `href` (a paste artefact); `&amp;` vs `&` in query strings.
8. UTM presence and spelling consistency across every link in the campaign.
9. **Tracking-token placement** — the click wrapper must wrap the final URL *after* merge
   substitution, and unsubscribe / `List-Unsubscribe` one-click URLs must **not** be wrapped.
10. Required links present: unsubscribe, physical address, view-in-browser.
11. `img` with no `alt` — feeds the plain-text generator (§5.3).

### 6.2 What it must refuse to do

**It must not fetch operator-supplied URLs from the server.** That is a server-side request forgery
primitive aimed at the operator's own network, and NM is self-hosted: the server usually sits inside
a private network, and on a cloud VM the first target is the instance metadata service at
`169.254.169.254` — the same address on AWS, GCP and Azure — which hands out role credentials. Any
operator with template-edit rights becomes the attacker, with no outside access needed.

A naive blocklist loses to all of these (OWASP SSRF Prevention Cheat Sheet): **DNS rebinding**, where
a second unchecked lookup between validation and connection changes the answer; **redirects** to a
private IP; **IPv6-mapped IPv4** (`::ffff:169.254.169.254`) and NAT64; hex, octal, dword and mixed
IP encodings; `0.0.0.0`; parser disagreement (`http://example.com\@evil.com`); and plain hostnames
that simply resolve into private space.

### 6.3 The defensible design

**Default and v1: static checks only. No fetch at any tier.** Everything in §6.1 is decidable from
the document, zero SSRF surface, and it catches the mistakes operators actually make. It does not
catch 404s — **say so plainly in the UI** rather than implying coverage we do not have.

If liveness checking is ever wanted, the order of preference is: (2) **fetch from the operator's
browser**, which uses their network position rather than the server's — with the caveat that
`no-cors` responses are opaque, so you can distinguish failure from success but not read a status
code; then (3) **an opt-in server-side fetch, off by default, behind an allowlist set at
server level that the web UI cannot change.** That last constraint is OpenProject's, and its stated
rationale is the right one for a multi-operator self-hosted product: allowing internal ranges is "a
server-level security decision, not an application-level one," because a UI setting "could be
changed by any administrator, which would undermine the protection." n8n ships the same shape but
with a documented precedence trap — hostname allowlists bypass its IP blocklist — which we should
not copy.

Tier 3, if it is ever built: resolve once and connect to the pinned IP, do not follow redirects,
`HEAD` with a short timeout, cap the response size, rate-limit per workspace, dedupe identical URLs,
and **never render the response body back to the operator** — that would turn SSRF into a read
primitive.

For contrast: Braze ships Connected Content, a server-side fetch of operator-supplied URLs at render
time, per recipient — and has SSRF **out of scope** in its bug bounty. That is the shape of hole we
would be opting into.

---

## 7. mrml's loaders, and the `cargo-deny` gate — verified

### 7.1 Which features carry the risk

Complete feature list at 6.0.1 (`packages/mrml-core/Cargo.toml`, matching docs.rs):

```
default  = ["json", "parse", "print", "render"]
json     parse     print     render     async     css-inline
local-loader
http-loader                  = ["http-loader-ureq"]     # alias
http-loader-base             = ["parse", "dep:url"]
http-loader-ureq             = ["dep:ureq", "http-loader-base"]
http-loader-blocking-reqwest = ["dep:reqwest", "http-loader-base", "reqwest/blocking"]
http-loader-async-reqwest    = ["dep:reqwest", "async", "http-loader-base"]
```

There is **no `orderedmap` feature**; `indexmap` is an unconditional dependency.

The risk is `<mj-include path="…">`, and mrml's own readme states the boundary: *"`mj-include` is
disabled by default: the default loader refuses every path."* With a loader enabled:

- **`local-loader`** reads from the filesystem, confined to a root directory after resolving `..`
  and symlinks.
- **`http-loader*`** fetches a remote URL. The readme is explicit: `HttpIncludeLoader::allow_all()`
  and `new_deny` "let a template make the server fetch any URL, including internal addresses
  (server-side request forgery)."

Two corrections to the framing in the task: **the alias to ban is `http-loader` *and* the four
concrete variants**, because `http-loader` merely re-exports `http-loader-ureq` and feature
unification can enable any of the others directly; and **the fetch happens at parse time, not render
time** — includes resolve during `parse_with_options`. So this is a parse-time SSRF, reachable the
moment operator MJML is accepted, which is earlier than "render" suggests.

`local-loader` belongs on the ban list for the same reason with a different payload: operator MJML
that reads server files.

Our current declaration — `features = ["parse", "render", "css-inline"]`, `default-features =
false` — enables none of them. **That is the correct posture. Keep it.**

### 7.2 `cargo-deny` can forbid them, and it was tested

The same mechanism `deny.toml` already uses for `lettre`'s `tracing` works on `mrml`. Both
directions were run in the dev container with `cargo deny check bans`:

**Positive case** — ban all six loader features, which are off:

```toml
[[bans.features]]
crate = "mrml"
deny = ["http-loader", "http-loader-base", "http-loader-ureq",
        "http-loader-blocking-reqwest", "http-loader-async-reqwest", "local-loader"]
```

→ `bans ok`. The stanza is accepted and the current tree passes.

**Negative control** — ban `css-inline`, which *is* enabled:

```toml
[[bans.features]]
crate = "mrml"
deny = ["css-inline"]
```

→ `bans FAILED`. The ban genuinely fires on this crate rather than being silently ignored.

`deny.toml` was restored after each run; `git diff deny.toml` is empty.

**Recommendation for M2b:** add the positive stanza, with a comment in the register the existing
`lettre` and RUSTSEC entries use — stating that these features turn operator-supplied MJML into a
parse-time SSRF and filesystem-read primitive, that feature unification can enable them from any
future dependency with nobody touching our `mrml` line, and that this is therefore a gate failure
rather than a comment asking people not to.

---

## 8. What I could not determine

- **mrml publishes no compatibility matrix.** The component list in §2.1 is read from the source
  tree and the 157-case golden corpus. A module existing is not attribute-level parity with mjml,
  and I did not verify parity per attribute.
- **I did not run mrml.** Everything in §2 is read from source, manifests and the project's own
  golden-file tooling at tag `mrml-v6.0.1`. In particular, **whether `css-inline`'s html5ever
  parse/serialise round-trip preserves the MSO conditional comments byte-exactly is untested.** It
  only triggers when a template uses `mj-style inline="inline"`, but if it corrupts the
  `<!--[if mso | IE]>` ghost tables it breaks every Outlook layout. This is cheap to test and M2b
  should: render one multi-column template with and without an inline style block and diff the
  conditional comments.
- **`css-inline`'s `!important` and specificity cascade behaviour is undocumented**, and the
  implementation was not read.
- **Outlook 2021's rendering engine** is not confirmed by any Microsoft source — only by vendor
  blogs treating it as the classic/Word engine.
- **caniemail's `prefers-color-scheme` data was last tested 2023-03-08**, nearly four years stale.
  The inversion-behaviour tables are vendor blogs from February 2025 and they contradict each other
  on Apple Mail and Gmail Android.
- **`data:` URIs in an `href`** across Gmail and Outlook: no authoritative source found. Reject them
  on that basis rather than on evidence.
- **No Mailchimp platform-level merge-tag XSS CVE was found** — every hit was a third-party
  WordPress plugin. Absence of a published CVE is not absence of the bug class.
- **Mailchimp inline fallback (`*|FNAME|Friend|*`) appears not to exist**, but this is an inference
  from documentation that describes only field-level defaults and `*|IF:|*` blocks.
- **SpamAssassin scores differ per scoreset and per installed version.** The numbers in §5.4 are
  trunk as of today, not a stable constant.
- The **framework bundle figures are marginal cost against a React app**, measured with esbuild. Our
  build is Vite/Rollup, whose output differs slightly; the differences are percentage points, not
  the 6 KB that would change the §1 conclusion.

---

## 9. What the spec has to decide

1. **No editor framework.** Hand-rolled, per §1.7. Write the §1.6 boundary into the spec as a
   constraint, not a note: the document lives in React state, `contenteditable` is per-text-block
   and holds inline runs only, and multi-block selection is explicitly out of scope.
2. **Fix the bundle gate before anyone reconsiders**, or state plainly that per-route closure is the
   real budget and the per-chunk check is its proxy (§1.5).
3. **Merge tags: closed vocabulary, parsed by us, compiled at save, context-escaped at render**
   (§4.3). Decide the exact syntax and freeze the filter list. Write the M2d constraint down now: a
   merge tag may never reach the target parameter of the click redirector.
4. **Plain text from the block tree, with an HTML fallback path for raw mode** whose `table`
   handling is "block" (§5.2). Never a stub.
5. **Link checker: static only in v1, and the UI says it does not check whether links resolve**
   (§6.3).
6. **Add the `[[bans.features]]` stanza for `mrml`** covering all six loader features (§7.2).
7. **Lint for mrml's two silent failures** — the `inline="inline"` spelling, and at-rules discarded
   inside an inline style block (§2.4). The second one eats dark mode, which is in M2b's scope.
8. **Label the dark preview with what it covers and what it does not** (§3.4), and spend the effort
   on the static lint rather than on simulating inversion.
9. **Say out loud in the editor that MJML buttons are square in classic Outlook** (§2.7), since no
   preview we can build will show it.

---

## Measured afterwards, against mrml 6.0.1 in this container

The research above flagged two unknowns as untested and serious. Both were cheap to settle, and
one of them is **not** what it looked like from reading the source.

### MSO conditional comments survive the inliner. The Outlook risk is not real.

A document with a raw `<!--[if mso]>…ghost table…<![endif]-->`, rendered twice — once with no
`inline="inline"` style so the inliner never runs, once with one so it does:

```
no-inliner : mso=true ghost=true count=8
inliner-on : mso=true ghost=true count=8
```

Identical. The html5ever round trip preserves conditional comments and their contents byte for
byte, and the count of mrml's own ghost tables is unchanged. **Nothing needs doing about this.**

Two facts read out of `mrml-6.0.1/src/mjml/render.rs:68-95` while confirming it, both worth
keeping:

- **The inliner only runs at all when the document contains an `<mj-style inline="inline">`.**
  No inline style, no round trip.
- mrml already builds the inliner with **`load_remote_stylesheets(false)`**, so the *inliner* is
  not a request-forgery vector. That is separate from the `http-loader` parse-time features, which
  still must be banned.

### Dark mode: at-rules survive in a plain `<mj-style>` and vanish in an inline one

The warning above — that `keep_at_rules: false` eats dark mode — is half right, and the half
matters:

```
at-rule in a PLAIN  mj-style survives : true
at-rule in an INLINE mj-style survives: false
```

A plain `<mj-style>` is preserved intact, because mrml builds the inliner with
`keep_style_tags(true)` and `inline_style_tags(false)`. An `<mj-style inline="inline">` has its
at-rules **silently dropped** — unsurprising, since an at-rule cannot be expressed as an inline
`style` attribute, but it disappears without an error.

**So the rule for M2b is: dark-mode CSS, and every other at-rule, belongs in a plain `<mj-style>`
and never an inline one.** That needs a lint rather than documentation, because the failure mode
is silent: the operator writes the rule, the light-mode preview looks correct, the rule is simply
gone, and nobody learns otherwise until a recipient's dark client renders unstyled mail.
