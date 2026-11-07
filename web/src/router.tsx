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

export const router = createRouter({
  routeTree: rootRoute.addChildren([homeRoute, loginRoute, accountSecurityRoute, inviteRoute]),
});

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
