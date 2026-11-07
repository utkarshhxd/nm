// SPDX-License-Identifier: AGPL-3.0-only

import { Link } from "@tanstack/react-router";

import { signOut, useMe } from "../lib/session";
import { SECONDARY_BUTTON } from "../lib/ui";

/** Placeholder shell; the real workspace screens arrive in Task 16. */
export default function Home() {
  const { data, isPending } = useMe();

  return (
    <main className="mx-auto max-w-xl p-8">
      <h1 className="text-2xl font-semibold">NM</h1>
      <p className="mt-4">{isPending || !data ? "Loading…" : `Signed in as ${data.email}`}</p>
      <p className="mt-4">
        <Link className="font-medium text-blue-800 underline dark:text-blue-200" to="/account/security">
          Account security
        </Link>
      </p>
      <button className={SECONDARY_BUTTON} type="button" onClick={() => void signOut()}>
        Sign out
      </button>
    </main>
  );
}
