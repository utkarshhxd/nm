# 54 — Work deferred from the launch cut of M2c and M2d

**Found by:** the task and whole-branch reviews of M2c (campaigns and the send pipeline) and M2d (tracking and send-time bounces).
**Blocks:** nothing at launch. Clear it before the instance is shared by many workspaces or sends at volume.

M2c and M2d were cut to ship by the end of November 2026. Every finding that could send a message twice, mail someone who must not get it, leak data, or open a redirect was fixed before merge. This issue lists what was found and deliberately left for later, so none of it is lost.

## Product gaps (decided, not defects)

1. **Bounces and complaints that the relay reports after the send are not read.** OCI reports most bounces after the send, not as an SMTP rejection, so at launch most real bounces through OCI will not be suppressed automatically. This depends on the open live-OCI questions in the roadmap.
2. **No automatic pause on bad signals.** A misconfigured custom relay that answers `5.1.1` to every recipient would suppress the whole audience. Each suppression names its campaign, so it can be reversed. The OCI relay does not behave this way.
3. **Tracking cannot be switched off yet**, per workspace or per campaign. EU senders may need the switch before they rely on the product.
4. **No automatic A/B winner.** The winner is picked by hand, guided by clicks per link.
5. **Rotating the master key would break tracked links in mail already sent.** Tokens carry no key id. Key rotation does not exist yet.
6. **Opens and clicks are written during the request** on the API's connection pool, bounded only by the per-IP limit. A click storm after a very large send could starve the API pool. An instance-wide cap or a buffered writer is hardening work.
7. **Log output has not been audited** for relay text that carries a recipient address. Stored relay text is redacted; logs are not yet checked.

## Code and test follow-ups

### Send pipeline
- An orphaned launch job whose give-up keeps failing is logged every tick and stays at the head of the sweep (`limit 16`). Sixteen such jobs would starve the rest.
- Launch resolution is driven from `contacts`, so a small list in a very large workspace scans the whole workspace. It is measured fast today (0.11 s for 1,000 members in 1,000,000 contacts), but only with fresh statistics.
- Two job-payload statements in `launch.rs` lack a `workspace_id` predicate. The id is the claimed job's own, so nothing leaks; add the predicate for consistency.
- Outcome-to-record matching and envelope building are duplicated across the campaign send, the test send and the confirmation path. Move them into `deliver.rs`.
- `Creds::load` decrypts every relay's password for each confirmation.
- A suppression-read error in the test-send path, and in M2a's confirmation path, is not covered by a test. It needs an injected database error.

### Tracking and the report
- Two concurrent resumes of one campaign can overwrite a link table that the first re-freeze appended to. The window is narrow.
- Rewriting a link searches the link table linearly. Build a map once per render before templates with very many links are common.
- The `NM_PUBLIC_URL` exclusion compares strings, not origins.
- No test pins Outlook's VML button copy staying direct, or an `mj-button` whose link is the public URL.
- The report's four reads are not one snapshot. This is documented.
- The report screen flashes busy on a window-focus refetch, and its scroll wrapper has no `tabindex`.

### Web
- The campaign progress view: a verb in flight can be re-confirmed after a reset. The server refuses the duplicate.
- A failed time-zone read silently falls back to UTC.
- Several polling tests use fixed sleeps.

### Tests and benchmarks
- The send benchmark leaks its scratch database if it panics before cleanup.
- A lock-order test's name promises more than it checks: it guards a lost pause, not a deadlock.
