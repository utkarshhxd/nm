// SPDX-License-Identifier: AGPL-3.0-only

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useParams } from "@tanstack/react-router";
import { useState } from "react";

import { api } from "../api/client";
import { Button } from "../components/button";
import { Audience } from "../components/campaign-editor/audience";
import { CampaignProgress } from "../components/campaign-editor/progress";
import { Content } from "../components/campaign-editor/content";
import { Preview } from "../components/campaign-editor/preview";
import { Schedule, type Sched } from "../components/campaign-editor/schedule";
import { Problem, SaveRow, Section } from "../components/campaign-editor/section";
import { Sender } from "../components/campaign-editor/sender";
import { TestSend } from "../components/campaign-editor/test-send";
import { ConfirmDialog } from "../components/confirm-dialog";
import { ErrorState } from "../components/error-state";
import { Skeleton } from "../components/skeleton";
import type { Role } from "../lib/roles";
import {
  type Campaign,
  type EditorForm,
  type UpdateCampaign,
  actionsFor,
  estimateText,
  pollMs,
  fieldErrors,
  namesShownField,
  fromAddress,
  initForm,
  resolveLocal,
  toFields,
} from "./campaign-model";
import { campaignSource } from "./campaign-source";
import { StateBadge, campaignProblem, campaignsKey, showInstant } from "./campaigns";
import { useWorkspace } from "./workspace-layout";

export default function CampaignDetail() {
  const { workspace } = useWorkspace();
  const { campaignId } = useParams({ from: "/w/$slug/campaigns/$campaignId" });
  const ws = workspace?.id ?? "";
  const campaign = useQuery({
    queryKey: [...campaignsKey(ws), campaignId],
    enabled: workspace !== null,
    queryFn: async () => {
      const r = await campaignSource.get(ws, campaignId);
      if (!r.ok) {
        throw new Error(campaignProblem(r.error, "The campaign could not be loaded."));
      }
      return r.data;
    },
    retry: false,
    // Only a sending campaign changes under the screen; every other state waits for a person.
    refetchInterval: (q) => pollMs(q.state.data?.state),
  });
  // The schedule starts in the workspace's own zone. Waiting for it keeps the zone field from
  // changing under someone who has started typing; if it cannot be read, UTC is the stated fallback.
  const zone = useQuery({
    queryKey: [...campaignsKey(ws), "zone"],
    enabled: workspace !== null && campaign.data?.state === "draft",
    queryFn: async () => {
      const { data, response } = await api.GET("/api/v1/workspaces/{id}", { params: { path: { id: ws } } });
      return response.ok && data ? data.timezone : "UTC";
    },
    retry: false,
  });
  if (!workspace) return null;
  return (
    <section className="mt-8">
      {campaign.isPending ? (
        <Skeleton label="Loading campaign" rows={3} />
      ) : campaign.data === undefined ? (
        <ErrorState title="The campaign could not be loaded." onRetry={() => void campaign.refetch()}>
          {campaign.error.message}
        </ErrorState>
      ) : campaign.data.state !== "draft" ? (
        <>
          {/* A failed refresh keeps the last good view, and any dialog open on it, rather than replacing them. */}
          {campaign.isRefetchError ? (
            <p role="status" className="mb-3 text-fg-muted">
              The latest refresh failed. Showing the last loaded view.
            </p>
          ) : null}
          <CampaignProgress
            campaign={campaign.data}
            source={campaignSource}
            ws={ws}
            slug={workspace.slug}
            role={workspace.role}
          />
        </>
      ) : zone.isPending ? (
        <Skeleton label="Loading the time zone" rows={3} />
      ) : (
        <Editor c={campaign.data} ws={ws} slug={workspace.slug} role={workspace.role} zone={zone.data ?? "UTC"} />
      )}
    </section>
  );
}

function Editor({ c, ws, slug, role, zone }: { c: Campaign; ws: string; slug: string; role: Role; zone: string }) {
  const qc = useQueryClient();
  const can = actionsFor(c.state, role, c);
  const editable = can.includes("edit");
  const detailKey = [...campaignsKey(ws), c.id];

  const [form, setForm] = useState<EditorForm>(() => initForm(c));
  const [sched, setSched] = useState<Sched>({ when: "now", local: "", zone, mode: "instant" });
  const [confirming, setConfirming] = useState(false);

  const campaignOptions = useQuery({
    queryKey: [...campaignsKey(ws), "options"],
    queryFn: async () => {
      const r = await campaignSource.options(ws);
      if (!r.ok) throw new Error(campaignProblem(r.error, "The lists, domains and templates could not be loaded."));
      return r.data;
    },
    retry: false,
  });
  const estimate = useQuery({
    queryKey: [...detailKey, "estimate"],
    queryFn: async () => {
      const r = await campaignSource.estimate(ws, c.id);
      if (!r.ok) throw new Error(campaignProblem(r.error, "The audience could not be counted."));
      return r.data;
    },
    retry: false,
  });
  const reach = estimate.isPending ? "Counting…" : estimate.isError ? estimate.error.message : estimateText(estimate.data);

  const save = useMutation({
    mutationFn: async (v: { section: string; patch: UpdateCampaign }) => {
      const r = await campaignSource.update(ws, c.id, v.patch);
      if (!r.ok) throw r.error;
      return r.data;
    },
    onSuccess: async (data) => {
      qc.setQueryData(detailKey, data);
      await qc.invalidateQueries({ queryKey: campaignsKey(ws) });
    },
  });

  const variantCount = form.variants.length;
  // A split test cannot follow each recipient's local time; the schedule section shows it so.
  const mode = variantCount > 1 ? "instant" : sched.mode;
  const resolved = sched.when === "at" ? resolveLocal(sched.local, sched.zone) : null;
  const send = useMutation({
    mutationFn: async () => {
      const r = await campaignSource.schedule(ws, c.id, {
        mode,
        zone: sched.zone,
        local: sched.when === "at" ? sched.local : null,
      });
      if (!r.ok) throw r.error;
      return r.data;
    },
    onSuccess: async (data) => {
      setConfirming(false);
      qc.setQueryData(detailKey, data);
      await qc.invalidateQueries({ queryKey: campaignsKey(ws) });
    },
    // A refusal that names a field is shown on that field, so the dialog gets out of its way.
    onError: (e) => {
      if (namesShownField(e)) setConfirming(false);
    },
  });

  // Editing clears the last answer: a stale refusal on a field the person is fixing is noise.
  const edit = (patch: Partial<EditorForm>) => {
    setForm((f) => ({ ...f, ...patch }));
    save.reset();
    send.reset();
  };
  const editSched = (patch: Partial<Sched>) => {
    setSched((s) => ({ ...s, ...patch }));
    send.reset();
  };

  const errors = { ...fieldErrors(save.error), ...fieldErrors(send.error) };
  const fields = toFields(form);
  const dirty = JSON.stringify(fields) !== JSON.stringify(toFields(initForm(c)));
  // What stops Send, in words: it sends the saved campaign, so what is on screen must be saved.
  const blocker = dirty
    ? "Save your changes before sending."
    : sched.when === "at" && resolved === null
      ? "Pick a valid date, time and zone first."
      : null;

  const usedTemplates = [...new Set(c.variants.map((v) => v.template_id))].flatMap((id) => {
    const t = campaignOptions.data?.templates.find((x) => x.id === id);
    return t ? [{ id, name: t.name }] : [];
  });
  const options = campaignOptions.data;
  const domain = options?.sendingDomains.find((d) => d.id === c.sending_domain_id)?.name ?? "";
  const from = fromAddress(c.from_name ?? "", c.from_local_part ?? "", domain);

  const sectionNote = (section: string) =>
    save.isError && save.variables?.section === section && !namesShownField(save.error) ? (
      <Problem error={save.error} slug={slug} templates={usedTemplates} />
    ) : null;
  const saveRow = (section: string, label: string, patch: UpdateCampaign) => (
    <SaveRow
      label={label}
      busy={save.isPending && save.variables?.section === section}
      saved={save.isSuccess && save.variables?.section === section}
      onSave={() => save.mutate({ section, patch })}
    >
      {sectionNote(section)}
    </SaveRow>
  );

  return (
    <>
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h2 className="text-xl font-semibold wrap-anywhere">{c.name}</h2>
        <StateBadge state={c.state} />
        {can.includes("send") ? (
          <Button
            variant="solid"
            disabled={blocker !== null}
            onClick={() => {
              send.reset();
              setConfirming(true);
            }}
          >
            Send
          </Button>
        ) : null}
      </div>
      {can.includes("send") && blocker ? <p className="mt-2 text-fg-muted">{blocker}</p> : null}

      {campaignOptions.isPending ? (
        <Skeleton label="Loading the editor" rows={4} />
      ) : campaignOptions.isError ? (
        <ErrorState title="The editor could not be loaded." onRetry={() => void campaignOptions.refetch()}>
          {campaignOptions.error.message}
        </ErrorState>
      ) : (
        <>
          <Section title="Audience" disabled={!editable}>
            <Audience options={campaignOptions.data} form={form} set={edit} errors={errors} estimate={reach} />
            {editable ? saveRow("audience", "Save audience", { list_ids: fields.list_ids, segment_id: fields.segment_id }) : null}
          </Section>
          <Section title="Content" disabled={!editable}>
            <Content templates={campaignOptions.data.templates} form={form} set={edit} errors={errors} />
            {editable
              ? saveRow("content", "Save content", { variants: fields.variants, ab_test_percent: fields.ab_test_percent })
              : null}
          </Section>
          <Section title="Sender" disabled={!editable}>
            <Sender domains={campaignOptions.data.sendingDomains} form={form} set={edit} errors={errors} />
            {editable
              ? saveRow("sender", "Save sender", {
                  sending_domain_id: fields.sending_domain_id,
                  from_local_part: fields.from_local_part,
                  from_name: fields.from_name,
                  reply_to: fields.reply_to,
                })
              : null}
          </Section>
        </>
      )}
      {can.includes("send") ? (
        <Section title="Schedule">
          <Schedule sched={{ ...sched, mode }} set={editSched} variantCount={variantCount} errors={errors} />
        </Section>
      ) : null}
      {can.includes("test") ? (
        <Section title="Test send">
          <TestSend ws={ws} id={c.id} variantCount={c.variants.length} slug={slug} templates={usedTemplates} />
        </Section>
      ) : null}
      <Section title="Preview">
        <Preview ws={ws} id={c.id} variantCount={c.variants.length} savedAt={c.updated_at} />
      </Section>

      <ConfirmDialog
        open={confirming}
        onClose={() => setConfirming(false)}
        title="Send campaign"
        confirmLabel={sched.when === "now" ? "Send now" : "Schedule send"}
        variant="solid"
        busy={send.isPending}
        onConfirm={() => {
          if (!send.isPending) send.mutate();
        }}
      >
        <p>
          Audience: <strong>{reach}</strong>
        </p>
        <p>
          When:{" "}
          <strong>
            {sched.when === "now"
              ? "as soon as you confirm"
              : `${sched.local.replace("T", " ")} ${sched.zone} (${showInstant(resolved)})`}
          </strong>
          {mode === "recipient_local" ? " for the earliest zone, then each recipient's own local time" : ""}
        </p>
        <p>
          From: <strong>{from ?? "not set yet"}</strong>
        </p>
        {send.isError ? <Problem error={send.error} slug={slug} templates={usedTemplates} /> : null}
      </ConfirmDialog>
    </>
  );
}
