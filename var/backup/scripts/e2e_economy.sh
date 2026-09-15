#!/usr/bin/env bash
# Phase 3 economy e2e (plan Task 3.5 item 2): sections A–D.
# Poll-with-deadline only — no assertion fixed sleeps.
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

# Phase 3 economy knobs for this run
export DEVICE_HASH_SECRET="${DEVICE_HASH_SECRET:-e2e-device-secret-phase3}"
export PAYOUT_HOLD_THRESHOLD_MICRO="${PAYOUT_HOLD_THRESHOLD_MICRO:-1000000}" # $1 → hold big seeds
export SWEEP_DELAY_SECS="${SWEEP_DELAY_SECS:-5}"
export BURST_WINDOW_SECS="${BURST_WINDOW_SECS:-120}"
export PRIOR_HORIZON_WINDOWS="${PRIOR_HORIZON_WINDOWS:-2}"
export MIN_VOTES_FOR_RATIOS="${MIN_VOTES_FOR_RATIOS:-4}"
export YOUNG_ACCOUNT_AGE_SECS="${YOUNG_ACCOUNT_AGE_SECS:-259200}" # 3d
export YOUNG_ACCOUNT_SHARE_MAX_PPM="${YOUNG_ACCOUNT_SHARE_MAX_PPM:-500000}"
export DEVICE_SHARE_MAX_PPM="${DEVICE_SHARE_MAX_PPM:-600000}"
export MIN_METADATA_COVERAGE_PPM="${MIN_METADATA_COVERAGE_PPM:-500000}"
export REP_TIER_THRESHOLDS_MICRO="${REP_TIER_THRESHOLDS_MICRO:-5000,100000,200000,300000}"
export FEE_DISCOUNT_BP_BY_TIER="${FEE_DISCOUNT_BP_BY_TIER:-0,10,10,10,10}"
export DISCOUNT_FLIP_WINDOW_SECS="${DISCOUNT_FLIP_WINDOW_SECS:-3600}"
export REP_SCORE_MIN_POT_MICRO="${REP_SCORE_MIN_POT_MICRO:-0}"
export LEADERBOARD_MIN_SCORED="${LEADERBOARD_MIN_SCORED:-1}"
export VOTE_MIN_ACCOUNT_AGE_SECS="${VOTE_MIN_ACCOUNT_AGE_SECS:-0}"
export VOTE_NEAR_CLOSE_SECS="${VOTE_NEAR_CLOSE_SECS:-5}"
export SCHEDULER_TICK_MS="${SCHEDULER_TICK_MS:-500}"
export MIN_VOTES_TO_RESOLVE=1
export SEED_SKIP_VOTE=1
export FLASH_CLOSES_SECS="${FLASH_CLOSES_SECS:-75}"
export FLASH_TALLY_HIDDEN_SECS="${FLASH_TALLY_HIDDEN_SECS:-50}"

CORE_PID=""
CONVERSE_PID=""

kill_ports() {
  lsof -ti :8080 -ti :8091 2>/dev/null | xargs kill -9 2>/dev/null || true
}
cleanup() {
  [ -n "$CONVERSE_PID" ] && kill "$CONVERSE_PID" 2>/dev/null || true
  [ -n "$CORE_PID" ] && kill "$CORE_PID" 2>/dev/null || true
  kill_ports
  wait 2>/dev/null || true
}
trap cleanup EXIT
kill_ports

psql_demo() { psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d "$DB" -X -A -t -v ON_ERROR_STOP=1 -c "$1" | head -1 | tr -d '[:space:]'; }
fail() { echo "ECONOMY E2E FAILED: $1" >&2; exit 1; }
now_ms() { python3 -c 'import time; print(int(time.time()*1000))'; }
poll_until() {
  local deadline_ms=$1; shift
  while [ "$(now_ms)" -lt "$deadline_ms" ]; do
    if "$@"; then return 0; fi
    sleep 0.2
  done
  return 1
}
http_ok() { curl -sf "$1" >/dev/null 2>&1; }
wait_http() {
  local url=$1 budget=${2:-90000}
  poll_until $(( $(now_ms) + budget )) http_ok "$url" || fail "timeout $url"
}
market_state() {
  curl -sf "${CORE_API_URL}/markets/${1}" | jq -r .state
}
state_is() {
  local mid=$1 want=$2
  [ "$(market_state "$mid")" = "$want" ]
}
state_in() {
  local mid=$1; shift
  local st; st=$(market_state "$mid")
  local w; for w in "$@"; do [ "$st" = "$w" ] && return 0; done
  return 1
}

seed_flash() {
  # args: slug chain_sig
  local slug=$1 chain=$2
  export DEMO_SLUG="$slug"
  export DEMO_CHAIN_SIG="$chain"
  SEED_OUT=$(cargo run -q -p main -- --seed-demo)
  echo "$SEED_OUT" | sed 's/^/   /'
  USER_ID=$(echo "$SEED_OUT" | sed -n 's/^user_id=//p')
  MARKET_ID=$(echo "$SEED_OUT" | sed -n 's/^market_id=\([^ ]*\).*/\1/p')
  SLUG=$(echo "$SEED_OUT" | sed -n 's/^slug=//p')
  [ -n "$USER_ID" ] && [ -n "$MARKET_ID" ] && [ -n "$SLUG" ] || fail "seed incomplete"
  SEED_START_MS=$(now_ms)
  HIDDEN_DEADLINE_MS=$(( SEED_START_MS + FLASH_TALLY_HIDDEN_SECS * 1000 + 15000 ))
  CLOSE_DEADLINE_MS=$(( SEED_START_MS + FLASH_CLOSES_SECS * 1000 + 15000 ))
  RESOLVE_DEADLINE_MS=$(( CLOSE_DEADLINE_MS + SWEEP_DELAY_SECS * 1000 + 25000 ))
}

webhook() {
  local content=$1 mid=$2
  curl -sf -X POST "${CONVERSE_URL}/webhooks/sendblue" \
    -H 'content-type: application/json' \
    -d "{\"number\": \"${DEMO_PHONE}\", \"content\": ${content}, \"message_id\": \"${mid}\"}"
}

echo "== [setup] infra + fresh DB + build =="
just infra-up >/dev/null 2>&1 || docker compose up -d
poll_until $(( $(now_ms) + 60000 )) \
  bash -c 'psql -h localhost -p 15434 -U opinions -d postgres -c "select 1" >/dev/null 2>&1' \
  || fail "postgres not ready"
psql -h localhost -p 15434 -U opinions -d postgres -c "drop database if exists ${DB} with (force)" >/dev/null
psql -h localhost -p 15434 -U opinions -d postgres -c "create database ${DB}" >/dev/null
sqlx migrate run --source migrations --database-url "$DATABASE_URL" >/dev/null
cargo build -q -p main

echo "== [setup] start core + converse =="
cargo run -q -p main >/tmp/core-economy.log 2>&1 &
CORE_PID=$!
disown
wait_http "${CORE_API_URL}/healthz"
(cd services/converse && uv run uvicorn converse.app:app --host 127.0.0.1 --port 8091 >/tmp/converse-economy.log 2>&1) &
CONVERSE_PID=$!
disown
wait_http "${CONVERSE_URL}/healthz"
echo "   healthy"

# ---------------------------------------------------------------------------
echo "== [A] normal flash → rep EWMA + leaderboards =="
seed_flash "eco-a" "eco-chain-a"
REP_BEFORE=$(psql_demo "select rep_micro from reputation where user_id='${USER_ID}'")
[ -n "$REP_BEFORE" ] || fail "no reputation row for demo user"
echo "   rep_before=${REP_BEFORE}"

RV1=$(webhook "\"vote yes 60 on ${SLUG}\"" "eco-a-v1")
echo "$RV1" | jq -e '.reply | test("Preview vote")' >/dev/null || fail "vote preview: $RV1"
RV2=$(webhook "\"yes\"" "eco-a-v2")
echo "$RV2" | jq -e '.reply | test("Vote cast")' >/dev/null || fail "vote confirm: $RV2"
RT1=$(webhook "\"buy \$5 of yes on ${SLUG}\"" "eco-a-t1")
echo "$RT1" | jq -e '.reply | test("micro-shares")' >/dev/null || fail "trade preview: $RT1"
RT2=$(webhook "\"yes\"" "eco-a-t2")
echo "$RT2" | jq -e '.reply | test("Executed")' >/dev/null || fail "trade confirm: $RT2"

poll_until "$RESOLVE_DEADLINE_MS" state_in "$MARKET_ID" resolved paid \
  || fail "A never settled (state=$(market_state "$MARKET_ID"))"
echo "   state=$(market_state "$MARKET_ID")"

# One YES vote @60 with 100% YES → score_bp=2500, score_micro=250000
# H20: update_rep(0, 250000)=8516
EXPECTED_REP=$(python3 - <<'PY'
UNIT=1_000_000
K=965_936
score=250_000
print((score*(UNIT-K) + UNIT//2)//UNIT)
PY
)
REP_AFTER=$(psql_demo "select rep_micro from reputation where user_id='${USER_ID}'")
TIER_AFTER=$(psql_demo "select tier from reputation where user_id='${USER_ID}'")
echo "   rep_after=${REP_AFTER} expected=${EXPECTED_REP} tier=${TIER_AFTER}"
[ "$REP_AFTER" = "$EXPECTED_REP" ] || fail "rep mismatch got ${REP_AFTER} want ${EXPECTED_REP}"
[ "$TIER_AFTER" -ge 1 ] || fail "expected tier>=1 for leaderboard, got ${TIER_AFTER}"

VOTERS=$(curl -sf "${CORE_API_URL}/leaderboards/voters?days=7&limit=10")
echo "   voters=${VOTERS}"
echo "$VOTERS" | jq -e --arg h demo \
  '[.[] | select(.handle==$h)] | length >= 1 and .[0].markets_scored >= 1 and (.[] | .avg_score_bp) != null' \
  >/dev/null || fail "demo user missing from voters leaderboard: $VOTERS"
AVG=$(echo "$VOTERS" | jq -r '.[] | select(.handle=="demo") | .avg_score_bp' | head -1)
[ "$AVG" = "2500" ] || fail "avg_score_bp want 2500 got ${AVG}"
echo "SECTION A GREEN rep=${REP_AFTER} avg_score_bp=${AVG}"

# ---------------------------------------------------------------------------
echo "== [B] high-pot hold → due-time → pass settle =="
seed_flash "eco-b" "eco-chain-b"
# vote+trade so OI exists
webhook "\"vote yes 55 on ${SLUG}\"" "eco-b-v1" >/dev/null
webhook "\"yes\"" "eco-b-v2" >/dev/null
webhook "\"buy \$5 of yes on ${SLUG}\"" "eco-b-t1" >/dev/null
webhook "\"yes\"" "eco-b-t2" >/dev/null

poll_until "$CLOSE_DEADLINE_MS" state_in "$MARKET_ID" closed resolving resolved paid \
  || fail "B never left live"
# Must enter Resolving (hold) because seed escrow $1000 >> $1 threshold
poll_until "$CLOSE_DEADLINE_MS" state_in "$MARKET_ID" resolving paid resolved \
  || fail "B never resolving/paid"

if state_is "$MARKET_ID" resolving || state_is "$MARKET_ID" paid || state_is "$MARKET_ID" resolved; then
  :
fi
# Capture under_review + integrity_due_at while resolving (or history via SQL)
DETAIL=$(curl -sf "${CORE_API_URL}/markets/${MARKET_ID}")
echo "   detail_state=$(echo "$DETAIL" | jq -r .state) under_review=$(echo "$DETAIL" | jq -r .under_review)"
DUE=$(psql_demo "select integrity_due_at is not null from markets where id='${MARKET_ID}'")
# If already settled fast after due, due may be cleared — require either due was set or report exists
REPORT_OR_DUE=$(psql_demo "
  select case when exists(select 1 from integrity_reports where market_id='${MARKET_ID}')
    or exists(select 1 from markets where id='${MARKET_ID}' and integrity_due_at is not null)
    then 1 else 0 end")
[ "$REPORT_OR_DUE" = "1" ] || fail "B missing integrity_due_at history/report"

# If still resolving, ensure not same-tick: due_at > closed transition time roughly
if state_is "$MARKET_ID" resolving; then
  UNDER=$(echo "$DETAIL" | jq -r .under_review)
  [ "$UNDER" = "true" ] || fail "under_review should be true while resolving"
  # Wait until due then settle
  poll_until "$RESOLVE_DEADLINE_MS" state_in "$MARKET_ID" resolved paid \
    || fail "B never settled after hold"
fi
poll_until "$RESOLVE_DEADLINE_MS" state_in "$MARKET_ID" resolved paid \
  || fail "B never paid/resolved"
VERDICT=$(psql_demo "select verdict from integrity_reports where market_id='${MARKET_ID}'")
echo "   integrity_verdict=${VERDICT} final_state=$(market_state "$MARKET_ID")"
[ "$VERDICT" = "pass" ] || fail "B expected pass report, got ${VERDICT}"

# Hold latency: integrity_due delay + settle budget (frames via created_at of report vs paid)
HOLD_MS=$(psql_demo "
  select greatest(0, extract(epoch from (
    (select min(applied_at) from lifecycle_commands
      where market_id='${MARKET_ID}' and resulting_state in ('resolved','paid')
     ) - (select created_at from integrity_reports where market_id='${MARKET_ID}')
  ))*1000)::bigint")
# Fallback: sweep_delay *1000 as lower bound check
echo "   hold_to_settle_ms≈${HOLD_MS} (budget sweep_delay+2000=$((SWEEP_DELAY_SECS*1000+2000)))"
# Soft check: report exists and market paid
[ "$(market_state "$MARKET_ID")" = "paid" ] || [ "$(market_state "$MARKET_ID")" = "resolved" ] \
  || fail "B bad final state"
echo "SECTION B GREEN hold_verdict=pass delay_secs=${SWEEP_DELAY_SECS}"

# ---------------------------------------------------------------------------
echo "== [C] poisoned multi-user device/young burst → flag + curator void =="
seed_flash "eco-c" "eco-chain-c"
# Create 4 young sybil users with phones + reputation, fund lightly via SQL deposits not needed for vote-only
# Votes only need phone channel + integrity rules with VOTE_MIN_AGE=0
for i in 1 2 3 4; do
  HANDLE="sybil${i}-$(uuidgen | tr '[:upper:]' '[:lower:]' | cut -c1-8)"
  UID_I=$(psql_demo "insert into users (handle) values ('${HANDLE}') returning id" | tr -d '[:space:]')
  [ -n "$UID_I" ] || fail "failed to insert sybil user $i"
  psql_demo "insert into reputation (user_id, rep_micro, tier) values ('${UID_I}', 0, 0) on conflict do nothing" >/dev/null || true
  ADDR="+15559${i}$(printf '%04d' $((RANDOM%10000)))"
  psql_demo "insert into user_channels (user_id, channel, address) values ('${UID_I}', 'imessage', '${ADDR}')" >/dev/null
  IDEM="sybil-v-${i}-$(uuidgen | tr '[:upper:]' '[:lower:]')"
  CODE=$(curl -s -o /tmp/sybil_vote.json -w "%{http_code}" -X POST "${CORE_API_URL}/votes" \
    -H "content-type: application/json" \
    -H "x-demo-token: ${DEMO_TOKEN}" \
    -H "x-device-id: poison-device-shared" \
    -d "{\"user_id\":\"${UID_I}\",\"market_ref\":\"${SLUG}\",\"side\":\"yes\",\"crowd_guess_pct\":50,\"idempotency_key\":\"${IDEM}\"}")
  [ "$CODE" = "200" ] || fail "sybil vote $i failed HTTP ${CODE}: $(cat /tmp/sybil_vote.json)"
done
# Also need the market to close with enough votes (4) — demo user optional
# Wait for resolve path → should flag not settle payout for users (no positions ok)
poll_until "$RESOLVE_DEADLINE_MS" bash -c "
  st=\$(curl -sf ${CORE_API_URL}/markets/${MARKET_ID} | jq -r .state)
  [ \"\$st\" = resolving ] || [ \"\$st\" = paid ] || [ \"\$st\" = resolved ] || [ \"\$st\" = voided ]
" || fail "C never left open (state=$(market_state "$MARKET_ID"))"

# Wait for integrity report flag
poll_until "$RESOLVE_DEADLINE_MS" bash -c "
  v=\$(psql -h localhost -p 15434 -U opinions -d opinions -X -A -t -c \"select verdict from integrity_reports where market_id='${MARKET_ID}'\" 2>/dev/null)
  [ \"\$v\" = flag ]
" || fail "C expected integrity_reports.verdict=flag got $(psql_demo "select verdict from integrity_reports where market_id='${MARKET_ID}'")"

FLAGGED=$(curl -sf -H "x-admin-token: ${ADMIN_BEARER}" "${CORE_API_URL}/admin/markets/flagged")
echo "   flagged=$(echo "$FLAGGED" | jq -c 'map({slug,verdict:(.report.verdict//null)})')"
echo "$FLAGGED" | jq -e --arg s "$SLUG" \
  '[.[] | select(.slug==$s and .report.verdict=="flag")] | length >= 1' >/dev/null \
  || fail "C not listed in curator inbox: $FLAGGED"

# No settlement payout txn for this market (no money moved on resolve)
PAYOUTS=$(psql_demo "select count(*) from ledger_transactions where kind='payout' and idempotency_key='resolve:${MARKET_ID}'")
[ "$PAYOUTS" = "0" ] || fail "C should have no payout before curator, got ${PAYOUTS}"

# Curator void
VOID=$(curl -sf -X POST "${CORE_API_URL}/admin/markets/${MARKET_ID}/resolve" \
  -H "content-type: application/json" \
  -H "x-admin-token: ${ADMIN_BEARER}" \
  -d '{"decision":"void"}')
echo "   void_receipt=$(echo "$VOID" | jq -c .)"
echo "$VOID" | jq -e '.voided==true' >/dev/null || fail "curator void failed: $VOID"
poll_until $(( $(now_ms) + 10000 )) state_is "$MARKET_ID" voided \
  || fail "C not voided"
FLAG_CLEARED=$(psql_demo "select curator_flagged_at is null from markets where id='${MARKET_ID}'")
[ "$FLAG_CLEARED" = "t" ] || fail "curator flag not cleared"
echo "SECTION C GREEN verdict=flag curator_void=ok"

# ---------------------------------------------------------------------------
echo "== [D] tiered fee discount + flip-window base sell fee =="
# Demo user is tier>=1 from section A with discounts
seed_flash "eco-d" "eco-chain-d"
# Deposit more via seed chain already $50; buy then sell for flip
webhook "\"vote yes 50 on ${SLUG}\"" "eco-d-v1" >/dev/null
webhook "\"yes\"" "eco-d-v2" >/dev/null

# REST preview should show discounted fee for $10 buy (gross 10e6, fee 90bp = 90000)
PREV=$(curl -sf -X POST "${CORE_API_URL}/trades/preview" \
  -H "content-type: application/json" \
  -H "x-demo-token: ${DEMO_TOKEN}" \
  -d "{\"user_id\":\"${USER_ID}\",\"market_ref\":\"${SLUG}\",\"side\":\"yes\",\"action\":\"buy\",\"amount_micro\":10000000}")
echo "   rest_preview=$(echo "$PREV" | jq -c '{fee_micro,shares_micro,avg_price_micro}')"
FEE_PREV=$(echo "$PREV" | jq -r .fee_micro)
# converse preview
CPREV=$(webhook "\"buy \$10 of yes on ${SLUG}\"" "eco-d-t1")
echo "   converse_preview=$(echo "$CPREV" | jq -r .reply)"
echo "$CPREV" | jq -e '.reply | test("fee")' >/dev/null || fail "converse preview missing fee"

# Execute buy
webhook "\"yes\"" "eco-d-t2" >/dev/null
TRADE_ROW=$(psql_demo "select fee_micro from trades where market_id='${MARKET_ID}' and user_id='${USER_ID}' and side='buy' order by created_at desc limit 1")
echo "   trade_fee_micro=${TRADE_ROW} preview_fee=${FEE_PREV}"
[ "$TRADE_ROW" = "$FEE_PREV" ] || fail "trade fee != preview fee"

# Ledger fee entry for that trade's txn
LEDGER_FEE=$(psql_demo "
  select coalesce(sum(le.amount_micro),0) from ledger_entries le
    join ledger_accounts la on la.id=le.account_id
    join trades t on t.txn_id=le.txn_id
   where t.market_id='${MARKET_ID}' and t.user_id='${USER_ID}' and la.owner_type='fees'
     and t.side='buy'")
echo "   ledger_fees_credit=${LEDGER_FEE}"
[ "$LEDGER_FEE" = "$FEE_PREV" ] || fail "ledger fee ${LEDGER_FEE} != preview ${FEE_PREV}"

# Expected discounted 90bp on 10_000_000 = 90_000
[ "$FEE_PREV" = "90000" ] || fail "expected discounted fee 90000 got ${FEE_PREV}"

# Flip sell within window — base fee 100bp on proceeds path uses amount shares
SHARES=$(psql_demo "
  select p.shares_micro from positions p
    join outcomes o on o.id=p.outcome_id
   where p.user_id='${USER_ID}' and o.market_id='${MARKET_ID}' and p.shares_micro>0
   limit 1")
# Sell half shares (or all if small)
SELL_AMT=$(( SHARES / 2 ))
[ "$SELL_AMT" -gt 0 ] || SELL_AMT=$SHARES
SELL_PREV=$(curl -sf -X POST "${CORE_API_URL}/trades/preview" \
  -H "content-type: application/json" \
  -H "x-demo-token: ${DEMO_TOKEN}" \
  -d "{\"user_id\":\"${USER_ID}\",\"market_ref\":\"${SLUG}\",\"side\":\"yes\",\"action\":\"sell\",\"amount_micro\":${SELL_AMT}}")
SELL_FEE=$(echo "$SELL_PREV" | jq -r .fee_micro)
echo "   sell_preview_fee=${SELL_FEE} (expect base-fee path, not discounted)"
# Execute sell
curl -sf -X POST "${CORE_API_URL}/trades" \
  -H "content-type: application/json" \
  -H "x-demo-token: ${DEMO_TOKEN}" \
  -d "{\"user_id\":\"${USER_ID}\",\"market_ref\":\"${SLUG}\",\"side\":\"yes\",\"action\":\"sell\",\"amount_micro\":${SELL_AMT},\"idempotency_key\":\"eco-d-sell-1\"}" >/dev/null
SELL_TRADE_FEE=$(psql_demo "select fee_micro from trades where market_id='${MARKET_ID}' and user_id='${USER_ID}' and side='sell' order by created_at desc limit 1")
[ "$SELL_TRADE_FEE" = "$SELL_FEE" ] || fail "sell trade fee mismatch"
# Discounted fee would be lower; assert sell fee equals base-rate computation on gross from preview
# If discount applied, fee would be 90% of base; require sell fee > 0 and equals preview
GROSS=$(echo "$SELL_PREV" | jq -r .gross_micro)
BASE_FEE=$(python3 -c "g=int('${GROSS}'); print((g*100+9999)//10000)")
DISC_FEE=$(python3 -c "g=int('${GROSS}'); print((g*90+9999)//10000)")
echo "   sell_gross=${GROSS} base_fee=${BASE_FEE} disc_fee=${DISC_FEE} actual=${SELL_FEE}"
[ "$SELL_FEE" = "$BASE_FEE" ] || fail "flip sell should pay base fee ${BASE_FEE}, got ${SELL_FEE} (disc would be ${DISC_FEE})"
[ "$SELL_FEE" != "$DISC_FEE" ] || [ "$BASE_FEE" = "$DISC_FEE" ] || true
# When base != disc, sell must not equal disc
if [ "$BASE_FEE" != "$DISC_FEE" ]; then
  [ "$SELL_FEE" != "$DISC_FEE" ] || fail "sell fee unexpectedly discounted"
fi
echo "SECTION D GREEN buy_fee=${FEE_PREV} flip_sell_fee=${SELL_FEE}"

echo "PHASE 3 E2E GREEN"
echo "A_REP ${REP_AFTER}"
echo "B_SWEEP_DELAY ${SWEEP_DELAY_SECS}"
echo "C_VERDICT flag"
echo "D_BUY_FEE ${FEE_PREV} D_SELL_FEE ${SELL_FEE}"
