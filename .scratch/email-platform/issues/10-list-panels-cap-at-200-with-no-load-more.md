Status: needs-triage

# Several panels load 200 rows and stop, with no Load more

Found in M1a task 17 review; the sentence below was corrected after the PR gate review, which
found that four of the nine sites carry no `shortcut:` comment -- and they are the four this issue
calls the worse case.

## The sites

Five carry a `shortcut:` comment, this repo's convention for a known limit rather than an
oversight:

- `web/src/routes/contact-detail.tsx:350`, `:540`, `:706` -- the contact's Lists, Tags and
  custom-field panels
- `web/src/components/contact-form.tsx:123` -- the custom field definitions the form renders
- `web/src/routes/lists.tsx:60` -- the all-lists query (`["lists", ws, "all"]`)

Four carry none, so there the cap is undocumented as well as silent -- the list and tag pickers,
which are the ones a bulk action is chosen from:

- `web/src/routes/contacts.tsx:153` and `:162` -- the pickers on the contacts grid
- `web/src/routes/contact-detail.tsx:365` and `:553` -- the pickers on contact detail

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
showing the first 200. Cheaper still, and worth doing first because it is a comment-only change:
give the four picker queries the `shortcut:` comment the other five already have, so the cap is at
least documented where someone reads the code.
