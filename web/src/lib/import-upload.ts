// SPDX-License-Identifier: AGPL-3.0-only

import type { components } from "../api/schema";

type Upload = components["schemas"]["Upload"];

/** What the mapping screen is given: the server's reading of the file, and the name the browser had for it. */
export type Uploaded = { upload: Upload; filename: string };

/**
 * Where an upload waits for the mapping screen. The mapping is never persisted between upload and
 * start, and the columns the server read are only in the upload response, so it travels in the query
 * cache and a reload of the mapping screen loses it. (The report is the server's, and survives.)
 */
export const uploadKey = (ws: string, id: string) => ["import-upload", ws, id] as const;
