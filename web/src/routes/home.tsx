// SPDX-License-Identifier: AGPL-3.0-only

import { useQuery } from "@tanstack/react-query";

import { api, setCsrfToken } from "../api/client";

/** Placeholder shell; the real screens arrive in Tasks 15 and 16. */
export default function Home() {
  const { data, error, isPending } = useQuery({
    queryKey: ["me"],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/me");
      if (error || !data) {
        throw new Error("could not load the signed-in user");
      }
      setCsrfToken(data.csrf_token);
      return data;
    },
    retry: false,
  });

  return (
    <main className="mx-auto max-w-xl p-8">
      <h1 className="text-2xl font-semibold">NM</h1>
      <p className="mt-4 text-gray-700">
        {isPending ? "Loading…" : error ? "Not signed in." : `Signed in as ${data.email}`}
      </p>
    </main>
  );
}
