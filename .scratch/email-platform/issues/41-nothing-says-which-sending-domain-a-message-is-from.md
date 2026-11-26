Status: open — campaign half closed by M2c; the transactional default remains, for M5

**Closed by M2c (R3, R5, R6):** a campaign carries `sending_domain_id`, `from_local_part`, an
optional `from_name` per variant and an optional `reply_to`, and header From and envelope From are
`{from_local_part}@{domain}` (spec M2c §4.4). A campaign message, and the test send of a campaign,
no longer take the workspace's first domain by name.

**Still open, and narrowed to this:** the confirmation email keeps the alphabetical shortcut below,
so a workspace with two sending domains has its double opt-in mail come from whichever sorts first.
The fix is the default flag (option 1 below), which needs its own delete rule and screen and belongs
to M5. The rest of this record is as M2a wrote it and is true of the confirmation path only.

# Nothing says which sending domain a message is from

Found by M2a Task 10's implementer, which named it as the one rule it had to invent and said so
rather than letting it pass as a decision somebody had made.

## What happens

`sending_domains` is a per-workspace list. Nothing marks one as the default, and nothing attaches a
from-address to a list, a form or a job. So when the confirmation handler needs a from-domain, it
has nothing to ask.

It takes **the workspace's first sending domain by name**, with a `shortcut:` comment saying so. A
workspace with one domain behaves correctly. A workspace with two sends from whichever sorts first
alphabetically, **silently** — add `a.example.com` beside an existing `mail.example.com` and every
confirmation email changes its from-address with no warning and nothing in the audit log.

## Why it was not fixed in M2a

M2a's goal is that a confirmation email arrives at all, and the common case — one sending domain
per workspace — is correct today. The honest fix is a choice between two designs that belong to
later milestones:

1. **A default flag on `sending_domains`.** Smallest. Answers "which domain" for every transactional
   send, and leaves campaigns to override it. Needs a migration, an API field, a UI control, and a
   rule for what happens when the default is deleted.
2. **A from-address per campaign, per form, or per list.** What M2c needs anyway, since an operator
   picking a from-address is part of composing a campaign. Much larger, and it still needs an answer
   for transactional sends that belong to no campaign.

They are not exclusive: (1) is the fallback (2) would use.

## Why this is `ready-for-human` rather than `needs-triage`

It is a product decision, not a technical one. Whoever owns the product should say whether a
workspace is expected to have several sending domains at all. If the answer is "rarely, and
campaigns will pick", then (1) is enough forever. If workspaces routinely send as several brands,
the from-address belongs much closer to the content and (2) is the real shape.

## What to do in the meantime

Leave the shortcut, and leave its comment. It is deterministic, which matters more than being
clever: the same workspace sends from the same domain on every tick, and the behaviour can be read
off the sorted list without running anything.

**Do not make it "most recently created" or "first verified"** as a quick improvement. Both are
also arbitrary, both change under the operator's feet, and neither is any more explainable than
alphabetical — they would just be harder to notice.

## Related

- `spec.md` §3 puts campaigns in M2c, which is where a from-address per campaign would land.
- M2a spec §9 records that envelope From must equal header From, so whatever picks the domain picks
  both.
