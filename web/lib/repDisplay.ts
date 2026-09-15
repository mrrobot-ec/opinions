/**
 * Display helpers for rep tier + fee line (no money math — lookup table only).
 * Defaults match docs/copy/scoring.md launch table (base 100 bp).
 */

/** Launch default fee discounts by tier (bp off base). */
export const FEE_DISCOUNT_BP_BY_TIER: readonly number[] = [0, 0, 10, 20, 30];

/** Launch base trade fee in basis points. */
export const BASE_FEE_BPS = 100;

/** Launch minimum fee after discount. */
export const MIN_FEE_BPS = 10;

export function clampTier(tier: number): number {
  if (!Number.isFinite(tier)) return 0;
  return Math.max(0, Math.min(4, Math.floor(tier)));
}

/** Effective fee bps for display: max(base − discount[tier], min). */
export function effectiveFeeBps(
  tier: number,
  baseBps = BASE_FEE_BPS,
  discounts: readonly number[] = FEE_DISCOUNT_BP_BY_TIER,
  minBps = MIN_FEE_BPS,
): number {
  const t = clampTier(tier);
  const disc = discounts[t] ?? 0;
  const raw = baseBps - disc;
  return Math.max(minBps, raw);
}

/** e.g. "Tier 2 · fee 0.90%" */
export function formatTierFeeLine(tier: number, baseBps = BASE_FEE_BPS): string {
  const t = clampTier(tier);
  const fee = effectiveFeeBps(t, baseBps);
  const pct = (fee / 100).toFixed(2);
  return `Tier ${t} · fee ${pct}%`;
}

/** avg_score_bp (0..10000) → "72.5" display for leaderboard. */
export function formatAvgScoreBp(avgScoreBp: number, digits = 1): string {
  if (!Number.isFinite(avgScoreBp)) return "—";
  return (avgScoreBp / 100).toFixed(digits);
}
