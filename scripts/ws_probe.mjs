#!/usr/bin/env node
/**
 * WS frame probe for Phase 2 live-loop (Task 2.5).
 *
 * Usage:
 *   node scripts/ws_probe.mjs --url ws://127.0.0.1:8080/ws --market <uuid> \
 *     --out /tmp/ws_probe.json --deadline-ms 120000
 *
 * Protocol (adapters/src/http/ws.rs):
 *   client → {"op":"subscribe","market_id":"<uuid>"}
 *   server → snapshot | price | trade | lifecycle | tally  (tagged type, v:1)
 *
 * Writes JSON lines of every applied frame to --out and exits 0 when the
 * ordered lifecycle chain closing→closed→resolved has been seen (after at
 * least one snapshot and one price+trade). Otherwise exits 1 on deadline.
 *
 * No fixed sleeps: pure event-driven wait with an absolute deadline.
 */

import { createWriteStream } from "node:fs";
import { WebSocket } from "ws";

function arg(name, fallback = null) {
  const i = process.argv.indexOf(`--${name}`);
  if (i >= 0 && process.argv[i + 1]) return process.argv[i + 1];
  return fallback;
}

const url = arg("url", process.env.WS_URL || "ws://127.0.0.1:8080/ws");
const marketId = arg("market", process.env.MARKET_ID);
const outPath = arg("out", "/tmp/ws_probe_frames.jsonl");
const deadlineMs = Number(arg("deadline-ms", "120000"));

if (!marketId) {
  console.error("ws_probe: --market <uuid> required");
  process.exit(2);
}

const seen = new Set();
const frames = [];
const states = [];
let gotSnapshot = false;
let gotPrice = false;
let gotTrade = false;
let tallyAfterClosing = 0;
let closedAtMs = null;

const out = createWriteStream(outPath, { flags: "w" });
const started = Date.now();

function record(frame) {
  const line = JSON.stringify({ t_ms: Date.now() - started, frame });
  out.write(line + "\n");
  frames.push({ t_ms: Date.now() - started, frame });
  process.stdout.write(`PROBE ${frame.type}${frame.state ? `:${frame.state}` : ""}\n`);
}

function maybeDone() {
  const hasClosing = states.includes("closing");
  const hasClosed = states.includes("closed");
  const hasResolved = states.includes("resolved");
  if (gotSnapshot && gotPrice && gotTrade && hasClosing && hasClosed && hasResolved) {
    finish(0, "chain complete");
  }
}

function finish(code, reason) {
  const summary = {
    reason,
    gotSnapshot,
    gotPrice,
    gotTrade,
    states,
    tallyAfterClosing,
    closedAtMs,
    frameCount: frames.length,
    ordered: frames.map((f) =>
      f.frame.type === "lifecycle"
        ? `lifecycle:${f.frame.state}`
        : f.frame.type,
    ),
  };
  out.write(JSON.stringify({ summary }) + "\n");
  out.end();
  try {
    ws.close();
  } catch {
    /* ignore */
  }
  console.log("PROBE_SUMMARY " + JSON.stringify(summary));
  process.exit(code);
}

const deadlineTimer = setTimeout(() => {
  finish(1, "deadline exceeded");
}, deadlineMs);

const ws = new WebSocket(url);

ws.on("open", () => {
  ws.send(JSON.stringify({ op: "subscribe", market_id: marketId }));
});

ws.on("message", (data) => {
  let frame;
  try {
    frame = JSON.parse(String(data));
  } catch {
    return;
  }
  if (!frame || typeof frame !== "object" || !frame.type) return;

  // Dedupe outbox frames by (outbox_seq, type)
  if (typeof frame.outbox_seq === "number") {
    const key = `${frame.outbox_seq}:${frame.type}`;
    if (seen.has(key)) return;
    seen.add(key);
  }

  record(frame);

  if (frame.type === "snapshot") {
    gotSnapshot = true;
    if (frame.state) states.push(String(frame.state).toLowerCase());
  } else if (frame.type === "price") {
    gotPrice = true;
  } else if (frame.type === "trade") {
    gotTrade = true;
  } else if (frame.type === "lifecycle") {
    const st = String(frame.state || "").toLowerCase();
    states.push(st);
    if (st === "closed" && closedAtMs == null) {
      closedAtMs = Date.now();
    }
  } else if (frame.type === "tally") {
    if (states.includes("closing") || states.includes("closed") || states.includes("resolved")) {
      tallyAfterClosing += 1;
    }
  }

  maybeDone();
});

ws.on("error", (err) => {
  console.error("ws_probe error:", err.message);
});

ws.on("close", () => {
  // If the chain completed we already exited; otherwise wait for deadline.
});

process.on("SIGINT", () => {
  clearTimeout(deadlineTimer);
  finish(1, "interrupted");
});
