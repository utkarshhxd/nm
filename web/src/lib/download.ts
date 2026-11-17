// SPDX-License-Identifier: AGPL-3.0-only

/**
 * Saves a blob as a file. `name` repeats the server's own `content-disposition` filename where
 * there is one: a body the page read itself never reaches the browser's download machinery, so
 * that header is not applied and the name has to be given here.
 */
export function download(blob: Blob, name: string): void {
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = name;
  // Firefox only downloads from a link that is in the document.
  document.body.append(link);
  link.click();
  link.remove();
  // One task later: the browser has taken the blob by then, and never revoking it would hold the
  // whole file in memory until the tab closes.
  window.setTimeout(() => URL.revokeObjectURL(url), 0);
}
