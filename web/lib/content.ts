/**
 * Pure Phase 5 content helpers — asset hot-swap, slot countdown, degrade.
 * No DOM / network.
 */

export type AssetKind = "poster" | "market_video" | "video";

export interface MarketAssets {
  poster_asset_url: string | null;
  video_asset_url: string | null;
}

/** Apply versioned WS `asset` frame to market asset fields. */
export function applyAssetFrame(
  current: MarketAssets,
  frame: { kind: string; url: string },
): MarketAssets {
  const kind = frame.kind.toLowerCase();
  if (kind === "poster") {
    return { ...current, poster_asset_url: frame.url };
  }
  if (kind === "market_video" || kind === "video") {
    return { ...current, video_asset_url: frame.url };
  }
  return current;
}

/**
 * Rehydrate assets from snapshot/REST (nulls → placeholders in UI).
 * Always prefer explicit snapshot fields over stale client state when provided.
 */
export function rehydrateAssetsFromSnapshot(
  snapshot: {
    poster_asset_url?: string | null;
    video_asset_url?: string | null;
  },
  prev?: MarketAssets | null,
): MarketAssets {
  return {
    poster_asset_url:
      snapshot.poster_asset_url !== undefined
        ? snapshot.poster_asset_url
        : (prev?.poster_asset_url ?? null),
    video_asset_url:
      snapshot.video_asset_url !== undefined
        ? snapshot.video_asset_url
        : (prev?.video_asset_url ?? null),
  };
}

/** Display media URL: video preferred when present, else poster. */
export function preferredMediaUrl(assets: MarketAssets): {
  url: string | null;
  kind: "video" | "poster" | "placeholder";
} {
  if (assets.video_asset_url) {
    return { url: assets.video_asset_url, kind: "video" };
  }
  if (assets.poster_asset_url) {
    return { url: assets.poster_asset_url, kind: "poster" };
  }
  return { url: null, kind: "placeholder" };
}

/** Absolute or core-relative asset path for <img src>. */
export function resolveAssetSrc(
  url: string | null | undefined,
  coreBase: string,
): string | null {
  if (!url) return null;
  if (url.startsWith("http://") || url.startsWith("https://") || url.startsWith("data:")) {
    return url;
  }
  const base = coreBase.replace(/\/$/, "");
  return url.startsWith("/") ? `${base}${url}` : `${base}/${url}`;
}

/** Share card endpoint path (image only — never inline HTML). */
export function shareCardPath(userId: string, marketId: string): string {
  return `/users/${encodeURIComponent(userId)}/share_card/${encodeURIComponent(marketId)}`;
}

export interface ScheduledDraftSlot {
  id: string;
  question: string;
  tier: "daily" | "flash" | string;
  publish_at: string;
  status?: string;
}

/**
 * Build rail items: real scheduled drafts first, then non-bookable empty state
 * if nothing reserved (never invent fake markets).
 */
export function buildUpcomingRail(
  drafts: ScheduledDraftSlot[],
  liveCount: number,
  minEmpty = 1,
): Array<
  | { kind: "draft"; draft: ScheduledDraftSlot }
  | { kind: "empty"; id: string }
> {
  const approved = drafts
    .filter((d) => d.publish_at && (d.status === "approved" || !d.status))
    .slice()
    .sort(
      (a, b) =>
        new Date(a.publish_at).getTime() - new Date(b.publish_at).getTime(),
    );
  const out: Array<
    { kind: "draft"; draft: ScheduledDraftSlot } | { kind: "empty"; id: string }
  > = approved.map((d) => ({ kind: "draft" as const, draft: d }));
  if (out.length === 0 && liveCount === 0) {
    for (let i = 0; i < minEmpty; i++) {
      out.push({ kind: "empty", id: `empty-${i}` });
    }
  } else if (out.length === 0 && minEmpty > 0) {
    out.push({ kind: "empty", id: "empty-0" });
  }
  return out;
}

/** Ms until publish_at from a server clock + mono (same shape as countdown). */
export function msUntilPublish(
  publishAt: string,
  serverNowMs: number,
  monoAtReceipt: number,
  nowMono: number,
): number {
  const target = Date.parse(publishAt);
  if (!Number.isFinite(target)) return 0;
  const elapsed = nowMono - monoAtReceipt;
  const now = serverNowMs + elapsed;
  return Math.max(0, target - now);
}

/** Detect double-booking of the same publish_at within a tier. */
export function findSlotConflicts(
  drafts: ScheduledDraftSlot[],
): Set<string> {
  const byKey = new Map<string, string[]>();
  for (const d of drafts) {
    if (!d.publish_at) continue;
    const key = `${d.tier}|${d.publish_at}`;
    const list = byKey.get(key) ?? [];
    list.push(d.id);
    byKey.set(key, list);
  }
  const conflicts = new Set<string>();
  for (const ids of byKey.values()) {
    if (ids.length > 1) {
      for (const id of ids) conflicts.add(id);
    }
  }
  return conflicts;
}

/** Slot fill rate: filled / (filled + unfilled) over a window, null if no data. */
export function slotFillRate(
  filled: number,
  unfilled: number,
): number | null {
  const t = filled + unfilled;
  if (t <= 0) return null;
  return filled / t;
}

export function formatFillRate(rate: number | null): string {
  if (rate == null || !Number.isFinite(rate)) return "—";
  return `${Math.round(rate * 100)}%`;
}
