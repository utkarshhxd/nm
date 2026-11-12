// SPDX-License-Identifier: AGPL-3.0-only

import type { ReactNode } from "react";

/**
 * Nothing yet, nothing matches, or nothing permitted: three different sentences, so the caller
 * writes it. One `<h2>`, one sentence, at most two actions, no illustration. Never put
 * persistent information here; it disappears when the first row arrives.
 */
export function EmptyState({
  title,
  children,
  actions,
}: {
  title: string;
  children: ReactNode;
  actions?: ReactNode;
}) {
  return (
    <div className="mx-auto max-w-[42ch] py-16 text-center">
      <h2 className="text-lg font-semibold">{title}</h2>
      <p className="mt-2 text-fg-muted">{children}</p>
      {actions ? <div className="mt-6 flex flex-wrap justify-center gap-3">{actions}</div> : null}
    </div>
  );
}
