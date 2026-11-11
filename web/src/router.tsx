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

// The view lives in the URL, so it can be linked and the back button works. An unknown value is
// dropped rather than rejected: a hand-edited link still opens.
const contactsRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "contacts",
  validateSearch: (search: Record<string, unknown>) => ({
    q: text(search["q"]),
    sort: pick(search["sort"], ["created", "email"]),
    dir: pick(search["dir"], ["asc", "desc"]),
    list_id: text(search["list_id"]),
    list_status: pick(search["list_status"], ["pending", "subscribed", "unsubscribed"]),
    tag_id: text(search["tag_id"]),
  }),
  component: lazyRouteComponent(() => import("./routes/contacts")),
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

export const router = createRouter({
  routeTree: rootRoute.addChildren([
    homeRoute,
    loginRoute,
    accountSecurityRoute,
    inviteRoute,
    newWorkspaceRoute,
    workspaceRoute.addChildren([contactsRoute, membersRoute, apiKeysRoute, auditLogRoute]),
  ]),
});

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
