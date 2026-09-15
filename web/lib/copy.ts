/**
 * Product copy — mirrors docs/copy/notifications.md (and scoring residual).
 * UI must use these strings; no lorem / invent-as-you-go.
 */

export const NOTIF_COPY = {
  resolution_trade: "Paid out {pnl} on '{question}'",
  resolution_trade_with_score:
    "Paid out {pnl} on '{question}' · your crowd call scored {score}",
  resolution_vote: "'{question}' resolved — your crowd call scored {score}",
  resolution_void:
    "'{question}' was voided — positions redeem at neutral value",
  comment_reply: "{handle} replied to you",
  mention: "{handle} mentioned you",
  rep_tier_change: "You reached tier {tier}",
  curator_needed: "Curator action needed on a flagged market",
  admin_report_queue:
    "Comment under review — reports reached the shadow threshold",
} as const;

export const SOCIAL_COPY = {
  report_confirmation:
    "Report submitted. Thanks for helping keep the section usable.",
  shadow_banner: "Only you can see this while it's under review",
  blocked_empty: "Comment can't be empty.",
  blocked_too_long: "Comment is too long.",
  blocked_rate: "You're commenting too fast — try again in a moment.",
  blocked_links: "Too many links — remove some and try again.",
  blocked_duplicate:
    "That looks like a repeat of something you just posted.",
  thread_too_deep:
    "This thread is as deep as it goes — reply higher up.",
  duplicate_vote: "You already voted on this comment.",
  reporter_floor:
    "You can't report yet — account age or tier is too low.",
  report_velocity: "Too many reports too quickly — slow down.",
  not_visible: "That comment isn't available.",
  holders_label: "by committed capital",
  holders_tooltip:
    "Ranked by remaining cost basis (what holders paid in), not mark-to-market value.",
  comments_empty: "No comments yet — start the thread.",
  comments_unavailable: "Comments aren't available yet.",
  login_to_comment: "Dev login required to comment.",
  login_to_vote: "Dev login required.",
} as const;

/** Types that may raise a live toast (plan: resolution_* + rep_tier_change only). */
export const TOASTABLE_NOTIF_TYPES = new Set([
  "resolution_trade",
  "resolution_vote",
  "resolution_void",
  "rep_tier_change",
]);

export function shouldToastNotifType(type: string): boolean {
  return TOASTABLE_NOTIF_TYPES.has(type);
}

function fill(template: string, vars: Record<string, string>): string {
  return template.replace(/\{(\w+)\}/g, (_, key: string) => vars[key] ?? "");
}

export interface NotifPayload {
  question?: string;
  pnl?: string;
  score?: string | number;
  handle?: string;
  tier?: string | number;
  realized_delta?: number;
  realized_delta_micro?: number;
  payout_total_micro?: number;
  score_bp?: number;
  redemption?: number;
  side?: string;
  [key: string]: unknown;
}

/** Render a notification row/toast from type + payload. */
export function formatNotificationText(
  type: string,
  payload: NotifPayload,
  opts: { formatPnl?: (micro: number) => string; formatScore?: (bp: number) => string } = {},
): string {
  const question = String(payload.question ?? "market");
  const handle = String(payload.handle ?? "someone");
  const tier = String(payload.tier ?? "");
  const fmtPnl =
    opts.formatPnl ??
    ((m: number) => {
      const sign = m < 0 ? "-" : "";
      const abs = Math.abs(m);
      const cents = Math.round(abs / 10_000);
      return `${sign}$${Math.floor(cents / 100)}.${(cents % 100).toString().padStart(2, "0")}`;
    });
  const fmtScore =
    opts.formatScore ??
    ((bp: number) => `${(bp / 100).toFixed(0)}`);

  const pnlStr =
    payload.pnl != null
      ? String(payload.pnl)
      : payload.realized_delta_micro != null
        ? fmtPnl(Number(payload.realized_delta_micro))
        : payload.realized_delta != null
          ? fmtPnl(Number(payload.realized_delta))
        : "$0.00";
  const scoreStr =
    payload.score != null
      ? String(payload.score)
      : payload.score_bp != null
        ? fmtScore(Number(payload.score_bp))
        : "—";

  switch (type) {
    case "resolution_trade":
      if (payload.score_bp != null || payload.score != null) {
        return fill(NOTIF_COPY.resolution_trade_with_score, {
          pnl: pnlStr,
          question,
          score: scoreStr,
        });
      }
      return fill(NOTIF_COPY.resolution_trade, { pnl: pnlStr, question });
    case "resolution_vote":
      return fill(NOTIF_COPY.resolution_vote, { question, score: scoreStr });
    case "resolution_void":
      return fill(NOTIF_COPY.resolution_void, { question });
    case "comment_reply":
      return fill(NOTIF_COPY.comment_reply, { handle });
    case "mention":
      return fill(NOTIF_COPY.mention, { handle });
    case "rep_tier_change":
      return fill(NOTIF_COPY.rep_tier_change, { tier });
    case "curator_needed":
      return NOTIF_COPY.curator_needed;
    default:
      return type.replace(/_/g, " ");
  }
}

export function blockedReasonCopy(reason: string): string {
  switch (reason) {
    case "empty":
      return SOCIAL_COPY.blocked_empty;
    case "too_long":
      return SOCIAL_COPY.blocked_too_long;
    case "rate":
      return SOCIAL_COPY.blocked_rate;
    case "links":
      return SOCIAL_COPY.blocked_links;
    case "duplicate":
      return SOCIAL_COPY.blocked_duplicate;
    default:
      return reason;
  }
}

export function apiErrorCopy(code: string, fallback: string): string {
  switch (code) {
    case "thread_too_deep":
    case "ThreadTooDeep":
      return SOCIAL_COPY.thread_too_deep;
    case "duplicate_vote":
    case "DuplicateVote":
      return SOCIAL_COPY.duplicate_vote;
    case "reporter_floor":
    case "ReporterFloor":
      return SOCIAL_COPY.reporter_floor;
    case "report_velocity":
    case "ReportVelocity":
      return SOCIAL_COPY.report_velocity;
    case "not_visible":
    case "NotVisible":
      return SOCIAL_COPY.not_visible;
    default:
      return fallback;
  }
}
