// SPDX-License-Identifier: AGPL-3.0-only

import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  // `manifest` is what scripts/check-bundle.mjs measures the initial JS from.
  build: { manifest: true },
  server: {
    // Run the backend with NM_PUBLIC_URL=http://localhost:5173 so its cookies match this origin.
    proxy: { "/api": "http://127.0.0.1:8080" },
  },
});
