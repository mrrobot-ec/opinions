/**
 * WebSocket client — plan Task 2.0 / 2.4.
 * Protocol: subscribe → snapshot first; price/trade/lifecycle/tally frames;
 * dedupe by (outbox_seq, type); reconnect with capped backoff → resubscribe → snapshot rehydration.
 */

import type {
  ClientSubscribe,
  ClientSubscribeUser,
  ServerFrame,
  UserServerFrame,
} from "./types";
import { shouldApplyNotif } from "./social";

export type FrameHandler = (frame: ServerFrame) => void;
export type StatusHandler = (status: "connecting" | "open" | "closed" | "error") => void;

export interface DedupeKey {
  outbox_seq: number;
  type: string;
}

/** Dedupe key for at-least-once frames (plan Task 2.0). */
export function frameDedupeKey(frame: ServerFrame): DedupeKey | null {
  if (frame.type === "snapshot") {
    const seq = frame.outbox_seq ?? 0;
    return { outbox_seq: seq, type: "snapshot" };
  }
  if (frame.type === "asset" && typeof frame.outbox_seq === "number") {
    return { outbox_seq: frame.outbox_seq, type: "asset" };
  }
  if ("outbox_seq" in frame && typeof frame.outbox_seq === "number") {
    return { outbox_seq: frame.outbox_seq, type: frame.type };
  }
  return null;
}

export function dedupeKeyString(k: DedupeKey): string {
  return `${k.outbox_seq}:${k.type}`;
}

/** Pure helper: return true if this frame should be applied (not seen before). */
export function shouldApplyFrame(
  seen: Set<string>,
  frame: ServerFrame,
  options: { skipSnapshotDedupe?: boolean } = {},
): boolean {
  if (frame.type === "snapshot" && options.skipSnapshotDedupe) {
    // Snapshots rehydrate state; always apply, but still record key if present.
    const key = frameDedupeKey(frame);
    if (key) seen.add(dedupeKeyString(key));
    return true;
  }
  const key = frameDedupeKey(frame);
  if (!key) return true;
  const s = dedupeKeyString(key);
  if (seen.has(s)) return false;
  seen.add(s);
  // Cap memory: keep last ~2000 keys
  if (seen.size > 2000) {
    const first = seen.values().next().value;
    if (first !== undefined) seen.delete(first);
  }
  return true;
}

const MAX_BACKOFF_MS = 15_000;
const BASE_BACKOFF_MS = 500;

export function nextBackoffMs(attempt: number): number {
  const exp = Math.min(MAX_BACKOFF_MS, BASE_BACKOFF_MS * 2 ** Math.max(0, attempt));
  // jitter ±20%
  const jitter = exp * (0.8 + Math.random() * 0.4);
  return Math.min(MAX_BACKOFF_MS, Math.round(jitter));
}

export class MarketSocket {
  private url: string;
  private ws: WebSocket | null = null;
  private marketId: string | null = null;
  private seen = new Set<string>();
  private attempt = 0;
  private closedByUser = false;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private onFrame: FrameHandler;
  private onStatus: StatusHandler;

  constructor(
    url: string,
    onFrame: FrameHandler,
    onStatus: StatusHandler = () => {},
  ) {
    this.url = url;
    this.onFrame = onFrame;
    this.onStatus = onStatus;
  }

  subscribe(marketId: string): void {
    this.marketId = marketId;
    this.closedByUser = false;
    this.connect();
  }

  unsubscribe(): void {
    this.closedByUser = true;
    this.marketId = null;
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.ws) {
      try {
        this.ws.close();
      } catch {
        /* ignore */
      }
      this.ws = null;
    }
    this.onStatus("closed");
  }

  /** Clear dedupe set (e.g. after intentional resubscribe full rehydrate). */
  clearDedupe(): void {
    this.seen.clear();
  }

  private connect(): void {
    if (typeof WebSocket === "undefined") {
      this.onStatus("error");
      return;
    }
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    this.onStatus("connecting");
    try {
      this.ws = new WebSocket(this.url);
    } catch {
      this.scheduleReconnect();
      return;
    }

    this.ws.onopen = () => {
      this.attempt = 0;
      this.onStatus("open");
      if (this.marketId) {
        // Clear dedupe on fresh connection so snapshot always rehydrates cleanly
        // but still dedupe subsequent outbox frames within this connection.
        this.seen.clear();
        const msg: ClientSubscribe = { op: "subscribe", market_id: this.marketId };
        this.ws?.send(JSON.stringify(msg));
      }
    };

    this.ws.onmessage = (ev) => {
      let frame: ServerFrame;
      try {
        frame = JSON.parse(String(ev.data)) as ServerFrame;
      } catch {
        return;
      }
      if (!frame || typeof frame !== "object" || !("type" in frame)) return;
      if (!shouldApplyFrame(this.seen, frame, { skipSnapshotDedupe: true })) return;
      this.onFrame(frame);
    };

    this.ws.onerror = () => {
      this.onStatus("error");
    };

    this.ws.onclose = () => {
      this.ws = null;
      this.onStatus("closed");
      if (!this.closedByUser && this.marketId) {
        this.scheduleReconnect();
      }
    };
  }

  private scheduleReconnect(): void {
    if (this.closedByUser) return;
    const delay = nextBackoffMs(this.attempt);
    this.attempt += 1;
    this.reconnectTimer = setTimeout(() => this.connect(), delay);
  }
}

export function defaultWsUrl(): string {
  if (typeof process !== "undefined" && process.env.NEXT_PUBLIC_WS_URL) {
    return process.env.NEXT_PUBLIC_WS_URL;
  }
  if (typeof window !== "undefined") {
    const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
    return `${proto}//${window.location.hostname}:8080/ws`;
  }
  return "ws://127.0.0.1:8080/ws";
}

export type UserFrameHandler = (frame: UserServerFrame) => void;

/**
 * User notification channel — PLAN Task 4.2/4.4.
 * subscribe_user replaces a single subscription after token check;
 * frames deduped by (id, source_seq).
 */
export class UserNotifSocket {
  private url: string;
  private ws: WebSocket | null = null;
  private userId: string | null = null;
  private token: string | null = null;
  private seen = new Set<string>();
  private attempt = 0;
  private closedByUser = false;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private onFrame: UserFrameHandler;
  private onStatus: StatusHandler;

  constructor(
    url: string,
    onFrame: UserFrameHandler,
    onStatus: StatusHandler = () => {},
  ) {
    this.url = url;
    this.onFrame = onFrame;
    this.onStatus = onStatus;
  }

  subscribeUser(userId: string, token: string): void {
    this.userId = userId;
    this.token = token;
    this.closedByUser = false;
    this.connect();
  }

  unsubscribe(): void {
    this.closedByUser = true;
    this.userId = null;
    this.token = null;
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.ws) {
      try {
        this.ws.close();
      } catch {
        /* ignore */
      }
      this.ws = null;
    }
    this.onStatus("closed");
  }

  clearDedupe(): void {
    this.seen.clear();
  }

  private connect(): void {
    if (typeof WebSocket === "undefined") {
      this.onStatus("error");
      return;
    }
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    this.onStatus("connecting");
    try {
      this.ws = new WebSocket(this.url);
    } catch {
      this.scheduleReconnect();
      return;
    }

    this.ws.onopen = () => {
      this.attempt = 0;
      this.onStatus("open");
      if (this.userId && this.token) {
        this.seen.clear();
        const msg: ClientSubscribeUser = {
          op: "subscribe_user",
          user_id: this.userId,
          token: this.token,
        };
        this.ws?.send(JSON.stringify(msg));
      }
    };

    this.ws.onmessage = (ev) => {
      let frame: UserServerFrame;
      try {
        frame = JSON.parse(String(ev.data)) as UserServerFrame;
      } catch {
        return;
      }
      if (!frame || typeof frame !== "object" || !("type" in frame)) return;
      if (frame.type === "notif_snapshot") {
        this.onFrame(frame);
        return;
      }
      if (frame.type === "notif") {
        if (!shouldApplyNotif(this.seen, frame.id, frame.source_seq)) return;
        this.onFrame(frame);
      }
    };

    this.ws.onerror = () => {
      this.onStatus("error");
    };

    this.ws.onclose = () => {
      this.ws = null;
      this.onStatus("closed");
      if (!this.closedByUser && this.userId) {
        this.scheduleReconnect();
      }
    };
  }

  private scheduleReconnect(): void {
    if (this.closedByUser) return;
    const delay = nextBackoffMs(this.attempt);
    this.attempt += 1;
    this.reconnectTimer = setTimeout(() => this.connect(), delay);
  }
}

/** Exported for tests: apply user notif frame through dedupe set. */
export function applyUserNotifFrame(
  seen: Set<string>,
  frame: UserServerFrame,
): boolean {
  if (frame.type === "notif_snapshot") return true;
  if (frame.type === "notif") {
    return shouldApplyNotif(seen, frame.id, frame.source_seq);
  }
  return false;
}
