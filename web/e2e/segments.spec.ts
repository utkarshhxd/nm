// SPDX-License-Identifier: AGPL-3.0-only

import AxeBuilder from "@axe-core/playwright";
import { type BrowserContext, type Page, expect, test } from "@playwright/test";

import { expectNoViolations } from "./support/axe";
import { ADMIN_EMAIL, ADMIN_PASSWORD } from "./support/server";

/**
 * The segments list and the rule builder (spec M1b sections 4.1 to 4.3, 5, 13 and 14).
 *
 * The three things the server punishes a builder for getting wrong each have a test that fails
 * when the behaviour goes, not merely when the screen does: a group that loses its last rule is
 * removed rather than left empty (an empty one is `422 empty-rules`, and under `match: "all"`
 * would mean every contact in the workspace); the count is debounced and asks again only when
 * the document really changed (preview is 30/minute/workspace); and a count renders the three
 * ways section 5 allows, with "estimate" carried as text and never as colour alone.
 *
 * One workspace with an admin and a viewer, seeded once and shared in order.
 */
test.describe.configure({ mode: "serial" });

const SLUG = "segmentsco";
const VIEWER = "viewer-segments@example.com";
const VIEWER_PASSWORD = "viewer-segments-password";

/** The two count routes, which several tests intercept. */
const PREVIEW = "**/segments/preview";
const COUNTS = "**/segments/counts*";
const CUSTOM_FIELDS = "**/custom-fields*";

/** A 500 that is a real problem document, so the client reads it the way it reads the server's. */
const BROKEN = {
  status: 500,
  contentType: "application/problem+json",
  body: JSON.stringify({
    type: "urn:nm:problem:internal",
    title: "Internal Server Error",
    status: 500,
    detail: "internal error",
  }),
};

let admin: BrowserContext;
let viewer: BrowserContext;
let page: Page;
let viewerPage: Page;
let workspaceId = "";
let csrf = "";
const ids = { list: "", tag: "", acme: "" };

const base = () => `/api/v1/workspaces/${workspaceId}`;

async function post(path: string, data?: unknown): Promise<{ id: string }> {
  const res = await admin.request.post(`${base()}${path}`, {
    data,
    headers: { "X-CSRF-Token": csrf },
  });
  expect(res.ok(), `post ${path} -> ${res.status()} ${await res.text()}`).toBe(true);
  return res.status() === 204 ? { id: "" } : ((await res.json()) as { id: string });
}

type ApiSegment = { id: string; name: string; description: string; rules: unknown; version: number };

async function apiSegments(): Promise<ApiSegment[]> {
  const res = await admin.request.get(`${base()}/segments?limit=100`);
  expect(res.ok(), `list segments -> ${res.status()}`).toBe(true);
  return ((await res.json()) as { data: ApiSegment[] }).data;
}

async function apiSegment(name: string): Promise<ApiSegment> {
  const found = (await apiSegments()).find((s) => s.name === name);
  expect(found, `no segment named ${name}`).toBeTruthy();
  return found as ApiSegment;
}

async function signIn(p: Page, email: string, password: string): Promise<void> {
  await p.goto("/login");
  await p.getByLabel("Email", { exact: true }).fill(email);
  await p.getByLabel("Password", { exact: true }).fill(password);
  await p.getByRole("button", { name: "Sign in", exact: true }).click();
  await expect(p.getByText(`Signed in as ${email}`)).toBeVisible();
}

/** The segments screen, loaded and settled. */
async function openSegments(p: Page = page): Promise<void> {
  await p.goto(`/w/${SLUG}/segments`);
  await expect(p.getByRole("heading", { name: "Segments", exact: true })).toBeVisible();
}

/** The new-segment drawer, open and ready. */
async function openBuilder(name: string): Promise<void> {
  await page.getByRole("button", { name: "New segment", exact: true }).click();
  await expect(page.getByRole("heading", { name: "New segment", exact: true })).toBeVisible();
  await page.getByLabel("Name", { exact: true }).fill(name);
}

/**
 * Every test starts with the page's counts stubbed away, and the ones the count is the subject
 * of drop the stub with `realCounts()`.
 *
 * A workspace gets thirty counts and previews a minute between them (`PREVIEW_PER_MINUTE`) and
 * this file opens the screen twenty times inside one, so without this the suite spends the
 * window on tests that never look at a count and the ones that do fail for a reason they are not
 * asserting. It is also the honest shape: a test should not exercise what it is not checking.
 */
test.beforeEach(async () => {
  for (const p of [page, viewerPage]) {
    await p.unroute(COUNTS);
    await p.route(COUNTS, (route) =>
      route.fulfill({ status: 200, contentType: "application/json", body: "[]" }),
    );
  }
});

/** This test asks the server for the real counts, or installs a stub of its own. */
const realCounts = (p: Page = page) => p.unroute(COUNTS);

const rule = (path: string) => page.getByRole("group", { name: `Rule ${path}`, exact: true });
const fieldOf = (path: string) => page.getByLabel(`Field for rule ${path}`, { exact: true });
const opOf = (path: string) => page.getByLabel(`Operator for rule ${path}`, { exact: true });
const valueOf = (path: string) => page.getByLabel(`Value for rule ${path}`, { exact: true });
const count = () => page.getByTestId("preview-count");
/** The open builder, so a count or a refusal on the list behind it cannot answer for it. */
const drawer = () => page.getByRole("dialog");

test.beforeAll(async ({ browser }) => {
  test.setTimeout(180_000);
  admin = await browser.newContext();
  viewer = await browser.newContext();
  page = await admin.newPage();
  viewerPage = await viewer.newPage();

  await signIn(page, ADMIN_EMAIL, ADMIN_PASSWORD);
  await page.goto("/workspaces/new");
  await page.getByLabel("Slug", { exact: true }).fill(SLUG);
  await page.getByLabel("Workspace name", { exact: true }).fill("Segments Co");
  await page.getByLabel("Time zone", { exact: true }).selectOption("Europe/London");
  await page.getByRole("button", { name: "Create workspace", exact: true }).click();
  await expect(page).toHaveURL(new RegExp(`/w/${SLUG}/members$`));

  const me = (await (await admin.request.get("/api/v1/me")).json()) as {
    csrf_token: string;
    workspaces: { id: string; slug: string }[];
  };
  csrf = me.csrf_token;
  workspaceId = me.workspaces.find((w) => w.slug === SLUG)?.id ?? "";
  expect(workspaceId).not.toBe("");

  const invite = (await post("/invites", { email: VIEWER, role: "viewer" })) as unknown as {
    invite_url: string;
  };
  await viewerPage.goto(new URL(invite.invite_url).pathname);
  await viewerPage.getByLabel("Your name", { exact: true }).fill("viewer segments");
  await viewerPage.getByLabel("Password", { exact: true }).fill(VIEWER_PASSWORD);
  await viewerPage.getByRole("button", { name: "Accept", exact: true }).click();
  await expect(viewerPage.getByText(`Signed in as ${VIEWER}`)).toBeVisible();

  await post("/custom-fields", { key: "company", label: "Company", kind: "text" });
  await post("/custom-fields", {
    key: "plan",
    label: "Plan",
    kind: "select",
    options: ["Free", "Pro"],
  });
  await post("/custom-fields", {
    key: "interests",
    label: "Interests",
    kind: "multi_select",
    options: ["Email", "SMS"],
  });
  ids.list = (await post("/lists", { name: "Newsletter", description: "", double_optin: false }))
    .id;
  ids.tag = (await post("/tags", { name: "VIP" })).id;

  ids.acme = (
    await post("/contacts", { email: "one@acme.test", attributes: { company: "Acme" } })
  ).id;
  await post("/contacts", { email: "two@acme.test" });
  await post("/contacts", { email: "three@other.test" });
});

test.afterAll(async () => {
  await admin.close();
  await viewer.close();
});

test("the_empty_state_says_what_a_segment_is_and_offers_to_build_one", async () => {
  await openSegments();
  await expect(page.getByRole("heading", { name: "No segments yet", exact: true })).toBeVisible();
  // One sentence, and it must say what a segment *is* rather than that there are none.
  await expect(page.getByText(/saved set of rules/i)).toBeVisible();
  await expect(page.getByRole("button", { name: "New segment", exact: true })).toBeVisible();
});

test("the_builder_saves_the_document_it_shows", async () => {
  await openSegments();
  await openBuilder("Acme or newsletter");
  await page.getByLabel("Description", { exact: true }).fill("Everyone at Acme, or subscribed.");

  await page.getByLabel("Match", { exact: true }).selectOption("any");
  await fieldOf("1").selectOption("email");
  await opOf("1").selectOption("contains");
  await valueOf("1").fill("@acme.test");

  await page.getByRole("button", { name: "Add group", exact: true }).click();
  await page.getByLabel("Match for group 2", { exact: true }).selectOption("all");
  await fieldOf("2.1").selectOption("list");
  await opOf("2.1").selectOption("member_of");
  await valueOf("2.1").selectOption(ids.list);
  await page.getByLabel("Membership status for rule 2.1", { exact: true }).selectOption("subscribed");

  await page.getByRole("button", { name: "Save segment", exact: true }).click();
  await expect(page.getByRole("heading", { name: "New segment", exact: true })).toBeHidden();
  await expect(page.getByRole("cell", { name: "Acme or newsletter", exact: true })).toBeVisible();

  // The exact document, not merely one that parses: a builder that drops `status`, flips the
  // connective or nests the group one level off still saves something the server accepts.
  const saved = await apiSegment("Acme or newsletter");
  expect(saved.rules).toEqual({
    match: "any",
    rules: [
      { field: "email", op: "contains", value: "@acme.test" },
      {
        group: {
          match: "all",
          rules: [{ field: "list", op: "member_of", value: ids.list, status: "subscribed" }],
        },
      },
    ],
  });
  expect(saved.description).toBe("Everyone at Acme, or subscribed.");
});

test("deleting_the_last_rule_in_a_group_removes_the_group", async () => {
  await openSegments();
  await openBuilder("No empty group");
  await fieldOf("1").selectOption("email");
  await opOf("1").selectOption("contains");
  await valueOf("1").fill("@acme.test");

  await page.getByRole("button", { name: "Add group", exact: true }).click();
  await expect(page.getByRole("group", { name: "Group 2", exact: true })).toBeVisible();
  await expect(rule("2.1")).toBeVisible();

  // The last rule in the group goes, so the group goes with it. An empty one is
  // `422 empty-rules`, and under `match: "all"` it would compile to every contact.
  await page.getByRole("button", { name: "Remove rule 2.1", exact: true }).click();
  await expect(page.getByRole("group", { name: "Group 2", exact: true })).toHaveCount(0);
  await expect(rule("2.1")).toHaveCount(0);

  await page.getByRole("button", { name: "Save segment", exact: true }).click();
  await expect(page.getByRole("heading", { name: "New segment", exact: true })).toBeHidden();
  expect((await apiSegment("No empty group")).rules).toEqual({
    match: "all",
    rules: [{ field: "email", op: "contains", value: "@acme.test" }],
  });
});

test("a_group_nested_in_a_group_is_removed_when_its_last_rule_goes", async () => {
  await openSegments();
  await openBuilder("Nested");
  await fieldOf("1").selectOption("email");
  await opOf("1").selectOption("contains");
  await valueOf("1").fill("@acme.test");

  await page.getByRole("button", { name: "Add group", exact: true }).click();
  await page.getByRole("button", { name: "Add group to group 2", exact: true }).click();
  await expect(page.getByRole("group", { name: "Group 2.2", exact: true })).toBeVisible();
  await expect(rule("2.2.1")).toBeVisible();

  // Two levels down, so the removal has to recurse rather than reach the top group directly.
  await page.getByRole("button", { name: "Remove rule 2.2.1", exact: true }).click();
  await expect(page.getByRole("group", { name: "Group 2.2", exact: true })).toHaveCount(0);
  await expect(page.getByRole("group", { name: "Group 2", exact: true })).toBeVisible();
  await expect(rule("2.1")).toBeVisible();

  // And again: group 2 is down to one rule, so removing that takes group 2 with it.
  await page.getByRole("button", { name: "Remove rule 2.1", exact: true }).click();
  await expect(page.getByRole("group", { name: "Group 2", exact: true })).toHaveCount(0);

  await page.getByRole("button", { name: "Save segment", exact: true }).click();
  await expect(page.getByRole("heading", { name: "New segment", exact: true })).toBeHidden();
  expect((await apiSegment("Nested")).rules).toEqual({
    match: "all",
    rules: [{ field: "email", op: "contains", value: "@acme.test" }],
  });
});

test("focus_lands_somewhere_deliberate_after_a_rule_is_removed", async () => {
  await openSegments();
  await openBuilder("Focus");
  await page.getByRole("button", { name: "Add group", exact: true }).click();
  await page.getByRole("button", { name: "Add rule to group 2", exact: true }).click();
  await expect(page.getByRole("group", { name: "Rule 2.2", exact: true })).toBeVisible();

  // The group survives this one, so its own "Add rule" takes the focus. Rows are keyed by
  // position, so without it focus is left on a Remove button that now belongs to another rule.
  await page.getByRole("button", { name: "Remove rule 2.1", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Add rule to group 2", exact: true }),
  ).toBeFocused();

  // This one takes the group with it, so the group that survives the cascade takes the focus.
  await page.getByRole("button", { name: "Remove rule 2.1", exact: true }).click();
  await expect(page.getByRole("group", { name: "Group 2", exact: true })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Add rule", exact: true })).toBeFocused();
  await page.getByRole("button", { name: "Cancel", exact: true }).click();
});

test("the_count_is_debounced_and_asks_again_only_when_the_document_changed", async () => {
  await openSegments();
  await openBuilder("Debounced");
  await fieldOf("1").selectOption("email");
  await opOf("1").selectOption("contains");

  const sent: number[] = [];
  page.on("request", (r) => {
    if (r.url().includes("/segments/preview")) sent.push(Date.now());
  });

  // Eight keystrokes inside half a second: one count, not eight. Preview is 30/minute/workspace.
  // The value is one no saved segment uses, so the count cannot come from a row's cached answer.
  await valueOf("1").pressSequentially("one@acme", { delay: 40 });
  const lastKey = Date.now();
  await expect(count()).toHaveText("1");
  expect(sent.length, `previews for one burst: ${sent.length}`).toBe(1);
  // Measured from the *last* keystroke, so an implementation that asks on the first one fails
  // here rather than passing on the time the typing itself took.
  expect((sent[0] ?? 0) - lastKey, "the count did not wait for the typing to stop").toBeGreaterThanOrEqual(400);

  // A character typed and taken back leaves the document as it was, so nothing is asked again.
  await valueOf("1").press("End");
  await valueOf("1").pressSequentially("x");
  await valueOf("1").press("Backspace");
  await page.waitForTimeout(1500);
  await expect(count()).toHaveText("1");
  expect(sent.length, `previews after a no-op edit: ${sent.length}`).toBe(1);
});

test("a_count_from_the_previous_document_is_never_shown_as_this_ones", async () => {
  await openSegments();
  await openBuilder("Stale");
  await fieldOf("1").selectOption("email");
  await opOf("1").selectOption("contains");
  await valueOf("1").fill("@acme.test");
  await expect(count()).toHaveText("2");

  // Hold the next count open. "2" is the answer to a question nobody is asking any more, and a
  // number presented as this document's answer when it is the last one's is the defect.
  await page.route(PREVIEW, async (route) => {
    await new Promise((done) => setTimeout(done, 2500));
    await route.continue().catch(() => undefined);
  });
  await valueOf("1").fill("one@acme");
  // Read *inside* the debounce, not after it. `toHaveCount(0)` on its own polls for five
  // seconds and the stale number clears itself the moment the debounce fires, so a poll passes
  // over the very window the defect lives in.
  await page.waitForTimeout(150);
  expect(await count().count(), "the previous document's number is still on screen").toBe(0);
  await expect(drawer().getByText("Counting contacts")).toBeAttached();
  await page.unroute(PREVIEW);
});

test("an_attribute_rule_says_too_many_to_count_exactly", async () => {
  await openSegments();
  await openBuilder("Attribute");
  await fieldOf("1").selectOption("attr:company");
  await opOf("1").selectOption("eq");
  await valueOf("1").fill("Acme");
  // Section 5: no statistics for `->>`, and the planner's figure measured 66x low.
  await expect(count()).toHaveText("too many to count exactly");
  await expect(count()).not.toHaveText(/~?\d/);
});

test("an_estimate_reads_as_a_tilde_number_with_the_word_estimate", async () => {
  // A real estimate needs a table big enough to time the count out at 1500 ms. The shape the
  // server sends for one is what this asserts.
  await page.route(PREVIEW, (route) =>
    route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ count: 12400, exact: false, took_ms: 1500 }),
    }),
  );
  await openSegments();
  await openBuilder("Estimated");
  await fieldOf("1").selectOption("email");
  await opOf("1").selectOption("contains");
  await valueOf("1").fill("@acme.test");
  await expect(count()).toHaveText("~12,400");
  // As text, never colour alone (WCAG 1.4.1).
  await expect(drawer().getByText("estimate", { exact: true })).toBeVisible();
  await page.unroute(PREVIEW);
});

test("a_rate_limited_count_says_to_slow_down", async () => {
  await page.route(PREVIEW, (route) =>
    route.fulfill({
      status: 429,
      contentType: "application/problem+json",
      headers: { "retry-after": "30" },
      body: JSON.stringify({
        type: "urn:nm:problem:rate-limited",
        title: "Too Many Requests",
        status: 429,
        detail: "rate limited",
      }),
    }),
  );
  await openSegments();
  await openBuilder("Limited");
  await fieldOf("1").selectOption("email");
  await opOf("1").selectOption("contains");
  await valueOf("1").fill("@acme.test");
  await expect(drawer().getByText(/slow down/i)).toBeVisible();
  // The stub sends `detail: "rate limited"`, which is what `problemText` falls through to when
  // this screen has no wording of its own. Asserting the server's terse code is *not* what the
  // person reads is an assertion the code under test can actually fail.
  await expect(drawer().getByText(/rate limited/)).toHaveCount(0);
  await page.unroute(PREVIEW);
});

test("the_servers_refusals_are_shown_rather_than_swallowed", async () => {
  await openSegments();
  await openBuilder("Refused");
  await fieldOf("1").selectOption("email");
  await opOf("1").selectOption("contains");
  await valueOf("1").fill("@acme.test");
  await expect(count()).toHaveText("2");

  // Documents the builder cannot produce, answered by the real server: the reserved engagement
  // fields and a field that is not on the whitelist.
  for (const [field, wording] of [
    ["opened", /milestone M2|send tracking/i],
    ["nope", /unknown field/i],
  ] as const) {
    await page.route(PREVIEW, (route) =>
      route.continue({
        postData: JSON.stringify({ rules: { match: "all", rules: [{ field, op: "is_true" }] } }),
      }),
    );
    await valueOf("1").fill(`@acme.test/${field}`);
    await expect(drawer().getByText(wording)).toBeVisible();
    await page.unroute(PREVIEW);
  }

  // A value the builder *can* produce and the server refuses: more than 36,500 days.
  await fieldOf("1").selectOption("created_at");
  await opOf("1").selectOption("in_last_days");
  await valueOf("1").fill("99999");
  await expect(drawer().getByText(/at most 36500/i)).toBeVisible();
});

test("a_duplicate_name_is_refused_in_the_builders_own_words", async () => {
  await openSegments();
  await openBuilder("Acme or newsletter");
  await fieldOf("1").selectOption("email");
  await opOf("1").selectOption("contains");
  await valueOf("1").fill("@acme.test");
  await page.getByRole("button", { name: "Save segment", exact: true }).click();
  await expect(page.getByText(/already exists/i)).toBeVisible();
  await expect(page.getByRole("heading", { name: "New segment", exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Cancel", exact: true }).click();
});

test("a_segment_deleted_underneath_the_editor_is_a_404_it_names", async () => {
  const doomed = await post("/segments", {
    name: "Doomed",
    description: "",
    rules: { match: "all", rules: [{ field: "email", op: "contains", value: "@acme.test" }] },
  });
  await openSegments();
  await page.getByRole("button", { name: "Edit Doomed", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Edit segment", exact: true })).toBeVisible();

  const gone = await admin.request.delete(`${base()}/segments/${doomed.id}`, {
    headers: { "X-CSRF-Token": csrf },
  });
  expect(gone.status()).toBe(204);

  await page.getByLabel("Name", { exact: true }).fill("Doomed again");
  await page.getByRole("button", { name: "Save segment", exact: true }).click();
  await expect(page.getByText(/no longer exists/i)).toBeVisible();
  await page.getByRole("button", { name: "Cancel", exact: true }).click();
});

test("the_list_shows_its_rows_before_the_counts_arrive", async () => {
  await realCounts();
  // The counts held for two seconds: the rows must be on screen well before that.
  await page.route(COUNTS, async (route) => {
    await new Promise((done) => setTimeout(done, 2000));
    // The page has moved on by the time this wakes, so the route may already be gone.
    await route.continue().catch(() => undefined);
  });
  const opened = Date.now();
  await openSegments();
  await expect(page.getByRole("cell", { name: "Acme or newsletter", exact: true })).toBeVisible({
    timeout: 1500,
  });
  expect(Date.now() - opened, "the list waited for the counts").toBeLessThan(2000);
  // Still counting while the rows are already readable.
  await expect(page.getByText("Counting contacts").first()).toBeAttached();
  await page.unroute(COUNTS);

  // And the count lands: "No empty group" matches the two @acme.test contacts.
  await realCounts();
  await openSegments();
  const row = page.getByRole("row", { name: /No empty group/ });
  await expect(row.getByTestId("segment-count")).toHaveText("2");
});

test("the_page_is_counted_in_one_request_and_an_archived_field_still_counts", async () => {
  const legacy = await post("/custom-fields", { key: "legacy", label: "Legacy", kind: "text" });
  await post("/segments", {
    name: "On a field since put away",
    description: "",
    rules: { match: "all", rules: [{ field: "attr:legacy", op: "eq", value: "x" }] },
  });
  const archived = await admin.request.delete(`${base()}/custom-fields/${legacy.id}`, {
    headers: { "X-CSRF-Token": csrf },
  });
  expect(archived.ok(), `archive -> ${archived.status()}`).toBe(true);

  const asked: string[] = [];
  page.on("request", (r) => {
    if (r.url().includes("/segments/counts")) asked.push("counts");
    if (r.url().includes("/segments/preview")) asked.push("preview");
  });
  await realCounts();
  await openSegments();

  // A stored document resolves archived definitions (section 4.2), so this counts rather than
  // being refused as a new rule naming a field that no longer takes data. An `attr:` rule gets
  // no number for section 5's reason, which is a count, not a failure.
  await expect(
    page.getByRole("row", { name: /On a field since put away/ }).getByTestId("segment-count"),
  ).toHaveText("too many to count exactly");
  await expect(page.getByText(/unknown field/i)).toHaveCount(0);

  // One request for the whole page, however many rows it has.
  expect((await apiSegments()).length, "fewer than two rows proves nothing").toBeGreaterThan(1);
  expect(asked, `count requests for one page: ${asked.join(", ")}`).toEqual(["counts"]);
});

test("a_rule_the_server_accepts_does_not_strand_the_editor", async () => {
  // `eq ""` is a document `check_value` accepts: "the value is empty", not "nothing chosen yet".
  const stored = { match: "all", rules: [{ field: "first_name", op: "eq", value: "" }] };
  await post("/segments", { name: "Empty value", description: "", rules: stored });
  await realCounts();
  await openSegments();
  const row = page.getByRole("row", { name: /Empty value/ });
  // Every contact here was created without a first name, and the column is `not null default ''`.
  await expect(row.getByTestId("segment-count")).toHaveText("3");

  await page.getByRole("button", { name: "Edit Empty value", exact: true }).click();
  await page.getByRole("button", { name: "Save segment", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Edit segment", exact: true })).toBeHidden();
  expect((await apiSegment("Empty value")).rules).toEqual(stored);
});

test("a_catalog_that_failed_to_load_is_never_read_as_archived", async () => {
  await post("/segments", {
    name: "On a live field",
    description: "",
    rules: { match: "all", rules: [{ field: "attr:company", op: "eq", value: "Acme" }] },
  });
  // Installed before the page loads, because the pickers are fetched when the screen mounts.
  await page.route(CUSTOM_FIELDS, (route) => route.fulfill(BROKEN));
  await openSegments();
  await page.getByRole("button", { name: "Edit On a live field", exact: true }).click();

  // `company` is live. Saying it was archived — and offering removal as the only way on — is a
  // false statement with a destructive fix attached.
  await expect(drawer().getByText(/could not be loaded/i)).toBeVisible();
  await expect(drawer().getByText(/archived/i)).toHaveCount(0);
  await expect(drawer().getByRole("button", { name: "Remove rule 1", exact: true })).toHaveCount(0);

  await page.unroute(CUSTOM_FIELDS);
  await drawer().getByRole("button", { name: "Retry", exact: true }).click();
  await expect(fieldOf("1")).toHaveValue("attr:company");
  await page.getByRole("button", { name: "Cancel", exact: true }).click();
});

test("a_saved_edit_changes_the_row_count_without_a_reload", async () => {
  await post("/segments", {
    name: "Two then one",
    description: "",
    rules: { match: "all", rules: [{ field: "email", op: "contains", value: "@acme.test" }] },
  });
  await realCounts();
  await openSegments();
  const row = page.getByRole("row", { name: /Two then one/ });
  await expect(row.getByTestId("segment-count")).toHaveText("2");

  await page.getByRole("button", { name: "Edit Two then one", exact: true }).click();
  await valueOf("1").fill("one@acme");
  // The drawer works out the new answer for itself, so the row has no excuse.
  await expect(count()).toHaveText("1");
  await page.getByRole("button", { name: "Save segment", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Edit segment", exact: true })).toBeHidden();

  // The counts are keyed by the ids on the page, which an edit does not change. Without the
  // invalidation the row keeps saying 2 until a reload, with nothing to say it is stale.
  await expect(row.getByTestId("segment-count")).toHaveText("1");
});

test("each_row_says_which_kind_of_answer_it_has", async () => {
  await realCounts();
  const all = await apiSegments();
  expect(all.length, "four rows are needed to show four states").toBeGreaterThan(3);
  const states = ["exact", "estimated", "no_estimate", "unreadable"] as const;
  const counts = [7, 12400, null, null];
  await page.route(COUNTS, (route) =>
    route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(
        all.map((s, i) => ({ id: s.id, state: states[i % 4], count: counts[i % 4] })),
      ),
    }),
  );
  await openSegments();
  // One sentence per state. "too many to count exactly" is section 5's wording for a document
  // that cannot be estimated honestly; a row that merely ran out of time must not borrow it.
  await expect(page.getByTestId("segment-count").first()).toHaveText("7");
  await expect(page.getByText("~12,400").first()).toBeVisible();
  await expect(page.getByText("estimate", { exact: true }).first()).toBeVisible();
  await expect(page.getByText("too many to count exactly").first()).toBeVisible();
  await expect(page.getByText("needs re-saving").first()).toBeVisible();
  await page.unroute(COUNTS);
});

test("a_failed_batch_is_announced_once_and_the_rows_stay_quiet", async () => {
  await realCounts();
  await page.route(COUNTS, (route) => route.fulfill(BROKEN));
  await openSegments();
  // One failure, one announcement: a row each would read the same event twenty-five times, and
  // twenty-five Retry buttons would all have the same name.
  await expect(page.getByRole("alert")).toHaveCount(1);
  await expect(
    page.getByRole("button", { name: "Count the segments again", exact: true }),
  ).toHaveCount(1);
  await expect(page.getByTestId("segment-count").first()).toHaveText("—");
  await page.unroute(COUNTS);
});

test("the_drawer_can_be_left_while_the_pickers_are_still_loading", async () => {
  await page.route(CUSTOM_FIELDS, async (route) => {
    await new Promise((done) => setTimeout(done, 3000));
    await route.continue().catch(() => undefined);
  });
  await openSegments();
  await page.getByRole("button", { name: "Edit On a live field", exact: true }).click();

  // The name, the description and the way out are there from the first frame; only the rules
  // wait. A drawer that is nothing but a heading and a skeleton has no way out for a mouse, and
  // leaves focus on the heading when the form finally arrives.
  await expect(page.getByLabel("Name", { exact: true })).toBeFocused();
  await expect(drawer().getByRole("button", { name: "Cancel", exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Cancel", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Edit segment", exact: true })).toBeHidden();
  await page.unroute(CUSTOM_FIELDS);
});

test("a_viewer_reads_segments_and_is_offered_no_way_to_change_one", async () => {
  await openSegments(viewerPage);
  await expect(viewerPage.getByRole("cell", { name: "Acme or newsletter", exact: true })).toBeVisible();
  // Absent, not disabled.
  await expect(viewerPage.getByRole("button", { name: "New segment", exact: true })).toHaveCount(0);
  await expect(viewerPage.getByRole("button", { name: /^Edit / })).toHaveCount(0);
  await expect(viewerPage.getByRole("button", { name: /^Delete / })).toHaveCount(0);
});

test("the_segments_screen_and_its_builder_are_axe_clean", async () => {
  await realCounts();
  await openSegments();
  await expectNoViolations(new AxeBuilder({ page }), "segments list");

  await openBuilder("Axe");
  await page.getByRole("button", { name: "Add group", exact: true }).click();
  await fieldOf("2.1").selectOption("attr:interests");
  await opOf("2.1").selectOption("has_any");
  await expectNoViolations(new AxeBuilder({ page }), "rule builder");
  await page.getByRole("button", { name: "Cancel", exact: true }).click();
});
