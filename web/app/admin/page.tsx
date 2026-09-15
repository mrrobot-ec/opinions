"use client";

import Link from "next/link";
import { useCallback, useEffect, useMemo, useState } from "react";
import AdminOpsNav from "@/app/components/AdminOpsNav";
import {
  ApiClientError,
  createDraft,
  getAdminToken,
  getSchedule,
  getSlotFillMetric,
  listDrafts,
  listSlotUnfilled,
  listVideoJobs,
  publishDraftNow,
  reviewDraft,
} from "@/lib/api";
import {
  findSlotConflicts,
  formatFillRate,
  slotFillRate,
  type ScheduledDraftSlot,
} from "@/lib/content";
import {
  CURATION,
  draftStatusLabel,
  jobStatusLabel,
  sourceLabel,
} from "@/lib/curationCopy";
import type {
  MarketDraftDto,
  ScheduleSlotDto,
  SlotFillMetricDto,
  SlotUnfilledEventDto,
  VideoJobDto,
} from "@/lib/types";

export default function AdminCurationPage() {
  const [adminReady, setAdminReady] = useState(false);
  const [available, setAvailable] = useState(true);
  const [drafts, setDrafts] = useState<MarketDraftDto[]>([]);
  const [schedule, setSchedule] = useState<ScheduleSlotDto[]>([]);
  const [fill, setFill] = useState<SlotFillMetricDto | null>(null);
  const [unfilled, setUnfilled] = useState<SlotUnfilledEventDto[]>([]);
  const [jobs, setJobs] = useState<VideoJobDto[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [topic, setTopic] = useState("");
  const [tier, setTier] = useState<"daily" | "flash">("flash");
  const [edits, setEdits] = useState<
    Record<string, { question: string; description: string; video_script: string }>
  >({});

  const load = useCallback(async () => {
    setError(null);
    const token = getAdminToken();
    setAdminReady(!!token);
    if (!token) {
      setAvailable(true);
      return;
    }
    try {
      const [d, s, f, u, j] = await Promise.all([
        listDrafts(),
        getSchedule(),
        getSlotFillMetric(),
        listSlotUnfilled(),
        listVideoJobs(),
      ]);
      // Any null → that surface unavailable; if all null, whole curation unavailable
      if (
        d === null &&
        s === null &&
        f === null &&
        u === null &&
        j === null
      ) {
        setAvailable(false);
        return;
      }
      setAvailable(true);
      if (d) {
        setDrafts(d);
        setEdits((prev) => {
          const next = { ...prev };
          for (const row of d) {
            if (!next[row.id]) {
              next[row.id] = {
                question: row.question,
                description: row.description ?? "",
                video_script: row.video_script ?? "",
              };
            }
          }
          return next;
        });
      }
      if (s) setSchedule(s);
      if (f) setFill(f);
      if (u) setUnfilled(u);
      if (j) setJobs(j);
    } catch (e) {
      setError(
        e instanceof ApiClientError
          ? e.message
          : e instanceof Error
            ? e.message
            : CURATION.err_generic,
      );
    }
  }, []);

  useEffect(() => {
    void load();
    const sync = () => void load();
    window.addEventListener("opinions-auth", sync);
    window.addEventListener("focus", sync);
    return () => {
      window.removeEventListener("opinions-auth", sync);
      window.removeEventListener("focus", sync);
    };
  }, [load]);

  const scheduledSlots: ScheduledDraftSlot[] = useMemo(() => {
    if (schedule.length) {
      return schedule
        .filter((s) => s.draft_id && s.publish_at)
        .map((s) => ({
          id: s.draft_id!,
          question: s.question ?? "—",
          tier: s.tier,
          publish_at: s.publish_at,
          status: s.status,
        }));
    }
    return drafts
      .filter((d) => d.status === "approved" && d.publish_at)
      .map((d) => ({
        id: d.id,
        question: d.question,
        tier: d.tier,
        publish_at: d.publish_at!,
        status: d.status,
      }));
  }, [schedule, drafts]);

  const conflicts = useMemo(
    () => findSlotConflicts(scheduledSlots),
    [scheduledSlots],
  );

  const rate = useMemo(() => {
    if (fill?.fill_rate != null) return fill.fill_rate;
    if (fill) return slotFillRate(fill.filled, fill.unfilled);
    return null;
  }, [fill]);

  async function onCreate() {
    if (!topic.trim()) return;
    setBusyId("create");
    setError(null);
    try {
      await createDraft({
        topic: topic.trim(),
        tier,
        use_llm: false,
        fallback: true,
      });
      setTopic("");
      await load();
    } catch (e) {
      setError(
        e instanceof ApiClientError ? e.message : CURATION.err_generic,
      );
    } finally {
      setBusyId(null);
    }
  }

  async function onReview(id: string, action: "approve" | "reject") {
    setBusyId(id);
    setError(null);
    try {
      const e = edits[id];
      await reviewDraft(id, {
        action,
        edits: e
          ? {
              question: e.question,
              description: e.description,
              video_script: e.video_script,
            }
          : undefined,
      });
      await load();
    } catch (e) {
      const msg =
        e instanceof ApiClientError
          ? e.code === "NoSlotFree" || e.message.includes("slot")
            ? CURATION.err_no_slot
            : e.message
          : CURATION.err_generic;
      setError(msg);
    } finally {
      setBusyId(null);
    }
  }

  async function onPublishNow(id: string) {
    setBusyId(id);
    setError(null);
    try {
      await publishDraftNow(id);
      await load();
    } catch (e) {
      if (e instanceof ApiClientError && e.status === 409) {
        setError(CURATION.err_pending_publish);
      } else {
        setError(
          e instanceof ApiClientError ? e.message : CURATION.err_generic,
        );
      }
    } finally {
      setBusyId(null);
    }
  }

  return (
    <div className="admin-page stack">
      <AdminOpsNav />
      <p className="crumb">
        <Link href="/">← Markets</Link>
      </p>
      <header className="panel">
        <h1 className="page-title">{CURATION.page_title}</h1>
        <p className="dev-banner" role="status" style={{ marginTop: "0.75rem" }}>
          {CURATION.dev_admin_banner}
        </p>
        <p className="panel-hint">{CURATION.flash_poster_note}</p>
      </header>

      {!adminReady && (
        <div className="state-box">
          <p>{CURATION.need_admin_token}</p>
        </div>
      )}

      {adminReady && !available && (
        <div className="state-box">
          <p>{CURATION.unavailable}</p>
        </div>
      )}

      {error && (
        <p className="field-error" role="alert">
          {error}
        </p>
      )}

      {adminReady && available && (
        <>
          <section className="panel admin-metrics">
            <div className="row" style={{ justifyContent: "space-between" }}>
              <div>
                <div className="lbl-muted">{CURATION.fill_rate_label}</div>
                <div className="num bigish">{formatFillRate(rate)}</div>
                {fill && (
                  <p className="panel-hint" style={{ margin: 0 }}>
                    filled {fill.filled} · unfilled {fill.unfilled}
                    {fill.window_label ? ` · ${fill.window_label}` : ""}
                  </p>
                )}
              </div>
              <button type="button" className="btn" onClick={() => void load()}>
                Refresh
              </button>
            </div>
          </section>

          <section className="panel">
            <h3>{CURATION.create_draft}</h3>
            <div className="field">
              <label htmlFor="topic">{CURATION.topic_label}</label>
              <input
                id="topic"
                value={topic}
                onChange={(e) => setTopic(e.target.value)}
                placeholder="e.g. weekend sports openers"
              />
            </div>
            <div className="row">
              <button
                type="button"
                className={`btn btn-chip ${tier === "flash" ? "active" : ""}`}
                onClick={() => setTier("flash")}
              >
                {CURATION.tier_flash}
              </button>
              <button
                type="button"
                className={`btn btn-chip ${tier === "daily" ? "active" : ""}`}
                onClick={() => setTier("daily")}
              >
                {CURATION.tier_daily}
              </button>
              <button
                type="button"
                className="btn btn-primary"
                disabled={busyId === "create" || !topic.trim()}
                onClick={() => void onCreate()}
              >
                {CURATION.create_draft}
              </button>
            </div>
          </section>

          <section className="panel">
            <h3>{CURATION.draft_queue}</h3>
            {drafts.length === 0 ? (
              <p className="panel-hint">{CURATION.no_pending}</p>
            ) : (
              <ul className="draft-list">
                {drafts.map((d) => {
                  const e = edits[d.id] ?? {
                    question: d.question,
                    description: d.description ?? "",
                    video_script: d.video_script ?? "",
                  };
                  return (
                    <li key={d.id} className="draft-card">
                      <div className="row draft-meta">
                        <span className="badge">{draftStatusLabel(d.status)}</span>
                        <span className="rail-tag">
                          {d.tier === "flash"
                            ? CURATION.tier_flash
                            : CURATION.tier_daily}
                        </span>
                        <span className="panel-hint" style={{ margin: 0 }}>
                          {sourceLabel(d.source)}
                        </span>
                      </div>
                      {d.fallback_from && (
                        <p className="fallback-banner" role="status">
                          {CURATION.fallback_banner}
                        </p>
                      )}
                      {d.status === "pending" || d.status === "approved" ? (
                        <>
                          <div className="field">
                            <label>{CURATION.edit_question}</label>
                            <input
                              value={e.question}
                              disabled={d.status !== "pending"}
                              onChange={(ev) =>
                                setEdits((prev) => ({
                                  ...prev,
                                  [d.id]: { ...e, question: ev.target.value },
                                }))
                              }
                            />
                          </div>
                          <div className="field">
                            <label>{CURATION.edit_description}</label>
                            <textarea
                              rows={2}
                              value={e.description}
                              disabled={d.status !== "pending"}
                              onChange={(ev) =>
                                setEdits((prev) => ({
                                  ...prev,
                                  [d.id]: {
                                    ...e,
                                    description: ev.target.value,
                                  },
                                }))
                              }
                            />
                          </div>
                          <div className="field">
                            <label>{CURATION.edit_script}</label>
                            <textarea
                              rows={2}
                              value={e.video_script}
                              disabled={d.status !== "pending"}
                              onChange={(ev) =>
                                setEdits((prev) => ({
                                  ...prev,
                                  [d.id]: {
                                    ...e,
                                    video_script: ev.target.value,
                                  },
                                }))
                              }
                            />
                          </div>
                        </>
                      ) : (
                        <p className="comment-body">{d.question}</p>
                      )}
                      {d.publish_at && (
                        <p className="panel-hint">
                          {CURATION.reserved_badge}: {d.publish_at}
                          {conflicts.has(d.id) && (
                            <span className="badge warn">
                              {" "}
                              {CURATION.conflict_badge}
                            </span>
                          )}
                        </p>
                      )}
                      <div className="row">
                        {d.status === "pending" && (
                          <>
                            <button
                              type="button"
                              className="btn btn-primary"
                              disabled={busyId === d.id}
                              onClick={() => void onReview(d.id, "approve")}
                            >
                              {CURATION.approve}
                            </button>
                            <button
                              type="button"
                              className="btn"
                              disabled={busyId === d.id}
                              onClick={() => void onReview(d.id, "reject")}
                            >
                              {CURATION.reject}
                            </button>
                          </>
                        )}
                        {d.status === "approved" && (
                          <button
                            type="button"
                            className="btn btn-primary"
                            title={CURATION.publish_now_hint}
                            disabled={busyId === d.id}
                            onClick={() => void onPublishNow(d.id)}
                          >
                            {CURATION.publish_now}
                          </button>
                        )}
                        {d.published_market_id && (
                          <Link
                            className="btn btn-chip"
                            href={`/m/${d.published_market_id}`}
                          >
                            Open market
                          </Link>
                        )}
                      </div>
                    </li>
                  );
                })}
              </ul>
            )}
          </section>

          <section className="panel">
            <h3>{CURATION.schedule_rail}</h3>
            {scheduledSlots.length === 0 ? (
              <p className="panel-hint">{CURATION.no_schedule}</p>
            ) : (
              <ul className="schedule-list">
                {scheduledSlots.map((s) => (
                  <li key={s.id}>
                    <span className="badge">{s.tier}</span>
                    <span className="num">{s.publish_at}</span>
                    <span>{s.question}</span>
                    {conflicts.has(s.id) && (
                      <span className="badge warn">{CURATION.conflict_badge}</span>
                    )}
                  </li>
                ))}
              </ul>
            )}
          </section>

          <section className="panel">
            <h3>{CURATION.slot_unfilled}</h3>
            {unfilled.length === 0 ? (
              <p className="panel-hint">{CURATION.no_unfilled}</p>
            ) : (
              <ul className="schedule-list">
                {unfilled.map((u, i) => (
                  <li key={u.id ?? `${u.slot_ts}-${i}`}>
                    <span className="badge warn">{u.tier}</span>
                    <span className="num">{u.slot_ts}</span>
                  </li>
                ))}
              </ul>
            )}
          </section>

          <section className="panel">
            <h3>{CURATION.video_jobs}</h3>
            {jobs.length === 0 ? (
              <p className="panel-hint">No jobs yet.</p>
            ) : (
              <ul className="job-chips">
                {jobs.map((j) => (
                  <li key={j.id} className={`job-chip status-${j.status}`}>
                    <span className="job-kind">{j.kind}</span>
                    <span className="badge">{jobStatusLabel(j.status)}</span>
                    {j.market_id && (
                      <span className="num">{j.market_id.slice(0, 8)}…</span>
                    )}
                    {j.error && (
                      <span className="field-error">{j.error}</span>
                    )}
                  </li>
                ))}
              </ul>
            )}
          </section>
        </>
      )}
    </div>
  );
}
