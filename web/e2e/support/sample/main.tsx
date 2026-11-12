// SPDX-License-Identifier: AGPL-3.0-only

// Every primitive once, in the states that have their own colours. It is a test fixture: the
// theme spec runs axe over it and the dialog test drives its dialog. Not part of the shipped app.

import { useState } from "react";
import { createRoot } from "react-dom/client";

import { Badge } from "../../../src/components/badge";
import { Button } from "../../../src/components/button";
import { Checkbox } from "../../../src/components/checkbox";
import { Dialog } from "../../../src/components/dialog";
import { EmptyState } from "../../../src/components/empty-state";
import { ErrorState } from "../../../src/components/error-state";
import { Field } from "../../../src/components/field";
import { Select } from "../../../src/components/select";
import { Skeleton } from "../../../src/components/skeleton";
import "../../../src/styles.css";

function Sample() {
  const [open, setOpen] = useState(false);
  return (
    <main className="mx-auto max-w-3xl p-6">
      <h1 className="text-xl font-semibold">Primitives</h1>

      <section aria-labelledby="s-actions" className="mt-8">
        <h2 id="s-actions" className="text-lg font-semibold">
          Actions
        </h2>
        <div className="mt-3 flex flex-wrap gap-3">
          <Button variant="solid" onClick={() => setOpen(true)}>
            Invite someone
          </Button>
          <Button>Export contacts</Button>
          <Button variant="ghost">Clear filters</Button>
          <Button variant="danger-solid">Delete contact</Button>
          <Button variant="danger-ghost">Suppress contact</Button>
          <Button disabled>Import contacts</Button>
          <Button busy>Saving contact</Button>
        </div>
      </section>

      <section aria-labelledby="s-fields" className="mt-8">
        <h2 id="s-fields" className="text-lg font-semibold">
          Fields
        </h2>
        <Field id="page-search" label="Search the page" placeholder="jo@example.com" />
        <Field id="f-hint" label="Display name" hint="Shown in the members list." defaultValue="Jo" />
        <Field id="f-error" label="Email" error="Enter an address like jo@example.com." defaultValue="jo@" />
        <Field id="f-disabled" label="Workspace" disabled defaultValue="Acme" />
        <div className="mt-4">
          <label className="block font-medium" htmlFor="f-role">
            Role
          </label>
          <Select id="f-role" className="mt-1" defaultValue="editor">
            <option value="viewer">viewer</option>
            <option value="editor">editor</option>
          </Select>
        </div>
        <div className="mt-4 flex items-center gap-3">
          <Checkbox aria-label="Select jo@example.com" />
          <Checkbox aria-label="Select all on this page" indeterminate />
          <Checkbox aria-label="Select a@example.com" defaultChecked />
        </div>
      </section>

      <section aria-labelledby="s-status" className="mt-8">
        <h2 id="s-status" className="text-lg font-semibold">
          Status
        </h2>
        <div className="mt-3 flex flex-wrap gap-3">
          <Badge variant="success" glyph="dot">Subscribed</Badge>
          <Badge variant="warning" glyph="ring">Unconfirmed</Badge>
          <Badge variant="neutral" glyph="slash">Unsubscribed</Badge>
          <Badge variant="danger" glyph="cross">Suppressed</Badge>
          <Badge variant="info">Scheduled</Badge>
          <Badge variant="accent">Q4 promo</Badge>
        </div>
        <div className="mt-6">
          <Skeleton label="Loading contacts" rows={2} />
        </div>
        <EmptyState title="No contacts yet" actions={<Button variant="solid">Import contacts</Button>}>
          Import a CSV, add one by hand, or point a signup form at this workspace.
        </EmptyState>
        <ErrorState title="Contacts could not be loaded." onRetry={() => undefined}>
          The server did not answer. Check the connection and try again.
        </ErrorState>
      </section>

      <Dialog open={open} onClose={() => setOpen(false)} title="Invite someone">
        <p className="mt-2 text-fg-muted">Nothing is emailed: send them the link this makes.</p>
        <Field id="d-email" label="Email" type="email" />
        <div className="mt-6 flex justify-end gap-3">
          <Button onClick={() => setOpen(false)}>Cancel</Button>
          <Button variant="solid" onClick={() => setOpen(false)}>
            Send invite
          </Button>
        </div>
      </Dialog>
    </main>
  );
}

const container = document.getElementById("root");
if (!container) throw new Error("sample/index.html is missing #root");
createRoot(container).render(<Sample />);
