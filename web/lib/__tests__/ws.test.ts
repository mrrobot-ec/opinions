import { afterEach, describe, expect, it, vi } from "vitest";
import {
  dedupeKeyString,
  frameDedupeKey,
  MarketSocket,
  nextBackoffMs,
  shouldApplyFrame,
} from "../ws";
import type { PriceFrame, SnapshotFrame, TradeFrame } from "../types";

const snap = (seq?: number): SnapshotFrame => ({
  type: "snapshot",
  v: 1,
  market_id: "m1",
  server_now: "2026-01-01T00:00:00Z",
  state: "live",
  price_yes_micro: 500_000,
  price_no_micro: 500_000,
  tally: null,
  closes_at: "2026-01-01T01:00:00Z",
  tally_hidden_at: "2026-01-01T00:50:00Z",
  outbox_seq: seq,
});

const price = (seq: number): PriceFrame => ({
  type: "price",
  v: 1,
  outbox_seq: seq,
  market_id: "m1",
  price_yes_micro: 510_000,
  price_no_micro: 490_000,
});

const trade = (seq: number): TradeFrame => ({
  type: "trade",
  v: 1,
  outbox_seq: seq,
  market_id: "m1",
  handle: "alice",
  side: "yes",
  action: "buy",
  collateral_micro: 1_000_000,
  trade_seq: 1,
  created_at: "2026-01-01T00:00:01Z",
});

afterEach(() => vi.unstubAllGlobals());

describe("market subscription protocol", () => {
  it("uses the backend op discriminator so reconnect snapshots are delivered", () => {
    class FakeWebSocket {
      static instance: FakeWebSocket;
      onopen: (() => void) | null = null;
      onmessage: ((event: { data: string }) => void) | null = null;
      onerror: (() => void) | null = null;
      onclose: (() => void) | null = null;
      send = vi.fn();
      close = vi.fn();

      constructor() {
        FakeWebSocket.instance = this;
      }
    }
    vi.stubGlobal("WebSocket", FakeWebSocket);

    const socket = new MarketSocket("ws://example.test/ws", vi.fn());
    socket.subscribe("m1");
    FakeWebSocket.instance.onopen?.();

    expect(FakeWebSocket.instance.send).toHaveBeenCalledWith(
      JSON.stringify({ op: "subscribe", market_id: "m1" }),
    );
    socket.unsubscribe();
  });
});

describe("frame dedupe", () => {
  it("keys by (outbox_seq, type)", () => {
    expect(frameDedupeKey(price(7))).toEqual({ outbox_seq: 7, type: "price" });
    expect(dedupeKeyString({ outbox_seq: 7, type: "price" })).toBe("7:price");
    // same seq different type is distinct
    expect(dedupeKeyString(frameDedupeKey(trade(7))!)).toBe("7:trade");
  });

  it("drops duplicate (seq, type) but keeps different types", () => {
    const seen = new Set<string>();
    expect(shouldApplyFrame(seen, price(1))).toBe(true);
    expect(shouldApplyFrame(seen, price(1))).toBe(false);
    expect(shouldApplyFrame(seen, trade(1))).toBe(true);
    expect(shouldApplyFrame(seen, trade(1))).toBe(false);
  });

  it("always applies snapshot with skipSnapshotDedupe", () => {
    const seen = new Set<string>();
    expect(shouldApplyFrame(seen, snap(0), { skipSnapshotDedupe: true })).toBe(true);
    expect(shouldApplyFrame(seen, snap(0), { skipSnapshotDedupe: true })).toBe(true);
  });
});

describe("reconnect backoff", () => {
  it("caps at 15s", () => {
    for (let i = 0; i < 20; i++) {
      expect(nextBackoffMs(i)).toBeLessThanOrEqual(15_000);
      expect(nextBackoffMs(i)).toBeGreaterThan(0);
    }
  });

  it("grows with attempt before cap", () => {
    // without randomness hard to assert exact, but low attempts stay small
    const a0 = nextBackoffMs(0);
    expect(a0).toBeLessThanOrEqual(700);
    expect(a0).toBeGreaterThanOrEqual(400);
  });
});
