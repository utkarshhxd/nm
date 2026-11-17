Status: needs-triage

# `chrono` and `chrono-tz` are unmaintained, and `cargo deny` is ignoring that on purpose

Filed from M1c, where a CI run went red on `cargo deny` without anything in this repository having
changed. Two advisories appeared from an advisory-database refresh:

- **RUSTSEC-2026-0543** — `chrono` is unmaintained; all versions affected.
- **RUSTSEC-2026-0555** — `chrono-tz` is unmaintained; all versions affected.

Both are `unmaintained`, not vulnerabilities. There is no exploit and no patched version, because
the finding *is* the maintenance status. `bans`, `licenses` and `sources` all stayed green.

Both are now in `deny.toml`'s `ignore`, with the rationale and the re-verify triggers written
beside them. **This issue exists because that ignore buys schedule, not safety.**

## Why it was not simply removed

`chrono` is not only a direct dependency. `Cargo.toml` builds `sqlx` with its `chrono` feature and
`utoipa` with its own, so dropping `chrono = { … }` and `chrono-tz = "0.10.4"` from our direct
list would leave the crate in the tree unchanged. Removing it for real means:

1. moving `sqlx` to its `time` backend, which changes the type every `timestamptz` column decodes
   into, across every query and every row struct in the codebase;
2. moving `utoipa` likewise, so the OpenAPI schema keeps describing the same wire format;
3. replacing `chrono-tz`'s timezone validation (`validate_timezone`) with an equivalent, since the
   IANA database has to come from somewhere;
4. regenerating `openapi.json` and `schema.d.ts` and confirming the wire format did not move —
   a serialisation change here is a breaking API change, not a refactor.

That is a project-wide migration. Doing it inside an unrelated milestone would have been the wrong
call, and doing it badly would change timestamps on the wire without anyone noticing.

## The actual risk being carried

An unmaintained crate is not harmless over time. Nothing is wrong with these versions today; the
exposure is that **the next CVE in either one never gets a fix**, and at that point the migration
above has to happen under time pressure rather than at leisure. `chrono` in particular has a
history of soundness findings, and it sits on every timestamp this product stores or serves.

## What should happen

Schedule the migration deliberately, as its own piece of work with its own plan — not as a
dependency bump. Before then, the ignore is reviewed on either of the triggers recorded in
`deny.toml`:

- either advisory is reclassified from `unmaintained` to a vulnerability or an unsoundness, or
- `sqlx` and `utoipa` both offer a `time` backend that can be taken together.

If the first trigger fires, the ignore comes out and the build goes red until the migration lands.
That is the intended behaviour, and the reason the ignore names the triggers instead of being
open-ended.
