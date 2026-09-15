/**
 * Hand-mirrored DTOs from openapi.json (+ Phase 2 plan frame/query contracts).
 * Each type cites its source path. Chart/tape/snapshot fields from plan Task 2.0/2.3
 * are not yet in openapi (built in parallel) — marked PLAN: where applicable.
 */

// openapi.json#/components/schemas/SideDto
export type SideDto = "yes" | "no";

// openapi.json#/components/schemas/TradeActionDto
export type TradeActionDto = "buy" | "sell";

// openapi.json#/components/schemas/MarketStateDto
export type MarketStateDto =
  | "draft"
  | "scheduled"
  | "live"
  | "closing"
  | "closed"
  | "resolving"
  | "resolved"
  | "paid"
  | "voided";

// openapi.json#/components/schemas/ApiError
export interface ApiError {
  code: string;
  message: string;
}

// openapi.json#/components/schemas/ConfigEntryDto
export interface ConfigEntryDto {
  key: string;
  value: unknown;
}

// openapi.json#/components/schemas/ConfigSnapshotDto
export interface ConfigSnapshotDto {
  generation: number;
  entries: ConfigEntryDto[];
}

export interface ConfigAppliedDto {
  generation: number;
  changed_keys: string[];
}

export interface ConfigProposalDto {
  id: string;
  status: string;
  base_generation: number;
  patch: Record<string, unknown>;
  reason: string;
  expires_at: string;
  resulting_generation: number | null;
}

export interface AuditActionDto {
  id: string;
  actor_role: string;
  actor_token_digest: string;
  action: string;
  subject: string;
  reason: string | null;
  at: string;
}

export interface AuditPageDto {
  actions: AuditActionDto[];
  next_before: string | null;
}

// openapi.json#/components/schemas/MarketSummaryDto
// PLAN phase3: under_review on detail/snapshot
export interface MarketSummaryDto {
  id: string;
  slug: string;
  state: MarketStateDto;
  yes_outcome_id: string;
  no_outcome_id: string;
  price_yes_micro: number;
  price_no_micro: number;
  /** Present once REST detail enrichment lands; optional for current summary. */
  question?: string;
  closes_at?: string;
  tally_hidden_at?: string;
  server_now?: string;
  /** final vote share bps when resolved (plan post-resolve UX). */
  final_vote_bps?: number | null;
  redemption_yes_micro?: number | null;
  redemption_no_micro?: number | null;
  void_reason?: string | null;
  /** Optional browse meta (when core exposes them). */
  pool_micro?: number | null;
  votes?: number | null;
  /** PLAN Task 3.2 — integrity hold (state Resolving or explicit flag). */
  under_review?: boolean;
  /** PLAN Task 5.2 — hot-swap media refs (null → branded placeholder). */
  poster_asset_url?: string | null;
  video_asset_url?: string | null;
  /** Optional flash vs daily tier label when core exposes it. */
  content_tier?: "daily" | "flash" | string | null;
}

// PLAN phase3-economy-integrity.md Task 3.3 VoterRow
export interface VoterLeaderboardRow {
  handle: string;
  avg_score_bp: number;
  markets_scored: number;
  tier: number;
  rep_micro?: number;
}

// PLAN phase3-economy-integrity.md Task 3.3 TraderRow
export interface TraderLeaderboardRow {
  handle: string;
  realized_pnl_micro: number;
  realizations: number;
}

export interface LeaderboardSnapshot {
  period: "weekly";
  top_voters: VoterLeaderboardRow[];
  top_traders: TraderLeaderboardRow[];
  available: boolean;
  empty_reason: string;
}

// openapi.json#/components/schemas/CastVoteRequest
export interface CastVoteRequest {
  user_id: string;
  market_ref: string;
  side: SideDto;
  crowd_guess_pct: number;
  idempotency_key: string;
  run_id?: string | null;
}

// openapi.json#/components/schemas/VoteReceiptDto
// Plan Task 2.2: seq is Option during tally-hidden window; openapi still marks required.
export interface VoteReceiptDto {
  vote_id: string;
  market_id: string;
  seq: number | null;
  side: SideDto;
  crowd_guess_pct: number;
  replayed: boolean;
}

// openapi.json#/components/schemas/PreviewTradeRequest
export interface PreviewTradeRequest {
  user_id: string;
  market_ref: string;
  side: SideDto;
  action: TradeActionDto;
  amount_micro: number;
}

// openapi.json#/components/schemas/TradePreviewDto
export interface TradePreviewDto {
  market_id: string;
  config_version: number;
  side: SideDto;
  action: TradeActionDto;
  shares_micro: number;
  gross_micro: number;
  fee_micro: number;
  avg_price_micro: number;
}

// openapi.json#/components/schemas/PlaceTradeRequest
export interface PlaceTradeRequest {
  user_id: string;
  market_ref: string;
  side: SideDto;
  action: TradeActionDto;
  amount_micro: number;
  expected_config_version: number;
  idempotency_key: string;
  pending_action_id?: string | null;
  run_id?: string | null;
}

// openapi.json#/components/schemas/TradeReceiptDto
export interface TradeReceiptDto {
  trade_id: string;
  ledger_txn: string;
  side: SideDto;
  action: TradeActionDto;
  shares_micro: number;
  gross_micro: number;
  fee_micro: number;
  avg_price_micro: number;
  replayed: boolean;
}

// openapi.json#/components/schemas/PositionDto (+ PLAN Task 3.1 rep fields)
export interface PositionDto {
  market_id: string;
  outcome_id: string;
  side: SideDto;
  shares_micro: number;
  cost_micro: number;
  realized_pnl_micro: number;
  /** PLAN Task 3.1 — same for all rows of a user when present. */
  rep_micro?: number;
  tier?: number;
}

// PLAN Task 3.1 / 4.3 profile DTO (404-degrade if missing)
export interface UserProfileDto {
  user_id: string;
  handle?: string;
  rep_micro: number;
  tier: number;
  created_at?: string;
  voter?: {
    avg_score_bp?: number | null;
    markets_scored: number;
  };
  /** PLAN 4.3 — voter stats from resolved markets. */
  avg_score_bp?: number | null;
  markets_scored?: number | null;
  realized_pnl_micro?: number | null;
  recent_trades?: ProfileTradeRow[];
  /** Side/score only when market resolved (privacy rule). */
  recent_votes?: ProfileVoteRow[];
}

export interface ProfileTradeRow {
  market_id: string;
  market_slug?: string;
  market_ref?: string;
  side: SideDto;
  action: TradeActionDto;
  collateral_micro: number;
  created_at: string;
  trade_seq?: number;
}

export interface ProfileVoteRow {
  market_id: string;
  market_slug?: string;
  market_question?: string;
  cast_at: string;
  /** Null/omitted pre-resolution. */
  side?: SideDto | null;
  score_bp?: number | null;
}

// openapi.json#/components/schemas/UserIdDto
export interface UserIdDto {
  user_id: string;
}

// openapi.json#/components/schemas/AdvanceMarketRequest
export interface AdvanceMarketRequest {
  event: string;
}

// openapi.json#/components/schemas/MarketAdvancedDto
export interface MarketAdvancedDto {
  market_id: string;
  state: MarketStateDto;
  from_state: MarketStateDto;
}

// openapi.json#/components/schemas/ResolveMarketDto
export interface ResolveMarketDto {
  market_id: string;
  state: MarketStateDto;
  actual_yes_bps: number;
  voided: boolean;
  ledger_txn?: string | null;
  replayed: boolean;
}

// --- PLAN: Task 2.3 chart + tape (not yet in openapi.json) ---

// PLAN phase2-core-loop.md Task 2.3 PricePoint
export interface PricePoint {
  bucket_start: string;
  avg_price_micro: number;
  volume_micro: number;
  trades: number;
}

// PLAN phase2-core-loop.md Task 2.3 TapeRow
export interface TapeRow {
  handle: string;
  side: SideDto;
  action: TradeActionDto;
  collateral_micro: number;
  created_at: string;
  seq: number;
}

// --- PLAN: Task 2.0 WebSocket frames ---

export interface TallyPair {
  yes: number;
  no: number;
}

// PLAN Task 2.0 snapshot frame (+ PLAN 3.2 under_review)
export interface SnapshotFrame {
  type: "snapshot";
  v: 1;
  market_id: string;
  server_now: string;
  state: MarketStateDto;
  price_yes_micro: number;
  price_no_micro: number;
  tally: TallyPair | null;
  closes_at: string;
  tally_hidden_at: string;
  /** outbox_seq may be 0 for pure snapshot. */
  outbox_seq?: number;
  final_vote_bps?: number | null;
  redemption_yes_micro?: number | null;
  redemption_no_micro?: number | null;
  under_review?: boolean;
  poster_asset_url?: string | null;
  video_asset_url?: string | null;
}

// PLAN Task 2.0 price frame
export interface PriceFrame {
  type: "price";
  v: 1;
  outbox_seq: number;
  market_id: string;
  price_yes_micro: number;
  price_no_micro: number;
}

// PLAN Task 2.0 trade frame
export interface TradeFrame {
  type: "trade";
  v: 1;
  outbox_seq: number;
  market_id: string;
  handle: string;
  side: SideDto;
  action: TradeActionDto;
  collateral_micro: number;
  trade_seq: number;
  created_at: string;
}

// PLAN Task 2.0 lifecycle frame
export interface LifecycleFrame {
  type: "lifecycle";
  v: 1;
  outbox_seq: number;
  market_id: string;
  state: MarketStateDto;
  final_vote_bps?: number | null;
  redemption_yes_micro?: number | null;
  redemption_no_micro?: number | null;
}

// PLAN Task 2.0 tally frame
export interface TallyFrame {
  type: "tally";
  v: 1;
  outbox_seq: number;
  market_id: string;
  tally: TallyPair;
}

// --- PLAN: Task 4.x social DTOs + user notification frames ---

export type ModerationStatus = "visible" | "shadow" | "blocked";

export interface CommentDto {
  id: string;
  market_id: string;
  author_id: string;
  author_handle: string;
  parent_id: string | null;
  body: string;
  score: number;
  depth: number;
  reply_count: number;
  moderation_status: ModerationStatus;
  created_at: string;
  /** Viewer's one-shot vote if present. */
  viewer_vote?: 1 | -1 | null;
  /** Hot score when sort=hot (optional). */
  hot_score?: number;
}

export interface CommentsPageDto {
  items: CommentDto[];
  /** Opaque cursor for next page; hot includes as_of. */
  next_cursor?: string | null;
  /** Echoed as_of for hot first page (when provided by server). */
  as_of?: string | null;
}

export interface HolderRowDto {
  user_id?: string;
  handle: string;
  cost_micro: number;
  tier?: number;
}

export interface HoldersDto {
  yes: HolderRowDto[];
  no: HolderRowDto[];
  /** Product label: by committed capital */
  ranking?: string;
}

export type NotificationType =
  | "resolution_trade"
  | "resolution_vote"
  | "resolution_void"
  | "comment_reply"
  | "mention"
  | "rep_tier_change"
  | "curator_needed"
  | string;

export interface NotificationDto {
  id: number;
  /** Present on locally constructed WebSocket rows; omitted by the REST DTO. */
  user_id?: string;
  type: NotificationType;
  market_id?: string | null;
  payload: Record<string, unknown>;
  read_at: string | null;
  created_at: string;
  source_seq?: number | null;
}

export interface NotificationsPageDto {
  items: NotificationDto[];
  unread_count: number;
}

export interface UnreadCountDto {
  unread_count: number;
}

/** PLAN 4.2 — user notif frame (rows exactly-once; frames best-effort). */
export interface NotifFrame {
  type: "notif";
  v: 1;
  id: number;
  source_seq: number | null;
  notif_type: string;
  /** Alias some servers may use */
  notifType?: string;
  payload: Record<string, unknown>;
  market_id?: string | null;
  created_at?: string;
}

export interface NotifSnapshotFrame {
  type: "notif_snapshot";
  v?: 1;
  unread_count: number;
}

export type UserServerFrame = NotifFrame | NotifSnapshotFrame;

/** PLAN Task 5.2 — versioned asset hot-swap frame. */
export interface AssetFrame {
  type: "asset";
  v: 1;
  outbox_seq: number;
  market_id: string;
  kind: string;
  url: string;
}

export type ServerFrame =
  | SnapshotFrame
  | PriceFrame
  | TradeFrame
  | LifecycleFrame
  | TallyFrame
  | NotifFrame
  | NotifSnapshotFrame
  | AssetFrame;

// --- PLAN Task 5.1 / 5.2 / 5.3 content curation DTOs ---

export type DraftStatus =
  | "pending"
  | "approved"
  | "rejected"
  | "published"
  | "expired"
  | string;

export type DraftTier = "daily" | "flash" | string;
export type DraftSource = "template" | "llm" | string;

export interface MarketDraftDto {
  id: string;
  question: string;
  description?: string;
  video_script?: string;
  source: DraftSource;
  fallback_from?: string | null;
  status: DraftStatus;
  tier: DraftTier;
  seed_micro?: number;
  fee_bps?: number;
  min_votes_to_resolve?: number;
  open_secs?: number;
  hidden_window_secs?: number;
  publish_at?: string | null;
  published_market_id?: string | null;
  publish_stage?: string | null;
  expires_at?: string | null;
  created_at?: string;
  updated_at?: string;
}

export interface DraftListDto {
  drafts: MarketDraftDto[];
}

export interface SlotFillMetricDto {
  filled: number;
  unfilled: number;
  /** Optional precomputed 0..1 */
  fill_rate?: number | null;
  window_label?: string;
}

export interface SlotUnfilledEventDto {
  id?: string;
  tier: string;
  slot_ts: string;
  created_at?: string;
}

export interface ScheduleSlotDto {
  tier: string;
  publish_at: string;
  draft_id?: string | null;
  question?: string | null;
  status?: string;
  conflict?: boolean;
}

export interface VideoJobDto {
  id: string;
  market_id?: string | null;
  draft_id?: string | null;
  kind: string;
  status: string;
  attempts?: number;
  error?: string | null;
  updated_at?: string;
}

export interface VideoJobListDto {
  jobs: VideoJobDto[];
}

export interface ClientSubscribe {
  op: "subscribe";
  market_id: string;
}

export interface ClientUnsubscribe {
  op: "unsubscribe";
  market_id: string;
}

/** PLAN 4.2 — subscribe_user replaces single user subscription after token check. */
export interface ClientSubscribeUser {
  op: "subscribe_user";
  user_id: string;
  token: string;
}
