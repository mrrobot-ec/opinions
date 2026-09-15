/**
 * Countdown math: server deadline + server_now + client monotonic delta.
 * Never uses client wall clock as the sole authority (plan Task 2.0 / 2.4).
 */

export interface ServerClock {
  /** ISO or epoch-ms parseable server_now from snapshot. */
  serverNowMs: number;
  /** performance.now() when server_now was received. */
  monoAtReceipt: number;
}

export function parseTimeMs(isoOrMs: string | number): number {
  if (typeof isoOrMs === "number") return isoOrMs;
  const postgres = isoOrMs.match(
    /^(\d{4}-\d{2}-\d{2}) (\d{2}:\d{2}:\d{2})(?:\.(\d+))? ([+-]\d{2}:\d{2})(?::\d{2})?$/,
  );
  const normalized = postgres
    ? `${postgres[1]}T${postgres[2]}.${(postgres[3] ?? "0").padEnd(3, "0").slice(0, 3)}${postgres[4]}`
    : isoOrMs;
  const t = Date.parse(normalized);
  if (Number.isNaN(t)) return 0;
  return t;
}

/** Estimated server time now using monotonic delta. */
export function estimatedServerNowMs(clock: ServerClock, monoNow = performance.now()): number {
  const delta = monoNow - clock.monoAtReceipt;
  return clock.serverNowMs + delta;
}

/** Milliseconds remaining until deadline (ISO or ms), clamped ≥ 0. */
export function msRemaining(
  deadline: string | number,
  clock: ServerClock,
  monoNow = performance.now(),
): number {
  const end = parseTimeMs(deadline);
  const now = estimatedServerNowMs(clock, monoNow);
  return Math.max(0, end - now);
}

export function formatCountdown(ms: number): string {
  if (ms <= 0) return "0:00";
  const totalSec = Math.floor(ms / 1000);
  const h = Math.floor(totalSec / 3600);
  const m = Math.floor((totalSec % 3600) / 60);
  const s = totalSec % 60;
  if (h > 0) {
    return `${h}:${m.toString().padStart(2, "0")}:${s.toString().padStart(2, "0")}`;
  }
  return `${m}:${s.toString().padStart(2, "0")}`;
}

/** Frozen-window: trading paused when state is closing OR now >= tally_hidden_at. */
export function isTradingFrozen(
  state: string,
  tallyHiddenAt: string | number | undefined | null,
  clock: ServerClock | null,
  monoNow = performance.now(),
): boolean {
  if (state === "closing") return true;
  if (state === "resolved" || state === "paid" || state === "voided") return false;
  if (!tallyHiddenAt || !clock) return false;
  return estimatedServerNowMs(clock, monoNow) >= parseTimeMs(tallyHiddenAt);
}
