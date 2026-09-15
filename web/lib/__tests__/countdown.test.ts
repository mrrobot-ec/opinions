import { describe, expect, it } from "vitest";
import {
  estimatedServerNowMs,
  formatCountdown,
  isTradingFrozen,
  msRemaining,
  parseTimeMs,
  type ServerClock,
} from "../countdown";

describe("countdown math", () => {
  const clock: ServerClock = {
    serverNowMs: Date.parse("2026-06-01T12:00:00.000Z"),
    monoAtReceipt: 1000,
  };

  it("parseTimeMs handles ISO", () => {
    expect(parseTimeMs("2026-06-01T12:00:00.000Z")).toBe(
      Date.parse("2026-06-01T12:00:00.000Z"),
    );
  });

  it("parseTimeMs handles PostgreSQL time wire values in WebKit", () => {
    expect(parseTimeMs("2026-08-13 09:43:53.608904 +00:00:00")).toBe(
      Date.parse("2026-08-13T09:43:53.608Z"),
    );
  });

  it("estimatedServerNow advances with monotonic delta", () => {
    // 5s later on mono
    expect(estimatedServerNowMs(clock, 6000)).toBe(
      Date.parse("2026-06-01T12:00:05.000Z"),
    );
  });

  it("msRemaining uses server clock not wall clock alone", () => {
    const deadline = "2026-06-01T12:01:00.000Z";
    // at receipt: 60s left
    expect(msRemaining(deadline, clock, 1000)).toBe(60_000);
    // 30s mono later: 30s left
    expect(msRemaining(deadline, clock, 31_000)).toBe(30_000);
    // past deadline clamps to 0
    expect(msRemaining(deadline, clock, 1000 + 120_000)).toBe(0);
  });

  it("formatCountdown", () => {
    expect(formatCountdown(0)).toBe("0:00");
    expect(formatCountdown(65_000)).toBe("1:05");
    expect(formatCountdown(3_661_000)).toBe("1:01:01");
  });

  it("isTradingFrozen for closing and tally_hidden boundary", () => {
    expect(isTradingFrozen("closing", null, clock, 1000)).toBe(true);
    expect(isTradingFrozen("live", null, clock, 1000)).toBe(false);
    const hidden = "2026-06-01T12:00:30.000Z";
    expect(isTradingFrozen("live", hidden, clock, 1000)).toBe(false);
    // mono + 30s → at boundary
    expect(isTradingFrozen("live", hidden, clock, 1000 + 30_000)).toBe(true);
    expect(isTradingFrozen("resolved", hidden, clock, 1000)).toBe(false);
    expect(isTradingFrozen("paid", hidden, clock, 1000 + 120_000)).toBe(false);
  });
});
