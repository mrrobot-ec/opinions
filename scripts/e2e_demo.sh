#!/usr/bin/env bash
# THE E2E DEMO (plan Task 1.5 steps 1-8): webhook text → preview → lexical
# confirm → executed trade → visible position, proven down to psql invariants.
set -euo pipefail

cd "$(dirname "$0")/.."

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
CONVERSE_URL="http://127.0.0.1:8091"

CORE_PID=""
CONVERSE_PID=""
kill_ports() {
  # cargo/uv wrap the real servers in child processes; reap by port so a
  # stale listener can never answer the next run's health checks.
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

psql_demo() { psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d "$DB" -X -A -t -c "$1"; }

fail() { echo "E2E FAILED: $1" >&2; exit 1; }

wait_http() { # url attempts
  local url=$1 attempts=${2:-60}
  for _ in $(seq 1 "$attempts"); do
    if curl -sf "$url" >/dev/null 2>&1; then return 0; fi
    sleep 0.5
  done
  fail "timeout waiting for $url"
}

echo "== [1/8] infra up + fresh database =="
just infra-up >/dev/null
for _ in $(seq 1 60); do
  if psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -c "select 1" >/dev/null 2>&1; then break; fi
  sleep 0.5
done
psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -c "drop database if exists ${DB} with (force)" >/dev/null
psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -c "create database ${DB}" >/dev/null
echo "   fresh ${DB} created (migrations run inside the core binary)"

echo "== [2/8] build + seed demo world (idempotent, via use cases) =="
cargo build -q -p main
SEED_OUT=$(cargo run -q -p main -- --seed-demo)
echo "$SEED_OUT" | sed 's/^/   /'
USER_ID=$(echo "$SEED_OUT" | sed -n 's/^user_id=//p')
MARKET_ID=$(echo "$SEED_OUT" | sed -n 's/^market_id=\([^ ]*\).*/\1/p')
[ -n "$USER_ID" ] && [ -n "$MARKET_ID" ] || fail "seed did not print user/market ids"
# Idempotency: a second run must replay, not duplicate.
SEED2=$(cargo run -q -p main -- --seed-demo)
echo "$SEED2" | grep -q "market_id=${MARKET_ID} replayed=true" || fail "seed re-run did not replay the market"

echo "== [3/8] start core + converse =="
cargo run -q -p main &
CORE_PID=$!
disown
wait_http "${CORE_API_URL}/healthz"
(cd services/converse && uv run uvicorn converse.app:app --host 127.0.0.1 --port 8091 >/tmp/converse-e2e.log 2>&1) &
CONVERSE_PID=$!
disown
wait_http "${CONVERSE_URL}/healthz"
echo "   core + converse healthy"

echo "== [4/8] webhook: preview =="
R1=$(curl -sf -X POST "${CONVERSE_URL}/webhooks/sendblue" \
  -H 'content-type: application/json' \
  -d "{\"number\": \"${DEMO_PHONE}\", \"content\": \"buy \$5 of yes on demo-coffee\", \"message_id\": \"e2e-1\"}")
echo "   reply 1: $(echo "$R1" | jq -r .reply)"
echo "$R1" | jq -e '.reply | test("micro-shares") and test("fee") and (ascii_downcase | test("yes"))' >/dev/null \
  || fail "preview reply must mention shares, fee and yes: $R1"

echo "== [5/8] webhook: lexical confirm =="
R2=$(curl -sf -X POST "${CONVERSE_URL}/webhooks/sendblue" \
  -H 'content-type: application/json' \
  -d "{\"number\": \"${DEMO_PHONE}\", \"content\": \"yes\", \"message_id\": \"e2e-2\"}")
echo "   reply 2: $(echo "$R2" | jq -r .reply)"
echo "$R2" | jq -e '.reply | test("Executed") and test("bought")' >/dev/null \
  || fail "confirm reply must be an executed trade: $R2"

echo "== [6/8] position visible via core API =="
POS=$(curl -sf "${CORE_API_URL}/users/${USER_ID}/positions")
echo "   positions: $POS"
echo "$POS" | jq -e '[.[] | select(.side == "yes" and .shares_micro > 0)] | length >= 1' >/dev/null \
  || fail "user must hold YES shares: $POS"

echo "== [7/8] psql invariants (grok-p1r1 M6 strengthened) =="
RUNS_OK=$(psql_demo "select count(*) from agent_runs where status = 'ok'")
[ "$RUNS_OK" = "2" ] || fail "expected 2 ok agent runs, got ${RUNS_OK}"

PENDING_OPEN=$(psql_demo "select count(*) from pending_actions where consumed_at is null")
[ "$PENDING_OPEN" = "0" ] || fail "pending_actions left unconsumed: ${PENDING_OPEN}"
PENDING_TOTAL=$(psql_demo "select count(*) from pending_actions")
[ "$PENDING_TOTAL" -ge 1 ] || fail "no pending_actions row recorded"

# consume-after-txn ordering: consumed_at must be at/after the ledger txn.
ORDERED=$(psql_demo "
  select count(*) from pending_actions p
    join trades t on t.pending_action_id = p.id
    join ledger_transactions lt on lt.id = t.txn_id
   where p.consumed_at >= lt.created_at")
[ "$ORDERED" = "1" ] || fail "pending consume must happen AFTER the trade's ledger txn (got ${ORDERED})"

CHAIN=$(psql_demo "select count(*) from trades where run_id is not null and pending_action_id is not null and txn_id is not null")
[ "$CHAIN" = "1" ] || fail "trade must carry run_id + pending_action_id + txn_id (got ${CHAIN})"

UNBALANCED=$(psql_demo "
  select count(*) from (
    select la.currency, sum(le.amount_micro) as s
      from ledger_entries le join ledger_accounts la on la.id = le.account_id
     group by la.currency having sum(le.amount_micro) <> 0
  ) bad")
[ "$UNBALANCED" = "0" ] || fail "ledger not per-currency zero-sum"

# Escrow invariant, defined: escrow == total YES micro-shares == total NO
# micro-shares, each total = user positions + pool reserve for that side
# (every micro-share pair is backed by exactly 1 micro-USD in escrow).
read -r ESCROW YES_TOTAL NO_TOTAL <<EOF2
$(psql_demo "
  with escrow as (
    select coalesce(sum(le.amount_micro), 0) as v
      from ledger_entries le
      join ledger_accounts la on la.id = le.account_id
     where la.owner_type = 'escrow' and la.owner_id = '${MARKET_ID}'
  ),
  sides as (
    select o.idx,
           coalesce((select sum(p.shares_micro) from positions p where p.outcome_id = o.id), 0)
           + coalesce((select sum(r.reserve_micro_shares) from pool_reserves r where r.outcome_id = o.id), 0) as total
      from outcomes o where o.market_id = '${MARKET_ID}'
  )
  select (select v from escrow),
         (select total from sides where idx = 0),
         (select total from sides where idx = 1)" | tr '|' ' ')
EOF2
echo "   escrow=${ESCROW} yes_total=${YES_TOTAL} no_total=${NO_TOTAL}"
[ "$ESCROW" = "$YES_TOTAL" ] && [ "$ESCROW" = "$NO_TOTAL" ] \
  || fail "complete-set collateralization violated: escrow=${ESCROW} yes=${YES_TOTAL} no=${NO_TOTAL}"

echo "== [8/8] all asserts green =="
echo "E2E DEMO GREEN"
