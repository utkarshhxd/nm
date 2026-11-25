// SPDX-License-Identifier: AGPL-3.0-only

import {
  Outlet,
  createRootRoute,
  createRoute,
  createRouter,
  lazyRouteComponent,
} from "@tanstack/react-router";

// Every route component is a lazy import: only the shell counts towards the 150 KB budget.
const rootRoute = createRootRoute({ component: () => <Outlet /> });

const homeRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  component: lazyRouteComponent(() => import("./routes/home")),
});

const loginRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/login",
  component: lazyRouteComponent(() => import("./routes/login")),
});

const accountSecurityRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/account/security",
  component: lazyRouteComponent(() => import("./routes/account-security")),
});

// `invite_url` in the API's create-invite response points here.
const inviteRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/invite/$token",
  component: lazyRouteComponent(() => import("./routes/invite")),
});

// The named export, so the create-workspace form stays in home's chunk rather than a second one.
const newWorkspaceRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/workspaces/new",
  component: lazyRouteComponent(() => import("./routes/home"), "NewWorkspace"),
});

// The workspace shell: the switcher, the navigation, and "Workspace not found" for a slug the
// signed-in person is not a member of. Its children render inside its `Outlet`.
const workspaceRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/w/$slug",
  component: lazyRouteComponent(() => import("./routes/workspace-layout")),
});

const text = (value: unknown) => (typeof value === "string" && value !== "" ? value : undefined);

const pick = <T extends string>(value: unknown, allowed: readonly T[]) =>
  allowed.find((a) => a === value);

type ContactsSearch = {
  q?: string | undefined;
  sort?: "created" | "email" | undefined;
  dir?: "asc" | "desc" | undefined;
  list_id?: string | undefined;
  list_status?: "pending" | "subscribed" | "unsubscribed" | undefined;
  tag_id?: string | undefined;
};

// The view lives in the URL, so it can be linked and the back button works. An unknown value is
// dropped rather than rejected: a hand-edited link still opens.
const contactsRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "contacts",
  // Optional keys, so a plain link to the contacts page need not spell out an empty search.
  validateSearch: (search: Record<string, unknown>): ContactsSearch => ({
    q: text(search["q"]),
    sort: pick(search["sort"], ["created", "email"]),
    dir: pick(search["dir"], ["asc", "desc"]),
    list_id: text(search["list_id"]),
    list_status: pick(search["list_status"], ["pending", "subscribed", "unsubscribed"]),
    tag_id: text(search["tag_id"]),
  }),
  component: lazyRouteComponent(() => import("./routes/contacts")),
});

const contactRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "contacts/$contactId",
  component: lazyRouteComponent(() => import("./routes/contact-detail")),
});

const listsRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "lists",
  component: lazyRouteComponent(() => import("./routes/lists")),
});

// The named export, so the one lists chunk serves both screens.
const listRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "lists/$listId",
  component: lazyRouteComponent(() => import("./routes/lists"), "ListDetail"),
});

const formsRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "forms",
  component: lazyRouteComponent(() => import("./routes/forms")),
});

const formRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "forms/$formId",
  component: lazyRouteComponent(() => import("./routes/form-detail")),
});

const segmentsRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "segments",
  component: lazyRouteComponent(() => import("./routes/segments")),
});

const tagsRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "tags",
  component: lazyRouteComponent(() => import("./routes/tags")),
});

const customFieldsRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "custom-fields",
  component: lazyRouteComponent(() => import("./routes/custom-fields")),
});

const suppressionsRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "suppressions",
  component: lazyRouteComponent(() => import("./routes/suppressions")),
});

// Outside the workspace tree on purpose: an instance-wide suppression belongs to no workspace,
// and it is gated on `is_instance_admin`, which a workspace admin is not. The named export keeps
// both suppression screens in the one chunk.
const instanceSuppressionsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/admin/suppressions",
  component: lazyRouteComponent(() => import("./routes/suppressions"), "InstanceSuppressions"),
});

const relaysRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "relays",
  component: lazyRouteComponent(() => import("./routes/relays")),
});

const sendingDomainsRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "sending-domains",
  component: lazyRouteComponent(() => import("./routes/sending-domains")),
});

const templatesRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "templates",
  component: lazyRouteComponent(() => import("./routes/templates")),
});

const templateRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "templates/$templateId",
  component: lazyRouteComponent(() => import("./routes/template-detail")),
});

const brandKitRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "brand-kit",
  component: lazyRouteComponent(() => import("./routes/brand-kit")),
});

const membersRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "members",
  component: lazyRouteComponent(() => import("./routes/members")),
});

const apiKeysRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "api-keys",
  component: lazyRouteComponent(() => import("./routes/api-keys")),
});

const auditLogRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "audit-log",
  component: lazyRouteComponent(() => import("./routes/audit-log")),
});

const importsRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "imports",
  component: lazyRouteComponent(() => import("./routes/imports")),
});

const importRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "imports/$importId",
  component: lazyRouteComponent(() => import("./routes/import-detail")),
});

export const router = createRouter({
  routeTree: rootRoute.addChildren([
    homeRoute,
    loginRoute,
    accountSecurityRoute,
    inviteRoute,
    newWorkspaceRoute,
    instanceSuppressionsRoute,
    workspaceRoute.addChildren([
      contactsRoute,
      contactRoute,
      listsRoute,
      listRoute,
      formsRoute,
      formRoute,
      segmentsRoute,
      tagsRoute,
      customFieldsRoute,
      suppressionsRoute,
      relaysRoute,
      sendingDomainsRoute,
      templatesRoute,
      templateRoute,
      brandKitRoute,
      membersRoute,
      apiKeysRoute,
      auditLogRoute,
      importsRoute,
      importRoute,
    ]),
  ]),
});

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
