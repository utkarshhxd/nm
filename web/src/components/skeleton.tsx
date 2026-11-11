// SPDX-License-Identifier: AGPL-3.0-only

/**
 * Placeholder rows for a first load. Each is exactly `--row-h` tall, so nothing shifts when the
 * data arrives. The label is the one polite announcement; the bars are decoration.
 */
export function Skeleton({ label, rows = 1 }: { label: string; rows?: number }) {
  return (
    <div role="status" aria-busy="true">
      <span className="sr-only">{label}</span>
      {Array.from({ length: rows }, (_, row) => (
        <div
          key={row}
          aria-hidden="true"
          className="h-(--row-h) border-b border-border bg-sunken motion-safe:animate-pulse"
        />
      ))}
    </div>
  );
}
