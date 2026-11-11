// SPDX-License-Identifier: AGPL-3.0-only

import type { ComponentProps } from "react";

import { CONTROL } from "./input";

/** The native <select>: the only control that gets the OS picker on a phone and a screen reader's support for free. */
export function Select({ className, ...rest }: ComponentProps<"select">) {
  return <select className={`${CONTROL}${className ? ` ${className}` : ""}`} {...rest} />;
}
