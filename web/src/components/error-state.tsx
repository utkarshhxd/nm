// SPDX-License-Identifier: AGPL-3.0-only

import type { ReactNode } from "react";

import { Button } from "./button";

/**
 * A failed query. Says what failed and what to do: no apology, no "Oops", no stack trace, no raw
 * problem URN. `onRetry` re-runs the query; it is not a page reload.
 */
export function ErrorState({
  title,
  children,
  onRetry,
}: {
  title: string;
  children?: ReactNode;
  onRetry?: () => void;
}) {
  return (
    <div role="alert" className="rounded-md border border-danger bg-surface p-6">
      <h2 className="text-lg font-semibold">{title}</h2>
      {children ? <p className="mt-2 text-fg-muted">{children}</p> : null}
      {onRetry ? (
        <Button className="mt-4" onClick={onRetry}>
          Retry
        </Button>
      ) : null}
    </div>
  );
}
