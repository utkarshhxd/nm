// SPDX-License-Identifier: AGPL-3.0-only

// Pure edits on runs: formatting, links, tags, normalisation. No browser page.

import { expect, test } from "@playwright/test";

import type { Mark, Run } from "./model";
import { breakAt, edit, hasTag, locate, hrefAllowed, hrefAt, marked, normalize, splitAt, tagSource, toggleMark, setLink } from "./runs";

const t = (text: string, ...marks: Mark[]): Run => ({ type: "Text", text, marks });
const tag: Run = { type: "Tag", source: '{{ first_name | "there" }}' };
const shape = (runs: Run[]) => runs.map((r) => (r.type === "Tag" ? "{tag}" : `${r.type === "Link" ? "link:" : ""}${r.text}[${r.marks.join("+")}]`));

test("bold_and_italic_sit_on_one_run", () => {
  let runs = toggleMark([t("hello")], 0, 5, "bold");
  runs = toggleMark(runs, 0, 5, "italic");
  expect(runs).toEqual([t("hello", "bold", "italic")]);
  expect(shape(toggleMark(runs, 0, 5, "bold"))).toEqual(["hello[italic]"]);
});

test("a_mark_applies_to_the_selection_only_and_a_partly_marked_selection_is_marked", () => {
  const runs = [t("ab"), t("cd", "bold")];
  expect(shape(toggleMark(runs, 1, 3, "bold"))).toEqual(["a[]", "bcd[bold]"]);
  expect(marked(runs, 1, 3, "bold")).toBe(false);
  expect(marked(runs, 2, 4, "bold")).toBe(true);
  // Removing it from a fully marked range takes it off that range alone.
  expect(shape(toggleMark(runs, 2, 3, "bold"))).toEqual(["abc[]", "d[bold]"]);
});

test("toggling_a_mark_never_edits_the_marks_array_it_was_given", () => {
  const shared: Mark[] = ["bold"];
  const a: Run = { type: "Text", text: "ab", marks: shared };
  const b: Run = { type: "Text", text: "cd", marks: shared };
  toggleMark([a, b], 0, 2, "italic");
  toggleMark([a, b], 0, 4, "bold");
  expect(shared).toEqual(["bold"]);
  expect(a.marks).toEqual(["bold"]);
  expect(b.marks).toEqual(["bold"]);
});

test("a_tag_inside_a_range_is_left_alone_and_counts_as_one", () => {
  const runs = [t("Hi "), tag, t("!")];
  expect(shape(toggleMark(runs, 0, 5, "italic"))).toEqual(["Hi [italic]", "{tag}", "![italic]"]);
  expect(splitAt(runs, 4)[0]).toHaveLength(2);
  expect(shape(edit(runs, 3, 4))).toEqual(["Hi !"].map((s) => `${s}[]`));
  expect(hasTag(runs, 3, 4)).toBe(true);
  expect(hasTag(runs, 0, 3)).toBe(false);
  expect(hasTag(runs, 4, 5)).toBe(false);
});

test("inserting_a_tag_replaces_the_selection_and_never_cuts_the_tag", () => {
  const out = edit([t("Hello world")], 5, 6, [tag]);
  expect(shape(out)).toEqual(["Hello[]", "{tag}", "world[]"]);
  // Anything typed after it is a separate run, so the tag's source is never touched.
  expect(shape(edit(out, 6, 6, [t("!")]))).toEqual(["Hello[]", "{tag}", "!world[]"]);
});

test("links_apply_to_a_range_keep_their_marks_and_come_off_again", () => {
  const linked = setLink([t("go here now", "bold")], 3, 7, "https://a.test");
  expect(shape(linked)).toEqual(["go [bold]", "link:here[bold]", " now[bold]"]);
  expect(hrefAt(linked, 0, 11)).toBe("https://a.test");
  expect(hrefAt(linked, 0, 2)).toBeNull();
  expect(shape(setLink(linked, 0, 11, null))).toEqual(["go here now[bold]"]);
});

test("normalize_has_one_spelling_for_the_same_text", () => {
  expect(shape(normalize([t("a"), t(""), t("b"), t("c", "bold")]))).toEqual(["ab[]", "c[bold]"]);
  expect(normalize([{ type: "Text", text: "x", marks: ["italic", "bold"] }])).toEqual([t("x", "bold", "italic")]);
  expect(normalize([])).toEqual([t("")]);
  expect(normalize([t("")])).toEqual([t("")]);
  expect(edit([t("ab")], 0, 2)).toEqual([t("")]);
});

test("a_cut_never_lands_between_the_halves_of_an_emoji", () => {
  const [head, tail] = splitAt([t("a\u{1F600}b")], 2);
  expect(shape(head)).toEqual(["a\u{1F600}[]"]);
  expect(shape(tail)).toEqual(["b[]"]);
});

test("a_merge_tag_field_accepts_the_name_with_or_without_braces", () => {
  expect(tagSource('first_name | "there"')).toBe('{{ first_name | "there" }}');
  expect(tagSource('{{first_name}}')).toBe("{{ first_name }}");
  expect(tagSource("  ")).toBeNull();
  expect(tagSource("{{ }}")).toBeNull();
  expect(tagSource("a }} b")).toBeNull();
});

test("only_http_https_mailto_and_tel_are_links", () => {
  for (const ok of ["https://a.test/x", "http://a.test", "mailto:a@b.test", "tel:+15550100"]) expect(hrefAllowed(ok)).toBe(true);
  for (const no of ["javascript:alert(1)", "data:text/html,x", "ftp://a.test", "//a.test", "a.test", ""]) expect(hrefAllowed(no)).toBe(false);
});

test("locate_names_a_run_and_a_code_point_offset_and_a_tag_has_two_edges", () => {
  const runs = [t("Hi "), tag, t("a\u{1F600}b")];
  expect(locate(runs, 2)).toEqual({ run: 0, offset: 2 });
  expect(locate(runs, 3)).toEqual({ run: 0, offset: 3 });
  expect(locate(runs, 4)).toEqual({ run: 1, offset: 1 });
  expect(locate(runs, 7)).toEqual({ run: 2, offset: 2 }); // after the emoji: 2 code points, 3 code units
  expect(locate([t("")], 0)).toEqual({ run: 0, offset: 0 });
});

test("a_line_break_goes_inside_the_run_it_is_in_and_replaces_the_selection", () => {
  expect(breakAt([t("ab", "bold"), t("cd")], 1, 1)).toEqual([t("a\nb", "bold"), t("cd")]);
  // At a join the earlier run takes it, as typing there would; at the very start the first one does.
  expect(breakAt([t("ab", "bold"), t("cd")], 2, 2)).toEqual([t("ab\n", "bold"), t("cd")]);
  expect(breakAt([t("ab", "bold")], 0, 0)).toEqual([t("\nab", "bold")]);
  expect(breakAt([t("abcd")], 1, 3)).toEqual([t("a\nd")]);
  // After a tag there is no text to put it in, so it makes its own run; before text it joins that.
  expect(breakAt([tag], 1, 1)).toEqual([tag, t("\n")]);
  expect(breakAt([tag, t("x", "italic")], 1, 1)).toEqual([tag, t("\nx", "italic")]);
  expect(breakAt([t("")], 0, 0)).toEqual([t("\n")]);
});
