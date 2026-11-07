// SPDX-License-Identifier: AGPL-3.0-only

import {
  type PublicKeyCredentialCreationOptionsJSON,
  type PublicKeyCredentialRequestOptionsJSON,
  startAuthentication,
  startRegistration,
} from "@simplewebauthn/browser";

import { api, problemDetail } from "../api/client";

/**
 * The API passes the WebAuthn options and the browser's answer through as opaque JSON objects
 * (`type: object` in the spec, so `Record<string, never>` in the generated types).
 * `@simplewebauthn/browser` is what knows their real shape, hence the casts here and nowhere
 * else.
 */
type Opaque = Record<string, never>;

/**
 * One WebAuthn sign-in ceremony. `useBrowserAutofill` is conditional UI: the promise then stays
 * pending until the person picks a passkey from the email field's autofill list, or rejects if
 * the browser cannot do conditional UI at all. Resolving means the session cookie is set.
 */
export async function signInWithPasskey(useBrowserAutofill: boolean): Promise<void> {
  const { data, error } = await api.POST("/api/v1/auth/passkey/login/start");
  if (error || !data) throw new Error(problemDetail(error, "Passkeys are unavailable here"));

  const credential = await startAuthentication({
    optionsJSON: data.options as unknown as PublicKeyCredentialRequestOptionsJSON,
    useBrowserAutofill,
  });

  const finished = await api.POST("/api/v1/auth/passkey/login/finish", {
    body: { challenge_id: data.challenge_id, credential: credential as unknown as Opaque },
  });
  if (finished.error || !finished.data) {
    throw new Error(problemDetail(finished.error, "That passkey could not be verified"));
  }
}

/** Registers a new passkey for the signed-in user under `name`. */
export async function registerPasskey(name: string): Promise<void> {
  const { data, error } = await api.POST("/api/v1/me/passkeys/register/start", { body: { name } });
  if (error || !data) throw new Error(problemDetail(error, "Could not start registration"));

  const credential = await startRegistration({
    optionsJSON: data.options as unknown as PublicKeyCredentialCreationOptionsJSON,
  });

  const finished = await api.POST("/api/v1/me/passkeys/register/finish", {
    body: { challenge_id: data.challenge_id, credential: credential as unknown as Opaque },
  });
  if (finished.error || !finished.data) {
    throw new Error(problemDetail(finished.error, "That passkey could not be registered"));
  }
}
