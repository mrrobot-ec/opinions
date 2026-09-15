/**
 * REST client for the opinions core.
 * Auth: demo token in localStorage (dev login) — labeled DEV in UI.
 * Chart/tape/leaderboards: feature-detect 404 → designed empty/hide.
 */

import { getOrCreateDeviceId } from "./device";
import type {
  ApiError,
  AuditPageDto,
  CastVoteRequest,
  CommentsPageDto,
  ConfigAppliedDto,
  ConfigProposalDto,
  ConfigSnapshotDto,
  DraftListDto,
  HoldersDto,
  MarketDraftDto,
  MarketSummaryDto,
  NotificationDto,
  NotificationsPageDto,
  PlaceTradeRequest,
  PositionDto,
  PreviewTradeRequest,
  PricePoint,
  ProfileTradeRow,
  ScheduleSlotDto,
  SideDto,
  SlotFillMetricDto,
  SlotUnfilledEventDto,
  TapeRow,
  TradeActionDto,
  TradePreviewDto,
  TradeReceiptDto,
  TraderLeaderboardRow,
  UnreadCountDto,
  UserIdDto,
  UserProfileDto,
  VideoJobDto,
  VideoJobListDto,
  VoteReceiptDto,
  VoterLeaderboardRow,
} from "./types";
import { commentsQuery, type CommentSort } from "./social";
import { shareCardPath } from "./content";

const TOKEN_KEY = "opinions_demo_token";
const USER_KEY = "opinions_user_id";
const ADMIN_TOKEN_KEY = "opinions_admin_token";

export class ApiClientError extends Error {
  status: number;
  code: string;
  constructor(status: number, code: string, message: string) {
    super(message);
    this.name = "ApiClientError";
    this.status = status;
    this.code = code;
  }
}

export function coreUrl(): string {
  if (typeof process !== "undefined" && process.env.NEXT_PUBLIC_CORE_URL) {
    return process.env.NEXT_PUBLIC_CORE_URL.replace(/\/$/, "");
  }
  return "http://127.0.0.1:8080";
}

export function getDemoToken(): string | null {
  if (typeof window === "undefined") return null;
  return localStorage.getItem(TOKEN_KEY);
}

export function getUserId(): string | null {
  if (typeof window === "undefined") return null;
  return localStorage.getItem(USER_KEY);
}

export function setDevSession(userId: string, token: string): void {
  localStorage.setItem(USER_KEY, userId);
  localStorage.setItem(TOKEN_KEY, token);
}

export function clearDevSession(): void {
  localStorage.removeItem(USER_KEY);
  localStorage.removeItem(TOKEN_KEY);
}

export function getAdminToken(): string | null {
  if (typeof window === "undefined") return null;
  return localStorage.getItem(ADMIN_TOKEN_KEY);
}

export function setAdminToken(token: string): void {
  localStorage.setItem(ADMIN_TOKEN_KEY, token);
}

export function clearAdminToken(): void {
  localStorage.removeItem(ADMIN_TOKEN_KEY);
}

export function newIdempotencyKey(): string {
  if (typeof crypto !== "undefined" && crypto.randomUUID) {
    return crypto.randomUUID();
  }
  return `idem-${Date.now()}-${Math.random().toString(36).slice(2)}`;
}

function applyAuth(headers: Headers): void {
  const token = getDemoToken();
  if (!token) return;
  // Core demo auth (Phase 1+): x-demo-token. Bearer kept for older probes.
  headers.set("x-demo-token", token);
  if (!headers.has("Authorization")) {
    headers.set("Authorization", `Bearer ${token}`);
  }
}

function applyAdmin(headers: Headers): void {
  const token = getAdminToken();
  if (!token) return;
  headers.set("x-admin-token", token);
}

async function request<T>(
  path: string,
  init: RequestInit = {},
  opts: { auth?: boolean; deviceId?: boolean; admin?: boolean } = {},
): Promise<T> {
  const headers = new Headers(init.headers);
  if (!headers.has("Content-Type") && init.body) {
    headers.set("Content-Type", "application/json");
  }
  if (opts.auth !== false) {
    applyAuth(headers);
  }
  if (opts.admin) {
    applyAdmin(headers);
  }
  if (opts.deviceId) {
    // Client-asserted device id (weak signal; server HMACs before store).
    headers.set("x-device-id", getOrCreateDeviceId());
  }
  const res = await fetch(`${coreUrl()}${path}`, { ...init, headers });
  if (res.status === 204) return undefined as T;
  const text = await res.text();
  let body: unknown = null;
  if (text) {
    try {
      body = JSON.parse(text);
    } catch {
      body = { code: "parse_error", message: text };
    }
  }
  if (!res.ok) {
    const err = body as ApiError | null;
    throw new ApiClientError(
      res.status,
      err?.code ?? "http_error",
      err?.message ?? res.statusText,
    );
  }
  return body as T;
}

/** Feature-detect: returns null on 404 so UI can hide / empty-state. */
async function optionalGet<T>(
  path: string,
  auth = false,
  admin = false,
): Promise<T | null> {
  try {
    return await request<T>(path, { method: "GET" }, { auth, admin });
  } catch (e) {
    if (e instanceof ApiClientError && e.status === 404) return null;
    // Unavailable Phase 5 skeleton may return 501/503 — treat as degrade.
    if (
      e instanceof ApiClientError &&
      (e.status === 501 || e.status === 503 || e.code === "unavailable")
    ) {
      return null;
    }
    throw e;
  }
}

export async function listMarkets(status?: string): Promise<MarketSummaryDto[]> {
  const q = status ? `?status=${encodeURIComponent(status)}` : "";
  return request<MarketSummaryDto[]>(`/markets${q}`, { method: "GET" }, { auth: false });
}

export async function getMarket(idOrSlug: string): Promise<MarketSummaryDto> {
  return request<MarketSummaryDto>(
    `/markets/${encodeURIComponent(idOrSlug)}`,
    { method: "GET" },
    { auth: false },
  );
}

export async function castVote(body: CastVoteRequest): Promise<VoteReceiptDto> {
  return request<VoteReceiptDto>(
    "/votes",
    {
      method: "POST",
      body: JSON.stringify(body),
    },
    { deviceId: true },
  );
}

export async function previewTrade(body: PreviewTradeRequest): Promise<TradePreviewDto> {
  return request<TradePreviewDto>("/trades/preview", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

export async function placeTrade(body: PlaceTradeRequest): Promise<TradeReceiptDto> {
  return request<TradeReceiptDto>("/trades", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

export async function userPositions(userId: string): Promise<PositionDto[]> {
  return request<PositionDto[]>(`/users/${encodeURIComponent(userId)}/positions`, {
    method: "GET",
  });
}

/** PLAN Task 3.1 — optional; 404 → null. */
export async function userProfile(userId: string): Promise<UserProfileDto | null> {
  type CoreProfile = UserProfileDto & {
    voter?: { avg_score_bp?: number | null; markets_scored: number };
    recent_trades?: Array<ProfileTradeRow & { market_ref?: string }>;
  };
  const profile = await optionalGet<CoreProfile>(
    `/users/${encodeURIComponent(userId)}/profile`,
    true,
  );
  if (profile === null) return null;
  return {
    ...profile,
    avg_score_bp: profile.voter?.avg_score_bp ?? profile.avg_score_bp,
    markets_scored: profile.voter?.markets_scored ?? profile.markets_scored,
    recent_trades: profile.recent_trades?.map((trade) => ({
      ...trade,
      market_slug: trade.market_slug ?? trade.market_ref,
    })),
  };
}

export async function userByChannel(
  channel: string,
  address: string,
): Promise<UserIdDto> {
  const q = `?channel=${encodeURIComponent(channel)}&address=${encodeURIComponent(address)}`;
  return request<UserIdDto>(`/users/by-channel${q}`, { method: "GET" }, { auth: false });
}

/** PLAN Task 2.3 — degrades to null when core lacks the route. */
export async function getChart(
  marketId: string,
  bucket = 60,
  since?: string,
): Promise<PricePoint[] | null> {
  const params = new URLSearchParams({ bucket: String(bucket) });
  params.set(
    "since",
    since ?? new Date(Date.now() - 24 * 60 * 60 * 1000).toISOString(),
  );
  return optionalGet<PricePoint[]>(
    `/markets/${encodeURIComponent(marketId)}/chart?${params}`,
  );
}

/** PLAN Task 2.3 — degrades to null when core lacks the route. */
export async function getTape(
  marketId: string,
  limit = 50,
): Promise<TapeRow[] | null> {
  const rows = await optionalGet<
    (Omit<TapeRow, "seq"> & { trade_seq: number })[]
  >(
    `/markets/${encodeURIComponent(marketId)}/tape?limit=${limit}`,
  );
  return rows?.map(({ trade_seq, ...row }) => ({ ...row, seq: trade_seq })) ?? null;
}

/**
 * PLAN Task 3.3 — one-shot REST (no polling). 404 → null (empty designed state).
 */
export async function getTopVoters(
  days = 7,
  limit = 10,
): Promise<VoterLeaderboardRow[] | null> {
  return optionalGet<VoterLeaderboardRow[]>(
    `/leaderboards/voters?days=${days}&limit=${limit}`,
  );
}

/** PLAN Task 3.3 — settled PnL leaderboard; 404 → null. */
export async function getTopTraders(
  days = 7,
  limit = 10,
): Promise<TraderLeaderboardRow[] | null> {
  return optionalGet<TraderLeaderboardRow[]>(
    `/leaderboards/traders?days=${days}&limit=${limit}`,
  );
}

export function voteBody(
  userId: string,
  marketRef: string,
  side: SideDto,
  crowdGuessPct: number,
): CastVoteRequest {
  return {
    user_id: userId,
    market_ref: marketRef,
    side,
    crowd_guess_pct: crowdGuessPct,
    idempotency_key: newIdempotencyKey(),
  };
}

export function tradeBody(
  userId: string,
  marketRef: string,
  side: SideDto,
  action: TradeActionDto,
  amountMicro: number,
  expectedConfigVersion: number,
): PlaceTradeRequest {
  return {
    user_id: userId,
    market_ref: marketRef,
    side,
    action,
    amount_micro: amountMicro,
    expected_config_version: expectedConfigVersion,
    idempotency_key: newIdempotencyKey(),
  };
}

// --- PLAN Task 4.3 / 4.4 social endpoints (404 → designed empty) ---

/** Comments list; 404 → null (section shows unavailable/empty). */
export async function getComments(
  marketId: string,
  opts: {
    sort?: CommentSort;
    limit?: number;
    cursor?: string | null;
    viewerId?: string | null;
  } = {},
): Promise<CommentsPageDto | null> {
  const q = commentsQuery({
    sort: opts.sort ?? "hot",
    limit: opts.limit ?? 40,
    cursor: opts.cursor,
    viewerId: opts.viewerId,
  });
  const page = await optionalGet<{
    comments: CommentsPageDto["items"];
    next_cursor?: string | null;
  }>(
    `/markets/${encodeURIComponent(marketId)}/comments?${q}`,
    true,
  );
  return page === null
    ? null
    : { items: page.comments, next_cursor: page.next_cursor ?? null };
}

export async function postComment(body: {
  market_id: string;
  user_id: string;
  body: string;
  parent_id?: string | null;
  idempotency_key?: string;
}): Promise<unknown> {
  const marketId = body.market_id;
  return request(
    `/markets/${encodeURIComponent(marketId)}/comments`,
    {
      method: "POST",
      body: JSON.stringify({
        user_id: body.user_id,
        body: body.body,
        parent_id: body.parent_id ?? null,
        idempotency_key: body.idempotency_key ?? newIdempotencyKey(),
      }),
    },
  );
}

export async function voteComment(body: {
  comment_id: string;
  user_id: string;
  value: 1 | -1;
}): Promise<{ score?: number; value?: 1 | -1 }> {
  return request(`/comments/${encodeURIComponent(body.comment_id)}/vote`, {
    method: "POST",
    body: JSON.stringify({
      user_id: body.user_id,
      value: body.value,
      idempotency_key: newIdempotencyKey(),
    }),
  });
}

export async function reportComment(body: {
  comment_id: string;
  user_id: string;
}): Promise<unknown> {
  return request(`/comments/${encodeURIComponent(body.comment_id)}/report`, {
    method: "POST",
    body: JSON.stringify({ user_id: body.user_id }),
  });
}

/** Top holders by committed capital; 404 → null. */
export async function getHolders(
  marketId: string,
  limit = 10,
): Promise<HoldersDto | null> {
  return optionalGet<HoldersDto>(
    `/markets/${encodeURIComponent(marketId)}/holders?limit=${limit}`,
  );
}

export async function getNotifications(
  userId: string,
  opts: { limit?: number; before?: string | null } = {},
): Promise<NotificationsPageDto | null> {
  const params = new URLSearchParams();
  if (opts.limit != null) params.set("limit", String(opts.limit));
  if (opts.before) params.set("before", opts.before);
  const q = params.toString();
  const page = await optionalGet<{
    notifications: NotificationDto[];
    unread_count: number;
  }>(
    `/users/${encodeURIComponent(userId)}/notifications${q ? `?${q}` : ""}`,
    true,
  );
  return page === null
    ? null
    : { items: page.notifications, unread_count: page.unread_count };
}

export async function getUnreadCount(userId: string): Promise<UnreadCountDto | null> {
  return optionalGet<UnreadCountDto>(
    `/users/${encodeURIComponent(userId)}/notifications/unread_count`,
    true,
  );
}

/** Scoped mark-read (plan: POST /users/{id}/notifications/read). */
export async function markNotificationsRead(
  userId: string,
  ids: number[],
): Promise<void> {
  await request(
    `/users/${encodeURIComponent(userId)}/notifications/read`,
    {
      method: "POST",
      body: JSON.stringify({ ids }),
    },
  );
}

// --- PLAN Task 5.1 / 5.3 curation (admin-token; 404/501 → null) ---

function normalizeDraftList(body: unknown): MarketDraftDto[] {
  if (!body) return [];
  if (Array.isArray(body)) return body as MarketDraftDto[];
  const o = body as DraftListDto & { items?: MarketDraftDto[] };
  if (Array.isArray(o.drafts)) return o.drafts;
  if (Array.isArray(o.items)) return o.items;
  return [];
}

/** GET /admin/drafts?status= — 404 → null. */
export async function listDrafts(
  status?: string,
): Promise<MarketDraftDto[] | null> {
  const q = status ? `?status=${encodeURIComponent(status)}` : "";
  const raw = await optionalGet<unknown>(`/admin/drafts${q}`, true, true);
  if (raw === null) return null;
  return normalizeDraftList(raw);
}

export async function createDraft(body: {
  topic: string;
  tier?: string;
  use_llm?: boolean;
  fallback?: boolean;
}): Promise<MarketDraftDto> {
  return request<MarketDraftDto>(
    "/admin/drafts",
    {
      method: "POST",
      body: JSON.stringify(body),
    },
    { admin: true },
  );
}

export async function reviewDraft(
  id: string,
  body: {
    action: "approve" | "reject";
    edits?: {
      question?: string;
      description?: string;
      video_script?: string;
      tier?: string;
      seed_micro?: number;
    };
  },
): Promise<MarketDraftDto> {
  return request<MarketDraftDto>(
    `/admin/drafts/${encodeURIComponent(id)}/review`,
    {
      method: "POST",
      body: JSON.stringify(body),
    },
    { admin: true },
  );
}

export async function publishDraftNow(id: string): Promise<MarketDraftDto> {
  return request<MarketDraftDto>(
    `/admin/drafts/${encodeURIComponent(id)}/publish_now`,
    { method: "POST", body: JSON.stringify({}) },
    { admin: true },
  );
}

/** Schedule / reserved slots — 404 → null. */
export async function getSchedule(): Promise<ScheduleSlotDto[] | null> {
  const raw = await optionalGet<unknown>("/admin/schedule", true, true);
  if (raw === null) return null;
  if (Array.isArray(raw)) return raw as ScheduleSlotDto[];
  const o = raw as { slots?: ScheduleSlotDto[] };
  return o.slots ?? [];
}

export async function getSlotFillMetric(): Promise<SlotFillMetricDto | null> {
  return optionalGet<SlotFillMetricDto>("/admin/metrics/slot_fill", true, true);
}

export async function listSlotUnfilled(): Promise<SlotUnfilledEventDto[] | null> {
  const raw = await optionalGet<unknown>("/admin/slot_unfilled", true, true);
  if (raw === null) return null;
  if (Array.isArray(raw)) return raw as SlotUnfilledEventDto[];
  const o = raw as { events?: SlotUnfilledEventDto[] };
  return o.events ?? [];
}

export async function listVideoJobs(
  marketId?: string,
): Promise<VideoJobDto[] | null> {
  const q = marketId ? `?market_id=${encodeURIComponent(marketId)}` : "";
  const raw = await optionalGet<unknown>(`/admin/video_jobs${q}`, true, true);
  if (raw === null) return null;
  if (Array.isArray(raw)) return raw as VideoJobDto[];
  const o = raw as VideoJobListDto;
  return o.jobs ?? [];
}

// --- Phase 6 ops control plane (admin-token protected) ---

export async function getOpsConfig(): Promise<ConfigSnapshotDto> {
  return request<ConfigSnapshotDto>(
    "/admin/config",
    { method: "GET" },
    { admin: true },
  );
}

export async function setOpsConfig(body: {
  patch: Record<string, unknown>;
  expected_base_generation?: number;
  reason: string;
  idempotency_key: string;
}): Promise<ConfigAppliedDto> {
  return request<ConfigAppliedDto>(
    "/admin/config",
    { method: "POST", body: JSON.stringify(body) },
    { admin: true },
  );
}

export async function createOpsConfigProposal(body: {
  patch: Record<string, unknown>;
  reason: string;
  idempotency_key: string;
}): Promise<ConfigProposalDto> {
  return request<ConfigProposalDto>(
    "/admin/config/proposals",
    { method: "POST", body: JSON.stringify(body) },
    { admin: true },
  );
}

export async function settleOpsConfigProposal(
  proposalId: string,
  action: "confirm" | "reject",
  reason?: string,
): Promise<ConfigProposalDto> {
  return request<ConfigProposalDto>(
    `/admin/config/proposals/${encodeURIComponent(proposalId)}/${action}`,
    {
      method: "POST",
      body: JSON.stringify(reason ? { reason } : {}),
    },
    { admin: true },
  );
}

export async function getOpsAudit(before?: string): Promise<AuditPageDto> {
  const query = before ? `?before=${encodeURIComponent(before)}` : "";
  return request<AuditPageDto>(
    `/admin/audit${query}`,
    { method: "GET" },
    { admin: true },
  );
}

/**
 * Share card image URL for <img src> only (plan: never inline SVG HTML).
 * Uses core absolute URL; 404 degrade is handled by onError on the img.
 */
export function shareCardImgSrc(userId: string, marketId: string): string {
  return `${coreUrl()}${shareCardPath(userId, marketId)}`;
}
