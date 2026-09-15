#!/usr/bin/env node
/**
 * WS user-channel probe for Phase 4 social e2e.
 *
 * Usage:
 *   node scripts/ws_user_probe.mjs --url ws://127.0.0.1:8080/ws \
 *     --user <uuid> --token demo-token --out /tmp/ws_user.jsonl \
 *     --deadline-ms 120000 [--expect-notif-types comment_reply]
 *
 * Protocol: client → {"op":"subscribe_user","user_id":"...","token":"..."}
 * Server → notif_snapshot | notif
 *
 * Exits 0 after deadline or when --min-notifs reached (if set).
 * Always writes frames to --out. Default: run until deadline.
 */

import { createWriteStream } from "node:fs";
import { WebSocket } from "ws";

function arg(name, fallback = null) {
  const i = process.argv.indexOf(`--${name}`);
  if (i >= 0 && process.argv[i + 1]) return process.argv[i + 1];
  return fallback;
}

const url = arg("url", process.env.WS_URL || "ws://127.0.0.1:8080/ws");
const userId = arg("user", process.env.USER_ID);
const token = arg("token", process.env.DEMO_TOKEN || "demo-token");
const outPath = arg("out", "/tmp/ws_user_probe.jsonl");
const deadlineMs = Number(arg("deadline-ms", "60000"));
const minNotifs = Number(arg("min-notifs", "0"));
const expectTypes = (arg("expect-notif-types", "") || "")
  .split(",")
  .map((s) => s.trim())
  .filter(Boolean);

if (!userId) {
  console.error("ws_user_probe: --user <uuid> required");
  process.exit(2);
}

const out = createWriteStream(outPath, { flags: "w" });
const started = Date.now();
const notifs = [];
let snapshot = null;
const seen = new Set();

function finish(code, reason) {
  const summary = {
    reason,
    snapshot,
    notifCount: notifs.length,
    types: notifs.map((n) => n.notification_type),
    frames: notifs.map((n) => ({
      id: n.id,
      source_seq: n.source_seq,
      type: n.notification_type,
    })),
  };
  out.write(JSON.stringify({ summary }) + "\n");
  out.end();
  try {
    ws.close();
  } catch {
    /* ignore */
  }
  console.log("USER_PROBE_SUMMARY " + JSON.stringify(summary));
  process.exit(code);
}

const deadlineTimer = setTimeout(() => {
  if (expectTypes.length) {
    const got = new Set(notifs.map((n) => n.notification_type));
    const missing = expectTypes.filter((t) => !got.has(t));
    if (missing.length) finish(1, `missing types: ${missing.join(",")}`);
  }
  if (minNotifs > 0 && notifs.length < minNotifs) {
    finish(1, `only ${notifs.length} notifs, need ${minNotifs}`);
  }
  finish(0, "deadline complete");
}, deadlineMs);

const ws = new WebSocket(url);

ws.on("open", () => {
  ws.send(
    JSON.stringify({
      op: "subscribe_user",
      user_id: userId,
      token,
    }),
  );
  process.stdout.write("USER_PROBE open subscribe_user\n");
});

ws.on("message", (data) => {
  let frame;
  try {
    frame = JSON.parse(String(data));
  } catch {
    return;
  }
  const line = JSON.stringify({ t_ms: Date.now() - started, frame });
  out.write(line + "\n");
  if (frame.type === "notif_snapshot") {
    snapshot = frame;
    process.stdout.write(`USER_PROBE notif_snapshot unread=${frame.unread_count}\n`);
    return;
  }
  if (frame.type === "notif") {
    const key = `${frame.id}:${frame.source_seq}`;
    if (seen.has(key)) return;
    seen.add(key);
    notifs.push(frame);
    process.stdout.write(
      `USER_PROBE notif type=${frame.notification_type} id=${frame.id}\n`,
    );
    if (minNotifs > 0 && notifs.length >= minNotifs) {
      const got = new Set(notifs.map((n) => n.notification_type));
      if (expectTypes.every((t) => got.has(t))) {
        clearTimeout(deadlineTimer);
        finish(0, "min notifs reached");
      }
    }
  }
});

ws.on("error", (err) => {
  console.error("USER_PROBE error", err.message);
});

ws.on("close", () => {
  /* exit via finish only */
});
