Status: needs-triage

# The admin screens' colour contrast at 320px is largely unverified, and nobody knew

Surfaced by M1d Task 12, which made the accessibility helper report axe's **`incomplete`** bucket —
the results of rules that **ran but could not decide**. The helper previously destructured only
`violations` and `passes`, so every undecided result was silently discarded and every affected
scan read as clean.

**This is not an M1d defect.** The M1d public pages produce **zero** incompletes, and
`color-contrast` appears in `passes` by name on all twenty of their scans. What the new print
revealed is a pre-existing condition on the admin screens built in earlier milestones.

## What the full e2e run reports

**124 scans report an undecided `color-contrast`.** The two largest, both at 320px:

```
audit log (light, 320px): axe could not decide: color-contrast×133
audit log (dark,  320px): axe could not decide: color-contrast×157
```

The rest range from 1 to 9 nodes and span instance suppressions, members, api keys and others —
overwhelmingly at the 320px breakpoint.

## Why it matters

An undecided result is not a pass. For those 133 and 157 nodes, **nothing in this repository knows
whether the text meets WCAG 1.4.3**, and the test that scanned them reported success. Every
accessibility claim made about the admin screens at narrow widths currently rests on a bucket that
was being thrown away.

The one measured counter-fact worth keeping in view: axe-core 4.14.0 **does** resolve `oklch()`
correctly — M1d proved that across twenty scans — so "the stylesheet uses a colour space axe cannot
parse" is **not** the explanation.

## What is not yet known

The cause. `incomplete` for `color-contrast` is commonly benign — a decorative element whose
content is not text, or an element axe cannot resolve a background for because something overlaps
or clips it, which is exactly what a 320px viewport provokes. `theme.spec.ts`'s four-per-theme
incompletes were investigated during M1d and are decorative `aria-hidden` badge glyphs reported as
*"Element content contains only non-text characters"* — benign, and now documented in place.

**Whether the audit log's 133 and 157 share that cause is unknown.** It has not been checked, and
this issue deliberately does not guess: a count that large at one breakpoint is as consistent with
a real layout problem as with a benign one.

## First step

Print the `incomplete` entries' **reasons**, not just their counts — axe gives each node a
`reason`. One run answers whether this is one benign class repeated across a long table, or
something that needs fixing. Only then is it worth deciding what to do.

The helper already reports the bucket unconditionally for all eleven consumers, so this cannot
hide again; what it does not yet carry is the reason.

## Related

- M1d Task 12 made the bucket visible and added `requireRules`, so a sweep that names
  `color-contrast` now fails unless that rule genuinely **passed**. It is applied only to the
  public-page sweeps; extending it to the admin sweeps is the natural follow-up once the cause
  above is known.
