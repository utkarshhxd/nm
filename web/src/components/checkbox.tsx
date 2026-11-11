// SPDX-License-Identifier: AGPL-3.0-only

import { type ComponentProps, useEffect, useRef } from "react";

type CheckboxProps = Omit<ComponentProps<"input">, "type"> & {
  /** The header tri-state. `indeterminate` is a DOM property with no HTML attribute. */
  indeterminate?: boolean;
};

/**
 * A native checkbox. It needs an accessible name from the caller: a wrapping <label>, or for a
 * table row `aria-label="Select jo@example.com"`, never a bare "Select".
 */
export function Checkbox({ indeterminate = false, className, ...rest }: CheckboxProps) {
  const ref = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (ref.current) ref.current.indeterminate = indeterminate;
  }, [indeterminate]);
  return (
    <input
      ref={ref}
      type="checkbox"
      // 20px plus a 2px margin each side is the 24px target WCAG 2.5.8 asks for.
      className={`m-0.5 size-5 accent-accent${className ? ` ${className}` : ""}`}
      {...rest}
    />
  );
}
