// SPDX-License-Identifier: AGPL-3.0-only

import { useId, useState } from "react";

import { Button } from "../button";
import { Field } from "../field";
import { isOurEnter } from "./keys";
import { tagSource } from "./runs";

const CLASS = "rounded-sm bg-accent-subtle px-1 font-mono text-sm text-accent-text";

/**
 * A merge tag as the editing surface shows it: one non-editable element, so the browser can move
 * the caret over it and delete it, but never into it. `data-source` is what the document holds;
 * the text is only its display.
 */
export function tagToken(source: string): HTMLElement {
  const el = document.createElement("span");
  el.dataset["tag"] = "";
  el.dataset["source"] = source;
  el.contentEditable = "false";
  el.className = CLASS;
  el.textContent = source;
  return el;
}

export const isTagToken = (n: Node): n is HTMLElement => n instanceof HTMLElement && n.dataset["tag"] !== undefined;

/** The field that puts a merge tag at the caret. Whether the tag parses is the server's to say, at save. */
export function TagInsert({ onInsert }: { onInsert: (source: string) => void }) {
  const id = useId();
  const [raw, setRaw] = useState("");
  const [error, setError] = useState<string>();
  function insert() {
    const source = tagSource(raw);
    if (source === null) return setError("Enter a merge tag such as first_name or first_name | \"there\".");
    setError(undefined);
    setRaw("");
    onInsert(source);
  }
  return (
    <div>
      <Field
        id={id}
        label="Merge tag"
        value={raw}
        error={error}
        hint={'For example first_name | "there"'}
        onChange={(e) => setRaw(e.target.value)}
        onKeyDown={(e) => {
          if (isOurEnter(e)) {
            e.preventDefault();
            insert();
          }
        }}
      />
      <Button className="mt-2" onMouseDown={(e) => e.preventDefault()} onClick={insert}>
        Insert merge tag
      </Button>
    </div>
  );
}
