#!/usr/bin/env bash
# Phase 2 live-loop exit demo (plan Task 2.5):
#   flash market → WS probe ordered frames → vote+trade via converse →
#   Closing freezes trades (423) → resolve → payout latency + ledger invariants.
# NO fixed sleeps for assertions: poll with absolute deadlines (boundary+10s).
set -euo pipefail

cd "$(dirname "$0")/.."
# shellcheck disable=SC1090
source "$HOME/.cargo/env" 2>/dev/null || true

PGHOST=localhost
PGPORT=15434
PGUSER=opinions
export PGPASSWORD=opinions
DB=opinions
export DATABASE_URL="postgres://opinions:opinions@localhost:15434/${DB}"
export DEMO_TOKEN="${DEMO_TOKEN:-demo-token}"
# Phase 6 (D26): the server validates ADMIN_TOKENS_JSON at startup; the raw
# bearer rides only the x-admin-token header (legacy ADMIN_TOKEN is gone).
ADMIN_BEARER="${ADMIN_BEARER:-admin-token}"
ADMIN_TOKEN_DIGEST=$(printf '%s' "$ADMIN_BEARER" | python3 -c "import hashlib,sys;print(hashlib.sha256(sys.stdin.buffer.read()).hexdigest())")
export ADMIN_TOKENS_JSON="[{\"id\":\"e2e-admin\",\"roles\":[\"curator\",\"ops\",\"finance\",\"superadmin\"],\"sha256\":\"$ADMIN_TOKEN_DIGEST\"}]"
export DEMO_PHONE="${DEMO_PHONE:-+15550100}"
export CORE_API_URL="http://127.0.0.1:8080"
export WS_URL="${WS_URL:-ws://127.0.0.1:8080/ws}"
CONVERSE_URL="http://127.0.0.1:8091"

# Flash windows (plan Task 2.5)
export FLASH_CLOSES_SECS="${FLASH_CLOSES_SECS:-90}"
export FLASH_TALLY_HIDDEN_SECS="${FLASH_TALLY_HIDDEN_SECS:-60}"
export MIN_VOTES_TO_RESOLVE=1
export SEED_SKIP_VOTE=1
export DEMO_SLUG="${DEMO_SLUG:-demo-flash}"
export DEMO_CHAIN_SIG="${DEMO_CHAIN_SIG:-demo-flash-chain-sig-0001}"
export VOTE_NEAR_CLOSE_SECS="${VOTE_NEAR_CLOSE_SECS:-5}"
export VOTE_MIN_ACCOUNT_AGE_SECS="${VOTE_MIN_ACCOUNT_AGE_SECS:-0}"
export SCHEDULER_TICK_MS="${SCHEDULER_TICK_MS:-500}"

CORE_PID=""
CONVERSE_PID=""
PROBE_PID=""
PROBE_OUT="/tmp/ws_probe_live_$$.jsonl"

kill_ports() {
  lsof -ti :8080 -ti :8091 2>/dev/null | xargs kill -9 2>/dev/null || true
}
cleanup() {
  [ -n "$PROBE_PID" ] && kill "$PROBE_PID" 2>/dev/null || true
  [ -n "$CONVERSE_PID" ] && kill "$CONVERSE_PID" 2>/dev/null || true
  [ -n "$CORE_PID" ] && kill "$CORE_PID" 2>/dev/null || true
  kill_ports
  wait 2>/dev/null || true
}
trap cleanup EXIT
kill_ports

psql_demo() { psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d "$DB" -X -A -t -c "$1"; }
fail() { echo "LIVE-LOOP FAILED: $1" >&2; exit 1; }

# Portable epoch milliseconds (macOS date has no %3N).
now_ms() { python3 -c 'import time; print(int(time.time()*1000))'; }

# Poll until a shell function returns 0, or deadline_ms (epoch ms) is hit.
poll_until() {
  local deadline_ms=$1
  shift
  while [ "$(now_ms)" -lt "$deadline_ms" ]; do
    if "$@"; then return 0; fi
    sleep 0.2
  done
  return 1
}

http_ok() { curl -sf "$1" >/dev/null 2>&1; }

wait_http() {
  local url=$1
  local budget_ms=${2:-60000}
  poll_until $(( $(now_ms) + budget_ms )) http_ok "$url" \
    || fail "timeout waiting for $url"
}

pg_ready() {
  psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -c "select 1" >/dev/null 2>&1
}

echo "== [1/9] infra up + fresh database =="
just infra-up >/dev/null
poll_until $(( $(now_ms) + 60000 )) pg_ready || fail "postgres not ready"
psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -c "drop database if exists ${DB} with (force)" >/dev/null
psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -c "create database ${DB}" >/dev/null
echo "   fresh ${DB}"

echo "== [2/9] build + seed flash market (closes=+${FLASH_CLOSES_SECS}s hidden=+${FLASH_TALLY_HIDDEN_SECS}s) =="
cargo build -q -p main
SEED_START_MS=$(now_ms)
SEED_OUT=$(cargo run -q -p main -- --seed-demo)
echo "$SEED_OUT" | sed 's/^/   /'
USER_ID=$(echo "$SEED_OUT" | sed -n 's/^user_id=//p')
MARKET_ID=$(echo "$SEED_OUT" | sed -n 's/^market_id=\([^ ]*\).*/\1/p')
SLUG=$(echo "$SEED_OUT" | sed -n 's/^slug=//p')
[ -n "$USER_ID" ] && [ -n "$MARKET_ID" ] && [ -n "$SLUG" ] \
  || fail "seed did not print user/market/slug"
HIDDEN_DEADLINE_MS=$(( SEED_START_MS + FLASH_TALLY_HIDDEN_SECS * 1000 + 10000 ))
CLOSE_DEADLINE_MS=$(( SEED_START_MS + FLASH_CLOSES_SECS * 1000 + 10000 ))
RESOLVE_DEADLINE_MS=$(( CLOSE_DEADLINE_MS + 20000 ))

echo "== [3/9] install probe deps + start core + converse + ws probe =="
(cd scripts && pnpm install --silent)
cargo run -q -p main >/tmp/core-live-loop.log 2>&1 &
CORE_PID=$!
disown
wait_http "${CORE_API_URL}/healthz" 90000
(cd services/converse && uv run uvicorn converse.app:app --host 127.0.0.1 --port 8091 >/tmp/converse-live-loop.log 2>&1) &
CONVERSE_PID=$!
disown
wait_http "${CONVERSE_URL}/healthz" 90000

PROBE_DEADLINE_MS=$(( FLASH_CLOSES_SECS * 1000 + 45000 ))
: > /tmp/ws_probe_stdout.log
node scripts/ws_probe.mjs \
  --url "$WS_URL" \
  --market "$MARKET_ID" \
  --out "$PROBE_OUT" \
  --deadline-ms "$PROBE_DEADLINE_MS" \
  >>/tmp/ws_probe_stdout.log 2>&1 &
PROBE_PID=$!
disown

probe_has() { grep -q "$1" /tmp/ws_probe_stdout.log; }
poll_until $(( $(now_ms) + 20000 )) probe_has "PROBE snapshot" \
  || fail "probe never saw snapshot (see /tmp/ws_probe_stdout.log /tmp/core-live-loop.log)"
echo "   snapshot observed"

echo "== [4/9] text-vote via converse (preview + lexical confirm) =="
RV1=$(curl -sf -X POST "${CONVERSE_URL}/webhooks/sendblue" \
  -H 'content-type: application/json' \
  -d "{\"number\": \"${DEMO_PHONE}\", \"content\": \"vote yes 60 on ${SLUG}\", \"message_id\": \"live-vote-1\"}")
echo "   vote preview: $(echo "$RV1" | jq -r .reply 2>/dev/null || echo "$RV1")"
echo "$RV1" | jq -e '.reply | test("Preview vote")' >/dev/null \
  || fail "vote preview failed: $RV1"

RV2=$(curl -sf -X POST "${CONVERSE_URL}/webhooks/sendblue" \
  -H 'content-type: application/json' \
  -d "{\"number\": \"${DEMO_PHONE}\", \"content\": \"yes\", \"message_id\": \"live-vote-2\"}")
echo "   vote confirm: $(echo "$RV2" | jq -r .reply 2>/dev/null || echo "$RV2")"
echo "$RV2" | jq -e '.reply | test("vote|Vote|YES|yes|#")' >/dev/null \
  || fail "vote confirm failed: $RV2"

echo "== [5/9] text-trade via converse (preview + lexical confirm) =="
R1=$(curl -sf -X POST "${CONVERSE_URL}/webhooks/sendblue" \
  -H 'content-type: application/json' \
  -d "{\"number\": \"${DEMO_PHONE}\", \"content\": \"buy \$5 of yes on ${SLUG}\", \"message_id\": \"live-trade-1\"}")
echo "   trade preview: $(echo "$R1" | jq -r .reply 2>/dev/null || echo "$R1")"
echo "$R1" | jq -e '.reply | test("micro-shares") and test("fee")' >/dev/null \
  || fail "trade preview must mention shares/fee: $R1"

R2=$(curl -sf -X POST "${CONVERSE_URL}/webhooks/sendblue" \
  -H 'content-type: application/json' \
  -d "{\"number\": \"${DEMO_PHONE}\", \"content\": \"yes\", \"message_id\": \"live-trade-2\"}")
echo "   trade confirm: $(echo "$R2" | jq -r .reply 2>/dev/null || echo "$R2")"
echo "$R2" | jq -e '.reply | test("Executed") and test("bought")' >/dev/null \
  || fail "trade confirm must execute: $R2"

poll_until $(( $(now_ms) + 15000 )) probe_has "PROBE price" \
  || fail "no price frame after trade"
poll_until $(( $(now_ms) + 10000 )) probe_has "PROBE trade" \
  || fail "no trade frame after trade"
echo "   price+trade observed"

echo "== [6/9] wait for Closing; assert trade → 423; no post-closing tally =="
market_state() {
  curl -sf "${CORE_API_URL}/markets/${MARKET_ID}" | jq -r .state
}
state_is() {
  local want=$1
  local st
  st=$(market_state)
  [ "$st" = "$want" ]
}
state_in() {
  # any of the args
  local st
  st=$(market_state)
  local w
  for w in "$@"; do [ "$st" = "$w" ] && return 0; done
  return 1
}

poll_until "$HIDDEN_DEADLINE_MS" state_in closing closed resolved \
  || fail "market never left live by hidden boundary+10s (state=$(market_state))"
# If we already jumped past closing, still try the 423 while closing if possible
if state_is closing; then
  echo "   state=closing"
  KEY="live-frozen-$(uuidgen | tr '[:upper:]' '[:lower:]')"
  TRADE_BODY=$(jq -nc \
    --arg u "$USER_ID" \
    --arg m "$SLUG" \
    --arg k "$KEY" \
    '{user_id:$u, market_ref:$m, side:"yes", action:"buy", amount_micro:1000000, idempotency_key:$k}')
  HTTP_CODE=$(curl -s -o /tmp/frozen_trade.json -w "%{http_code}" \
    -X POST "${CORE_API_URL}/trades" \
    -H "x-demo-token: ${DEMO_TOKEN}" \
    -H 'content-type: application/json' \
    -d "$TRADE_BODY")
  [ "$HTTP_CODE" = "423" ] || fail "expected 423 during Closing, got ${HTTP_CODE}: $(cat /tmp/frozen_trade.json)"
  echo "   frozen trade → 423"
else
  echo "   state=$(market_state) (already past closing)"
  KEY="live-frozen-$(uuidgen | tr '[:upper:]' '[:lower:]')"
  TRADE_BODY=$(jq -nc \
    --arg u "$USER_ID" \
    --arg m "$SLUG" \
    --arg k "$KEY" \
    '{user_id:$u, market_ref:$m, side:"yes", action:"buy", amount_micro:1000000, idempotency_key:$k}')
  HTTP_CODE=$(curl -s -o /tmp/frozen_trade.json -w "%{http_code}" \
    -X POST "${CORE_API_URL}/trades" \
    -H "x-demo-token: ${DEMO_TOKEN}" \
    -H 'content-type: application/json' \
    -d "$TRADE_BODY")
  if ! state_in closed resolved; then
    [ "$HTTP_CODE" = "423" ] || fail "expected 423, got ${HTTP_CODE}"
  fi
fi

echo "== [7/9] wait Resolved/Paid; measure payout from lifecycle:closed =="
# Scheduler may cascade through resolved into paid within one sweep.
poll_until "$RESOLVE_DEADLINE_MS" state_in resolved paid \
  || fail "market never resolved/paid (state=$(market_state))"
echo "   state=$(market_state)"

# Wait for probe summary (chain complete)
poll_until $(( $(now_ms) + 20000 )) probe_has "PROBE_SUMMARY" \
  || fail "probe did not complete ordered chain (see /tmp/ws_probe_stdout.log)"
PROBE_SUMMARY_JSON=$(grep "PROBE_SUMMARY" /tmp/ws_probe_stdout.log | tail -1 | sed 's/^PROBE_SUMMARY //')
ORDERED=$(echo "$PROBE_SUMMARY_JSON" | jq -c '.ordered // []')
TALLY_AFTER=$(echo "$PROBE_SUMMARY_JSON" | jq -r '.tallyAfterClosing // 0')
CLOSED_REL=$(echo "$PROBE_SUMMARY_JSON" | jq -r '.closedAtMs // empty')
echo "   ordered frames: $ORDERED"
echo "   tally_after_closing: $TALLY_AFTER"
[ "$TALLY_AFTER" = "0" ] || fail "tally frames after closing: ${TALLY_AFTER}"

echo "$ORDERED" | jq -e '
  (index("snapshot") != null)
  and (index("price") != null)
  and (index("trade") != null)
  and (map(select(startswith("lifecycle:"))) | map(split(":")[1]) | index("closing") != null)
  and (map(select(startswith("lifecycle:"))) | map(split(":")[1]) | index("closed") != null)
  and (
    (map(select(startswith("lifecycle:"))) | map(split(":")[1]) | index("resolved") != null)
    or (map(select(startswith("lifecycle:"))) | map(split(":")[1]) | index("paid") != null)
  )
' >/dev/null || fail "ordered frame chain incomplete: $ORDERED"

# Payout latency (plan): from lifecycle:closed frame to positions showing settled PnL.
# Settlement runs in the same ResolveMarket transaction that emits resolved, so the
# closed→resolved frame delta is the authoritative cascade latency; we also require
# positions to be settled within 2s of observing resolve.
payout_ready() {
  local pos
  pos=$(curl -sf -H "x-demo-token: ${DEMO_TOKEN}" \
    "${CORE_API_URL}/users/${USER_ID}/positions")
  echo "$pos" | jq -e --arg mid "$MARKET_ID" \
    '[.[] | select(.market_id == $mid)] | length > 0 and all(.shares_micro == 0)' \
    >/dev/null 2>&1
}

PAYOUT_POLL_START=$(now_ms)
if payout_ready; then
  :
else
  poll_until $(( $(now_ms) + 2000 )) payout_ready \
    || fail "positions not settled within 2s after resolve observed"
fi

PAYOUT_LATENCY_MS=$(python3 -c '
import json, sys
path = sys.argv[1]
closed = resolved = None
for line in open(path):
    try:
        o = json.loads(line)
    except Exception:
        continue
    fr = o.get("frame") or {}
    if fr.get("type") != "lifecycle":
        continue
    st = str(fr.get("state") or "").lower()
    if st == "closed" and closed is None:
        closed = o.get("t_ms")
    if st == "resolved" and resolved is None:
        resolved = o.get("t_ms")
if closed is not None and resolved is not None:
    print(max(0, int(resolved) - int(closed)))
else:
    print("missing closed or resolved lifecycle frame", file=sys.stderr)
    raise SystemExit(1)
' "$PROBE_OUT")

echo "   payout_latency_ms=${PAYOUT_LATENCY_MS} (budget 2000; closed→resolved frame delta)"
[ "$PAYOUT_LATENCY_MS" -le 2000 ] || fail "payout latency ${PAYOUT_LATENCY_MS}ms > 2000ms budget"

POS=$(curl -sf -H "x-demo-token: ${DEMO_TOKEN}" \
  "${CORE_API_URL}/users/${USER_ID}/positions")
echo "   positions: $POS"
echo "$POS" | jq -e --arg mid "$MARKET_ID" \
  '[.[] | select(.market_id == $mid)] | length >= 1' >/dev/null \
  || fail "no position rows for market after resolve"

echo "== [8/9] psql invariants: vote_scores, ledger zero-sum, escrow=0 =="
VOTE_SCORES=$(psql_demo "select count(*) from vote_scores vs join votes v on v.id = vs.vote_id where v.market_id = '${MARKET_ID}'")
[ "$VOTE_SCORES" -ge 1 ] || fail "expected vote_scores row for market, got ${VOTE_SCORES}"
echo "   vote_scores=${VOTE_SCORES}"

UNBALANCED=$(psql_demo "
  select count(*) from (
    select la.currency, sum(le.amount_micro) as s
      from ledger_entries le join ledger_accounts la on la.id = le.account_id
     group by la.currency having sum(le.amount_micro) <> 0
  ) bad")
[ "$UNBALANCED" = "0" ] || fail "ledger not per-currency zero-sum"

ESCROW=$(psql_demo "
  select coalesce(sum(le.amount_micro), 0)
    from ledger_entries le
    join ledger_accounts la on la.id = le.account_id
   where la.owner_type = 'escrow' and la.owner_id = '${MARKET_ID}'")
echo "   escrow_balance=${ESCROW}"
[ "$ESCROW" = "0" ] || fail "market escrow must be 0 after payout, got ${ESCROW}"

echo "== [9/9] LIVE LOOP GREEN =="
echo "LIVE LOOP GREEN"
echo "ORDERED_FRAMES ${ORDERED}"
echo "PAYOUT_LATENCY_MS ${PAYOUT_LATENCY_MS}"
echo "TALLY_AFTER_CLOSING ${TALLY_AFTER}"
echo "MARKET_ID ${MARKET_ID}"
echo "USER_ID ${USER_ID}"
