import { afterEach, describe, expect, it, vi } from "vitest";
import {
  commentsQuery,
  createCoalesce,
  decodeHotCursor,
  encodeHotCursor,
  HOLDERS_COALESCE_MS,
  notifDedupeKey,
  optimisticCommentVote,
  reconcileCommentVote,
  shouldApplyNotif,
  commentIndentPx,
} from "../social";
import {
  formatNotificationText,
  shouldToastNotifType,
  NOTIF_COPY,
  SOCIAL_COPY,
} from "../copy";
import { applyUserNotifFrame } from "../ws";
import type { NotifFrame, NotifSnapshotFrame } from "../types";

describe("hot cursor as_of passthrough", () => {
  it("round-trips as_of with score and id", () => {
    const c = {
      as_of: "2026-08-12T15:00:00Z",
      hot_score: 250000,
      created_at: "2026-08-12T14:00:00Z",
      id: "c-uuid",
    };
    const enc = encodeHotCursor(c);
    expect(decodeHotCursor(enc)).toEqual(c);
  });

  it("puts sort and before cursor in query", () => {
    const q = commentsQuery({
      sort: "hot",
      limit: 20,
      cursor: "as|1|t|id",
      viewerId: "u1",
    });
    expect(q).toContain("sort=hot");
    expect(q).toContain("before=");
    expect(q).toContain("viewer_id=u1");
    expect(q).toContain("limit=20");
  });
});

describe("holders coalesce", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("is event-driven debounce not a fixed interval poll", () => {
    vi.useFakeTimers();
    const run = vi.fn();
    const c = createCoalesce(HOLDERS_COALESCE_MS, run);
    c.trigger();
    c.trigger();
    c.trigger();
    expect(run).not.toHaveBeenCalled();
    vi.advanceTimersByTime(HOLDERS_COALESCE_MS - 1);
    expect(run).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(run).toHaveBeenCalledTimes(1);
  });
});

describe("optimistic comment vote reconcile", () => {
  it("applies +1 when no prior vote", () => {
    expect(optimisticCommentVote(3, 0, 1)).toEqual({ score: 4, value: 1 });
  });

  it("rejects when already voted (one-shot)", () => {
    expect(optimisticCommentVote(3, 1, -1)).toBeNull();
  });

  it("reconciles server ok and rolls back on error", () => {
    expect(
      reconcileCommentVote(4, 1, { ok: true, score: 4, value: 1 }),
    ).toEqual({ score: 4, value: 1 });
    expect(reconcileCommentVote(4, 1, { ok: false })).toEqual({
      score: 3,
      value: 0,
    });
  });
});

describe("notif frame dedupe (id, source_seq)", () => {
  it("keys and drops duplicates", () => {
    const seen = new Set<string>();
    expect(shouldApplyNotif(seen, 10, 5)).toBe(true);
    expect(shouldApplyNotif(seen, 10, 5)).toBe(false);
    expect(shouldApplyNotif(seen, 10, 6)).toBe(true);
    expect(notifDedupeKey(10, 5)).toBe("10:5");
  });

  it("applyUserNotifFrame always allows snapshot", () => {
    const seen = new Set<string>();
    const snap: NotifSnapshotFrame = {
      type: "notif_snapshot",
      unread_count: 2,
    };
    expect(applyUserNotifFrame(seen, snap)).toBe(true);
    const n: NotifFrame = {
      type: "notif",
      v: 1,
      id: 1,
      source_seq: 9,
      notif_type: "mention",
      payload: {},
    };
    expect(applyUserNotifFrame(seen, n)).toBe(true);
    expect(applyUserNotifFrame(seen, n)).toBe(false);
  });
});

describe("toast posture + copy", () => {
  it("toasts only resolution_* and rep_tier_change", () => {
    expect(shouldToastNotifType("resolution_trade")).toBe(true);
    expect(shouldToastNotifType("resolution_vote")).toBe(true);
    expect(shouldToastNotifType("resolution_void")).toBe(true);
    expect(shouldToastNotifType("rep_tier_change")).toBe(true);
    expect(shouldToastNotifType("comment_reply")).toBe(false);
    expect(shouldToastNotifType("mention")).toBe(false);
  });

  it("formats holder-voter resolution with score", () => {
    const t = formatNotificationText("resolution_trade", {
      question: "Will it rain?",
      realized_delta: 1_500_000,
      score_bp: 8200,
    });
    expect(t).toContain("Paid out");
    expect(t).toContain("Will it rain?");
    expect(t).toContain("scored");
  });

  it("formats the realized-delta field emitted by the notifier", () => {
    const t = formatNotificationText("resolution_trade", {
      realized_delta_micro: 1_500_000,
    });
    expect(t).toContain("$1.50");
  });

  it("shadow banner and reply templates match product copy", () => {
    expect(SOCIAL_COPY.shadow_banner).toBe(
      "Only you can see this while it's under review",
    );
    expect(formatNotificationText("comment_reply", { handle: "alice" })).toBe(
      NOTIF_COPY.comment_reply.replace("{handle}", "alice"),
    );
  });
});

describe("comment indent", () => {
  it("caps depth indent", () => {
    expect(commentIndentPx(0)).toBe(0);
    expect(commentIndentPx(2)).toBe(32);
    expect(commentIndentPx(100)).toBe(128);
  });
});
