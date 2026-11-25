-- SPDX-License-Identifier: AGPL-3.0-only

-- What a campaign's body comes from (`templates`) and the look a workspace applies to all of them
-- (`brand_kits`).
--
-- Every index a table will ever need is in its `create table` below: `sqlx::migrate!` wraps every
-- migration in a transaction, so `create index concurrently` is not available later (0026
-- section 6 has the long version).
--
-- Both tables carry `workspace_id`. `templates` also carries a unique key on `(workspace_id, id)`,
-- as `relays` does (0028), so a later child table can reference it with a composite foreign key: a
-- row of one workspace naming a template of another cannot be written, and the handler's 404 is
-- backed by the database's refusal.

-- ---------------------------------------------------------------------------------------------
-- 1. `templates`
create table templates (
  id uuid primary key,
  workspace_id uuid not null references workspaces (id) on delete cascade,
  name text not null,
  kind text not null check (kind in ('blocks', 'mjml', 'html')),
  tree jsonb,
  source text,
  compiled jsonb,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  -- Not redundant with the primary key: Postgres requires a unique constraint on exactly these
  -- columns for a child table's composite foreign key to point at.
  constraint templates_workspace_id_id_key unique (workspace_id, id),
  -- A tree of `null`, `7` or `[]` would deserialize to an error at the moment the editor opens it.
  -- Null stays legal: a raw template has no tree at all (`templates_tree_xor_source` decides when).
  constraint templates_tree_is_object check (tree is null or jsonb_typeof(tree) = 'object'),
  -- Exactly one of `tree` and `source`, decided by `kind`. Here and not in the handler, because
  -- the handler is where M1's equivalent drifted.
  constraint templates_tree_xor_source check (
    case kind
      when 'blocks' then tree is not null and source is null
      else tree is null and source is not null
    end
  )
);

-- The list screen: "this workspace's templates, most recently edited first". `id` makes the order
-- total, so paging never repeats or skips a row that shares a timestamp.
create index templates_recent on templates (workspace_id, updated_at desc, id desc);

comment on table templates is
  'What a campaign body is made from. `kind` says how it is authored: `blocks` is the visual '
  'editor and keeps its document in `tree`; `mjml` and `html` are written by hand and keep '
  '`source`. Exactly one of the two is set, enforced by `templates_tree_xor_source`.';
comment on column templates.compiled is
  'The output of compiling `tree` or `source`, kept so a send does not recompile. Derived, so it '
  'may be null (never compiled yet) and may always be rebuilt from the authored column.';

-- ---------------------------------------------------------------------------------------------
-- 2. `brand_kits`
-- One row per workspace, which is why `workspace_id` is the primary key and there is no `id`.
create table brand_kits (
  workspace_id uuid primary key references workspaces (id) on delete cascade,
  logo_url text,
  palette jsonb not null default '{}'::jsonb,
  font_stack text,
  footer jsonb not null default '{}'::jsonb,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  -- `not null` does not stop the JSON value `null`, `7` or `[]`; the renderer reads these as maps.
  constraint brand_kits_palette_is_object check (jsonb_typeof(palette) = 'object'),
  constraint brand_kits_footer_is_object check (jsonb_typeof(footer) = 'object')
);

comment on table brand_kits is
  'The logo, colours, fonts and footer a workspace applies to its templates. At most one row per '
  'workspace; a workspace with no row simply has no brand kit.';
