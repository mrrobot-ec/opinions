import { describe, expect, it } from "vitest";
import {
  configEntryMap,
  configValueText,
  isSensitiveConfigKey,
  redactTokenDigest,
  switchRows,
  toggleSwitchPatch,
} from "../ops";
import type { ConfigSnapshotDto } from "../types";

const snapshot: ConfigSnapshotDto = {
  generation: 9,
  entries: [
    { key: "market_paused:market-2", value: true },
    { key: "trade_fee_bps", value: 100 },
    { key: "trading_paused", value: false },
    { key: "voting_paused:market-1", value: true },
    { key: "flash_markets", value: true },
  ],
};

describe("ops config helpers", () => {
  it("maps snapshots and formats scalar and structured values", () => {
    expect(configEntryMap(snapshot).get("trade_fee_bps")).toBe(100);
    expect(configValueText(true)).toBe("on");
    expect(configValueText(false)).toBe("off");
    expect(configValueText("daily")).toBe("daily");
    expect(configValueText({ b: 2, a: 1 })).toBe('{"b":2,"a":1}');
  });

  it("selects only pause switches in deterministic key order", () => {
    expect(switchRows(snapshot)).toEqual([
      { key: "market_paused:market-2", enabled: true },
      { key: "trading_paused", enabled: false },
      { key: "voting_paused:market-1", enabled: true },
    ]);
    expect(toggleSwitchPatch("trading_paused", false)).toEqual({
      trading_paused: true,
    });
  });

  it("classifies two-phase keys and redacts token digests", () => {
    expect(isSensitiveConfigKey("trade_fee_bps")).toBe(true);
    expect(isSensitiveConfigKey("seed_micro_flash")).toBe(true);
    expect(isSensitiveConfigKey("integrity_device_share_max_ppm")).toBe(true);
    expect(isSensitiveConfigKey("voting_paused:market-1")).toBe(true);
    expect(isSensitiveConfigKey("trading_paused")).toBe(false);
    expect(isSensitiveConfigKey("flash_markets")).toBe(false);
    expect(redactTokenDigest("0123456789abcdef")).toBe("0123456789ab…");
    expect(redactTokenDigest("short")).toBe("short");
  });
});
