-- SPDX-License-Identifier: AGPL-3.0-only

-- Labels on a contact. A tag is never a send target and carries no subscription state: tags
-- organise, lists (see 0012) receive mail. So this table has a name and nothing else to say.
-- `unique (workspace_id, id)` is not redundant with the primary key: `contact_tags` takes a
-- composite foreign key onto it.
create table tags (id uuid primary key,
  workspace_id uuid not null references workspaces(id) on delete cascade,
  name text not null,
  created_at timestamptz not null default now(),
  unique (workspace_id, name), unique (workspace_id, id));

-- BOTH foreign keys are composite and carry `workspace_id`: a row joining a contact of one
-- workspace to a tag of another cannot be written at all. The database refuses it, not a handler.
-- Deleting a tag (or a contact) takes its rows with it.
create table contact_tags (workspace_id uuid not null,
  contact_id uuid not null, tag_id uuid not null,
  created_at timestamptz not null default now(),
  primary key (contact_id, tag_id),
  foreign key (workspace_id, contact_id) references contacts (workspace_id, id) on delete cascade,
  foreign key (workspace_id, tag_id) references tags (workspace_id, id) on delete cascade);

-- The contact list's `tag_id` filter, and "who has this tag".
create index contact_tags_by_tag on contact_tags (workspace_id, tag_id, contact_id);
