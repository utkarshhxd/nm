// SPDX-License-Identifier: AGPL-3.0-only

// Pure tests, no browser: the rule table (contract section 4) and the in-memory source (section 5).
// The table below is written out from the contract by hand, NOT derived from `actionsFor`, so it is
// a second statement of the rule and not a mirror of the code.

import { expect, test } from "@playwright/test";

import { type Action, type CampaignState, actionsFor, awaitingWinner, canEdit } from "./campaign-model";
import { blankCampaign, mockCampaignSource } from "./campaign-source";
import type { Role } from "../lib/roles";

const WS = "11111111-1111-4111-8111-111111111111";
const OTHER = "22222222-2222-4222-8222-222222222222";

const STATES: CampaignState[] = ["draft", "scheduled", "sending", "paused", "sent", "cancelled", "failed"];

/** Per state: what an editor gets and what an admin gets (owner is the same as admin). Viewer: preview and estimate. */
const TABLE: Record<CampaignState, { editor: Action[]; admin: Action[] }> = {
  draft: { editor: ["edit", "test"], admin: ["edit", "test", "send"] },
  scheduled: { editor: ["test"], admin: ["test", "unschedule", "cancel"] },
  sending: { editor: ["pause"], admin: ["pause", "cancel"] },
  paused: { editor: [], admin: ["resume", "cancel"] },
  sent: { editor: [], admin: [] },
  cancelled: { editor: [], admin: [] },
  failed: { editor: [], admin: [] },
};
const BASE: Action[] = ["preview", "estimate"];
const sorted = (a: Action[]) => [...a].sort();

test("actionsFor_matches_the_contract_table_for_every_state_and_role", () => {
  const c = blankCampaign(WS, { name: "C" });
  for (const state of STATES) {
    const want: Record<Role, Action[]> = {
      viewer: [],
      editor: TABLE[state].editor,
      admin: TABLE[state].admin,
      owner: TABLE[state].admin,
    };
    for (const role of ["viewer", "editor", "admin", "owner"] as const) {
      expect(sorted(actionsFor(state, role, c)), `${state} as ${role}`).toEqual(
        sorted([...BASE, ...want[role]]),
      );
    }
  }
});

test("winner_is_offered_to_an_admin_only_while_sending_or_paused_and_awaiting_a_winner", () => {
  for (const state of STATES) {
    const waiting = blankCampaign(WS, { name: "C", state, awaiting_winner: true });
    const live = state === "sending" || state === "paused";
    expect(actionsFor(state, "admin", waiting).includes("winner"), `${state} awaiting`).toBe(live);
    expect(actionsFor(state, "editor", waiting).includes("winner"), `${state} editor`).toBe(false);
    const decided = { ...waiting, awaiting_winner: false };
    expect(actionsFor(state, "admin", decided).includes("winner"), `${state} not awaiting`).toBe(false);
    expect(awaitingWinner(waiting), state).toBe(live);
  }
  // A winner already picked is not awaited again, whatever the flag says.
  const picked = blankCampaign(WS, { name: "C", state: "sending", awaiting_winner: true, winner: 1 });
  expect(awaitingWinner(picked)).toBe(false);
});

test("only_a_draft_can_be_edited", () => {
  expect(STATES.filter(canEdit)).toEqual(["draft"]);
});

test("the_mock_walks_the_state_machine_and_refuses_the_rest_with_409_campaign_state", async () => {
  const src = mockCampaignSource();
  const made = await src.create(WS, { name: "Spring" });
  if (!made.ok) throw new Error("create failed");
  const id = made.data.id;

  const state = async (r: ReturnType<typeof src.get>) => {
    const x = await r;
    return x.ok ? x.data.state : (x.error as { type: string }).type;
  };
  const SCHEDULE = { mode: "instant" as const, zone: "UTC", local: "2027-03-01T09:30" };

  expect(await state(src.pause(WS, id))).toBe("urn:nm:problem:campaign-state"); // draft cannot pause
  expect(await state(src.schedule(WS, id, SCHEDULE))).toBe("scheduled");
  const again = await src.schedule(WS, id, SCHEDULE);
  expect(again.ok && again.data.state).toBe("scheduled"); // into the state it is in: 200
  const s = await src.get(WS, id);
  // `local` is read as UTC, the mock's stated shortcut.
  expect(s.ok && s.data.schedule?.send_at).toBe("2027-03-01T09:30:00.000Z");
  const refused = await src.update(WS, id, { name: "Changed" });
  expect(refused).toMatchObject({ ok: false, error: { status: 409, type: "urn:nm:problem:campaign-state" } });
  expect(await src.remove(WS, id)).toMatchObject({ ok: false, error: { status: 409 } });
  expect(await state(src.unschedule(WS, id))).toBe("draft");
  expect(await state(src.cancel(WS, id))).toBe("urn:nm:problem:campaign-state"); // draft cannot cancel
  await src.schedule(WS, id, SCHEDULE);
  expect(await state(src.cancel(WS, id))).toBe("cancelled");
  expect(await state(src.resume(WS, id))).toBe("urn:nm:problem:campaign-state");
});

test("the_mock_answers_404_for_another_workspaces_campaign_on_every_route", async () => {
  const mine = blankCampaign(WS, { name: "Mine", state: "sending", awaiting_winner: true });
  const src = mockCampaignSource({ campaigns: [mine] });
  const id = mine.id;
  const calls = {
    get: src.get(OTHER, id),
    update: src.update(OTHER, id, { name: "x" }),
    remove: src.remove(OTHER, id),
    preview: src.preview(OTHER, id, {}),
    estimate: src.estimate(OTHER, id),
    test: src.test(OTHER, id, { addresses: ["a@example.com"] }),
    schedule: src.schedule(OTHER, id, { mode: "instant", zone: "UTC", local: null }),
    unschedule: src.unschedule(OTHER, id),
    pause: src.pause(OTHER, id),
    resume: src.resume(OTHER, id),
    cancel: src.cancel(OTHER, id),
    winner: src.winner(OTHER, id, { variant: 0 }),
  };
  for (const [name, call] of Object.entries(calls)) {
    expect(await call, name).toMatchObject({ ok: false, error: { status: 404 } });
  }
  const list = await src.list(OTHER);
  expect(list).toMatchObject({ ok: true, data: { data: [] } });
  // And it is untouched where it lives.
  expect(await src.get(WS, id)).toMatchObject({ ok: true, data: { name: "Mine", state: "sending" } });
});

test("the_mock_lists_newest_first_in_pages_with_a_cursor", async () => {
  const campaigns = [1, 2, 3].map((n) =>
    blankCampaign(WS, { name: `C${n}`, created_at: `2026-01-0${n}T00:00:00Z` }),
  );
  const src = mockCampaignSource({ campaigns });
  const one = await src.list(WS, { limit: 2 });
  if (!one.ok) throw new Error("list failed");
  expect(one.data.data.map((c) => c.name)).toEqual(["C3", "C2"]);
  const two = await src.list(WS, { limit: 2, after: one.data.next_cursor ?? "" });
  expect(two).toMatchObject({ ok: true, data: { data: [{ name: "C1" }], next_cursor: null } });
});

test("the_mock_pauses_a_sending_campaign_and_resumes_a_paused_one", async () => {
  const live = blankCampaign(WS, { name: "Live", state: "sending" });
  const src = mockCampaignSource({ campaigns: [live] });
  expect(await src.pause(WS, live.id)).toMatchObject({ ok: true, data: { state: "paused" } });
  expect(await src.get(WS, live.id)).toMatchObject({ ok: true, data: { state: "paused" } });
  expect(await src.resume(WS, live.id)).toMatchObject({ ok: true, data: { state: "sending" } });
  // Resuming what is already sending is 200: a transition into the current state changes nothing.
  expect(await src.resume(WS, live.id)).toMatchObject({ ok: true, data: { state: "sending" } });
});

test("the_mock_records_a_winner_only_while_one_is_awaited", async () => {
  const waiting = blankCampaign(WS, { name: "Split", state: "paused", awaiting_winner: true });
  const src = mockCampaignSource({ campaigns: [waiting] });
  expect(await src.winner(WS, waiting.id, { variant: 1 })).toMatchObject({
    ok: true,
    data: { winner: 1, awaiting_winner: false },
  });
  // A second pick, and a pick on a campaign that never awaited one, are 409 campaign-state.
  const refused = { ok: false, error: { status: 409, type: "urn:nm:problem:campaign-state" } };
  expect(await src.winner(WS, waiting.id, { variant: 0 })).toMatchObject(refused);
  // Each refusal on its own: a draft, a live campaign not awaiting one, and one whose winner is set.
  const never = [
    blankCampaign(WS, { name: "Draft", awaiting_winner: true }),
    blankCampaign(WS, { name: "Not awaiting", state: "sending", awaiting_winner: false }),
    blankCampaign(WS, { name: "Picked", state: "sending", awaiting_winner: true, winner: 0 }),
  ];
  const other = mockCampaignSource({ campaigns: never });
  for (const c of never) expect(await other.winner(WS, c.id, { variant: 1 }), c.name).toMatchObject(refused);
});
