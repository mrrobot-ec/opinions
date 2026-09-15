#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

export DATABASE_URL=${DATABASE_URL:-postgres://opinions:opinions@localhost:15434/opinions}
export PGPASSWORD=${PGPASSWORD:-opinions}
export BIND_ADDR=${BIND_ADDR:-127.0.0.1:18080}
export DEMO_TOKEN=${DEMO_TOKEN:-phase5-demo}
# Phase 6 (D26): the server validates ADMIN_TOKENS_JSON at startup; the raw
# bearer rides only the x-admin-token header (legacy ADMIN_TOKEN is gone).
ADMIN_BEARER="${ADMIN_BEARER:-phase5-admin}"
ADMIN_TOKEN_DIGEST=$(printf '%s' "$ADMIN_BEARER" | python3 -c "import hashlib,sys;print(hashlib.sha256(sys.stdin.buffer.read()).hexdigest())")
export ADMIN_TOKENS_JSON="[{\"id\":\"e2e-admin\",\"roles\":[\"curator\",\"ops\",\"finance\",\"superadmin\"],\"sha256\":\"$ADMIN_TOKEN_DIGEST\"}]"
export SCHEDULER_TICK_MS=100
export DRAFT_PUBLISHER_ENABLED=true
export VIDEO_WORKER_ENABLED=false
export MODERATION_RUNNER_ENABLED=false
export FLASH_CADENCE_SECS=2
export MAX_SLOT_HORIZON_SECS=60
export DRAFT_TTL_SECS=120
export DAILY_SEED_BUDGET_MICRO=1000000000
export FLASH_OPEN_SECS=15
export FLASH_HIDDEN_WINDOW_SECS=4
export FLASH_SEED_MICRO=1000000
export FLASH_SEED_FLOOR_MICRO=1000000
export FLASH_MIN_VOTES_TO_RESOLVE=1
export FLASH_MIN_VOTES_FLOOR=1
export VOTE_MIN_ACCOUNT_AGE_SECS=0
export CONTENT_RENDER_DIR=${CONTENT_RENDER_DIR:-/tmp/opinions-phase5-content}

HOST=${BIND_ADDR%:*}
PORT=${BIND_ADDR##*:}
BASE="http://${HOST}:${PORT}"
WS="ws://${HOST}:${PORT}/ws"
RUN_TAG="phase5-$(date +%s)-$$"
SERVER_PID=
PROBE_PID=

fail() {
  echo "PHASE 5 E2E FAILED: $*" >&2
  exit 1
}

section() {
  echo "== $* =="
}

cleanup() {
  if [ -n "${PROBE_PID:-}" ]; then kill "$PROBE_PID" 2>/dev/null || true; fi
  if [ -n "${SERVER_PID:-}" ]; then kill "$SERVER_PID" 2>/dev/null || true; fi
}
trap cleanup EXIT INT TERM

sql() {
  psql -X -v ON_ERROR_STOP=1 -At -h 127.0.0.1 -p 15434 -U opinions -d opinions -c "$1"
}

wait_until() {
  local label=$1
  local deadline=$2
  shift 2
  local end=$((SECONDS + deadline))
  until "$@"; do
    if [ "$SECONDS" -ge "$end" ]; then fail "deadline exceeded: $label"; fi
    sleep 0.2
  done
}

http_ok() {
  curl -fsS "$BASE/healthz" >/dev/null 2>&1
}

draft_is_published() {
  [ "$(sql "select status from market_drafts where id='$1'")" = published ]
}

market_is_settled() {
  case "$(curl -fsS "$BASE/markets/$1" | jq -r .state)" in
    resolved|paid) return 0 ;;
    *) return 1 ;;
  esac
}

admin_post() {
  local path=$1
  local body=$2
  curl -fsS -H "x-admin-token: $ADMIN_BEARER" -H 'content-type: application/json' \
    -d "$body" "$BASE$path"
}

admin_patch() {
  local path=$1
  local body=$2
  curl -fsS -X PATCH -H "x-admin-token: $ADMIN_BEARER" -H 'content-type: application/json' \
    -d "$body" "$BASE$path"
}

create_drafts() {
  local topics=$1
  local source=${2:-template}
  local fallback=${3:-false}
  admin_post /admin/drafts "$(jq -cn --argjson topics "$topics" --arg source "$source" \
    --argjson fallback "$fallback" '{topics:$topics,tier:"flash",source:$source,allow_fallback:$fallback}')"
}

approve_draft() {
  admin_post "/admin/drafts/$1/approve" "$(jq -cn --arg user "$USER_ID" '{reviewer_id:$user}')"
}

ws_probe() {
  local mode=$1
  local market=$2
  local out=$3
  node --input-type=module - "$WS" "$market" "$mode" "$out" <<'NODE'
import { createWriteStream } from "node:fs";
import { WebSocket } from "./scripts/node_modules/ws/wrapper.mjs";
const [, , url, market, mode, path] = process.argv;
const out = createWriteStream(path, { flags: "w" });
const socket = new WebSocket(url);
const timer = setTimeout(() => finish(1, "deadline"), 20000);
let ended = false;
function finish(code, reason, frame = null) {
  if (ended) return;
  ended = true;
  clearTimeout(timer);
  out.end(JSON.stringify({ reason, frame }) + "\n", () => process.exit(code));
  socket.close();
}
socket.on("open", () => socket.send(JSON.stringify({ op: "subscribe", market_id: market })));
socket.on("message", data => {
  let frame;
  try { frame = JSON.parse(String(data)); } catch { return; }
  if (mode === "asset" && frame.type === "asset" && frame.market_id === market &&
      frame.kind === "poster" && typeof frame.outbox_seq === "number") {
    finish(0, "asset", frame);
  }
  if (mode === "snapshot" && frame.type === "snapshot" && frame.market_id === market &&
      typeof frame.poster_asset_url === "string" && frame.poster_asset_url.length > 0) {
    finish(0, "snapshot", frame);
  }
});
socket.on("error", error => finish(1, error.message));
NODE
}

section "fresh native PostgreSQL + migrations"
dropdb --if-exists --force -h 127.0.0.1 -p 15434 -U opinions opinions
createdb -h 127.0.0.1 -p 15434 -U opinions opinions
cargo sqlx migrate run --source migrations --database-url "$DATABASE_URL" >/dev/null
rm -rf "$CONTENT_RENDER_DIR"
mkdir -p "$CONTENT_RENDER_DIR"

section "build driver + capitalize demo user"
cargo build -q -p main
cargo build -q -p adapters --example content_worker
SEED_SKIP_VOTE=true DEMO_PHONE=+15550105 target/debug/main --seed-demo >/tmp/opinions-phase5-seed.log
USER_ID=$(sql "select id from users where handle='demo'")
[ -n "$USER_ID" ] || fail "demo user was not created"

section "start primary API with publication sweep"
target/debug/main >/tmp/opinions-phase5-server.log 2>&1 &
SERVER_PID=$!
wait_until "server health" 20 http_ok

section "template draft -> curator edit + approve -> reserved slot"
CREATED=$(create_drafts "$(jq -cn --arg topic "$RUN_TAG-primary" '[$topic]')")
DRAFT_ID=$(jq -er '.[0].id' <<<"$CREATED")
SLUG=$(jq -er '.[0].spec.slug' <<<"$CREATED")
QUESTION="Will </text><script>alert('phase5')</script> settle?"
SPEC=$(jq -c --arg question "$QUESTION" '.[0].spec | .question=$question' <<<"$CREATED")
EDITED=$(admin_patch "/admin/drafts/$DRAFT_ID" "$(jq -cn --arg user "$USER_ID" --argjson spec "$SPEC" '{reviewer_id:$user,spec:$spec}')")
[ "$(jq -r .spec.question <<<"$EDITED")" = "$QUESTION" ] || fail "curator edit did not persist"
APPROVED=$(approve_draft "$DRAFT_ID")
[ "$(jq -r .status <<<"$APPROVED")" = approved ] || fail "draft did not approve"
SLOT=$(jq -er .publish_at <<<"$APPROVED")
[ -n "$SLOT" ] || fail "approval did not reserve a slot"

section "saga sweep publishes with a live poster placeholder"
wait_until "primary publication" 20 draft_is_published "$DRAFT_ID"
MARKET_ID=$(sql "select published_market_id from market_drafts where id='$DRAFT_ID'")
[ -n "$MARKET_ID" ] || fail "saga did not persist a market id"
SNAPSHOT=$(curl -fsS "$BASE/markets/$MARKET_ID")
[ "$(jq -r .state <<<"$SNAPSHOT")" = live ] || fail "published market is not live"
[ "$(jq -r .poster_asset_url <<<"$SNAPSHOT")" = null ] || fail "poster placeholder was not null"
[ "$(jq -r .question <<<"$SNAPSHOT")" = "$QUESTION" ] || fail "curated question was not copied to market"

section "poster lease/render/attach -> asset frame -> snapshot rehydration"
ASSET_PROBE=/tmp/opinions-phase5-asset.json
ws_probe asset "$MARKET_ID" "$ASSET_PROBE" &
PROBE_PID=$!
DATABASE_URL="$DATABASE_URL" CONTENT_RENDER_DIR="$CONTENT_RENDER_DIR" \
  target/debug/examples/content_worker render-one >/tmp/opinions-phase5-worker.log
wait "$PROBE_PID" || fail "asset websocket frame was not observed"
PROBE_PID=
ASSET_URL=$(jq -er .frame.url "$ASSET_PROBE")
curl -fsS "$BASE$ASSET_URL" | grep -q '<svg' || fail "attached poster was not served"
SNAPSHOT_PROBE=/tmp/opinions-phase5-snapshot.json
ws_probe snapshot "$MARKET_ID" "$SNAPSHOT_PROBE"
[ "$(jq -r .frame.poster_asset_url "$SNAPSHOT_PROBE")" = "$ASSET_URL" ] || \
  fail "second websocket connection did not rehydrate poster URL"

section "vote + trade machine market -> resolve -> real-PnL inert share card"
VOTE=$(curl -fsS -H "x-demo-token: $DEMO_TOKEN" -H 'content-type: application/json' \
  -d "$(jq -cn --arg user "$USER_ID" --arg market "$MARKET_ID" --arg key "$RUN_TAG-vote" \
    '{user_id:$user,market_ref:$market,side:"yes",crowd_guess_pct:70,idempotency_key:$key}')" \
  "$BASE/votes")
[ "$(jq -r .market_id <<<"$VOTE")" = "$MARKET_ID" ] || fail "vote missed machine market"
TRADE=$(curl -fsS -H "x-demo-token: $DEMO_TOKEN" -H 'content-type: application/json' \
  -d "$(jq -cn --arg user "$USER_ID" --arg market "$MARKET_ID" --arg key "$RUN_TAG-trade" \
    '{user_id:$user,market_ref:$market,side:"yes",action:"buy",amount_micro:500000,idempotency_key:$key}')" \
  "$BASE/trades")
[ "$(jq -r .replayed <<<"$TRADE")" = false ] || fail "first trade unexpectedly replayed"
wait_until "machine market settlement" 30 market_is_settled "$MARKET_ID"
CARD=/tmp/opinions-phase5-card.svg
curl -fsS "$BASE/users/$USER_ID/share_card/$MARKET_ID" >"$CARD"
grep -q 'Payout \$' "$CARD" || fail "share card has no real payout"
grep -q 'Return +' "$CARD" || fail "share card has no positive realized return"
grep -q '&lt;' "$CARD" || fail "adversarial question was not escaped"
if grep -q '<script' "$CARD"; then fail "adversarial question became active SVG markup"; fi

section "two approvals reserve distinct slots and publish distinct markets"
BATCH=$(create_drafts "$(jq -cn --arg a "$RUN_TAG-two-a" --arg b "$RUN_TAG-two-b" '[$a,$b]')")
A_ID=$(jq -er '.[0].id' <<<"$BATCH")
B_ID=$(jq -er '.[1].id' <<<"$BATCH")
A_APPROVED=$(approve_draft "$A_ID")
B_APPROVED=$(approve_draft "$B_ID")
A_SLOT=$(jq -er .publish_at <<<"$A_APPROVED")
B_SLOT=$(jq -er .publish_at <<<"$B_APPROVED")
[ "$A_SLOT" != "$B_SLOT" ] || fail "two drafts reserved one slot"
wait_until "first batch publication" 20 draft_is_published "$A_ID"
wait_until "second batch publication" 20 draft_is_published "$B_ID"
A_MARKET=$(sql "select published_market_id from market_drafts where id='$A_ID'")
B_MARKET=$(sql "select published_market_id from market_drafts where id='$B_ID'")
[ "$A_MARKET" != "$B_MARKET" ] || fail "two drafts converged on one market"

section "empty flash boundary publishes nothing and dedupes SlotUnfilled"
BASELINE_SEQ=$(sql "select coalesce(max(seq),0) from events_outbox where event_type='SlotUnfilled'")
new_empty_slot() {
  [ "$(sql "select count(*) from events_outbox where event_type='SlotUnfilled' and seq>$BASELINE_SEQ")" -ge 1 ]
}
wait_until "empty flash boundary" 10 new_empty_slot
EMPTY_SLOT=$(sql "select payload->>'slot_unix' from events_outbox where event_type='SlotUnfilled' and seq>$BASELINE_SEQ order by seq limit 1")
[ "$(sql "select count(*) from events_outbox where event_type='SlotUnfilled' and payload->>'slot_unix'='$EMPTY_SLOT'")" = 1 ] || \
  fail "one empty slot emitted more than one SlotUnfilled"
[ "$(sql "select count(*) from market_drafts where extract(epoch from publish_at)::bigint=$EMPTY_SLOT")" = 0 ] || \
  fail "empty boundary published a market"

section "publish_now concurrent with sweep converges"
RACE=$(create_drafts "$(jq -cn --arg topic "$RUN_TAG-race" '[$topic]')")
RACE_ID=$(jq -er '.[0].id' <<<"$RACE")
RACE_APPROVED=$(approve_draft "$RACE_ID")
RACE_EPOCH=$(sql "select extract(epoch from publish_at)::bigint from market_drafts where id='$RACE_ID'")
while [ "$(date +%s)" -lt "$RACE_EPOCH" ]; do sleep 0.1; done
set +e
curl -sS -o /tmp/opinions-phase5-race.json -w '%{http_code}' -H "x-admin-token: $ADMIN_BEARER" \
  -X POST "$BASE/admin/drafts/$RACE_ID/publish_now" >/tmp/opinions-phase5-race.code &
RACE_CURL=$!
wait "$RACE_CURL"
set -e
RACE_CODE=$(cat /tmp/opinions-phase5-race.code)
case "$RACE_CODE" in 200|409) ;; *) fail "race endpoint returned HTTP $RACE_CODE" ;; esac
wait_until "race convergence" 15 draft_is_published "$RACE_ID"
[ "$(sql "select count(*) from markets m join market_drafts d on d.published_market_id=m.id where d.id='$RACE_ID'")" = 1 ] || \
  fail "race did not converge on one market"
[ "$(sql "select count(*) from events_outbox where event_type='DraftPublished' and aggregate_id='$RACE_ID'")" = 1 ] || \
  fail "race emitted duplicate DraftPublished"
[ "$(sql "select count(*) from video_jobs where draft_id='$RACE_ID'")" = 1 ] || \
  fail "race issued duplicate poster jobs"

section "two workers claim disjoint jobs"
[ "$(sql "select count(*) from video_jobs where status='queued'")" -ge 2 ] || fail "need two queued jobs"
DATABASE_URL="$DATABASE_URL" target/debug/examples/content_worker claim-one >/tmp/opinions-phase5-claim-a &
CLAIM_A_PID=$!
DATABASE_URL="$DATABASE_URL" target/debug/examples/content_worker claim-one >/tmp/opinions-phase5-claim-b &
CLAIM_B_PID=$!
wait "$CLAIM_A_PID"
wait "$CLAIM_B_PID"
CLAIM_A=$(tail -1 /tmp/opinions-phase5-claim-a)
CLAIM_B=$(tail -1 /tmp/opinions-phase5-claim-b)
[ "$CLAIM_A" != none ] && [ "$CLAIM_B" != none ] || fail "a worker found no job"
[ "$CLAIM_A" != "$CLAIM_B" ] || fail "two workers claimed the same job"
[ "$(sql "select count(*) from video_jobs where id in ('$CLAIM_A','$CLAIM_B') and status='rendering' and claim_token is not null")" = 2 ] || \
  fail "claimed jobs do not carry independent leases"

section "keyless LLM request falls back honestly"
FALLBACK=$(create_drafts "$(jq -cn --arg topic "$RUN_TAG-keyless-llm" '[$topic]')" llm true)
[ "$(jq -r '.[0].source' <<<"$FALLBACK")" = template ] || fail "keyless draft source is not template"
[ "$(jq -r '.[0].fallback_from' <<<"$FALLBACK")" = llm ] || fail "keyless fallback_from is not llm"

echo "PHASE 5 E2E GREEN"
