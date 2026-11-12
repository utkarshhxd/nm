Status: needs-triage
Labels: needs-triage

# Several panels load 200 rows and stop, with no Load more

Found in M1a task 17 review. Each site already carries a `shortcut:` comment saying so, which is
this repo's convention for a known limit rather than an oversight.

## The sites

- `web/src/routes/contact-detail.tsx` -- the contact's Lists, Tags and custom-field panels
- `web/src/components/contact-form.tsx` -- the custom field definitions the form renders
- `web/src/routes/lists.tsx` -- the all-lists query (`["lists", ws, "all"]`)
- the list and tag pickers on the contacts grid and on contact detail

All request `limit: 200` and render what comes back. None renders a "Load more", and none tells
the reader there is a tail.

## What a workspace past 200 sees

Silence, which is the problem. A workspace with 250 tags shows 200 of them in the picker and no
indication that 50 are missing, so a bulk action is run on the tag someone could find rather than
the tag they meant. The lists screen is the same. Nothing looks wrong on screen, which is why this
needs a deliberate fix rather than a bug report later.

## Scope

Two different answers, which should not be conflated:

- **Panels** (contact detail's lists and tags) want the paging the main screens already have:
  `useInfiniteQuery` and a Load more button, as `suppressions.tsx` does.
- **Pickers** want search-as-you-type against the server, not paging -- nobody scrolls a
  2,000-item `<select>`. That is a different component and a different endpoint shape, and it is
  the larger piece of work.

Until either lands, the honest interim is one line of text under a capped list saying it is
showing the first 200.
