// SPDX-License-Identifier: AGPL-3.0-only

import { useParams } from "@tanstack/react-router";

/** Claims the invite token the API put in `invite_url`. The real screen arrives in Task 16. */
export default function Invite() {
  const { token } = useParams({ from: "/invite/$token" });

  return (
    <main className="mx-auto max-w-xl p-8">
      <h1 className="text-2xl font-semibold">Invitation</h1>
      <p className="mt-4 break-all text-gray-700">Invite token: {token}</p>
    </main>
  );
}
