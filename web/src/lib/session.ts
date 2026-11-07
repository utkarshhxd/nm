// SPDX-License-Identifier: AGPL-3.0-only

import { useQuery } from "@tanstack/react-query";

import { api, setCsrfToken } from "../api/client";
import type { components } from "../api/schema";

export type Me = components["schemas"]["MeResponse"];

/**
 * The signed-in user, and the only place the CSRF token is picked up: every unsafe request the
 * app makes needs one, so a screen that writes anything must render behind this hook.
 *
 * A missing session answers 401 `unauthenticated`, which the API client turns into a redirect
 * to `/login?next=…`, so this never settles as "signed out".
 */
export function useMe() {
  return useQuery({
    queryKey: ["me"],
    queryFn: async (): Promise<Me> => {
      const { data, error } = await api.GET("/api/v1/me");
      if (error || !data) throw new Error("could not load the signed-in user");
      setCsrfToken(data.csrf_token);
      return data;
    },
    retry: false,
  });
}

/**
 * `GET /me` that answers `null` instead of redirecting to the sign-in screen.
 *
 * The invite screen is the one page a signed-out visitor is *meant* to land on — the invitee
 * may have no account at all — but it still has to know whether someone is signed in, to choose
 * between an Accept button and a "Sign in to accept" link. The redirect lives in the API
 * client's middleware, so asking without being bounced means going around that client.
 */
export async function fetchMeOrNull(): Promise<Me | null> {
  const res = await fetch("/api/v1/me", { headers: { accept: "application/json" } });
  if (res.status === 401) return null;
  if (!res.ok) throw new Error("could not load the signed-in user");
  const me = (await res.json()) as Me;
  setCsrfToken(me.csrf_token);
  return me;
}

/** Ends the session. The reload is deliberate: it drops every cached query of the old user. */
export async function signOut(): Promise<void> {
  await api.POST("/api/v1/auth/logout");
  window.location.assign("/login");
}
