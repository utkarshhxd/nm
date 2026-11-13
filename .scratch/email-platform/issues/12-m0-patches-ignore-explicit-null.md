Status: needs-triage

# Two M0 PATCH bodies silently ignore an explicit `null`, and the fix is one shared helper

Found in M1b task 7 while fixing the same defect on `PATCH /lists/{id}`. Not fixed there, on
purpose: they sit outside lists and contacts, and the task fixed only what it could test inside
its own scope.

## The defect

A plain `Option<T>` field cannot tell an absent key from `"key": null`; both deserialize to `None`,
and a `None` means "leave it alone". So a client that sends `null` gets `200` and no change, which
looks like success. M1a ruled against this for contacts (`timezone: null` did nothing), and M1b
fixed it for segments and lists. Two M0 bodies still have it:

| Type | Where | Fields that swallow `null` |
| --- | --- | --- |
| `UpdateCustomField` | `src/custom_fields/routes.rs:58` | `label`, `kind`, `options` |
| `UpdateWorkspace` | `src/workspaces/mod.rs:66` | `name`, `timezone` |

`workspace.timezone: null` is the **exact** shape ruled against for contacts in M1a: a known
defect class recurring in M0 routes, not a new question.

(`UpdateTag.name` is a required `String`, so `null` is already a `422` from serde. It needs
nothing.)

## What each field should do

Decide per field, by the rule the contacts, segments and lists routes already follow:

- `null` on a column that is `not null` with nothing to clear to: **`422`** (`validation`, "cannot be
  null"), and the whole patch is refused. By the migrations: `workspaces.name` and
  `workspaces.timezone` (`not null default 'UTC'`, so `null` is a `422`, not a reset), and
  `custom_fields.label` and `kind`.
- `null` where there is something to clear to: clear it. The one open choice is
  `custom_fields.options` (`not null default '[]'`): `null` could clear to `[]`, as a contact's
  name clears to `''`, or be a `422`. Decide it when this is picked up; either is fine, silence is not.
- The generated client must not offer `null` where the server answers `422`. Pin it with a schema
  test, as `the_patch_schema_is_nullable_only_where_null_means_something` (segments) and
  `the_list_patch_schema_is_nullable_only_where_null_means_something` (lists) do.

## The work is one extraction, not two local patches

The helper that makes this possible, `present`, is now **copy-pasted in four places**:

| Copy | Where |
| --- | --- |
| contacts | `src/contacts/routes.rs` |
| segments (routes) | `src/segments/routes.rs` |
| segments (rules) | `src/segments/rules.rs` |
| lists | `src/lists/routes.rs` |

```rust
fn present<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where D: Deserializer<'de>, T: Deserialize<'de> { T::deserialize(d).map(Some) }
```

Patching the two M0 bodies with a fifth and sixth copy is the wrong shape. Extract it **once**
(somewhere both `error` and the route modules can reach, `src/http/` or next to `ApiJson` in
`src/error.rs`), delete the four copies, and apply it to `UpdateCustomField` and `UpdateWorkspace`.
One focused task, one place to read what `null` means in a patch.

## Acceptance

- One definition of `present`; `grep -rn "fn present"` finds it once.
- For every field in the two tables: an explicit `null` is either `422` or a clear, never a silent
  `200` with no change. A test per field shows the row unchanged after a refused `null`.
- A schema test per type pins which fields the generated client may send `null` for.
- Each test shown red against a build with the helper removed from that type.
- Same sweep for any other `Update*` body added since: none known today.
