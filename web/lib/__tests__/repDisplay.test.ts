import { describe, expect, it } from "vitest";
import {
  clampTier,
  effectiveFeeBps,
  formatAvgScoreBp,
  formatTierFeeLine,
} from "../repDisplay";

describe("repDisplay", () => {
  it("clamps tier 0..4", () => {
    expect(clampTier(-1)).toBe(0);
    expect(clampTier(2.9)).toBe(2);
    expect(clampTier(99)).toBe(4);
  });

  it("effective fee matches scoring.md launch table", () => {
    expect(effectiveFeeBps(0)).toBe(100);
    expect(effectiveFeeBps(1)).toBe(100);
    expect(effectiveFeeBps(2)).toBe(90);
    expect(effectiveFeeBps(3)).toBe(80);
    expect(effectiveFeeBps(4)).toBe(70);
  });

  it("formatTierFeeLine", () => {
    expect(formatTierFeeLine(2)).toBe("Tier 2 · fee 0.90%");
    expect(formatTierFeeLine(0)).toBe("Tier 0 · fee 1.00%");
  });

  it("formatAvgScoreBp", () => {
    expect(formatAvgScoreBp(7250)).toBe("72.5");
    expect(formatAvgScoreBp(10000)).toBe("100.0");
  });
});
