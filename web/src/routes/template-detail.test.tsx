// SPDX-License-Identifier: AGPL-3.0-only

/**
 * The template detail screen with the API stubbed at the HTTP boundary, in a real browser.
 *
 * This is the level the stubs belong at: what the SCREEN does with a given response is this
 * lane's code, and every branch below is one the server can take — except the 409 on a save,
 * which `update_template` never answers (review W6, m1; left as found). That the server takes
 * them — that a `422 template-uncheckable` exists at all, that a merge tag naming an unknown
 * field is refused, that another workspace's template is a 404 — is proved against the real one
 * in `e2e/templates.spec.ts`, not here. Neither file would be evidence on its own.
 */

import { type Page, expect } from "@playwright/test";

import { test } from "../../e2e/support/app";
import type { components } from "../api/schema";

type Me = components["schemas"]["MeResponse"];
type TemplateView = components["schemas"]["TemplateView"];

const WS = "11111111-1111-4111-8111-111111111111";
const ID = "33333333-3333-4333-8333-333333333333";

const TEMPLATE: TemplateView = {
  id: ID,
  workspace_id: WS,
  name: "Welcome",
  kind: "blocks",
  tree: {
    blocks: [
      { type: "Text", runs: [{ type: "Text", text: "Hello", marks: [] }] },
      { type: "Button", label: "Read more", href: "" },
    ],
  },
  source: null,
  created_at: "2026-01-01T00:00:00Z",
  updated_at: "2026-01-01T00:00:00Z",
};

/** An RFC 9457 problem as `src/error.rs` writes one. */
const problem = (status: number, code: string, detail: string, errors?: unknown[]) => ({
  status,
  contentType: "application/problem+json",
  body: JSON.stringify({
    type: `urn:nm:problem:${code}`,
    title: "Error",
    status,
    detail,
    ...(errors ? { errors } : {}),
  }),
});

type Stubs = {
  template?: TemplateView | null;
  /** What `POST …/preview` answers. A function so a test can look at the request. */
  preview?: Parameters<Page["route"]>[1];
  patch?: Parameters<Page["route"]>[1];
  role?: Me["workspaces"][number]["role"];
};

/** A `200` preview carrying these findings, the way `PreviewResult` puts them on the wire. */
const withFindings = (findings: unknown[]) => ({
  status: 200,
  contentType: "application/json",
  body: JSON.stringify({
    html: "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>E</title></head><body></body></html>",
    text: "",
    findings,
  }),
});

const OK_PREVIEW = {
  status: 200,
  contentType: "application/json",
  body: JSON.stringify({
    html: "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Email</title></head><body><p>Hello</p></body></html>",
    text: "Hello",
    findings: [],
  }),
};

async function open(page: Page, app: string, stubs: Stubs = {}): Promise<void> {
  const me: Me = {
    csrf_token: "t",
    display_name: "Ada",
    email: "ada@example.com",
    id: "22222222-2222-4222-8222-222222222222",
    is_instance_admin: false,
    passkey_count: 0,
    totp_enabled: false,
    workspaces: [{ id: WS, name: "Acme", role: stubs.role ?? "editor", slug: "acme" }],
  };
  await page.route("**/api/v1/me", (route) => route.fulfill({ json: me }));
  // Anchored on the API path, not merely on `/templates/<id>`: the SCREEN's own address ends
  // that way too, and a pattern that loose answers the page's own navigation with JSON.
  // Registered before the preview route on purpose: Playwright tries the most recently added
  // handler first, and `…/templates/{id}/preview` also matches `…/templates/{id}`.
  await page.route(/\/api\/v1\/workspaces\/[^/]+\/templates\/[^/]+$/, (route) => {
    if (route.request().method() === "PATCH") {
      return stubs.patch
        ? stubs.patch(route, route.request())
        : route.fulfill({ json: { ...TEMPLATE, name: "Welcome" } });
    }
    const found = stubs.template === undefined ? TEMPLATE : stubs.template;
    return found
      ? route.fulfill({ json: found })
      : route.fulfill(problem(404, "not-found", "resource not found"));
  });
  await page.route(/\/api\/v1\/workspaces\/[^/]+\/templates\/[^/]+\/preview$/, (route, request) =>
    stubs.preview ? stubs.preview(route, request) : route.fulfill(OK_PREVIEW),
  );
  await page.goto(`${app}/w/acme/templates/${ID}`);
}

const unavailable = (page: Page) =>
  page.getByText("Preview and checks unavailable.", { exact: false });

test("a_finding_carries_a_path_and_the_panel_offers_the_block_it_names", async ({ page, app }) => {
  await open(page, app, {
    preview: (route) =>
      route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          html: "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>E</title></head><body></body></html>",
          text: "",
          findings: [
            // `at` is a PATH into the block tree (contract section 12), one-based on screen.
            { check: "link", severity: "warning", rule: "empty_href", message: "This link has no address.", at: [1] },
            { check: "lint", severity: "warning", rule: "at_rule_in_inline_style", message: "About the whole email.", at: null },
          ],
        }),
      }),
  });
  await expect(page.getByTestId("findings-count")).toHaveText("2 findings.");
  // The one with a path offers the control; the one without it does not, so exactly one exists.
  await expect(page.getByRole("button", { name: /^Show block/ })).toHaveCount(1);
  await expect(page.getByRole("button", { name: "Show block 2", exact: true })).toBeVisible();
});

test("the_preview_asks_about_the_document_being_edited_not_the_stored_one", async ({ page, app }) => {
  const bodies: unknown[] = [];
  await open(page, app, {
    preview: (route, request) => {
      bodies.push(request.postDataJSON());
      return route.fulfill(OK_PREVIEW);
    },
  });
  await expect(page.getByTestId("findings-count")).toHaveText("No findings.");
  // An empty body previews what is STORED, which is a different question: the operator is asking
  // what the email in front of them looks like, including the edit they have not saved.
  expect(bodies[0]).toEqual({ tree: TEMPLATE.tree });

  await page.getByRole("button", { name: "Add divider", exact: true }).click();
  await expect.poll(() => bodies.length).toBeGreaterThan(1);
  const last = bodies[bodies.length - 1] as { tree: { blocks: unknown[] } };
  expect(last.tree.blocks).toHaveLength(3);
});

test("a_check_that_could_not_run_is_never_shown_as_an_empty_findings_list", async ({ page, app }) => {
  await open(page, app, {
    preview: (route) =>
      route.fulfill(
        problem(422, "template-uncheckable", "this template is not valid MJML, so it could not be checked: bad token"),
      ),
  });
  await expect(unavailable(page)).toBeVisible();
  // The two ways this could have been read as a pass.
  await expect(page.getByTestId("findings-count")).toHaveCount(0);
  await expect(page.getByText("No findings.")).toHaveCount(0);
  await expect(page.getByRole("list", { name: "Findings" })).toHaveCount(0);
});

test("a_rate_limited_or_refused_preview_is_not_a_pass_either", async ({ page, app }) => {
  // 30 previews a minute for the whole workspace, shared with the segment preview: an editor
  // that debounces reaches it, and the person who does has done nothing wrong.
  for (const refusal of [
    problem(429, "rate-limited", "too many requests"),
    problem(403, "insufficient-role", "insufficient role"),
    problem(409, "template-stale", "the stored rendition was built from a different block tree. Open it and save it again."),
  ]) {
    await open(page, app, { preview: (route) => route.fulfill(refusal) });
    await expect(unavailable(page)).toBeVisible();
    await expect(page.getByText("No findings.")).toHaveCount(0);
  }
});

test("another_workspaces_template_is_not_found_and_is_not_an_error_state", async ({ page, app }) => {
  await open(page, app, { template: null });
  await expect(page.getByRole("heading", { name: "Template not found" })).toBeVisible();
  await expect(page.getByRole("button", { name: "Save template" })).toHaveCount(0);
  // Not "could not be loaded": a 404 is an answer, and offering Retry invites the operator to
  // ask again for something that is not there.
  await expect(page.getByText("The template could not be loaded.")).toHaveCount(0);
});

test("the_servers_own_sentence_is_what_a_refused_save_shows", async ({ page, app }) => {
  const UNKNOWN_FIELD = 'a merge tag names the field "not_a_field", which does not exist';
  await open(page, app, {
    patch: (route) =>
      route.fulfill(
        problem(422, "validation", "request validation failed", [{ field: "tree", message: UNKNOWN_FIELD }]),
      ),
  });
  await expect(page.getByRole("button", { name: "Save template", exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Save template", exact: true }).click();
  // The field's own message, which names the tag the operator wrote. "The template could not be
  // saved." is true of every refusal and tells them nothing.
  await expect(page.getByRole("alert")).toHaveText(UNKNOWN_FIELD);
  await expect(page.getByText("Template saved.")).toHaveCount(0);
});

test("the_save_sends_the_document_in_the_editor_under_the_column_its_kind_uses", async ({
  page,
  app,
}) => {
  const sent: unknown[] = [];
  await open(page, app, {
    patch: (route, request) => {
      sent.push(request.postDataJSON());
      return route.fulfill({ json: TEMPLATE });
    },
  });
  await page.getByRole("button", { name: "Add divider", exact: true }).click();
  await page.getByRole("button", { name: "Save template", exact: true }).click();
  await expect(page.getByText("Template saved.")).toBeVisible();
  // The name alone would save and say "Template saved." while the edit went nowhere. `kind` is
  // absent because it is not patchable, and `source` because a `blocks` template has none.
  expect(sent).toHaveLength(1);
  expect(sent[0]).toEqual({
    name: "Welcome",
    tree: { blocks: [...(TEMPLATE.tree?.blocks ?? []), { type: "Divider" }] },
  });
});

test("a_stale_rendition_says_which_one_and_that_saving_again_is_the_cure", async ({ page, app }) => {
  const STALE =
    "this template was compiled against custom fields that have since changed type or been purged (plan), so its merge tags no longer mean what they meant when it was saved. Open it and save it again.";
  await open(page, app, { patch: (route) => route.fulfill(problem(409, "template-stale", STALE)) });
  await page.getByRole("button", { name: "Save template", exact: true }).click();
  await expect(page.getByRole("alert")).toHaveText(STALE);
});

test("a_finding_inside_a_column_is_located_by_its_whole_path", async ({ page, app }) => {
  // Contract section 12 exists for this case: a block in a `Columns` child. A path cut to its
  // first index would still offer a button, but it would name and select the whole Columns block.
  await open(page, app, {
    template: {
      ...TEMPLATE,
      tree: {
        blocks: [
          {
            type: "Columns",
            children: [
              [{ type: "Text", runs: [{ type: "Text", text: "left", marks: [] }] }],
              [{ type: "Button", label: "Read more", href: "" }],
            ],
          },
        ],
      },
    },
    preview: (route) =>
      route.fulfill(
        withFindings([
          { check: "link", severity: "warning", rule: "empty_href", message: "This link has no address.", at: [0, 1, 0] },
        ]),
      ),
  });
  await page.getByRole("button", { name: "Show block 1.2.1", exact: true }).click();
  await expect(page.getByRole("group", { name: "Button properties" })).toBeVisible();
  await expect(page.getByRole("group", { name: "Columns properties" })).toHaveCount(0);
});

for (const kind of ["mjml", "html"] as const) {
  test(`a_hand_written_${kind}_template_is_previewed_from_its_source_and_its_findings_shown`, async ({
    page,
    app,
  }) => {
    // The server's own findings for each kind: an mrml dark-mode lint the operator cannot
    // otherwise discover, and for raw HTML the advisory that nothing was checked — which must
    // read as a finding, never as "No findings.".
    const FINDING =
      kind === "mjml"
        ? { check: "lint", severity: "warning", rule: "at_rule_in_inline_style", message: "`@media` inside `<mj-style inline=\"inline\">` is discarded without a warning", at: null }
        : { check: "lint", severity: "warning", rule: "not_checked", message: "a raw HTML template is not link-checked or style-checked", at: null };
    const bodies: unknown[] = [];
    const written = kind === "mjml" ? "<mjml><mj-body></mj-body></mjml>" : "<p>Hi</p>";
    await open(page, app, {
      template: { ...TEMPLATE, kind, tree: null, source: written },
      preview: (route, request) => {
        bodies.push(request.postDataJSON());
        return route.fulfill(withFindings([FINDING]));
      },
    });
    await expect(page.getByRole("list", { name: "Findings" })).toContainText(FINDING.message);
    await expect(page.getByText("No findings.")).toHaveCount(0);
    // It previews the text in the box, unsaved: `{source}`, never `{}` (the stored rendition).
    expect(bodies[0]).toEqual({ source: written });
    const box = page.getByRole("textbox", { name: kind === "mjml" ? "MJML" : "HTML", exact: true });
    await box.fill(`${written} `);
    await expect.poll(() => bodies.length).toBeGreaterThan(1);
    expect(bodies.at(-1)).toEqual({ source: `${written} ` });
  });
}

test("the_reason_a_preview_was_refused_is_shown_under_the_alert", async ({ page, app }) => {
  await open(page, app, {
    preview: (route) => route.fulfill(problem(429, "rate-limited", "too many requests")),
  });
  await expect(page.getByRole("alert")).toContainText(
    "Previewing is busy for this workspace — it will catch up in a moment.",
  );
});

test("a_viewer_is_not_offered_the_editor_or_the_save", async ({ page, app }) => {
  await open(page, app, { role: "viewer" });
  await expect(page.getByText("Your role can read templates but not change them.")).toBeVisible();
  await expect(page.getByRole("button", { name: "Save template" })).toHaveCount(0);
  await expect(page.getByRole("region", { name: "Block editor" })).toHaveCount(0);
  await page.getByRole("link", { name: "All templates" }).click();
  await expect(page.getByRole("heading", { name: "Templates", exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "New template" })).toHaveCount(0);
  await expect(page.getByRole("button", { name: /^Delete / })).toHaveCount(0);
});

test("a_viewer_previews_the_stored_rendition_with_an_empty_body_and_is_offered_no_editor", async ({ page, app }) => {
  const bodies: unknown[] = [];
  await open(page, app, {
    role: "viewer",
    preview: (route, request) => {
      bodies.push(request.postDataJSON());
      return route.fulfill(OK_PREVIEW);
    },
  });
  // `{}` is the server's stored-preview mode, the one a viewer may call: never `{tree}` or `{source}`.
  await expect(page.getByTitle("Email preview")).toBeVisible();
  expect(bodies).toEqual([{}]);
  await expect(page.getByRole("region", { name: "Preview and checks" })).toBeVisible();
  await expect(page.getByRole("region", { name: "Block editor" })).toHaveCount(0);
  await expect(page.getByRole("textbox")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Save template" })).toHaveCount(0);
});

test("switching_workspace_never_shows_the_last_workspaces_templates", async ({ page, app }) => {
  // Two workspaces this person belongs to. The list is cached per workspace; a cache keyed
  // without it would paint Acme's rows on Other's page while Other's own request is in flight.
  const OTHER = "44444444-4444-4444-8444-444444444444";
  const me: Me = {
    csrf_token: "t",
    display_name: "Ada",
    email: "ada@example.com",
    id: "22222222-2222-4222-8222-222222222222",
    is_instance_admin: false,
    passkey_count: 0,
    totp_enabled: false,
    workspaces: [
      { id: WS, name: "Acme", role: "editor", slug: "acme" },
      { id: OTHER, name: "Other", role: "editor", slug: "other" },
    ],
  };
  let release = () => {};
  const held = new Promise<void>((r) => (release = r));
  await page.route("**/api/v1/me", (route) => route.fulfill({ json: me }));
  await page.route(/\/api\/v1\/workspaces\/[^/]+\/templates(\?.*)?$/, async (route, request) => {
    if (request.url().includes(OTHER)) {
      await held;
      return route.fulfill({ json: { data: [], next_cursor: null } });
    }
    return route.fulfill({ json: { data: [TEMPLATE], next_cursor: null } });
  });
  await page.goto(`${app}/w/acme/templates`);
  await expect(page.getByRole("link", { name: "Welcome", exact: true })).toBeVisible();

  // Client-side, so the query cache survives: the switcher, then the nav link.
  await page.getByRole("combobox", { name: "Workspace", exact: true }).selectOption("other");
  await page.getByRole("link", { name: "Templates", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Templates", exact: true })).toBeVisible();
  await expect(page.getByRole("link", { name: "Welcome", exact: true })).toHaveCount(0);
  release();
  await expect(page.getByText("No templates yet")).toBeVisible();
});
