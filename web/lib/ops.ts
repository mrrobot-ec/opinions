import type { ConfigSnapshotDto } from "./types";

const SENSITIVE_EXACT = new Set([
  "trade_fee_bps",
  "min_fee_bps",
  "discount_flip_window_secs",
  "position_cap_micro_by_tier",
  "rep_tier_thresholds_micro",
  "rep_score_min_pot_micro",
  "seed_micro_daily",
  "seed_micro_flash",
  "daily_seed_budget_micro",
  "hidden_window_secs",
  "min_votes_to_resolve_floor",
  "oi_floor_micro",
  "payout_hold_threshold_micro",
  "max_votes_per_window",
  "vote_window_secs",
  "fee_discount_bp_by_tier",
  "remedial_credit_market_cap_micro",
  "remedial_credit_daily_cap_micro",
]);

export function configEntryMap(
  snapshot: ConfigSnapshotDto,
): Map<string, unknown> {
  return new Map(snapshot.entries.map((entry) => [entry.key, entry.value]));
}

export function configValueText(value: unknown): string {
  if (value === true) return "on";
  if (value === false) return "off";
  if (typeof value === "string") return value;
  return JSON.stringify(value);
}

export function isSensitiveConfigKey(key: string): boolean {
  return (
    SENSITIVE_EXACT.has(key) ||
    key.startsWith("integrity_") ||
    key.startsWith("voting_paused:")
  );
}

export interface SwitchRow {
  key: string;
  enabled: boolean;
}

export function switchRows(snapshot: ConfigSnapshotDto): SwitchRow[] {
  return snapshot.entries
    .filter(
      (entry) =>
        entry.key === "trading_paused" ||
        entry.key.startsWith("market_paused:") ||
        entry.key.startsWith("voting_paused:"),
    )
    .map((entry) => ({ key: entry.key, enabled: entry.value === true }))
    .sort((left, right) => left.key.localeCompare(right.key));
}

export function toggleSwitchPatch(
  key: string,
  enabled: boolean,
): Record<string, boolean> {
  return { [key]: !enabled };
}

export function redactTokenDigest(digest: string): string {
  return digest.length <= 12 ? digest : `${digest.slice(0, 12)}…`;
}
