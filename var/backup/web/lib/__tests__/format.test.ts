import { describe, expect, it } from "vitest";
import {
  dollarsToMicro,
  formatBps,
  formatCentsPrice,
  formatDollars,
  formatPnl,
  formatPoolCompact,
  formatYesPct,
  microToCents,
  microToDollars,
} from "../format";

describe("micro formatting", () => {
  it("microToDollars divides by 1e6", () => {
    expect(microToDollars(1_000_000)).toBe(1);
    expect(microToDollars(500_000)).toBe(0.5);
    expect(microToDollars(0)).toBe(0);
  });

  it("microToCents maps 1¢ = 10_000 micro", () => {
    expect(microToCents(420_000)).toBe(42);
    expect(microToCents(1_000_000)).toBe(100);
    expect(microToCents(0)).toBe(0);
  });

  it("formatDollars always two decimals", () => {
    expect(formatDollars(1_000_000)).toBe("$1.00");
    expect(formatDollars(5_000_000)).toBe("$5.00");
    expect(formatDollars(123_456)).toBe("$0.12");
    expect(formatDollars(-2_500_000)).toBe("-$2.50");
  });

  it("formatCentsPrice for YES/NO headers", () => {
    expect(formatCentsPrice(420_000)).toBe("42¢");
    expect(formatCentsPrice(500_000)).toBe("50¢");
    expect(formatCentsPrice(425_000)).toBe("42.5¢");
  });

  it("formatYesPct and formatBps", () => {
    expect(formatYesPct(500_000)).toBe("50.0%");
    expect(formatBps(7250)).toBe("72.5%");
  });

  it("dollarsToMicro and formatPnl", () => {
    expect(dollarsToMicro(5)).toBe(5_000_000);
    expect(formatPnl(1_000_000)).toBe("+$1.00");
    expect(formatPnl(-500_000)).toBe("-$0.50");
    expect(formatPnl(0)).toBe("$0.00");
  });

  it("formatPoolCompact for browse meta", () => {
    expect(formatPoolCompact(null)).toBe("—");
    expect(formatPoolCompact(undefined)).toBe("—");
    expect(formatPoolCompact(127_800_000_000)).toBe("$127.8K");
    expect(formatPoolCompact(1_000_000)).toBe("$1.00");
  });
});
