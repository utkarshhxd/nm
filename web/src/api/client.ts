// SPDX-License-Identifier: AGPL-3.0-only

import createClient, { type Middleware } from "openapi-fetch";

import type { paths } from "./schema";

/** Methods the backend's CSRF middleware guards. */
const UNSAFE_METHODS = new Set(["POST", "PUT", "PATCH", "DELETE"]);

const LOGIN_PATH = "/login";
const UNAUTHENTICATED = "urn:nm:problem:unauthenticated";

let csrfToken = "";

/** `GET /api/v1/me` returns the token; call this with it after every sign-in. */
export function setCsrfToken(token: string): void {
  csrfToken = token;
}

/**
 * The `detail` of an RFC 9457 problem body, or `fallback` when there is none.
 *
 * The generated types describe every error response as having no content, so `openapi-fetch`
 * types the rejected branch as empty even though the body really is a problem document. These
 * three readers are where that gap is bridged, once, instead of at every call site.
 */
export function problemDetail(error: unknown, fallback: string): string {
  const detail = (error as { detail?: unknown } | null | undefined)?.detail;
  return typeof detail === "string" && detail !== "" ? detail : fallback;
}

/** The first field message of a 422 `validation` problem, else `problemDetail`. */
export function problemMessage(error: unknown, fallback: string): string {
  const errors = (error as { errors?: { message?: unknown }[] } | null | undefined)?.errors;
  const first = errors?.[0]?.message;
  return typeof first === "string" && first !== "" ? first : problemDetail(error, fallback);
}

/** The problem `type` URN, e.g. `urn:nm:problem:mfa-restart`, or `""`. */
export function problemType(error: unknown): string {
  const type = (error as { type?: unknown } | null | undefined)?.type;
  return typeof type === "string" ? type : "";
}

/** True only for the 401 that means "you have no session", never for a rejected credential. */
async function isUnauthenticated(response: Response): Promise<boolean> {
  if (!response.headers.get("content-type")?.includes("application/problem+json")) {
    return false;
  }
  try {
    const problem = (await response.clone().json()) as { type?: unknown };
    return problem.type === UNAUTHENTICATED;
  } catch {
    return false;
  }
}

const session: Middleware = {
  onRequest({ request }) {
    if (csrfToken && UNSAFE_METHODS.has(request.method)) {
      request.headers.set("X-CSRF-Token", csrfToken);
    }
    return request;
  },
  async onResponse({ response }) {
    // Only `unauthenticated` is a missing session. invalid-credentials, invalid-code,
    // mfa-restart, invalid-passkey, login-required and invalid-api-key are answers to
    // something the user just submitted: they go back to the form that caused them, so it
    // can show the error instead of losing what was typed.
    if (
      response.status === 401 &&
      window.location.pathname !== LOGIN_PATH &&
      (await isUnauthenticated(response))
    ) {
      const next = encodeURIComponent(window.location.pathname + window.location.search);
      window.location.assign(`${LOGIN_PATH}?next=${next}`);
    }
    return response;
  },
};

// The generated path keys already carry `/api/v1` (utoipa emits absolute paths), so the base
// URL stays empty — `baseUrl: "/api/v1"` would request `/api/v1/api/v1/me`.
export const api = createClient<paths>({ baseUrl: "" });
api.use(session);
