/**
 * Pure UI helpers for home/detail layout — no money math, no network.
 */

import type { MarketStateDto, MarketSummaryDto } from "./types";

/** Prefer a live hero, then scheduled, then any non-terminal, then first row. */
export function pickHeroMarket(
  markets: MarketSummaryDto[],
): MarketSummaryDto | null {
  if (markets.length === 0) return null;
  const live = markets.find((m) => m.state === "live" || m.state === "closing");
  if (live) return live;
  const scheduled = markets.find((m) => m.state === "scheduled");
  if (scheduled) return scheduled;
  const openish = markets.find(
    (m) => m.state !== "resolved" && m.state !== "paid" && m.state !== "voided",
  );
  return openish ?? markets[0] ?? null;
}

/** Markets for the Live & Upcoming rail (exclude pure draft). */
export function railMarkets(markets: MarketSummaryDto[]): MarketSummaryDto[] {
  return markets.filter((m) => m.state !== "draft");
}

export type ComingSoonSlot = {
  kind: "coming_soon";
  id: string;
  label: string;
  hint: string;
};

/** Pad rail to a designed minimum of empty cadence slots. */
export function padComingSoon(
  liveCount: number,
  minSlots = 3,
): ComingSoonSlot[] {
  const need = Math.max(0, minSlots - liveCount);
  const labels = [
    { label: "Next flash", hint: "Hourly market slot" },
    { label: "Evening open", hint: "Scheduled drop" },
    { label: "Late window", hint: "Cadence slot" },
    { label: "Weekend heat", hint: "Cadence slot" },
  ];
  return Array.from({ length: need }, (_, i) => ({
    kind: "coming_soon" as const,
    id: `soon-${i}`,
    label: labels[i % labels.length]!.label,
    hint: labels[i % labels.length]!.hint,
  }));
}

export function isTerminalState(state: MarketStateDto): boolean {
  return state === "resolved" || state === "paid" || state === "voided";
}

/** Integrity hold / Resolving — terminal-family styling, not a settled market. */
export function isUnderReview(
  market: { state: MarketStateDto; under_review?: boolean } | null | undefined,
): boolean {
  if (!market) return false;
  if (market.under_review === true) return true;
  return market.state === "resolving";
}

/** Integer 0–100 for split bar from YES price micro (display only). */
export function yesPctFromMicro(priceYesMicro: number): number {
  const pct = (priceYesMicro / 1_000_000) * 100;
  if (!Number.isFinite(pct)) return 50;
  return Math.min(100, Math.max(0, Math.round(pct)));
}

export function noPctFromYes(yesPct: number): number {
  return Math.min(100, Math.max(0, 100 - yesPct));
}
