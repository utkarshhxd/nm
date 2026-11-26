// SPDX-License-Identifier: AGPL-3.0-only

/**
 * True when an Enter keydown is the editor's to act on. Enter also confirms an input method's
 * candidate, and that one belongs to the input method: Chromium flags it `isComposing`, and Safari
 * delivers it after `compositionend` with `isComposing` false and `keyCode` 229. Every place that
 * takes over Enter asks here, so the guard is written once.
 */
export const isOurEnter = (e: { key: string; keyCode: number; nativeEvent: { isComposing: boolean } }) =>
  e.key === "Enter" && !e.nativeEvent.isComposing && e.keyCode !== 229;
