# M2b — templates, the block editor, and MJML: implementation plan

**Goal:** An operator composes an email in a block editor, personalises it with merge tags that
cannot become an injection vector, and sees honest previews of it — HTML, dark, and plain text.

**Architecture:** Two lanes with disjoint file sets, running in parallel worktrees. **Lane R**
(`src/`, `migrations/`, `tests/`) owns the merge-tag language, the escaping renderer, the MJML
emitter, the plain-text generator, the lints and the API. **Lane W** (`web/`) owns the bundle gate,
the hand-rolled block editor, the previews and the brand-kit screen. The single seam is
`web/src/api/schema.d.ts`, generated from the Rust OpenAPI: Lane W builds everything that does not
need it first, and wires to real endpoints at W6, after R7 lands.

**Tech Stack:** Rust (axum, sqlx, Postgres 16), `mrml 6.0.1` with its loader features denied,
React 19 + TanStack Router, Vite, no editor framework.

**Spec:** `.scratch/email-platform/specs/2026-11-25-m2b-templates-and-editor.md`

## Lanes and branches

- Integration branch: `m2b-templates-and-editor`.
- Lane R works directly on it (the main working tree).
- Lane W works on `m2b-web` in a worktree, merged into the integration branch at the end of each
  completed W task, so divergence never exceeds one task.
- **Never dispatch two implementers into the same lane at once.** One per lane, two in flight.

## Global Constraints

- Every tenant table carries `workspace_id`. Another workspace's resource returns **404** — never
  403, and never the data.
- Secrets never appear in logs, audit metadata or API responses.
- Windows cannot link this project. Every `cargo`, `sqlx` and `cargo deny` command runs **inside the
  Linux dev container**, compose project `-p nm`, via `scripts/dev.sh`.
- Never stop, modify or remove the containers `oci-email-postgres` or `marketing-automation`.
- Gate commands run with `set -eo pipefail`. Without it, `cmd | tail` reports `tail`'s status and a
  failing build exits 0.
- In a parallel lane, use `cargo fmt --check`, never `cargo fmt --all` — the latter rewrites the
  other lane's half-written files.
- Per-route gzipped JS budget: **51,200 bytes**. Shell budget: 150 KB.
- The merge-tag filter list is frozen at `upper`, `lower`, `title`, `date:<fmt>`. Adding one is a
  spec change, not an implementation choice.
- URL scheme allowlist, frozen: `{http, https, mailto, tel}`.
- No new dependency without a `deny.toml` licence check. `mrml`'s six loader features stay denied.
- TDD: the failing test is written and *run* before the implementation. A test that passes on
  broken code is not evidence.

## Review Focus

Five input classes the spec implies that no task's happy path exercises. Each has a test pinned to
the task that owns the code.

1. **A merge tag inside an attribute that `mrml` itself rewrites** (`href` on `mj-button`,
   `src` on `mj-image`). If context classification runs on the MJML rather than on `mrml`'s output
   HTML, the classifier judges a position that no longer exists. Pinned to R3.
2. **A contact value that itself looks like a merge tag** (`{{ email }}`, `{{# admin }}`). The
   substituted value must never be re-scanned — one pass, or a contact controls the template.
   Pinned to R2.
3. **Deeply nested conditionals.** There are no loops, so there is no runaway render, but a
   recursive renderer still has a stack. A 10,000-deep nest must be refused at compile, not
   discovered at render. Pinned to R2.
4. **Unicode in a text and a URL position** — RTL override `U+202E`, zero-width joiner, a combining
   mark, an IDN host. The encoder must not produce mojibake, and the URL parser must punycode the
   host rather than emit raw Unicode. Pinned to R3.
5. **A custom field whose `kind` changes after a template compiled against it** (text → select, or
   archived → purged). The stored AST holds an assumption that the registry no longer backs.
   Pinned to R7.

---

# Lane R — server

### Task R1: schema, and the migration that cannot currently run

**Files:**
- Create: `migrations/0032_templates_and_brand_kits.sql`
- Modify: `.scratch/email-platform/issues/40-the-dev-database-cannot-be-migrated-because-0023-was-edited-after-it-was-applied.md`
- Test: `tests/templates.rs` (new, schema-level assertions only)

**Interfaces:**
- Produces: tables `templates`, `brand_kits`; the column names every later R task selects.

`templates`: `id`, `workspace_id`, `name`, `kind` (`check (kind in ('blocks','mjml','html'))`),
`tree jsonb`, `source text`, `compiled jsonb`, `created_at`, `updated_at`, plus
`constraint templates_workspace_id_id_key unique (workspace_id, id)` so a later child table can
carry a composite foreign key rather than trusting a second lookup.

Exactly one of `tree` and `source` is populated, enforced by a check keyed on `kind` — not by
application code, which is where M1's equivalent drifted.

`brand_kits`: one row per workspace (`primary key (workspace_id)`), logo URL, colour palette
`jsonb`, font stack, footer `jsonb`.

- [ ] **Step 1: Resolve `issues/40` before writing the migration.** The dev database refuses
      `sqlx migrate` because `0023` was edited after it was applied, so its checksum no longer
      matches. Confirm the cause with `sqlx migrate info` inside the container, then fix it by
      recreating the dev database volume — **not** by editing `_sqlx_migrations`, which hides the
      divergence instead of removing it. Record in the issue what the checksum was and what fixed
      it. A green suite that coexists with a migrate that cannot run is the exact failure M2a
      filed this issue for.
- [ ] **Step 2: Write the failing test.** `tables_exist_and_reject_a_tree_on_a_raw_template` —
      insert a `kind='mjml'` row with a non-null `tree` and assert the check constraint rejects it;
      insert `kind='blocks'` with a null `tree` and assert the same.
- [ ] **Step 3: Run it.** Expected: FAIL, relation `templates` does not exist.
- [ ] **Step 4: Write `0032_templates_and_brand_kits.sql`.**
- [ ] **Step 5: Run the test.** Expected: PASS.
- [ ] **Step 6: Commit.**

---

### Task R2: the merge-tag parser and compiler

**Files:**
- Create: `src/template/mod.rs`, `src/template/tag.rs`
- Modify: `src/lib.rs` (register the module)
- Test: `src/template/tag.rs` (unit tests, `#[cfg(test)]`)

**Interfaces:**
- Consumes: `custom_fields::definitions_for(db, workspace_id) -> sqlx::Result<Vec<Definition>>`.
- Produces:
  ```rust
  pub enum Node { Text(String), Field { key: String, filter: Option<Filter>, fallback: Option<String> },
                  Present { key: String, body: Vec<Node> }, Absent { key: String, body: Vec<Node> },
                  Equals { key: String, value: String, body: Vec<Node> } }
  pub enum Filter { Upper, Lower, Title, Date(String) }
  pub struct Registry { /* system fields + this workspace's live custom fields */ }
  impl Registry { pub fn from_definitions(defs: &[Definition]) -> Self }
  pub fn compile(source: &str, registry: &Registry) -> Result<Vec<Node>, TagError>
  pub enum TagError { UnknownField(String), ArchivedField(String), UnknownFilter(String),
                      NonLiteralFallback, Unbalanced(String), TooDeep, Malformed(String) }
  ```

The grammar is §5.3 of the spec, verbatim. A fallback is a **quoted literal only**. The filter list
is frozen. Nesting depth is capped at a constant `MAX_DEPTH: usize = 32`, and exceeding it is
`TagError::TooDeep` at compile — never a render-time stack overflow.

- [ ] **Step 1: Write the failing tests.**
  ```rust
  fn a_known_field_compiles()
  fn an_unknown_field_is_a_compile_error()
  fn an_archived_field_is_a_compile_error()          // Review Focus: see R7 for the other half
  fn a_fallback_must_be_a_quoted_literal()           // `{{ x | y }}` where y is not a filter
  fn an_unfrozen_filter_is_a_compile_error()         // `{{ x | env }}`
  fn present_absent_and_equals_blocks_compile()
  fn an_unbalanced_block_is_a_compile_error()
  fn nesting_past_max_depth_is_a_compile_error()     // Review Focus 3
  fn a_substituted_value_is_never_re_parsed()        // Review Focus 2: render a contact whose
                                                     // first_name is literally "{{ email }}" and
                                                     // assert the output contains that text, not
                                                     // the contact's email
  ```
- [ ] **Step 2: Run them.** Expected: FAIL, `compile` not defined.
- [ ] **Step 3: Implement `compile` and the types above in `src/template/tag.rs`.** A single pass
      over the source; the output AST is the only thing stored. Approach note: scan for `{{`,
      classify the next byte (`#`, `^`, `/`, else field), and maintain an explicit block stack
      rather than recursing — the depth cap then falls out of the stack length.
- [ ] **Step 4: Run them.** Expected: PASS.
- [ ] **Step 5: Commit.**

---

### Task R3: the context-classifying renderer, and the URL allowlist

**Files:**
- Create: `src/template/render.rs`
- Test: `src/template/render.rs` (unit tests)

**Interfaces:**
- Consumes: `tag::Node`, `tag::Filter` from R2.
- Produces:
  ```rust
  pub struct Contact<'a> { /* the one row a render reads */ }
  pub fn render_html(nodes: &[Node], contact: &Contact<'_>) -> Result<String, RenderError>
  pub fn classify(html: &str) -> Result<Vec<Site>, RenderError>   // used at COMPILE time
  pub enum Site { Text, Attribute, Url, Css, Dangerous }
  pub enum RenderError { DangerousContext { position: usize }, ... }
  ```

**This task is the security core of the slice.** Two rules decide everything:

1. **Classification happens against the HTML `mrml` produced**, not against the MJML source.
   `mrml` rewrites attributes, so a position classified in the MJML may not exist in the output.
   (Review Focus 1.)
2. **A tag in a dangerous context is refused, not escaped.** Inside `<script>`, inside an HTML
   comment, in a tag name, in an attribute name: `RenderError::DangerousContext`. Encoding does not
   make those positions safe.

URLs: substitute, parse with a real URL parser, canonicalise, check the scheme against
`{http, https, mailto, tel}`, re-serialise. On failure emit `#` and log. **No blocklist, no regex.**

- [ ] **Step 1: Write the failing tests.**
  ```rust
  fn text_position_is_entity_encoded()
  fn attribute_position_is_attribute_encoded_with_forced_double_quotes()
  fn url_position_is_percent_encoded()
  fn a_tag_inside_script_fails_to_compile()
  fn a_tag_inside_an_html_comment_fails_to_compile()
  fn a_tag_in_a_tag_name_fails_to_compile()
  fn a_tag_in_an_attribute_name_fails_to_compile()
  fn obfuscated_javascript_schemes_all_render_hash()
      // "java\tscript:", "&#106;avascript:", "JaVaScript:", "\0javascript:", "\njavascript:"
  fn a_conditional_branch_obeys_the_same_rules()
  fn classification_runs_on_mrml_output_not_on_mjml()   // Review Focus 1
  fn unicode_survives_text_and_punycodes_an_idn_host()  // Review Focus 4: U+202E, ZWJ, combining
                                                        // mark, and an IDN host
  ```
- [ ] **Step 2: Run them.** Expected: FAIL.
- [ ] **Step 3: Implement.** Use an HTML5-conformant parser for classification and the `url` crate
      for URL handling — both need a `deny.toml` licence check before the commit.
- [ ] **Step 4: Run them.** Expected: PASS.
- [ ] **Step 5: Commit.**

---

### Task R4: the block tree, the MJML emitter, and the `mrml` render

**Files:**
- Create: `src/template/block.rs`, `src/template/mjml.rs`
- Test: `src/template/mjml.rs` (unit tests)

**Interfaces:**
- Produces:
  ```rust
  pub enum Block { Text { runs: Vec<Run> }, Heading { level: u8, runs: Vec<Run> },
                   Image { src: String, alt: String }, Button { label: String, href: String },
                   Divider, Spacer { height: u16 }, Columns { children: Vec<Vec<Block>> } }
  pub enum Run { Plain(String), Bold(..), Italic(..), Link { href: String, .. }, Tag(tag::Node) }
  pub struct BrandKit { /* colours, fonts, logo, footer */ }
  pub fn to_mjml(blocks: &[Block], brand: &BrandKit) -> String
  pub fn render_mjml(mjml: &str) -> Result<String, MjmlError>
  ```

`Block` and `Run` are the serde shape the editor posts. They are **the** contract between the
lanes: Lane W serialises exactly this.

`render_mjml` wraps `mrml` with `default-features = false, features = ["parse","render","css-inline"]`
— already in `Cargo.toml` on this branch. An `mrml` parse error surfaces with its message, since
`mrml` hard-errors on input mjml.io accepts.

- [ ] **Step 1: Write the failing tests.** Every `Block` variant emits valid MJML that
      `render_mjml` accepts; `Columns` nests; a brand kit's colours and fonts reach the output; an
      `mrml` parse failure returns `MjmlError` carrying mrml's own message, not a generic string.
- [ ] **Step 2: Run them.** Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them.** Expected: PASS.
- [ ] **Step 5: Commit.**

---

### Task R5: the plain-text part

**Files:**
- Create: `src/template/text.rs`
- Test: `src/template/text.rs` (unit tests)

**Interfaces:**
- Consumes: `Block`, `Run` from R4.
- Produces:
  ```rust
  pub fn text_from_blocks(blocks: &[Block], contact: &Contact<'_>) -> String
  pub fn text_from_html(html: &str) -> String   // raw-mode fallback, `table` handling = "block"
  ```

**The encoder is its own.** Plain text receives characters, never entities. listmonk runs its
alternative body through `html/template` and delivers `&amp;` and `&#39;` to plain-text readers;
this is the test that stops us repeating it.

- [ ] **Step 1: Write the failing tests.**
  ```rust
  fn a_heading_becomes_a_line_not_a_tag()
  fn a_button_becomes_its_label_and_its_url()
  fn an_image_becomes_its_alt_text()
  fn the_output_contains_no_html_entities()   // &amp; &#39; &lt; &gt; &quot; — none of them
  fn the_output_contains_no_markup()
  fn raw_mode_html_tables_become_blocks_not_one_line()
  ```
- [ ] **Step 2: Run them.** Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them.** Expected: PASS.
- [ ] **Step 5: Commit.**

---

### Task R6: the link checker and the two `mrml` lints

**Files:**
- Create: `src/template/lint.rs`
- Test: `src/template/lint.rs` (unit tests)

**Interfaces:**
- Produces:
  ```rust
  pub struct Finding { pub rule: Rule, pub message: String, pub block: Option<usize> }
  pub enum Rule { MalformedUrl, UnsupportedScheme, EmptyHref, TagInLinkPosition,
                  MailtoWithoutAddress, UnencodedSpace,
                  AtRuleInInlineStyle, MisspelledInlineAttribute }
  pub fn lint(mjml: &str, blocks: Option<&[Block]>) -> Vec<Finding>
  ```

**`lint` fetches nothing.** Everything worth checking is decidable offline, and a checker that
resolves operator-supplied URLs is a request-forgery engine with a friendly name.

The two `mrml` lints catch failures that are **silent** — no error, no warning, wrong mail:

- `AtRuleInInlineStyle`: a `@media` (or any at-rule) inside `<mj-style inline="inline">`. `mrml`
  builds its inliner with `keep_style_tags(true)` / `inline_style_tags(false)`, so the rule is
  discarded. This is what eats dark mode.
- `MisspelledInlineAttribute`: any `inline` value other than exactly `"inline"` silently does
  nothing.

- [ ] **Step 1: Write the failing tests**, one per `Rule`, plus
      `lint_makes_no_network_call` — assert by construction (no HTTP client is reachable from this
      module) rather than by observing a quiet run.
- [ ] **Step 2: Run them.** Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them.** Expected: PASS.
- [ ] **Step 5: Commit.**

---

### Task R7: templates and brand-kit API

**Files:**
- Create: `src/template/routes.rs`, `src/brand/mod.rs`, `src/brand/routes.rs`
- Modify: `src/http/mod.rs` (merge inside `http::routes()`, **never** in `app::router` after the
  CSRF layer — a route merged there silently gets neither CSRF nor headers), `src/openapi.rs`
- Test: `tests/templates.rs`

**Interfaces:**
- Consumes: R2 `compile`, R3 `render_html`, R4 `to_mjml`/`render_mjml`, R5, R6 `lint`.
- Produces: the OpenAPI schema Lane W generates `schema.d.ts` from. **W6 blocks on this task.**

Routes: `GET/POST/PATCH/DELETE /api/templates`, `POST /api/templates/{id}/preview`
(returns HTML, plain text and findings for a sample contact), `GET/PUT /api/brand-kit`.
Role gates follow the existing modules — and the gate goes on **every** mutating verb, not one of
three, which is a defect M2a's review found and this task does not get to repeat.

- [ ] **Step 1: Write the failing tests.**
  ```rust
  fn another_workspaces_template_is_404()
  fn every_mutating_verb_is_role_gated()      // POST, PATCH and DELETE — assert all three
  fn saving_a_tag_naming_an_unknown_field_is_422_with_the_field_name()
  fn saving_a_tag_naming_an_archived_field_is_422()
  fn a_template_compiled_before_archiving_still_renders()        // the other half of R2's rule
  fn a_field_kind_change_after_compile_is_detected_not_mis_rendered()  // Review Focus 5
  fn preview_returns_html_text_and_findings()
  fn the_brand_kit_is_one_row_per_workspace()
  ```
- [ ] **Step 2: Run them.** Expected: FAIL.
- [ ] **Step 3: Implement the routes and register them in `http::routes()` and `openapi.rs`.**
- [ ] **Step 4: Run them.** Expected: PASS.
- [ ] **Step 5: Run `pnpm gen:api`** so `schema.d.ts` is current, and commit it. This is the
      handoff to W6.
- [ ] **Step 6: Commit.**

---

### Task R8: re-run the ADR 0005 lock-order enumeration

**Files:**
- Modify: `docs/adr/0005-one-lock-order-for-every-path.md`
- Test: `tests/lock_order.rs`

The test is **"does this statement take a row lock on, or wait on, anything else"** — not "is it
one statement". M2a's `form_lists` reached the right ruling by the wrong reasoning, and line 45 of
the ADR names that failure.

- [ ] **Step 1: Enumerate** every write path R1–R7 added and record, per path, what it locks and
      what it waits on. Check foreign keys with `pg_constraint`, not by reading the migration —
      M2a produced a false claim about `audit_log` by reading rather than querying.
- [ ] **Step 2: Rule** on whether `templates` or `brand_kits` is a lock class, with the reasoning
      written out.
- [ ] **Step 3: Add a `tests/lock_order.rs` case** for any new ordering, using that file's
      per-test pool pattern (`own_pool`, max 6) — **not** a `#[sqlx::test]` pool, which draws from
      the process-wide master capped at 20 and produces the 40-second false failure M2a chased.
- [ ] **Step 4: Run the suite.** Expected: PASS, five runs in a row.
- [ ] **Step 5: Commit.**

---

# Lane W — web

### Task W1: the bundle gate measures a route's closure

**Files:**
- Modify: `web/scripts/check-bundle.mjs`
- Test: `web/scripts/check-bundle.test.mjs`

Today the gate checks each lazy chunk against `CHUNK_LIMIT` individually, so a `manualChunks`
split lets a framework through in two pieces with the gate green. **This task runs first in Lane W**
because the whole no-framework ruling rests on a measurement this gate is supposed to defend.

**Interfaces:**
- Produces: a gate that sums, per lazy entry, the gzipped size of that entry plus every chunk it
  imports transitively, minus the shell chunks already counted against `LIMIT`.

- [ ] **Step 1: Write the failing test.** Build a synthetic manifest where one route splits into
      two 30 KB chunks and assert the gate fails it. Today's gate passes it.
- [ ] **Step 2: Run it.** Expected: FAIL (the gate passes something it should reject).
- [ ] **Step 3: Implement the closure sum.**
- [ ] **Step 4: Run it, then run `pnpm build && pnpm check:bundle`** and confirm the existing
      routes still pass. If a real route now fails, **stop and report** — that is a genuine budget
      finding, not something to fix by raising the limit.
- [ ] **Step 5: Commit.**

---

### Task W2: the block editor core

**Files:**
- Create: `web/src/components/block-editor/model.ts`, `editor.tsx`, `block.tsx`
- Test: `web/src/components/block-editor/model.test.ts`

**Interfaces:**
- Produces: the TypeScript mirror of R4's `Block` and `Run`, and
  ```ts
  type Doc = { blocks: Block[] }
  function apply(doc: Doc, op: Op): Doc      // pure; undo is a stack of Docs
  ```

**The §5.1 boundary is a constraint, not a regret.** The document lives in React state.
`contenteditable` is **per text block** and holds inline runs only. **Multi-block selection is out
of scope** and must not half-work: an operation spanning two blocks is not offered, not silently
partial.

- [ ] **Step 1: Write the failing tests** for `apply`: insert, delete, reorder, split a text block,
      undo and redo, and that an op naming two blocks is rejected.
- [ ] **Step 2: Run them.** Expected: FAIL.
- [ ] **Step 3: Implement `model.ts`, then the `editor.tsx` shell that renders blocks from state.**
- [ ] **Step 4: Run them.** Expected: PASS.
- [ ] **Step 5: Commit.**

---

### Task W3: block types, drag reorder, and the property panel

**Files:**
- Create: `web/src/components/block-editor/blocks/*.tsx`, `properties.tsx`, `tag-token.tsx`
- Test: `web/src/components/block-editor/editor.test.tsx` (Playwright component tests, as
  `dialog.test.tsx` does)

Every `Block` variant from R4 gets an editing surface. A merge tag is an **atomic inline token** —
it deletes whole, never half. Drag reorder uses the Pointer Events API; no drag-and-drop library.

- [ ] **Step 1: Write the failing tests:** each block type renders and edits; a tag token deletes
      atomically; reorder moves a block; the property panel edits the selected block only.
- [ ] **Step 2: Run them.** Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them, then `pnpm build && pnpm check:bundle`.** Expected: PASS, route under
      51,200 bytes gzipped.
- [ ] **Step 5: Commit.**

---

### Task W4: previews — desktop, mobile, dark, plain text

**Files:**
- Create: `web/src/components/block-editor/preview.tsx`
- Test: `web/src/components/block-editor/preview.test.tsx`

**The dark preview is labelled with what it covers and what it cannot.** Three distinct client
behaviours exist and a browser preview reproduces one. A preview that implies otherwise is the
spec's §4.1 anti-goal: the operator stops checking.

The editor also states in words that **MJML buttons render square in classic Outlook on Windows** —
no preview we can build will show it.

- [ ] **Step 1: Write the failing tests:** all four modes render; the dark mode carries its
      coverage label; the Outlook note is present; findings from R6 appear inline while composing,
      not only on save.
- [ ] **Step 2: Run them.** Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them.** Expected: PASS.
- [ ] **Step 5: Commit.**

---

### Task W5: the brand-kit screen

**Files:**
- Create: `web/src/routes/brand-kit.tsx`
- Test: `web/src/routes/brand-kit.test.tsx`

Follows the existing route pattern (`sending-domains.tsx` is the closest shape). Logo, colour
palette, font stack, footer. Until R7 lands, the form posts against a local mock; W6 swaps in the
generated client.

- [ ] **Step 1: Write the failing test:** the form renders, validates a colour, and reports a
      failed save with the problem detail, via the existing `problemMessage` helper.
- [ ] **Step 2: Run it.** Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run it.** Expected: PASS.
- [ ] **Step 5: Commit.**

---

### Task W6: templates list and detail, wired to the real API

**Files:**
- Create: `web/src/routes/templates.tsx`, `web/src/routes/template-detail.tsx`
- Modify: `web/src/routes/workspace-layout.tsx` (navigation), `web/src/api/schema.d.ts`
  (regenerated)
- Test: `web/e2e/templates.spec.ts`

**Blocks on R7.** Before starting, merge the integration branch into `m2b-web` and run
`pnpm gen:api` so `schema.d.ts` reflects the real routes.

- [ ] **Step 1: Merge the integration branch and regenerate the client.**
- [ ] **Step 2: Write the failing e2e:** create a template, edit it in the editor, save, reopen it
      and confirm the tree round-tripped; a merge tag naming an unknown field shows the server's
      422 with the field name.
- [ ] **Step 3: Run it.** Expected: FAIL.
- [ ] **Step 4: Implement both routes and the navigation entry.**
- [ ] **Step 5: Run the e2e, then `pnpm build && pnpm check:bundle`.** Expected: PASS, under
      budget.
- [ ] **Step 6: Commit, then merge `m2b-web` into the integration branch.**

---

## Finishing

1. Merge `m2b-web` into `m2b-templates-and-editor`; remove the worktree.
2. Run the full gate **serialised** inside the container, with `set -eo pipefail`:
   `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo deny check`, `cargo test`,
   `pnpm typecheck`, `pnpm test`, `pnpm build`, `pnpm check:bundle`, `pnpm e2e`.
3. Whole-branch review on the most capable model, with the spec's §7 invariants as the lens.
4. Fix wave, then PR and merge to `main`.
