/**
 * Pure social-surface helpers — comments, holders coalesce, notif dedupe,
 * optimistic comment votes. No DOM / network.
 */

/** Hot list cursor: first page pins as_of; later pages reuse it (plan codex B2). */
export interface HotCursor {
  as_of: string;
  hot_score: number;
  created_at: string;
  id: string;
}

export interface RecentCursor {
  created_at: string;
  id: string;
}

export type CommentSort = "hot" | "recent";

/** Encode cursor for query string; pass as_of through on hot pages. */
export function encodeHotCursor(c: HotCursor): string {
  return [
    c.as_of,
    String(c.hot_score),
    c.created_at,
    c.id,
  ]
    .map(encodeURIComponent)
    .join("|");
}

export function decodeHotCursor(raw: string): HotCursor | null {
  const parts = raw.split("|").map(decodeURIComponent);
  if (parts.length !== 4) return null;
  const hot_score = Number(parts[1]);
  if (!Number.isFinite(hot_score)) return null;
  return {
    as_of: parts[0],
    hot_score,
    created_at: parts[2],
    id: parts[3],
  };
}

export function encodeRecentCursor(c: RecentCursor): string {
  return [c.created_at, c.id].map(encodeURIComponent).join("|");
}

export function decodeRecentCursor(raw: string): RecentCursor | null {
  const parts = raw.split("|").map(decodeURIComponent);
  if (parts.length !== 2) return null;
  return { created_at: parts[0], id: parts[1] };
}

/**
 * Build query for comments list. Hot: first page omits cursor (server sets as_of);
 * subsequent pages must pass the full cursor including as_of.
 */
export function commentsQuery(opts: {
  sort: CommentSort;
  limit?: number;
  cursor?: string | null;
  viewerId?: string | null;
}): string {
  const params = new URLSearchParams();
  params.set("sort", opts.sort);
  if (opts.limit != null) params.set("limit", String(opts.limit));
  if (opts.cursor) params.set("before", opts.cursor);
  if (opts.viewerId) params.set("viewer_id", opts.viewerId);
  return params.toString();
}

/** Event-driven coalesce: many trade frames → one fetch after windowMs. Not a poll. */
export function createCoalesce(windowMs: number, run: () => void): {
  trigger: () => void;
  cancel: () => void;
  pending: () => boolean;
} {
  let timer: ReturnType<typeof setTimeout> | null = null;
  return {
    trigger() {
      if (timer != null) clearTimeout(timer);
      timer = setTimeout(() => {
        timer = null;
        run();
      }, windowMs);
    },
    cancel() {
      if (timer != null) {
        clearTimeout(timer);
        timer = null;
      }
    },
    pending() {
      return timer != null;
    },
  };
}

export const HOLDERS_COALESCE_MS = 2000;

/** Notification frame dedupe key (plan: id + source_seq). */
export function notifDedupeKey(id: number | string, sourceSeq: number | null | undefined): string {
  return `${id}:${sourceSeq ?? "null"}`;
}

export function shouldApplyNotif(
  seen: Set<string>,
  id: number | string,
  sourceSeq: number | null | undefined,
): boolean {
  const k = notifDedupeKey(id, sourceSeq);
  if (seen.has(k)) return false;
  seen.add(k);
  if (seen.size > 2000) {
    const first = seen.values().next().value;
    if (first !== undefined) seen.delete(first);
  }
  return true;
}

/**
 * One-shot comment vote optimistic apply.
 * prevValue: 0 = no prior vote; ±1 = already voted (no change this phase).
 * Returns null if already voted (duplicate).
 */
export function optimisticCommentVote(
  score: number,
  prevValue: 0 | 1 | -1,
  nextValue: 1 | -1,
): { score: number; value: 1 | -1 } | null {
  if (prevValue !== 0) return null;
  return { score: score + nextValue, value: nextValue };
}

/** Reconcile after server response or error rollback. */
export function reconcileCommentVote(
  optimisticScore: number,
  optimisticValue: 1 | -1,
  server: { ok: true; score: number; value: 1 | -1 } | { ok: false },
): { score: number; value: 0 | 1 | -1 } {
  if (server.ok) {
    return { score: server.score, value: server.value };
  }
  // rollback optimistic
  return { score: optimisticScore - optimisticValue, value: 0 };
}

/** Nest flat comments by parent_id for depth-indented render. */
export interface FlatComment {
  id: string;
  parent_id: string | null;
  depth: number;
}

export function nestCommentsByParent<T extends FlatComment>(
  items: T[],
): { root: T; children: T[] }[] {
  const byParent = new Map<string | null, T[]>();
  for (const c of items) {
    const key = c.parent_id;
    const list = byParent.get(key) ?? [];
    list.push(c);
    byParent.set(key, list);
  }
  const roots = byParent.get(null) ?? items.filter((c) => c.parent_id == null);
  // Prefer items that appear as roots in the page window
  const rootIds = new Set(roots.map((r) => r.id));
  const orderedRoots = items.filter(
    (c) => c.parent_id == null || !items.some((p) => p.id === c.parent_id),
  );
  const useRoots = orderedRoots.length ? orderedRoots : roots;
  const seen = new Set<string>();
  const out: { root: T; children: T[] }[] = [];
  for (const r of useRoots) {
    if (seen.has(r.id)) continue;
    if (r.parent_id != null && rootIds.has(r.parent_id)) continue;
    seen.add(r.id);
    const children = (byParent.get(r.id) ?? []).filter((c) => {
      if (seen.has(c.id)) return false;
      seen.add(c.id);
      return true;
    });
    out.push({ root: r, children });
  }
  return out;
}

/** Indent style from depth (cap visual indent). */
export function commentIndentPx(depth: number, step = 16, max = 8): number {
  return Math.min(Math.max(0, depth), max) * step;
}
