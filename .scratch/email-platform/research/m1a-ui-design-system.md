# M1a UI foundation — design-system proposal

Status: proposal. Written 2026-11-11, before M1a implementation starts.
Scope: the token layer, the primitive set, the contacts data table, and the page patterns that M1a needs. No application code is written by this document; the CSS blocks below are the proposal, not a committed file.

Binding context this proposal was written against:

- Spec §7 "UI", §9 "Performance targets", §6 "Features", and M0's global constraints (review notes kept outside the repository).
- Spec §13.2 is still open: "UI look and feel: settle it with mockups before the M0 design system is built." That open item is why the current `web/src/lib/ui.tsx` is deliberately plain Tailwind. **This document closes §13.2 for the functional layer** (tokens, primitives, table, page patterns) and leaves one axis open: a brand display typeface, if the mockups ask for one. Everything here is designed so that adding a typeface later changes one `@theme` line and nothing else.
- Measured baseline: the current app shell is **108,532 bytes gzipped** of the 153,600-byte budget (measured from `web/dist/.vite/manifest.json` with the same algorithm `web/scripts/check-bundle.mjs` uses). **Headroom for the shell is 45,068 bytes gzipped.** Every size figure below is measured, not estimated — see §2.4 for the method.

---

## 1. Token layer

### 1.1 How the tokens are wired into Tailwind v4

Three layers, in the order the `design-system` and `better-colors` sources both describe: primitive → semantic → component. Only two of them are written down.

- **Primitives** are OKLCH literals. They live *only* inside the semantic definitions below. No component ever names a hue.
- **Semantics** are plain CSS custom properties on `:root`, re-declared under the dark selector. They are what components use.
- **Component tokens** are not a layer in this codebase. With Tailwind utilities, `--button-bg` is one more indirection that buys nothing; a button names `bg-accent` directly. Add a component token only when a component needs a value no semantic token can express.

The wiring detail that matters, and which gets this wrong in most Tailwind v4 codebases: **semantic colours must go through `@theme inline`, not `@theme`.** A plain `@theme` block bakes the literal value into every generated utility, so redefining the variable under a dark selector changes nothing. `@theme inline` makes `.bg-surface` compile to `background-color: var(--surface)`, which *is* overridable at runtime. Scales that never change between themes (spacing, radius, type) use plain `@theme`.

One theme-switching mechanism, not two. `better-colors` is explicit that mixing `prefers-color-scheme` with a class selector produces states neither rule covers. The product needs an explicit user choice as well as a system default, so the single mechanism is a `data-theme` attribute on `<html>`, written by a ~15-line inline script in `index.html` before first paint (reads `localStorage.theme`, falls back to `matchMedia("(prefers-color-scheme: dark)")`, sets `data-theme` and `color-scheme`). CSS never reads `prefers-color-scheme` for colour. This also keeps the existing `dark:` utilities in `ui.tsx` and the seven route files working during the migration, via one `@custom-variant` line.

### 1.2 Colour

Every value below was generated in OKLCH, verified to be inside sRGB, and contrast-checked against the background it actually renders on (`better-colors`: "Measure a foreground against the background it actually renders on, not the page background"). The hex column is the exact sRGB equivalent — it exists because the MJML templates and the brand kit in M2 need hex, and because designers need something to paste into a design tool.

**Hue budget.** The accent cannot be blue, green, amber or red, because this product renders five simultaneous status hues on the same screen — delivered, bounced, complained, suppressed, scheduled — and `better-colors` requires at least 15° of OKLCH hue separation between an accent and a status colour. A blue accent collides with "info/scheduled" and a green accent collides with "delivered". The accent is therefore **violet, hue 295°**: 45° from info (250°), 145° from success (150°), 223° from warning (72°), 268° from danger (27°). The choice is functional, not decorative, and it is not one of the four template defaults — no warm cream, no acid green on near-black, no broadsheet hairlines, no identical rounded cards.

Neutrals carry a slight cool cast (hue 265, chroma 0.003–0.018) so that the five status colours read as signal against instrument grey rather than competing with a warm background.

#### Light (`:root`)

| Token | OKLCH | sRGB | Role |
|---|---|---|---|
| `--canvas` | `oklch(0.985 0.003 265)` | `#f9fafc` | Page background behind panels |
| `--surface` | `oklch(1 0 0)` | `#ffffff` | Panels, table body, dialogs |
| `--sunken` | `oklch(0.968 0.005 265)` | `#f3f4f8` | Table header, toolbars, inset wells |
| `--row-hover` | `oklch(0.962 0.006 265)` | `#f0f2f7` | Row hover |
| `--row-selected` | `oklch(0.955 0.022 295)` | `#f1eefe` | Selected row |
| `--fg` | `oklch(0.23 0.018 265)` | `#191d26` | Primary text — 16.9:1 on surface |
| `--fg-muted` | `oklch(0.50 0.016 265)` | `#5f636d` | Secondary text, column headers — 6.0:1 |
| `--fg-subtle` | `oklch(0.565 0.014 265)` | `#72767e` | Placeholders, disabled text — 4.56:1 |
| `--border` | `oklch(0.905 0.006 265)` | `#dee0e4` | Row rules, panel edges (decorative) |
| `--border-strong` | `oklch(0.645 0.012 265)` | `#8a8e95` | Input and control borders — 3.30:1 on surface, 3.01:1 on sunken |
| `--accent` | `oklch(0.53 0.195 295)` | `#7849ce` | Primary action fill, links, focus ring — 5.78:1 |
| `--accent-hover` | `oklch(0.47 0.20 295)` | `#6833bc` | Primary action hover/active |
| `--on-accent` | `oklch(1 0 0)` | `#ffffff` | Text on accent — 5.78:1 |
| `--accent-subtle` | `oklch(0.955 0.022 295)` | `#f1eefe` | Accent-tinted background |
| `--accent-text` | `oklch(0.45 0.19 295)` | `#6130b1` | Text on `--accent-subtle` — 7.14:1 |
| `--danger` | `oklch(0.52 0.19 27)` | `#be2323` | Destructive fill, hard bounce, complaint |
| `--on-danger` | `oklch(1 0 0)` | `#ffffff` | 6.08:1 |
| `--danger-subtle` | `oklch(0.955 0.022 27)` | `#ffebe8` | Badge background |
| `--danger-text` | `oklch(0.45 0.16 27)` | `#9b1f1d` | 7.04:1 on `--danger-subtle` |
| `--success` | `oklch(0.50 0.13 150)` | `#137738` | Delivered, subscribed, verified |
| `--success-subtle` | `oklch(0.955 0.025 150)` | `#e5f5e7` | |
| `--success-text` | `oklch(0.44 0.12 150)` | `#03642b` | 6.50:1 on `--success-subtle` |
| `--warning` | `oklch(0.52 0.105 72)` | `#8e5d11` | Soft bounce, paused, unconfirmed |
| `--warning-subtle` | `oklch(0.955 0.035 78)` | `#fdeed7` | |
| `--warning-text` | `oklch(0.45 0.095 72)` | `#754b03` | 6.64:1 on `--warning-subtle` |
| `--info` | `oklch(0.50 0.13 250)` | `#1666aa` | Scheduled, queued, informational |
| `--info-subtle` | `oklch(0.955 0.022 250)` | `#e5f2ff` | |
| `--info-text` | `oklch(0.45 0.115 250)` | `#145892` | 6.53:1 on `--info-subtle` |
| `--ring` | `oklch(0.53 0.195 295)` | `#7849ce` | Focus indicator — 5.78:1 on surface, 5.26:1 on sunken, 5.04:1 on selected row |

#### Dark (`:root[data-theme="dark"]`)

Not a reversed light ramp. Chroma on the accent drops from 0.195 to 0.155, the dark end of the neutral ramp is widened so that four distinguishable surfaces exist below `#2c2f36`, and every pair was re-measured.

| Token | OKLCH | sRGB | Contrast |
|---|---|---|---|
| `--canvas` | `oklch(0.165 0.010 265)` | `#0c0e13` | |
| `--surface` | `oklch(0.205 0.011 265)` | `#15171c` | |
| `--sunken` | `oklch(0.245 0.012 265)` | `#1e2026` | |
| `--row-hover` | `oklch(0.255 0.013 265)` | `#202329` | |
| `--row-selected` | `oklch(0.285 0.045 295)` | `#2c253e` | |
| `--fg` | `oklch(0.95 0.004 265)` | `#edeef1` | 15.5:1 on surface |
| `--fg-muted` | `oklch(0.735 0.012 265)` | `#a5a9b1` | 7.63:1 |
| `--fg-subtle` | `oklch(0.635 0.013 265)` | `#878b93` | 5.22:1 |
| `--border` | `oklch(0.305 0.013 265)` | `#2c2f36` | decorative |
| `--border-strong` | `oklch(0.525 0.016 265)` | `#666a74` | 3.32:1 on surface, 3.01:1 on sunken |
| `--accent` | `oklch(0.72 0.155 295)` | `#ac8efa` | 6.87:1 |
| `--accent-hover` | `oklch(0.78 0.125 295)` | `#bda7ff` | |
| `--on-accent` | `oklch(0.17 0.03 295)` | `#100d1b` | 7.36:1 on accent, 9.25:1 on accent-hover |
| `--accent-subtle` | `oklch(0.30 0.055 295)` | `#302846` | |
| `--accent-text` | `oklch(0.80 0.105 295)` | `#c2b0fb` | 7.21:1 on `--accent-subtle` |
| `--danger` | `oklch(0.73 0.155 27)` | `#fa7d71` | 7.02:1 |
| `--on-danger` | `oklch(0.17 0.03 27)` | `#1b0a08` | 7.53:1 |
| `--danger-subtle` | `oklch(0.30 0.06 27)` | `#47211d` | |
| `--danger-text` | `oklch(0.80 0.105 27)` | `#faa399` | 7.18:1 |
| `--success` | `oklch(0.78 0.15 150)` | `#67d283` | 9.48:1 |
| `--success-subtle` | `oklch(0.30 0.05 150)` | `#1a3520` | |
| `--success-text` | `oklch(0.83 0.14 150)` | `#7fe196` | 8.33:1 |
| `--warning` | `oklch(0.82 0.14 75)` | `#f9b64f` | 10.08:1 |
| `--warning-subtle` | `oklch(0.31 0.05 75)` | `#3f2c10` | |
| `--warning-text` | `oklch(0.86 0.105 78)` | `#f7c980` | 8.58:1 |
| `--info` | `oklch(0.76 0.11 250)` | `#79b6f4` | 8.39:1 |
| `--info-subtle` | `oklch(0.30 0.05 250)` | `#192f46` | |
| `--info-text` | `oklch(0.82 0.085 250)` | `#9ac9fa` | 7.84:1 |
| `--ring` | `oklch(0.78 0.125 295)` | `#bda7ff` | 8.64:1 on surface, 9.29:1 on canvas, 7.00:1 on selected row |

**Focus ring rule.** `--ring` equals the accent fill, so the indicator must always be drawn as `outline: 2px solid var(--ring); outline-offset: 2px`. The 2px offset renders the surface colour between the control and the ring, which is what makes the indicator's contrast measurable against the surface rather than against the button it surrounds. A ring drawn flush against an accent-filled button is a 1:1 pair and fails 1.4.11. There is no exception to the offset.

**Status colour is never the only cue** (WCAG 1.4.1). Every status badge carries a shape or glyph as well as a colour: a filled dot for subscribed, a hollow dot for unconfirmed, a slash for unsubscribed, a cross for suppressed. A monochrome screenshot of the contacts table must still be readable.

### 1.3 Spacing

Tailwind v4's dynamic spacing already generates every multiple of `--spacing`; there is no scale to define, only a scale to *use*.

```css
--spacing: 0.25rem;  /* Tailwind default. Do not change it. */
```

Permitted steps, and nothing between them: `0, 1, 1.5, 2, 3, 4, 6, 8, 12, 16` → 0, 4, 6, 8, 12, 16, 24, 32, 48, 64 px.

Rules taken from `better-layout`:

- Space groups first, background shapes second, separator lines last. A border is the third choice, not the first. The current `CARD` constant is a border where 24px of space would do.
- The gap *between* two groups is at least twice the gap *within* a group. Inside a form: fields 12px apart, field groups 24px apart, sections 48px apart.
- One step per level of nesting, with 16px as the default page gutter — 16px inline on mobile, 24px at `md`, 32px at `xl`.
- Adjacent bordered or filled controls sit 12px apart; borderless text and icon controls sit 24px apart.

### 1.4 Type

```css
@theme {
  --font-sans: ui-sans-serif, system-ui, -apple-system, "Segoe UI Variable Text",
               "Segoe UI", Roboto, "Helvetica Neue", Arial, "Noto Sans", sans-serif,
               "Apple Color Emoji", "Segoe UI Emoji";
  --font-mono: ui-monospace, SFMono-Regular, "SF Mono", "Cascadia Mono",
               "Segoe UI Mono", "Roboto Mono", Menlo, Consolas, monospace;

  --text-2xs: 0.6875rem;  --text-2xs--line-height: 1rem;      /* 11px — micro-labels only */
  --text-xs:  0.75rem;    --text-xs--line-height: 1rem;       /* 12px — table meta, badges */
  --text-sm:  0.8125rem;  --text-sm--line-height: 1.125rem;   /* 13px — dense table body */
  --text-base:0.875rem;   --text-base--line-height: 1.25rem;  /* 14px — UI default */
  --text-md:  1rem;       --text-md--line-height: 1.5rem;     /* 16px — prose, detail body */
  --text-lg:  1.125rem;   --text-lg--line-height: 1.5rem;     /* 18px — section heading */
  --text-xl:  1.375rem;   --text-xl--line-height: 1.75rem;    /* 22px — page title */
  --text-2xl: 1.75rem;    --text-2xl--line-height: 2rem;      /* 28px — rare, empty states */

  --tracking-tight: -0.011em;   /* ≥ 22px only */
  --tracking-normal: 0;
}
```

**No webfont.** This is a decision, not an omission. A self-hosted install must not make a third-party request (spec §7 Security: "self-hosted, no third party"; §7 Privacy), must work on an air-gapped network, and must not spend bytes from a 45 KB headroom on a typeface. The system stack costs zero bytes, has zero FOUT, and raises no licence question for the AGPL gate. One candidate pairing was Fira Code/Fira Sans loaded from `fonts.googleapis.com`; the pairing is reasonable and the delivery method is not. If the §13.2 mockups demand a brand face, it ships as one subset self-hosted `woff2` (latin, ~18 KB, `font-display: swap`) applied to `--font-sans` headings only, and nothing else in this document changes.

Other type rules:

- `html` stays at the browser's 16px. Never set `html { font-size: 14px }` to get density — it breaks every user who has set a larger default and it breaks the 200%-reflow requirement. Density comes from `--text-base: 0.875rem`, which the user's own zoom still scales.
- `font-variant-numeric: tabular-nums` on every number that sits in a column or updates in place: counts, rates, dates, the selection count, the send stats in M3. Proportional figures make a changing count jitter.
- `--text-2xs` is for a label that is never the only copy of its information. Never for prose, never for an error message.
- Prose at `--text-md` with a `max-width: 68ch`. Table cells, buttons and labels at `--text-base` or `--text-sm`.
- Sentence case everywhere, including column headers and buttons (`better-writing`: sentence case is the default when the project has no policy). The existing screens already do this.

### 1.5 Radius, elevation, density, motion

```css
@theme {
  --radius-xs: 2px;     /* checkbox, tiny chips */
  --radius-sm: 4px;     /* buttons, inputs, selects */
  --radius-md: 6px;     /* panels, cards, popovers */
  --radius-lg: 10px;    /* dialogs, drawers */
  --radius-full: 9999px;/* status pills, avatars */
}
```

One radius on everything is a well-known template tell. Here the radius encodes scale: a table row has **radius 0** — rows are a continuous surface, not a stack of cards.

```css
/* Elevation. Dense UI separates with borders and surface steps, not shadows.
   Exactly two shadows exist, and only the top layer may use them. */
--shadow-popover: 0 4px 12px -2px oklch(0.23 0.018 265 / 0.12),
                  0 1px 3px      oklch(0.23 0.018 265 / 0.08);
--shadow-dialog:  0 16px 48px -8px oklch(0.23 0.018 265 / 0.22),
                  0 2px 8px        oklch(0.23 0.018 265 / 0.10);
/* dark theme: shadows barely read on a dark canvas — halve the alpha and
   rely on --border + the surface step instead. */

/* Density. One variable drives row height, cell padding and control height. */
--row-h: 2.25rem;                      /* 36px, default */
/* compact 2rem (32px) · comfortable 2.75rem (44px); stored per user in localStorage */
@media (pointer: coarse) { --row-h: 2.75rem; }   /* 44px, non-negotiable on touch */
--control-h: 2rem;                     /* 32px; 2.75rem on coarse pointers */
--tap-min: 24px;                       /* WCAG 2.2 2.5.8 floor for every target */

/* Motion. Two durations, one easing. */
--ease-out: cubic-bezier(0.2, 0, 0, 1);
--dur-1: 100ms;   /* state change: hover, checked, press */
--dur-2: 160ms;   /* layer change: dialog, drawer, popover */
@media (prefers-reduced-motion: reduce) { --dur-1: 0ms; --dur-2: 0ms; }
```

Rows never animate. Nothing in a data table fades, slides or staggers: a 500-row list with per-row transitions is both an INP problem and a vestibular one.

### 1.6 The file, end to end

```css
/* web/src/styles.css */
@import "tailwindcss";

/* One theme mechanism. The inline script in index.html sets data-theme before paint. */
@custom-variant dark (&:where([data-theme="dark"], [data-theme="dark"] *));

:root {
  color-scheme: light;
  --canvas: oklch(0.985 0.003 265);
  --surface: oklch(1 0 0);
  /* … the full light table from §1.2 … */
}

:root[data-theme="dark"] {
  color-scheme: dark;
  --canvas: oklch(0.165 0.010 265);
  --surface: oklch(0.205 0.011 265);
  /* … the full dark table from §1.2 … */
}

/* @theme inline, so each utility compiles to var(--token) and the dark block can
   override it at runtime. Plain @theme here would silently break dark mode. */
@theme inline {
  --color-canvas: var(--canvas);
  --color-surface: var(--surface);
  --color-sunken: var(--sunken);
  --color-fg: var(--fg);
  --color-fg-muted: var(--fg-muted);
  --color-fg-subtle: var(--fg-subtle);
  --color-border: var(--border);
  --color-border-strong: var(--border-strong);
  --color-accent: var(--accent);
  --color-on-accent: var(--on-accent);
  --color-danger: var(--danger);
  /* … etc … */
}

/* Scales never vary by theme, so plain @theme is correct for them. */
@theme { /* --text-*, --radius-*, --font-* from §1.4 and §1.5 */ }

@layer base {
  body { background: var(--canvas); color: var(--fg); font-size: var(--text-base); }
  :focus-visible { outline: 2px solid var(--ring); outline-offset: 2px; }
  /* Sticky table headers must not hide the focused row — WCAG 2.2 2.4.11. */
  [data-scroller] { scroll-padding-block-start: var(--row-h); }
}
```

### 1.7 Migration from today's `ui.tsx`

The existing constants become token-based in one pass, with no behaviour change:
`bg-blue-700` → `bg-accent`, `border-gray-500 dark:border-gray-400` → `border-border-strong` (one class, no `dark:` variant needed — the variable flips), `text-red-800 dark:text-red-300` → `text-danger`, `border-gray-300 dark:border-gray-700` → `border-border`. The `FOCUS` constant collapses into the `:focus-visible` base rule and disappears. That is a net deletion from `ui.tsx` and removes every `dark:` variant from the seven route files.

---

## 2. Component inventory for M1a

### 2.1 The dependency decision

**M1a adds exactly one runtime dependency: `@tanstack/react-virtual@3.14.26` (MIT, 10.2 KB gzipped), and it lands in a lazy route chunk, not the shell.**

Everything else is hand-built on native elements. That is not asceticism; it is what the measurements force. The numbers in §2.4 show that a single Radix dropdown menu costs 31.6 KB gzipped — 70% of the entire remaining shell budget — to deliver a widget this milestone does not need, because every menu in M1a can be a dialog or a row of visible buttons.

The two native elements doing the heaviest lifting:

- **`<dialog>` + `showModal()`** replaces a dialog library outright. It gives the focus trap, the `inert`-ing of the rest of the page, Escape-to-close, the top layer, and `::backdrop`, in the browser, for zero bytes. `better-accessibility` states the rule directly: "Prefer `<dialog>` with `showModal()`." The two things it does not give are focus restoration to the trigger and initial-focus placement; both are about six lines in a shared hook. Radix Dialog costs 13.9 KB gzipped to re-implement what the platform already ships.
- **`<input type="checkbox">`** with `accent-color: var(--accent)` and `ref.indeterminate` for the header tri-state. Fully accessible, form-associated, keyboard-operable, 0 bytes. Radix Checkbox costs 5.9 KB gzipped for a styled `<button role="checkbox">`.

### 2.2 The primitives

Each row: what it is, build-or-buy, and the accessibility contract that is not optional.

| Primitive | Purpose | Source | Accessibility contract |
|---|---|---|---|
| **Button** / **IconButton** | Every action. Variants: solid (one per view), outline, ghost, danger-solid, danger-ghost. | Hand-built, native `<button type="button">`. Replaces `BUTTON`, `SECONDARY_BUTTON`, `ROW_BUTTON`. | Accessible name never contains the role word ("Close", not "Close button"). Icon-only requires `aria-label` and an `aria-hidden` glyph. Target ≥ 24×24 (2.5.8), ≥ 44×44 on coarse pointers. Never `disabled` on a form submit — validate on submit instead. Busy state keeps the label and adds `aria-busy`, so the width does not change. |
| **Field** | Label + control + hint + error. | Hand-built; `ui.tsx` already has it and it is correct. Extend to textarea/select. | Visible `<label for>` always; a placeholder is never a label. `aria-describedby` chains hint then error. `aria-invalid` on failure. Errors sit below the field, not only in a summary. |
| **Checkbox** | Row select, select-all, filter lists, list membership. | Hand-built on native `<input type=checkbox>`. | Row checkboxes need a per-row accessible name ("Select jo@example.com"), not a bare "Select". Header checkbox is tri-state via the `indeterminate` DOM property with `aria-checked="mixed"`. Space toggles; the label is clickable. |
| **Select** | Single choice from a short closed set (role, status, density). | Native `<select>`; `SELECT` already exists. | Keep it native. It is the only control that gets the OS picker on mobile and screen-reader support for free. |
| **Badge / StatusPill** | Subscription status, suppression reason, consent state, tag. | Hand-built `<span>` using the `*-subtle` / `*-text` pairs. | Shape or glyph as well as colour (1.4.1). `white-space: nowrap`. The visible text is the full status word, not an abbreviation. Non-interactive — a removable tag is a Badge with a separate real `<button>` inside it. |
| **Panel** | Grouped content. Replaces `CARD`. | Hand-built. | A panel with a heading uses `<section aria-labelledby>`. A panel without one is a `<div>` — never an unlabelled landmark. |
| **Dialog** | Create/edit forms, confirmations, filter and column pickers, the tag picker. | Hand-built on native `<dialog showModal()>`. | `aria-labelledby` points at the dialog's `<h2>`. Initial focus to the first field, or the heading if the dialog is informational — never to a destructive button. Focus returns to the trigger on close. Escape closes (free). Closing a dirty form asks first. |
| **Drawer** | The same thing, anchored to the inline-end edge: contact quick-view, filter builder. | The **same** `<dialog>` component with a layout prop. One implementation, two shapes. | Identical to Dialog. It is modal — a non-modal side panel that steals focus is worse than either. |
| **StatusRegion** | One polite `role="status"` and one `role="alert"`, mounted empty at app start. | Hand-built, ~40 lines, lives in the shell. | Mounted **before** it has content — a live region created together with its text is not announced. `role="status"` for success and progress; `role="alert"` only for an error not tied to a field. Never `aria-live="assertive"` on a toast. Messages are also visible on screen, never announcement-only. |
| **EmptyState** | Zero rows, zero lists, zero tags, zero suppressions. | Hand-built. | See §4.5. |
| **ErrorState** | A failed query. | Hand-built. | See §4.6. |
| **Skeleton** | First load of a table or a detail page. | Hand-built. | Fixed height equal to `--row-h` so nothing shifts when data arrives (CLS). `aria-busy="true"` on the region and one polite "Loading contacts". No shimmer animation under `prefers-reduced-motion`. |
| **DataTable** | §3. | Hand-built, over `@tanstack/react-virtual`. | §3.6. |
| **SelectionBar** | Bulk action bar. | Hand-built. | §4.7. |
| **ConfirmButton** | In-row two-step confirm. | Already exists in `ui.tsx` and is correct, including the `autoFocus` comment. | Keep. |
| **CopyButton** | Already exists, including the non-secure-context handling. | Keep. | Keep. |

### 2.3 Deliberately not built in M1a

| Not built | Why | What replaces it | What it would have cost |
|---|---|---|---|
| **Menu / dropdown** | No M1a surface needs more than four contextual actions. | Visible buttons in the row or the toolbar; anything longer is a Dialog with a list. | 31.6 KB gz (Radix DropdownMenu) |
| **Tooltip** | A tooltip is a place to hide a label. | Visible labels; `aria-describedby` to visible hint text. | 19.9 KB gz (Radix Tooltip) |
| **Tabs** | Contact detail reads better as one scrollable column with headings and a sticky in-page nav; tabs hide the suppression state, which is the thing an operator most needs to see. | Headings + anchors. | 9.5 KB gz (Radix Tabs) |
| **Combobox / multi-select** | The one place M1a needs it (assigning lists and tags) works better as a searchable checkbox list. | Dialog + search input + checkbox list, virtualised if the workspace has thousands of tags. | — |
| **Popover** | Nothing in M1a needs anchored non-modal content. | Dialog or inline disclosure. | 24.4 KB gz (Radix Popover) |
| **Command palette** | Planned, not M1a. The shell ships only the Cmd/Ctrl+K key handler (~0.2 KB) that `import()`s the palette chunk on first press and prefetches it on idle. | — | 17.5 KB gz (cmdk, when it lands, in its own chunk) |
| **Charts** | M3. | — | lazy chunk, budget then |

Each of these has a documented trigger for revisiting. The menu's trigger: the first surface that needs more than four contextual actions on one row. When it comes, hand-build it to the APG menu pattern (roving tabindex, typeahead, Escape, Home/End) rather than paying 31.6 KB.

### 2.4 Measured sizes, and the method

Measured on this machine: each package installed at the version shown, bundled with esbuild (`bundle: true, minify: true, format: "esm", target: es2022`, `process.env.NODE_ENV="production"`, React and React-DOM external), then gzip level 9 — the same compression `web/scripts/check-bundle.mjs` applies. Imports are realistic usage, not `import *` of a barrel, so tree-shaking is counted.

| Package | Version | Licence | gzip | Verdict |
|---|---|---|---|---|
| `@tanstack/react-virtual` | 3.14.26 | MIT | **10.2 KB** | **Adopt** (contacts route chunk) |
| `@tanstack/virtual-core` | 3.19.11 | MIT | 9.5 KB | (the React adapter is only 0.7 KB of the above) |
| `react-window` | 1.8.11 | MIT | 3.8 KB | Reject — 6.4 KB cheaper, but owns the DOM, no dynamic measurement, fights `<table>` |
| `react-virtuoso` | 4.18.21 | MIT | 20.4 KB | Reject — 2× the cost, takes over the scroll container |
| `@tanstack/react-table` | 8.21.3 | MIT | 13.0 KB | **Reject** — see §3.2 |
| `@radix-ui/react-dialog` | 1.3.1 | MIT | 13.9 KB | Reject — native `<dialog>` is 0 KB |
| `@radix-ui/react-checkbox` | 1.4.1 | MIT | 5.9 KB | Reject — native input is 0 KB |
| `@radix-ui/react-dropdown-menu` | 2.2.1 | MIT | 31.6 KB | Reject — 70% of shell headroom |
| `@radix-ui/react-select` | 2.4.1 | MIT | 31.8 KB | Reject — native `<select>` is 0 KB |
| `@radix-ui/react-popover` | 1.3.1 | MIT | 24.4 KB | Reject |
| `@radix-ui/react-tooltip` | 1.4.1 | MIT | 19.9 KB | Reject |
| `@radix-ui/react-tabs` | 1.2.1 | MIT | 9.5 KB | Reject |
| Radix set (7 packages above) | — | MIT | **44.5 KB** | Reject — 98.7% of the 45.1 KB headroom |
| `react-aria-components` (3 components) | 1.22.1 | Apache-2.0 | 38.7 KB | Reject — licence is fine, size is not |
| `@headlessui/react` (3 components) | 2.2.10 | MIT | 38.9 KB | Reject |
| `@ariakit/react` (3 components) | 0.4.43 | MIT | 40.3 KB | Reject |
| `@base-ui/react` (3 components) | 1.9.0 | MIT | 65.6 KB (23.4 KB for Dialog alone) | Reject |
| `cmdk` (Command only) | 1.1.1 | MIT | 17.5 KB | Defer to the palette milestone, lazy chunk |
| `@floating-ui/react-dom` | 2.1.9 | MIT | 7.6 KB | Hold — the cheapest route to anchored positioning if a Menu is ever needed |

This revises spec §7's "Radix primitives + Tailwind". Radix was a reasonable default before anyone measured it; at 44.5 KB gzipped against 45.1 KB of headroom it cannot coexist with a data table, a command palette and seven more milestones. The spec's other two frontend library calls stand unchanged: `@tanstack/react-virtual` for virtualisation (§7 already names it) and `@xyflow/react` (MIT) for M4's flow builder, lazily loaded. Every licence above is on the `web/scripts/check-licenses.mjs` allow-list; no new entry is needed.

### 2.5 Budget after M1a

| | gzip |
|---|---|
| Shell today (measured) | 108,532 B |
| + theme bootstrap, StatusRegion, Cmd+K stub, token-based `ui.tsx` (net ≈ 0, `FOCUS` and the `dark:` variants are deleted) | ≈ +1,000 B |
| **Shell after M1a** | **≈ 109.5 KB of 150 KB** |
| Contacts route chunk: react-virtual 10.2 + table, selection store, toolbar ≈ 9 | ≈ 19 KB |
| Contact detail / lists / tags / fields / suppressions chunks | 2–4 KB each |

**Add a second gate to `web/scripts/check-bundle.mjs`: no single lazy route chunk over 50 KB gzipped.** The existing check only guards the shell, which means the contacts route could grow to any size without CI noticing — and the contacts route is the one that will.

---

## 3. The data table

The constraint is "usable at 10 million rows". The first design decision is the one that makes all the others tractable: **the browser never knows there are 10 million rows.** It holds at most ~2,000. Everything below follows from that.

### 3.1 Data flow: keyset pagination, bounded window

```
GET /api/v1/workspaces/{id}/contacts
    ?q=&filter=<rules>&sort=email&dir=asc&after=<cursor>&limit=100
→ { data: [...100], next_cursor, selection_token, count_estimate, count_exact? }
```

- **Keyset (cursor) pagination only. No `OFFSET`, no page numbers.** `OFFSET 9_999_000` makes Postgres walk ten million rows to discard them; spec §9 budgets 150 ms p95 for a list response and that query cannot meet it at any index. The cursor is `(sort_value, contact_id)` — the id tiebreaker is mandatory, otherwise two contacts with the same name make the keyset non-total and a page boundary silently drops or repeats a row.
- **Every sortable column must be backed by an index including the id tiebreaker.** A column with no such index is not sortable in the UI. The column spec carries a `sortable` flag and the UI simply does not render a sort control for the rest; it never offers a sort it cannot serve in 150 ms.
- **TanStack Query `useInfiniteQuery` with `maxPages: 20`** and both `getNextPageParam` and `getPreviousPageParam` defined. `maxPages` evicts pages from the far end as the user scrolls, so memory is bounded at 2,000 rows regardless of how far they scroll, and scrolling back re-fetches. This is a v5 feature already available in the pinned `@tanstack/react-query@5.107.1` — no new dependency.
- **The audit log's hand-rolled cursor accumulator does not generalise to this.** Its comment ("Deliberately not `useInfiniteQuery` … Reach for it if a second screen needs one") anticipated exactly this moment. M1a is that second screen; adopt `useInfiniteQuery` here and leave the audit log alone until it needs changing for another reason.
- **The scrollbar is honest about being infinite**, not proportional to 10M. There is no "jump to row 4,500,000" — it has no meaning in a keyset world and no user has ever wanted it. What users want instead is search, filters and sort, which are the things that actually narrow 10M to 50.

### 3.2 Why no `@tanstack/react-table`

Every row-model this library exists to build — filtering, sorting, pagination, grouping — happens in Postgres. What would remain is column definitions and header rendering: a `Column[]` array of `{ id, header, accessor, width, priority, sortable, align }` and a `.map()`. That is about 100 lines, it is the thing we would read most often, and it costs 13.0 KB gzipped to import instead of writing. Reject. If M3's analytics screens ever need genuine client-side grouping over a small result set, revisit it there, in that chunk.

### 3.3 Virtualisation

**`@tanstack/react-virtual@3.14.26` (MIT, 10.2 KB gzipped), `useVirtualizer` in vertical mode.**

Why it beats the alternatives, concretely:

- **vs `react-window@1.8.11` (3.8 KB, MIT)** — 6.4 KB cheaper, but it renders its own absolutely-positioned `div` scaffolding. Getting a real `<table>` out of it means `innerElementType`/`outerElementType` hacks that break `position: sticky` on `<thead>`, and v1 cannot measure a row whose height it was not told. We need dynamic measurement for the comfortable density, where a long email address wraps, and at 200% zoom where everything wraps.
- **vs `react-virtuoso@4.18.21` (20.4 KB, MIT)** — twice the size, and it owns the scroll container, which makes a sticky header plus a sticky first column its problem rather than ours.
- **vs CSS `content-visibility: auto` (0 KB)** — free and genuinely good for long documents, but it does not reduce the number of React elements, does not give us `scrollToIndex` for keyboard navigation, and the browser still lays out every row's box.
- **For `@tanstack/react-virtual`** — headless: it returns offsets and we render the DOM, so the markup stays a real `<table>`. `scrollToIndex({ align })` is what makes arrow-key navigation out of the rendered window work at all. It shares `@tanstack/virtual-core` conventions with the two TanStack packages already shipping, so one mental model covers routing, data and virtualisation. MIT, already on the licence allow-list, and already named in spec §7.

**Rendering approach: real `<table>` with two spacer rows.** Not absolute positioning.

```
<table class="w-full table-fixed">        table-layout: fixed
  <colgroup>  one <col> per column, width from the column spec
  <thead>     <th scope="col"> sticky, opaque --sunken background
  <tbody>
    <tr role="presentation" style="height: {paddingTop}px">
    …the ~30 rendered rows, each with aria-rowindex…
    <tr role="presentation" style="height: {paddingBottom}px">
```

The spacer rows preserve `<table>` semantics, which means `position: sticky` on `<th>` keeps working, column widths stay consistent, and a screen reader still sees a table. Absolute positioning inside a `<table>` destroys all three.

- Fixed `estimateSize: () => rowHeightPx` read from `--row-h`. No per-row measurement in compact or default density — it is unnecessary work on the main thread for rows that are all the same height by construction (`overflow: hidden` + middle-truncation, §3.5).
- `measureElement` is enabled only in comfortable density and above 150% zoom, where cells wrap.
- `overscan: 8`.
- ~30 rows rendered at any time on a 1080p screen. React reconciles 30 rows per scroll frame, not 2,000.

### 3.4 Selection, when "all matching" is 1.2 million

**Selection is a predicate, not a list of ids.** This is the single most important decision in this document, because getting it wrong in an email product means sending to people the operator did not select.

```ts
type Selection =
  | { mode: "none" }
  | { mode: "include"; ids: Set<string> }                                  // ≤ 1,000
  | { mode: "all-matching"; token: string; excluded: Set<string> };        // unbounded
```

The server contract:

- Every list response carries a **`selection_token`**: opaque, signed, server-side, encoding `(workspace_id, query, filters, sort, as_of)` where `as_of` is the statement timestamp of the first page. Valid for one hour.
- Bulk endpoints accept **either** `{ ids: [...] }` (capped at 1,000) **or** `{ selection_token, excluded_ids: [...] }`. The browser never sends a million ids, and never could.
- The server re-resolves the token's query **as of `as_of`**, so a contact imported between the moment the operator pressed "Select all matching" and the moment they pressed "Add to list" is *not* included. Without the timestamp snapshot, a running CSV import silently joins every bulk action in progress. For a delete or a suppression that is a data-loss bug; for a list-add that feeds a campaign it is a compliance bug.
- A bulk action over a few thousand rows returns `202` with a `job_id`, not a result. The UI shows a progress row in the SelectionBar and a polite live-region update when it finishes.

The count, which is the other half of the problem:

- `count_estimate` comes back with the first page, from the planner's row estimate. It is rendered as **"About 1,240,000 contacts"**.
- `count_exact` is computed asynchronously (or synchronously when the estimate is under ~50,000) and swaps in: **"1,243,118 contacts"**. Spec §9 explicitly permits this: "The exact count may arrive asynchronously, and an estimate while typing is fine."
- **No screen ever blocks on `COUNT(*)` over 10M rows.** Not the table, not the segment preview, not the SelectionBar.
- Reserve the width of the count slot with `tabular-nums` and a `min-width`, so the estimate becoming exact does not reflow the toolbar.

The interaction, step by step:

1. Header checkbox unchecked. Operator checks it → the **loaded page** is selected (100 rows). The SelectionBar appears: *"100 contacts on this page selected."* with a link: *"Select all 1,243,118 matching"*.
2. They take the link → `{ mode: "all-matching" }`. Bar reads: *"All 1,243,118 matching contacts selected."* + *"Clear selection"*.
3. They uncheck a row → it joins `excluded`, the count decrements to 1,243,117, and the bar stays in all-matching mode. The header checkbox shows `aria-checked="mixed"`.
4. Changing the search, a filter or the sort **clears the selection** and says so in the live region: *"Selection cleared because the filters changed."* Silently carrying a selection across a query change is how an operator deletes the wrong million rows.
5. Destructive bulk actions over 1,000 rows (delete, suppress) open a Dialog whose title names the action and the object — *"Delete 1,243,118 contacts"*, never *"Are you sure?"* — and require typing the count to enable the confirm button. Reversible actions (add to list, add tag) do not confirm; they complete and offer Undo in the status region.

### 3.5 Column sizing, sticky headers, responsive

**Sizing.** `table-layout: fixed` with an explicit `<colgroup>`. Widths come from the column spec in `ch` units, which track the font: select 3rem, status 9ch, name 20ch, email 28ch, created 12ch, one `1fr` flex column. Fixed layout means the browser does not re-measure every cell when rows stream in — a `table-layout: auto` table over 2,000 rows reflows on every page append and will blow the INP budget on its own.

**Resizing is deferred past M1a**, because a density toggle plus sensible defaults covers the actual need and a resize gripper is a WCAG 2.5.7 problem. When it does land, the contract is fixed now: the gripper is a focusable `<div role="separator" aria-orientation="vertical" aria-valuenow aria-valuemin aria-valuemax>`, `ArrowLeft`/`ArrowRight` move it ±8px and `Shift` ±32px, and `Home` resets the column. Pointer dragging is an enhancement on top of that, never the only route — 2.5.7 requires a single-pointer alternative to every dragging movement.

**Sticky headers.** `position: sticky; top: 0` on each `<th>` (not on `<thead>`), with an opaque `--sunken` background and a `--border` bottom edge, `z-index: 2`. The scroll container carries `scroll-padding-block-start: var(--row-h)` so that a row scrolled into view by keyboard is never parked underneath the header — WCAG 2.2 2.4.11 Focus Not Obscured, which `better-layout` and `better-accessibility` both flag as a HIGH failure when a sticky header covers focus. The identity column (checkbox + name) is `position: sticky; left: 0; z-index: 1` with the row background, so it survives horizontal scrolling.

**Responsive, without a second component.** Every column carries `priority: 1 | 2 | 3`.

- Container ≥ 64rem: all priorities.
- 40–64rem: priorities 1 and 2; priority 3 drops out.
- < 40rem: priority 1 only — select, identity (name over email, two lines), status. The rest reachable by opening the contact.
- Whatever still overflows scrolls horizontally inside the scroller, with the identity column stuck to the inline-start edge.

Driven by a **container query** on the table's wrapper, not a viewport media query, so the table behaves the same inside a drawer as on a full page. One DOM, one code path, one set of ARIA. The alternative — a card list below the breakpoint — doubles the component and changes the accessible structure between breakpoints, which is worse for a screen-reader user who resizes.

Row height on coarse pointers is 44px via `--row-h`, and row actions are **always visible, never hover-revealed** — hover-only actions do not exist on touch, and `better-layout` calls revealing the only action on hover a critical failure.

### 3.6 Keyboard and screen reader

**The table is `role="grid"` with roving tabindex at the row level.** One tab stop for the whole table.

A plain `<table>` is the right default for a static list, but here each row carries a checkbox, a link and an action button: 100 rendered rows would otherwise mean 300+ tab stops between the toolbar and the pagination, which is unusable with a keyboard. Cell-level roving tabindex (the full APG grid pattern) is the other extreme and makes simple row-by-row reading needlessly slow. Row-level roving, with opt-in cell navigation, is what Gmail and GitHub converged on and it is right here too.

| Key | Action |
|---|---|
| `Tab` | Into the table (lands on the last-focused row, or the first), then out to the next control |
| `↑` / `↓` | Previous / next row; `scrollToIndex` pulls rows in when moving past the rendered window |
| `→` | Enter cell navigation within the focused row |
| `←` | Back out to row level |
| `Home` / `End` | First / last **loaded** row |
| `Ctrl+Home` | First row of the result set (re-queries from the start cursor) |
| `PageUp` / `PageDown` | One viewport of rows |
| `Space` | Toggle selection of the focused row |
| `Shift+Space`, `Shift+↑/↓` | Extend the selection range |
| `Ctrl/Cmd+A` | Select all **loaded** rows; the SelectionBar then offers all-matching. Never silently select 10M. |
| `Enter` | Open the focused contact (activates the row's real `<a>`) |
| `Escape` | Clear the selection |
| `/` | Focus the search input |

The row's identity cell is a **real `<a href>`**, so middle-click and Ctrl+click open a new tab. The `<tr>` itself carries no click handler — a clickable row containing a checkbox and a link is three overlapping targets and no accessible name.

**Screen reader contract for a virtualised grid.** A virtualised table lies about its size unless told not to:

- `aria-rowcount` on the `<table>`: the exact total when known; **`-1`** while only an estimate exists. `-1` is the defined value for "unknown" and is the honest answer — announcing a planner estimate as a row count is worse than announcing nothing.
- `aria-rowindex` on every rendered `<tr>`, 1-based including the header row. Without it, a screen reader announces "row 3 of 1,243,118" for whatever happens to be rendered third.
- `aria-colindex` on cells, because columns drop out responsively and the visual column number stops matching the DOM position.
- `aria-colcount` on the table — the full column count, not the rendered one.
- Sortable headers: a `<button>` inside the `<th>`, with `aria-sort="ascending" | "descending"` on the `<th>` itself and only ever on one at a time.
- `aria-selected` on selected rows (valid under `role="grid"`), plus the row checkbox state, plus the `--row-selected` background, plus a 3px inline-start accent bar — four cues, because selection is the state most often lost.
- Spacer rows are `role="presentation"` and carry no `aria-rowindex`.
- One polite live region for the whole table: *"1,243,118 contacts"*, *"Loading more"*, *"24 contacts selected"*. Debounced to 500 ms, so typing in the search box does not produce a stream of announcements. Never assertive.
- `<caption>` names the table ("Contacts") even when a visible heading already does; it is what a screen reader's table list shows.

### 3.7 INP under 200 ms

The specific failure modes for a table this size, and the specific countermeasures:

1. **Typing in search re-renders 2,000 rows per keystroke.** The search input is uncontrolled with a 250 ms trailing debounce before the query changes. The input's own value never lives in a state that the table subscribes to.
2. **Ticking one checkbox re-renders every row.** Selection lives in an external store read through `useSyncExternalStore`, with each row subscribing to its own id. Toggling a row re-renders one row. A `useState<Set>` in the table component re-renders all of them and is the single easiest way to miss the INP budget here.
3. **Scroll handlers doing work.** None. The virtualiser already batches on rAF. No `onScroll` in application code.
4. **Appending a page re-reconciles the whole body.** Stable `key={contact.id}`, fixed row heights, `table-layout: fixed`.
5. **Sorting or filtering client-side.** Never. Both are server round-trips; the table shows the previous data at reduced opacity with `aria-busy` while the new page loads, rather than unmounting to a spinner (which costs a layout and a scroll-position reset).
6. **Hover styling via JS.** CSS `:hover` only.

CI already runs Lighthouse per spec §7; add the contacts route to it with an INP assertion, and a Playwright test that renders 2,000 rows and measures the time from a checkbox click to the next paint.

---

## 4. Page patterns

### 4.1 List page (contacts, lists, tags, custom fields, suppressions)

```
┌ h1 + count ─────────────────────── [ primary action ] ┐   title row
├ search  [filters] [sort] [density] [columns] ─────────┤   toolbar, sticky
├ ═══ SelectionBar (replaces the toolbar when active) ══┤
├ table ────────────────────────────────────────────────┤   scroller, sticky thead
└ "Loading more…" / "End of results" ───────────────────┘
```

- One `<h1>`, the resource name, with the count beside it — "Contacts · about 1,240,000".
- Exactly one solid button per page (`better-layout`: one primary action per view). On contacts that is "Add contact"; import and export are outline buttons.
- The toolbar is sticky below the page header and is the only sticky element besides `<thead>`, so that `scroll-padding-block-start` has one known value.
- Search is debounced, server-side, and its current value is reflected in the URL so the view is linkable and the back button works.
- Filters are a Drawer, not a popover: the segment rule builder in M1b needs the room, and the same component then serves both.
- The row count, the filters and the sort all live in the URL. A support engineer must be able to ask an operator for the link.

### 4.2 Detail page (contact)

One column, maximum `68ch` for prose, with a sticky in-page nav on the inline-start at `lg` and above. Order is fixed and matters:

1. **Identity header** — email as the `<h1>`, display name, and immediately beneath it the two states that change what the system will do: **suppression** and **subscription**. If the contact is suppressed, a danger-subtle banner says so, says why (hard bounce, complaint, manual, instance-wide), says when, and is the first thing after the heading. An operator who misses this sends a campaign that will never arrive.
2. **Custom fields** — a two-column `<dl>` at `md` and above, one column below. Typed rendering: dates in the viewer's locale, booleans as Yes/No not ✓/✗, numbers right-aligned with `tabular-nums`, empty values as a muted "Not set" rather than a blank cell or an orphaned dash.
3. **List memberships** — one row per list with its own subscription status (subscribed / unconfirmed / unsubscribed), the date, and the source (form, import, API, manual). Per-list status is the thing Interspire gets wrong and the thing this screen exists to show.
4. **Tags** — badges with a remove button each, plus "Add tags" opening the picker Dialog.
5. **Consent and activity** — the consent log; the activity timeline is a Later feature and its heading is simply absent until then.

Edits are inline per section with a per-section Save, not one page-wide form. A page-wide form over five independent sections means a validation error in a custom field blocks saving a tag change.

### 4.3 Settings page (custom field definitions, workspace settings)

Panels in one column, `max-width: 48rem`, each with an `<h2>` and a one-sentence description under it. Destructive settings live in a final panel with a `--danger` border, separated by 48px. Each panel saves independently; nothing auto-saves without a visible confirmation in the status region. Changing a custom field's **type** is a migration, not a setting: it opens a Dialog that states what happens to existing values and requires typing the field name.

### 4.4 Modal and drawer

One component. `<dialog showModal()>`, `--radius-lg`, `--shadow-dialog`, backdrop `oklch(0.23 0.018 265 / 0.45)` fading over `--dur-2`.

- **Dialog**: centred, `min(32rem, 100% - 2rem)`, for create/edit/confirm. On viewports under 40rem it becomes a bottom sheet occupying the lower 90%, because a centred dialog with a keyboard open on a phone shows about two fields.
- **Drawer**: inline-end edge, `min(36rem, 100%)`, full height, for filters and quick-view. Also modal — a non-modal panel that takes focus is the worst of both.
- Title is an `<h2>` referenced by `aria-labelledby`. The footer holds at most two actions; the confirming one is last in the DOM and visually last.
- Escape closes; a dirty form asks first. Focus returns to the trigger. Never nest a dialog inside a dialog.

### 4.5 Empty state

Three distinct kinds, and conflating them is the most common bug in this pattern:

| Kind | Copy pattern | Action |
|---|---|---|
| **Nothing yet** | "No contacts yet. Import a CSV, add one by hand, or point a signup form at this workspace." | Primary: Import contacts. Secondary: Add contact. |
| **Nothing matches** | "No contacts match **jo@**." — names the query back. | "Clear search" / "Clear filters". |
| **Nothing permitted** | "You need the editor role to see contacts." | Who to ask, not a dead button. |

Centred, `max-width: 42ch`, 64px of vertical space, `--fg-muted` body, one `<h2>`, one sentence, at most two actions. No illustration — it costs bytes and says nothing. Never put persistent information in an empty state, because it vanishes the moment the first row arrives.

### 4.6 Error state

Says what failed, in the interface's voice, and what to do. Never apologises, never says "Oops", never shows a stack trace or a raw problem URN. The API's `problemMessage`/`problemType` helpers already exist for this and are already used correctly in `members.tsx`.

- **Whole-page failure**: `role="alert"` panel with a `--danger` border — "Contacts could not be loaded." + the server's message when it is human-readable + a Retry button that re-runs the query (not a page reload).
- **Partial failure** (the list loaded, a bulk action failed): the status region's alert channel, plus the SelectionBar staying put so the selection is not lost.
- **Per-field failure**: inline, below the field, `aria-describedby`, `aria-invalid` — as `Field` already does.
- A form with more than three fields also gets a focusable error summary at the top (`role="alert" tabindex="-1"`, focused after a failed submit, each item linking to its field) **in addition to** the inline errors, never instead of them.
- **Never a toast-only error.** A toast that disappears after four seconds is not an error report.

### 4.7 Bulk action bar

Replaces the toolbar in place when a selection exists — it does not float over the table and does not push the rows down. Sticky at the top of the scroller, `--accent-subtle` background, `--accent` inline-start edge.

```
[✓] All 1,243,118 matching contacts selected.   Clear
    [Add to list] [Add tag] [Remove tag] [Export]   [Suppress] [Delete]
```

- The count is the left-most thing and uses `tabular-nums`.
- Destructive actions are separated by 24px and use the danger-ghost variant — they are not solid, because a solid danger button in a bar that appears automatically is an accident waiting to happen.
- Every label is a verb plus an object. No "Apply", no "Actions ▾".
- Reversible actions complete and offer Undo in the status region. Irreversible actions over 1,000 rows require typing the count.
- The bar is a landmark-free `<div role="region" aria-label="Bulk actions">`, announced politely when it appears, and reachable with one `Shift+Tab` from the table.

---

## 5. What NOT to do

**Data and scale**

1. Do not paginate with `OFFSET`. At 10M rows `OFFSET` is a sequential scan of everything it discards, and §9's 150 ms p95 is unreachable.
2. Do not block a screen on `COUNT(*)`. Render the planner estimate as "about N", swap in the exact count when it arrives.
3. Do not send a list of ids for a bulk action. Send the query. A million-id request body is not a performance problem, it is a broken design.
4. Do not let "select all" quietly mean "the hundred rows I have loaded". Say which it is, every time, in words.
5. Do not re-resolve a bulk action against live data. Snapshot the query with `as_of`, or a running import joins the selection between the preview and the confirm.
6. Do not carry a selection across a filter change. Clear it and say so.
7. Do not offer a sort on a column with no supporting index. Do not render a control the server cannot serve.
8. Do not hold more than ~2,000 rows in the browser. `maxPages` exists; use it.

**Bundle**

9. Do not adopt a component library wholesale. The Radix set measures 44.5 KB gzipped against 45.1 KB of headroom; MUI, Ant Design and Chakra are an order of magnitude worse and none of them is on the licence allow-list without checking.
10. Do not import a library to replace a native element that already works. `<dialog>`, `<select>`, `<input type=checkbox>` and `<details>` are free, accessible and already installed in every browser.
11. Do not put the command palette, the editor, the flow builder or any chart library in the shell chunk. `import()` them.
12. Do not let the contacts route grow unmetered — the existing bundle gate only measures the shell.

**Accessibility**

13. Do not remove a focus outline without a verified replacement, and never draw the ring flush against an accent fill (1:1 contrast). `outline-offset: 2px`, always.
14. Do not use `aria-live="assertive"` for success, progress or anything a person is not blocked on.
15. Do not create a live region together with its first message — mount it empty at app start or it is never announced.
16. Do not disable a submit button because a form is invalid. Validate on submit and move focus to the first invalid field.
17. Do not use a placeholder as a label, and do not block paste in any field — including the TOTP field (WCAG 2.2 3.3.8).
18. Do not let a sticky header or a bulk-action bar cover the focused row (2.4.11).
19. Do not reveal a row's only action on hover; there is no hover on a phone.
20. Do not make column resizing drag-only (2.5.7 requires a single-pointer alternative — arrow keys on the separator).
21. Do not convey subscription or suppression status with colour alone (1.4.1).
22. Do not reorder content with CSS `order` or `row-reverse`; DOM order is the reading order.
23. Do not virtualise a grid without `aria-rowcount`/`aria-rowindex` — a screen reader will announce "row 3 of 30" over a million-row result.

**Tokens and theme**

24. Do not put semantic colours in a plain `@theme` block. Tailwind v4 inlines those values into the generated utilities, dark mode silently stops working, and the bug is invisible in review. Use `@theme inline` over `:root` variables.
25. Do not run `prefers-color-scheme` *and* a theme attribute. Pick one; the attribute, set before first paint.
26. Do not reverse the light ramp to make the dark one. Drop the accent's chroma and re-measure every pair.
27. Do not name a semantic token after a hue (`--color-blue-button`) or a component (`--contacts-table-border`).
28. Do not raise `html { font-size }` down to 14px to get density.
29. Do not give everything the same border radius, the same 1px border and the same soft grey shadow — that is the SaaS-card template default. Rows have no radius; only the top layer has a shadow.

**Copy and content** (from `better-writing` and `break-ui`)

30. Do not write "1 members". `Intl.PluralRules`, everywhere, including the selection count.
31. Do not truncate a number, an amount or a date. Truncate secondary metadata at the end; truncate **emails, file names and paths in the middle**, where the distinguishing part is at the end.
32. Do not render a bare dash for a missing optional value. "Not set", in `--fg-muted`.
33. Do not title a confirmation "Are you sure?". Name the action and the object: "Delete 1,243,118 contacts".
34. Do not label a button "Apply", "Submit" or "OK". Verb plus object, and the same verb in the resulting confirmation: "Suppress" → "Suppressed".
35. Do not show a number without `tabular-nums` if it changes in place.
36. Do not test the table with tidy fixtures. Test it with a 254-character email, a name in a script with tall diacritics, a tag called "🎉 Q4 — промо", a contact with 47 list memberships, a count of exactly 1, zero rows, and 1,000 rows — then at 320px and at 200% zoom, in both themes.

---

## 6. Sources

**Repository (read in full)**

- `web/src/lib/ui.tsx` — the current `BUTTON`/`SECONDARY_BUTTON`/`CARD`/`ROW_BUTTON`/`SELECT` constants plus `Field`, `Alert`, `ConfirmButton`, `CopyButton`. The accessibility wiring here is already correct and this proposal keeps all of it; what changes is that the colours become tokens and the `FOCUS` constant becomes a base rule. `ConfirmButton`'s `autoFocus` comment and `CopyButton`'s non-secure-context handling are both load-bearing — do not simplify them.
- `web/src/routes/*.tsx` — `members.tsx` and `audit-log.tsx` are the two existing tables and both are honest about their limits in comments. The audit log's "Deliberately not `useInfiniteQuery` … reach for it if a second screen needs one" is what §3.1 acts on. `members.tsx`'s `failure()` / `problemMessage` split is the error-copy pattern §4.6 generalises.
- `web/src/styles.css`, `web/package.json`, `web/vite.config.ts`, `web/index.html` — Tailwind v4 via `@tailwindcss/vite`, no config file, `prefers-color-scheme` only. §1.1 replaces the theming mechanism and nothing else.
- `web/scripts/check-bundle.mjs` — the measurement method every size figure in this document uses. `web/scripts/check-licenses.mjs` — the allow-list every dependency above was checked against.
- `web/src/router.tsx` — every route is already `lazyRouteComponent`, which is what makes "put it in the route chunk" a real answer rather than a hope.
- `.scratch/email-platform/spec.md` §6, §7 (UI, Security, Licensing), §9, §13 — §7 already names TanStack Virtual and xyflow; §2.4 revises only its "Radix primitives" line, with measurements. §13.2 is partly closed by §1.
- M0's global constraints (review notes kept outside the repository) — "Build functional, accessible screens with plain Tailwind and no bespoke design system" was correct while §13.2 was open; this document is the proposal to supersede it.

**Measurements taken for this document**

- Bundle sizes: each package installed at the version listed in §2.4 and bundled with the esbuild already present in the Vite toolchain (`bundle, minify, format=esm, target=es2022`, React external, `NODE_ENV=production`), then gzip level 9 — identical compression to `check-bundle.mjs`. Licences read from each package's own `package.json`.
- Contrast: every pair in §1.2 computed from OKLCH through the sRGB transfer function to WCAG 2.x relative luminance, with an sRGB gamut check on each colour. Five candidate values failed on the first pass (`--fg-subtle` at 4.28:1, `--border-strong` at 2.88:1 light and 2.41:1 dark, and four colours outside sRGB) and were corrected before being written down. The ratios in the tables are the measured values for the published tokens.
