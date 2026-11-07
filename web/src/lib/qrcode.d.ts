// SPDX-License-Identifier: AGPL-3.0-only

// `qrcode` ships no types and `@types/qrcode` is a whole extra dependency for one call. This
// declares only what the TOTP setup panel uses; widen it if a second call site appears.
declare module "qrcode" {
  export function toDataURL(text: string): Promise<string>;
}
