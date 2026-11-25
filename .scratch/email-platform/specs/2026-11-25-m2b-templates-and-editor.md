<!-- SPDX-License-Identifier: AGPL-3.0-only -->

# M2b — templates, the block editor, and MJML

**Milestone:** M2 Sending core, second slice.
**Precedes:** M2c (campaigns and the send pipeline), M2d (tracking and one-click unsubscribe).
**Research:** `.scratch/email-platform/research/m2b-editor-and-mjml.md` — every number quoted here
was measured in this repository's container, not estimated. Where this spec and that document
disagree, this spec wins; the research records how the ruling was reached.

---

## 1. Intent

An operator composes an email without writing HTML, and what they see is what a recipient gets.
The composition is stored as a block tree we own, rendered to MJML, rendered by `mrml` to the HTML
that leaves the building, and rendered again — by the *same* code — to the hosted view-in-browser
page M2d will serve.

The slice ends with a template a campaign can send. It does not send it.

## 2. Goals

- A drag-and-drop block editor producing MJML, plus a raw HTML/MJML mode for operators who want it.
- Templates and a per-workspace brand kit: logo, colours, fonts, footer.
- Merge tags with literal fallbacks, and conditional blocks.
- A dark-mode preview that is honest about what it covers.
- An auto-generated plain-text part, built from the block tree.
- A link checker that is useful offline and says so.

## 3. Non-goals for this slice

- Sending anything. No campaign, no recipient, no queue. M2c owns that.
- Click or open tracking, and the `/t/c/{token}` redirector. M2d owns those — but §5.4 below
  constrains them now, because the constraint is M2b's to impose and M2d's to obey.
- The view-in-browser *page*. M2b builds the renderer it will call; M2d routes to it.
- Spam scoring, the accessibility checker, and multi-language versions. The product spec lists all
  three under "later", and they stay there.
- Multi-block selection in the editor. §5.1 makes this a constraint, not an omission.

## 4. Anti-goals — failure even if it all works

1. **A preview that lies.** A dark-mode preview showing a rendering no real client produces is
   worse than no preview: the operator stops checking. §5.7.
2. **A merge tag that becomes an injection vector.** Two shipped products reached this exact
   failure — listmonk's CVE-2025-49136 leaked SMTP and database credentials through Sprig's `env`,
   and Mautic's CVE-2026-9558 is a 9.9-severity Twig SSTI. We do not ship a general template
   language, so there is nothing to escape from. §5.3.
3. **An open redirect on a warmed sending domain.** If a merge tag can reach the target of M2d's
   click redirector, the product hands an attacker a credential-phishing link carrying our
   reputation and our DKIM signature. This is documented, in-the-wild abuse. §5.4.
4. **A plain-text part that is a stub**, or that delivers `&amp;` and `&#39;` to plain-text
   readers. §5.6.
5. **A silent MJML failure.** Two of `mrml`'s divergences fail with no error at all. The operator
   writes the rule, the preview looks right, and only the recipient learns otherwise. §5.8.

## 5. Rulings

### 5.1 No editor framework. The editor is hand-rolled, and that is a measured decision

The per-route bundle budget is **51,200 bytes gzipped**. Measured, gzipped, today:

| Candidate | Bytes |
| --- | --- |
| `prosemirror-view` alone | 57,413 |
| `lexical` core | 61,354 |
| `slate` + `slate-react` | 71,466 |
| `@tiptap/react` + starter kit | 131,094 |

The cheapest usable framework exceeds the whole route budget before one line of our code exists.
A representative hand-rolled block editor — six block types, drag reorder, a property panel,
Selection-API inline formatting, an MJML emitter and undo, 639 lines of TSX — measured
**4,467 bytes gzipped**, 12.9 times cheaper than the cheapest framework. At this repository's
observed compression ratio the budget buys roughly 7,300 lines of TSX.

**The boundary this buys, written as a constraint and not as a regret:**

- The document lives in React state. It is the single source of truth.
- `contenteditable` is **per text block** and holds inline runs only — bold, italic, link, and a
  merge tag as an atomic inline token.
- **Multi-block selection is out of scope.** Selecting across two blocks, and any operation on such
  a selection, is not supported and must not half-work. Block-level operations act on one block.
- Undo is per-document and application-level, not the browser's.

An implementer who finds themselves wanting a framework should re-read this table, not re-litigate
it. A framework lands only together with a budget change, argued separately.

### 5.2 The bundle gate measures chunks, and the honest budget is per-route closure

`web/scripts/check-bundle.mjs` checks each emitted chunk against the budget individually. A
`manualChunks` split would therefore let a framework through in two pieces, each under the limit,
with the gate green and the route twice as heavy.

**Ruling:** the real budget is the **transitive closure of a route's chunks**, and the per-chunk
check is a proxy for it. M2b fixes the gate to sum a route's closure rather than leaving the
loophole documented and open — the measurement above is the entire basis for §5.1, and a gate that
can be walked around by accident does not protect it.

### 5.3 Merge tags: a closed vocabulary we parse, compiled at save, escaped by context at render

Not Liquid, not Handlebars, not Twig, not Go templates. The frozen grammar:

```
{{ first_name }}                        lookup against the registry
{{ first_name | "there" }}              fallback — a LITERAL ONLY, never an expression
{{ first_name | upper }}                filter from the fixed list below
{{# first_name }} … {{/ first_name }}   present-block
{{^ first_name }} … {{/ first_name }}   absent-block
{{# country = "US" }} … {{/ country }}  equality against a literal
```

The filter list is **frozen at `upper`, `lower`, `title`, `date:<fmt>`**. There is no `env`, no
`include`, no `render`, no HTTP, no loops, no arbitrary expressions. No loops means no resource
exhaustion, so no instruction budget, no render timeout and no process isolation. No sandbox means
no sandbox escape.

**The registry** is the system fields plus *this workspace's* declared custom fields, read through
the existing `custom_fields::definitions_for(db, workspace_id)`.

- **An unknown name is a compile error at save time**, not a render-time surprise.
- **An archived field may not be named by a newly saved tag, and an already-compiled tag naming one
  keeps rendering.** This is the rule `definitions_for`'s own doc comment already states for
  contact values: archiving must stop fresh uses without destroying existing ones. A template
  saved before the field was archived still has the contact's value to render, and silently
  blanking it would change what a live template sends.
- Parse once at save, **store the compiled AST**, render the AST at send. Operator text is never
  re-parsed at send time.

**Escaping is decided by the parser, never by the author.** The model is Go's `html/template`:
parse the HTML *together with* the tags, classify each tag's context, and apply the matching
encoder — entity encoding in text, attribute encoding with forced double quotes, percent-encoding
in a URL parameter position, CSS hex in CSS.

For the contexts OWASP calls dangerous — inside `<script>`, inside an HTML comment, in a **tag
name**, in an **attribute name** — **refuse to compile a tag there.** Do not escape; reject.
Encoding does not save those positions.

A conditional's branches are rendered content, so every rule above applies inside them unchanged. A
conditional is not a way to smuggle a tag into a dangerous context.

There is no typed-string escape hatch for operators. A field that genuinely needs raw HTML is a
separately named field *type* behind an element and attribute allowlist — a thing M2b does not
build and does not foreclose.

### 5.4 URLs are parsed, allowlisted, and never reach a redirector target — the debt M2b owes M2d

After substitution, every URL is parsed with a real URL parser, canonicalised, checked against the
scheme allowlist **`{http, https, mailto, tel}`**, and re-serialised. On failure it renders a dead
`#` and logs. Never a blocklist, and never a regex for the string `javascript:` — that loses to
`java\tscript:`, `&#106;avascript:`, case changes, NUL bytes and leading newlines.

**The constraint M2d inherits, stated here because M2b is where tags are defined:** a merge tag may
never reach the target parameter of the click redirector. M2d's `/t/c/{token}` must resolve its
destination from a stored, workspace-scoped row — never from anything an operator's tag can shape.
Discovering this in M2d is discovering it too late.

### 5.5 One renderer serves the email and the hosted page

The HTML that leaves the building and the HTML of M2d's view-in-browser page come from the same
code path. The hosted page is a real browser on our real origin with no email-client sanitiser in
front of it; a second rendering path there is exactly where a merge-tag XSS becomes operator
session theft.

### 5.6 The plain-text part is generated from the block tree, and has its own encoder

Not from the rendered HTML, and never a stub. The block tree knows a heading is a heading and a
button is a call to action; the rendered HTML knows only tables.

- Raw-mode templates have no tree, so they take an HTML fallback path whose `table` handling is
  **"block"** — tables become block-level text, not a collapsed line.
- **The plain-text encoder is its own.** listmonk runs its alternative body through `html/template`
  and delivers `&amp;` and `&#39;` to plain-text readers. We do not copy that: plain text receives
  characters, not entities.

### 5.7 The dark preview states what it covers; the effort goes into the lint

Three distinct client behaviours exist and only one is reproducible in a browser preview. A preview
that silently implies it covers all three is the §4.1 anti-goal. The preview is **labelled with
what it covers and what it cannot**, and the engineering effort goes into the static lint of §5.8,
which catches a real and silent failure, rather than into simulating colour inversion, which cannot
be done honestly.

The editor also says plainly that **MJML buttons render square in classic Outlook on Windows**. No
preview we can build will show it, so the editor says it in words.

### 5.8 Two mrml failures are silent, so both get a lint

Measured against `mrml 6.0.1` in this container:

1. **CSS at-rules survive in a plain `<mj-style>` and are silently dropped from an
   `inline="inline"` one.** `mrml` builds its inliner with `keep_style_tags(true)` and
   `inline_style_tags(false)`. A `@media (prefers-color-scheme: dark)` rule written in an inline
   style block vanishes with no error — and dark mode is in this slice's scope. **Dark-mode CSS
   must live in a plain `<mj-style>`, and the lint enforces it.**
2. **The `inline="inline"` spelling itself.** Any other value silently does nothing.

Also measured, and worth recording because it closes a risk rather than opening one: **MSO
conditional comments survive the CSS inliner intact** — identical counts with and without the
inliner, ghost tables whole. The Outlook corruption risk raised in research is not real.

### 5.9 The link checker is static in v1, and the UI says so

Everything worth checking is decidable offline: malformed URLs, unsupported schemes, an empty
`href`, a merge tag in a link position, a `mailto:` with no address, an unencoded space. The
checker does **not** fetch anything — a checker that resolves operator-supplied URLs is a
request-forgery engine with a friendly name, reaching whatever the server can reach.

The UI states that links are checked for form, not for whether they resolve. An operator who
believes a dead link was checked is worse off than one who knows it was not.

### 5.10 mrml's loader features stay denied

Already landed in `deny.toml` on this branch and verified in both directions. `mj-include` resolves
paths and addresses **at parse time**, so with any loader feature enabled, operator-supplied MJML
chooses what the server fetches. All six are denied — the alias, the base machinery, the three
clients, and `local-loader`, which is the same problem aimed at files. Rendering is not the risk:
`css-inline` already builds with `load_remote_stylesheets(false)`.

### 5.11 The ADR enumeration is re-run, not assumed

`docs/adr/0005-one-lock-order-for-every-path.md` is re-run for this slice's new tables, as M2a did.
The test is **"does this statement take a row lock on, or wait on, anything else"** — not "is it one
statement". M2a's `form_lists` reached the right ruling by the wrong reasoning once; the ADR's line
45 names that failure, and re-running the enumeration is how it is not repeated.

## 6. What gets built

**Schema**
- `templates` — workspace-scoped, `kind` in `('blocks','mjml','html')`, the block tree as `jsonb`
  for `blocks`, the source text for the raw kinds, the compiled merge-tag AST, and the rendered
  artefacts cached with the input hash that produced them.
- `brand_kits` — one per workspace: logo, colour palette, font stack, footer content.

**Server**
- The merge-tag parser, compiler and registry check.
- The context-classifying HTML renderer, shared by email and hosted page (§5.5).
- The MJML emitter from the block tree, and the `mrml` render path.
- The plain-text generator from the tree, plus the raw-mode fallback.
- The static link checker.
- The two mrml lints.
- Template and brand-kit CRUD, workspace-scoped, under the existing role gates.

**Web**
- The hand-rolled block editor, within §5.1's boundary.
- Raw HTML/MJML mode.
- The brand-kit screen.
- Preview: desktop, mobile, dark (labelled per §5.7), and the plain-text part.
- The link-checker and lint results surfaced where the operator is composing, not on save.

**Gate**
- `check-bundle.mjs` summing a route's transitive closure (§5.2).

## 7. Correctness invariants for this slice

Each needs an automated test, and a test that passes on broken code is not evidence.

1. Rendering is a **pure function of (compiled AST, one contact row)**. Same inputs, same bytes.
2. A merge tag in a dangerous context — `<script>`, an HTML comment, a tag name, an attribute name
   — **fails to compile**. It is never escaped and shipped.
3. Every URL reaching the output has a scheme in `{http, https, mailto, tel}` or is `#`. The test
   includes `java\tscript:`, `&#106;avascript:`, mixed case, a leading NUL and a leading newline.
4. A tag naming a field outside this workspace's registry fails at save. A tag naming an
   **archived** field fails at save; one compiled before archiving still renders.
5. The plain-text part contains **no HTML entities** and no markup.
6. A `@media` rule in an `inline="inline"` style block is **caught by the lint**, not silently lost.
7. The rendered email HTML and the hosted-page HTML for the same template and contact come from one
   code path — verified by construction, not by comparing two outputs.
8. Render is workspace-isolated: a tag cannot read another workspace's field definitions, and
   requesting another workspace's template returns 404.

## 8. Acceptance — the slice is done when

- An operator builds a multi-block email in the editor, saves it, and sees desktop, mobile, dark
  and plain-text previews of it.
- A brand kit set at workspace level reaches that email without the operator restating it.
- A merge tag with a literal fallback renders a real contact's value, and the fallback when absent.
- A conditional block appears and disappears on a real contact row.
- Every §7 invariant has a test that fails when the behaviour is removed.
- `cargo deny`, `cargo clippy`, `cargo fmt --check`, the Rust suite, `pnpm typecheck`,
  `pnpm check:bundle` and the web suite are green on the branch, run serialised.
- The whole-branch review is clean, or its findings are parked with a recorded ruling.

**Not required for M2b:** the six live-tenancy answers of `issues/43`. They gate M2 *sign-off*, not
this slice. M2b must not assume any particular answer to them.

## 9. Known constraints this slice inherits

- Every tenant table carries `workspace_id`; another workspace's resource returns **404**, never
  403, and never the data.
- Secrets never appear in logs, audit metadata or API responses.
- Windows cannot link this project: every `cargo`, `sqlx` and `cargo deny` command runs inside the
  Linux dev container, compose project `-p nm`.
- `issues/40` stands: the dev database refuses `sqlx migrate` because `0023` was edited after being
  applied. A green suite coexists with a migrate that cannot run — M2b adds migrations, so this is
  in the path and is resolved rather than worked around.
- `mrml` targets MJML 4.18 and hard-errors on input mjml.io accepts; `mj-html-attributes` is
  missing. The editor emits only what `mrml` accepts, and raw mode surfaces the parse error.
