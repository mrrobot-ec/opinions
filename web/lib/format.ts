/**
 * Display formatting only — no money math.
 * micro units: 1_000_000 micro = $1.00 = 100¢.
 */

const MICRO_PER_DOLLAR = 1_000_000;
const MICRO_PER_CENT = 10_000;

/** micro → dollars as fixed-point number (for display pipelines). */
export function microToDollars(micro: number): number {
  return micro / MICRO_PER_DOLLAR;
}

/** micro → cents (price in ¢). Rounds half away from zero via integer div bias. */
export function microToCents(micro: number): number {
  return Math.round(micro / MICRO_PER_CENT);
}

/** Format micro as `$X.YY` (always 2 decimal places, sign preserved). */
export function formatDollars(micro: number): string {
  const sign = micro < 0 ? "-" : "";
  const abs = Math.abs(micro);
  const centsTotal = Math.round(abs / 10_000);
  const w = Math.floor(centsTotal / 100);
  const c = centsTotal % 100;
  return `${sign}$${w}.${c.toString().padStart(2, "0")}`;
}

/** Format micro-shares as a fixed-point quantity without a currency symbol. */
export function formatShares(micro: number): string {
  const sign = micro < 0 ? "-" : "";
  const abs = Math.abs(micro);
  const hundredths = Math.round(abs / 10_000);
  const whole = Math.floor(hundredths / 100);
  const fraction = hundredths % 100;
  return `${sign}${whole}.${fraction.toString().padStart(2, "0")}`;
}

/** Format price micro as cents string e.g. `42¢` or `42.5¢` when fractional. */
export function formatCentsPrice(micro: number): string {
  const cents = micro / MICRO_PER_CENT;
  if (Number.isInteger(cents)) return `${cents}¢`;
  const rounded = Math.round(cents * 10) / 10;
  return `${rounded}¢`;
}

/** Percent of YES price: micro/1e6 * 100. */
export function formatYesPct(priceYesMicro: number, digits = 1): string {
  const pct = (priceYesMicro / MICRO_PER_DOLLAR) * 100;
  return `${pct.toFixed(digits)}%`;
}

/** Compact pool label for browse cards (display only). */
export function formatPoolCompact(micro: number | null | undefined): string {
  if (micro == null || !Number.isFinite(micro)) return "—";
  const dollars = Math.abs(micro) / MICRO_PER_DOLLAR;
  if (dollars >= 1_000_000) {
    return `$${(dollars / 1_000_000).toFixed(1)}M`;
  }
  if (dollars >= 1_000) {
    return `$${(dollars / 1_000).toFixed(1)}K`;
  }
  return formatDollars(micro);
}

/** Basis points (0–10000) → display percent. */
export function formatBps(bps: number, digits = 1): string {
  return `${(bps / 100).toFixed(digits)}%`;
}

/** Dollars input string/number → micro integer (UI chip helper only). */
export function dollarsToMicro(dollars: number): number {
  return Math.round(dollars * MICRO_PER_DOLLAR);
}

/** Signed PnL with + prefix when positive. */
export function formatPnl(micro: number): string {
  const base = formatDollars(micro);
  if (micro > 0) return `+${base}`;
  return base;
}
