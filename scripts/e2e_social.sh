#!/usr/bin/env bash
# Phase 4 social + notifications e2e (plan Task 4.5).
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

# Social knobs for this run
export REPORT_SHADOW_THRESHOLD="${REPORT_SHADOW_THRESHOLD:-3}"
export REPORTER_MIN_AGE_SECS="${REPORTER_MIN_AGE_SECS:-259200}" # 72h
export REPORTER_MIN_TIER="${REPORTER_MIN_TIER:-1}"
export MENTION_NOTIFS_PER_HOUR="${MENTION_NOTIFS_PER_HOUR:-3}"
export MAX_COMMENTS_PER_WINDOW="${MAX_COMMENTS_PER_WINDOW:-200}"
export MAX_REPORTS_PER_WINDOW="${MAX_REPORTS_PER_WINDOW:-50}"
export PAYOUT_HOLD_THRESHOLD_MICRO="${PAYOUT_HOLD_THRESHOLD_MICRO:-999999999999}" # skip hold
export MIN_VOTES_TO_RESOLVE=1
export SEED_SKIP_VOTE=1
export VOTE_MIN_ACCOUNT_AGE_SECS=0
export VOTE_NEAR_CLOSE_SECS=5
export SCHEDULER_TICK_MS="${SCHEDULER_TICK_MS:-500}"
export FLASH_CLOSES_SECS="${FLASH_CLOSES_SECS:-45}"
export FLASH_TALLY_HIDDEN_SECS="${FLASH_TALLY_HIDDEN_SECS:-30}"
export SWEEP_DELAY_SECS="${SWEEP_DELAY_SECS:-2}"
export ADMIN_HANDLES="${ADMIN_HANDLES:-admin}"

CORE_PID=""
PROBE_PID=""
PROBE_OUT="/tmp/ws_user_social_$$.jsonl"

kill_ports() {
  lsof -ti :8080 2>/dev/null | xargs kill -9 2>/dev/null || true
}
cleanup() {
  [ -n "${PROBE_PID:-}" ] && kill "$PROBE_PID" 2>/dev/null || true
  [ -n "${CORE_PID:-}" ] && kill "$CORE_PID" 2>/dev/null || true
  kill_ports
  wait 2>/dev/null || true
}
trap cleanup EXIT
kill_ports

psql_demo() { psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d "$DB" -X -A -t -v ON_ERROR_STOP=1 -c "$1" | head -1 | tr -d '[:space:]'; }
psql_q() { psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d "$DB" -X -A -t -v ON_ERROR_STOP=1 -c "$1"; }
fail() { echo "SOCIAL E2E FAILED: $1" >&2; exit 1; }
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
state_in() {
  local mid=$1; shift
  local st; st=$(market_state "$mid")
  local w; for w in "$@"; do [ "$st" = "$w" ] && return 0; done
  return 1
}
uuid() { uuidgen | tr '[:upper:]' '[:lower:]'; }
# Mentions only match @[a-z0-9_]{3,32} — no hyphens.
rand_handle() {
  local prefix=$1
  echo "${prefix}$(uuid | tr -d '-' | cut -c1-10)"
}

auth_hdr() { echo "x-demo-token: ${DEMO_TOKEN}"; }
admin_hdr() { echo "x-admin-token: ${ADMIN_BEARER}"; }

create_user() {
  # args: handle [age_secs_ago] [tier]
  local handle=$1
  local age=${2:-0}
  local tier=${3:-0}
  local uid
  uid=$(psql_demo "insert into users (handle) values ('${handle}') returning id")
  [ -n "$uid" ] || fail "insert user ${handle}"
  if [ "$age" -gt 0 ]; then
    psql_demo "update users set created_at = now() - interval '${age} seconds' where id='${uid}'" >/dev/null
  fi
  psql_demo "insert into reputation (user_id, rep_micro, tier) values ('${uid}', 0, ${tier}) on conflict (user_id) do update set tier=${tier}" >/dev/null
  local phone="+1555$(printf '%07d' $((RANDOM % 10000000)))"
  psql_demo "insert into user_channels (user_id, channel, address) values ('${uid}', 'imessage', '${phone}')" >/dev/null
  # Fund for trading if needed: credit via ledger is complex; use credit_deposit path through seed house
  # For vote-only no funds needed. For holder path we use demo user from seed.
  echo "$uid"
}

post_comment() {
  local market=$1 user=$2 body=$3 parent=${4:-}
  local idem; idem=$(uuid)
  local payload
  if [ -n "$parent" ]; then
    payload=$(jq -nc --arg u "$user" --arg b "$body" --arg p "$parent" --arg k "$idem" \
      '{user_id:$u, body:$b, parent_id:$p, idempotency_key:$k}')
  else
    payload=$(jq -nc --arg u "$user" --arg b "$body" --arg k "$idem" \
      '{user_id:$u, body:$b, parent_id:null, idempotency_key:$k}')
  fi
  curl -sf -X POST "${CORE_API_URL}/markets/${market}/comments" \
    -H "content-type: application/json" \
    -H "$(auth_hdr)" \
    -d "$payload"
}

vote_comment() {
  local cid=$1 user=$2 val=$3
  local idem; idem=$(uuid)
  curl -sf -X POST "${CORE_API_URL}/comments/${cid}/vote" \
    -H "content-type: application/json" \
    -H "$(auth_hdr)" \
    -d "{\"user_id\":\"${user}\",\"value\":${val},\"idempotency_key\":\"${idem}\"}"
}

report_comment_http() {
  local cid=$1 user=$2
  # return body + code
  curl -s -o /tmp/report_body.json -w "%{http_code}" -X POST "${CORE_API_URL}/comments/${cid}/report" \
    -H "content-type: application/json" \
    -H "$(auth_hdr)" \
    -d "{\"user_id\":\"${user}\"}"
}

list_comments() {
  local market=$1 sort=${2:-hot} extra=${3:-}
  curl -sf "${CORE_API_URL}/markets/${market}/comments?sort=${sort}&limit=50${extra}" \
    -H "$(auth_hdr)"
}

unread() {
  curl -sf "${CORE_API_URL}/users/${1}/notifications/unread_count" -H "$(auth_hdr)" | jq -r .unread_count
}

list_notifs() {
  curl -sf "${CORE_API_URL}/users/${1}/notifications?limit=50" -H "$(auth_hdr)"
}

echo "== [setup] infra + fresh DB + core =="
just infra-up >/dev/null 2>&1 || docker compose up -d
poll_until $(( $(now_ms) + 60000 )) \
  bash -c 'psql -h localhost -p 15434 -U opinions -d postgres -c "select 1" >/dev/null 2>&1' \
  || fail "postgres not ready"
psql -h localhost -p 15434 -U opinions -d postgres -c "drop database if exists ${DB} with (force)" >/dev/null
psql -h localhost -p 15434 -U opinions -d postgres -c "create database ${DB}" >/dev/null
sqlx migrate run --source migrations --database-url "$DATABASE_URL" >/dev/null
# Confirm 0006 present
psql_demo "select to_regclass('public.outbox_cursors') is not null" | grep -q t || fail "0006 missing outbox_cursors"
cargo build -q -p main

cargo run -q -p main >/tmp/core-social.log 2>&1 &
CORE_PID=$!
disown
wait_http "${CORE_API_URL}/healthz"
echo "   healthy"

# ---------------------------------------------------------------------------
echo "== [seed] flash market + users A/B =="
export DEMO_SLUG="social-flash"
export DEMO_CHAIN_SIG="social-chain-001"
SEED_OUT=$(cargo run -q -p main -- --seed-demo)
echo "$SEED_OUT" | sed 's/^/   /'
DEMO_USER=$(echo "$SEED_OUT" | sed -n 's/^user_id=//p')
MARKET_ID=$(echo "$SEED_OUT" | sed -n 's/^market_id=\([^ ]*\).*/\1/p')
SLUG=$(echo "$SEED_OUT" | sed -n 's/^slug=//p')
[ -n "$DEMO_USER" ] && [ -n "$MARKET_ID" ] && [ -n "$SLUG" ] || fail "seed incomplete"
SEED_START_MS=$(now_ms)
RESOLVE_DEADLINE_MS=$(( SEED_START_MS + FLASH_CLOSES_SECS * 1000 + 30000 ))

# Age demo user for reporter floor (qualified path later)
psql_demo "update users set created_at = now() - interval '30 days' where id='${DEMO_USER}'" >/dev/null
psql_demo "update reputation set tier=2, rep_micro=500000 where user_id='${DEMO_USER}'" >/dev/null

USER_B=$(create_user "$(rand_handle userb)" 0 0)
USER_A=$(create_user "$(rand_handle usera)" 0 0)
# Ensure handles resolvable for @mentions (lowercase)
HANDLE_B=$(psql_demo "select handle from users where id='${USER_B}'")
HANDLE_A=$(psql_demo "select handle from users where id='${USER_A}'")
echo "   A=${USER_A} (@${HANDLE_A}) B=${USER_B} (@${HANDLE_B}) market=${MARKET_ID}"

# Start user-B WS probe early for reply collapse section
rm -f "$PROBE_OUT"
(cd scripts && node ws_user_probe.mjs --url "$WS_URL" --user "$USER_B" --token "$DEMO_TOKEN" \
  --out "$PROBE_OUT" --deadline-ms 90000 --min-notifs 1 --expect-notif-types comment_reply) \
  >/tmp/ws_user_probe.log 2>&1 &
PROBE_PID=$!
sleep 0.5  # allow subscribe handshake (not an assertion wait)

# ---------------------------------------------------------------------------
echo "== [1] comment + reply@mention collapse (exactly one notif for B) =="
CB=$(post_comment "$MARKET_ID" "$USER_B" "Hello from B — opening the thread")
CID_B=$(echo "$CB" | jq -r .id)
[ -n "$CID_B" ] && [ "$CID_B" != null ] || fail "B comment failed: $CB"
echo "   B_comment=${CID_B}"

CA=$(post_comment "$MARKET_ID" "$USER_A" "Replying to @${HANDLE_B} with a mention" "$CID_B")
CID_A=$(echo "$CA" | jq -r .id)
[ -n "$CID_A" ] && [ "$CID_A" != null ] || fail "A reply failed: $CA"
echo "   A_reply=${CID_A}"

# Wait for materializer + notif row
poll_until $(( $(now_ms) + 30000 )) bash -c "
  n=\$(psql -h localhost -p 15434 -U opinions -d opinions -X -A -t -c \
    \"select count(*) from notifications where user_id='${USER_B}'\" | tr -d '[:space:]')
  [ \"\$n\" -ge 1 ]
" || fail "B never received notification rows"

NOTIFS_B=$(list_notifs "$USER_B")
echo "   B_notifs=$(echo "$NOTIFS_B" | jq -c '{count:(.notifications|length), types:[.notifications[].type], unread:.unread_count}')"
COUNT_B=$(echo "$NOTIFS_B" | jq '.notifications | length')
[ "$COUNT_B" = "1" ] || fail "B expected EXACTLY 1 notification (reply/mention collapse), got ${COUNT_B}: $NOTIFS_B"
TYPE_B=$(echo "$NOTIFS_B" | jq -r '.notifications[0].type')
[ "$TYPE_B" = "comment_reply" ] || fail "expected comment_reply, got ${TYPE_B}"
# Must not also have mention for same event chain
MENTION_N=$(echo "$NOTIFS_B" | jq '[.notifications[] | select(.type=="mention")] | length')
[ "$MENTION_N" = "0" ] || fail "mention should collapse into reply"

# Unread moved
UNREAD_B=$(unread "$USER_B")
[ "$UNREAD_B" -ge 1 ] || fail "unread should move for B, got ${UNREAD_B}"

# WS frame path
poll_until $(( $(now_ms) + 20000 )) bash -c "
  grep -q '\"type\":\"notif\"' '${PROBE_OUT}' 2>/dev/null
" || {
  # probe may still be running; check log
  cat /tmp/ws_user_probe.log || true
  fail "no WS notif frame for B in ${PROBE_OUT}"
}
WS_TYPES=$(python3 - <<PY
import json
types=[]
with open("${PROBE_OUT}") as f:
  for line in f:
    line=line.strip()
    if not line: continue
    o=json.loads(line)
    if "frame" in o and o["frame"].get("type")=="notif":
      types.append(o["frame"].get("notification_type"))
print(",".join(types))
PY
)
echo "   ws_notif_types=${WS_TYPES}"
echo "$WS_TYPES" | grep -q comment_reply || fail "WS missing comment_reply frame"
echo "SECTION 1 GREEN reply_mention_collapse=1 unread=${UNREAD_B} ws=ok"

# ---------------------------------------------------------------------------
echo "== [2] comment vote flips hot ordering =="
# Second root comment that will get upvoted above first
C2=$(post_comment "$MARKET_ID" "$USER_A" "Second root — will be hot after votes")
CID2=$(echo "$C2" | jq -r .id)
# Third voter users to upvote C2
V1=$(create_user "$(rand_handle voter1)")
V2=$(create_user "$(rand_handle voter2)")
vote_comment "$CID2" "$V1" 1 >/dev/null
vote_comment "$CID2" "$V2" 1 >/dev/null
# Optional downvote B's root so C2 clearly wins
vote_comment "$CID_B" "$V1" -1 >/dev/null || true

HOT=$(list_comments "$MARKET_ID" hot)
TOP_ID=$(echo "$HOT" | jq -r '.comments[0].id')
TOP_SCORE=$(echo "$HOT" | jq -r '.comments[0].score')
echo "   hot_top=${TOP_ID} score=${TOP_SCORE}"
[ "$TOP_ID" = "$CID2" ] || fail "expected ${CID2} on top of hot, got ${TOP_ID}: $(echo "$HOT" | jq -c '[.comments[]|{id,score}]')"
echo "SECTION 2 GREEN hot_top=${TOP_ID} score=${TOP_SCORE}"

# ---------------------------------------------------------------------------
echo "== [3] brigade: young/tier-0 fail floor; qualified shadow; restore epoch =="
TARGET=$(post_comment "$MARKET_ID" "$USER_A" "Controversial take for report brigade")
TID=$(echo "$TARGET" | jq -r .id)

# threshold young tier-0
declare -a YOUNG=()
for i in $(seq 1 "$REPORT_SHADOW_THRESHOLD"); do
  YOUNG+=("$(create_user "$(rand_handle young${i})" 0 0)")
done
YOUNG_OK=0
for u in "${YOUNG[@]}"; do
  code=$(report_comment_http "$TID" "$u")
  echo "   young_report user=${u} http=${code} body=$(cat /tmp/report_body.json)"
  if [ "$code" = "200" ]; then YOUNG_OK=$((YOUNG_OK+1)); fi
  [ "$code" = "403" ] || fail "young reporter bypassed floor with HTTP ${code}: $(cat /tmp/report_body.json)"
done
[ "$YOUNG_OK" = "0" ] || fail "young reporter floor accepted ${YOUNG_OK} reports"
# After young attempts, comment must still be visible
STAT=$(psql_demo "select moderation_status from comments where id='${TID}'")
[ "$STAT" = "visible" ] || fail "young brigade must not shadow (status=${STAT})"
echo "   young_http200_count=${YOUNG_OK} status=${STAT}"

# Qualified reporters: aged + tier>=1
declare -a QUAL=()
for i in $(seq 1 "$REPORT_SHADOW_THRESHOLD"); do
  QUAL+=("$(create_user "$(rand_handle qual${i})" 400000 2)")
done
for u in "${QUAL[@]}"; do
  code=$(report_comment_http "$TID" "$u")
  echo "   qual_report user=${u} http=${code} body=$(cat /tmp/report_body.json)"
  [ "$code" = "200" ] || fail "qualified report failed HTTP ${code}: $(cat /tmp/report_body.json)"
done
poll_until $(( $(now_ms) + 10000 )) bash -c "
  s=\$(psql -h localhost -p 15434 -U opinions -d opinions -X -A -t -c \
    \"select moderation_status from comments where id='${TID}'\" | tr -d '[:space:]')
  [ \"\$s\" = shadow ]
" || fail "qualified brigade did not shadow (status=$(psql_demo "select moderation_status from comments where id='${TID}'"))"

# Invisible to C, visible to author A with viewer_id
USER_C=$(create_user "$(rand_handle userc)")
LIST_C=$(curl -sf "${CORE_API_URL}/markets/${MARKET_ID}/comments?sort=recent&limit=50&viewer_id=${USER_C}" -H "$(auth_hdr)")
echo "$LIST_C" | jq -e --arg id "$TID" '[.comments[].id] | index($id) | not' >/dev/null \
  || fail "shadow comment visible to stranger C"
LIST_A=$(curl -sf "${CORE_API_URL}/markets/${MARKET_ID}/comments?sort=recent&limit=50&viewer_id=${USER_A}" -H "$(auth_hdr)")
echo "$LIST_A" | jq -e --arg id "$TID" '[.comments[].id] | index($id) != null' >/dev/null \
  || fail "shadow comment not visible to author A"

# Admin restore = epoch reset
curl -sf -X POST "${CORE_API_URL}/admin/comments/${TID}/moderate" \
  -H "content-type: application/json" \
  -H "$(admin_hdr)" \
  -d '{"status":"visible"}' >/dev/null
STAT2=$(psql_demo "select moderation_status from comments where id='${TID}'")
[ "$STAT2" = "visible" ] || fail "restore failed status=${STAT2}"
RC=$(psql_demo "select count(*) from comment_reports where comment_id='${TID}'")
[ "$RC" = "0" ] || fail "epoch reset should delete report rows, count=${RC}"

# One new report must NOT re-shadow
code=$(report_comment_http "$TID" "${QUAL[0]}")
[ "$code" = "200" ] || fail "post-restore report failed"
STAT3=$(psql_demo "select moderation_status from comments where id='${TID}'")
[ "$STAT3" = "visible" ] || fail "single post-restore report re-shadowed (want full threshold)"
echo "SECTION 3 GREEN young_no_shadow qual_shadow restore_epoch one_report_no_reshadow"

# ---------------------------------------------------------------------------
echo "== [4] resolution fanout: holder∧voter + voter-only =="
# Demo user votes + trades (holder∧voter). USER_C votes only.
IDEM_V=$(uuid)
curl -sf -X POST "${CORE_API_URL}/votes" \
  -H "content-type: application/json" \
  -H "$(auth_hdr)" \
  -H "x-device-id: social-e2e-device" \
  -d "{\"user_id\":\"${DEMO_USER}\",\"market_ref\":\"${SLUG}\",\"side\":\"yes\",\"crowd_guess_pct\":70,\"idempotency_key\":\"${IDEM_V}\"}" >/dev/null
IDEM_T=$(uuid)
curl -sf -X POST "${CORE_API_URL}/trades" \
  -H "content-type: application/json" \
  -H "$(auth_hdr)" \
  -d "{\"user_id\":\"${DEMO_USER}\",\"market_ref\":\"${SLUG}\",\"side\":\"yes\",\"action\":\"buy\",\"amount_micro\":5000000,\"idempotency_key\":\"${IDEM_T}\"}" >/dev/null

IDEM_VC=$(uuid)
curl -sf -X POST "${CORE_API_URL}/votes" \
  -H "content-type: application/json" \
  -H "$(auth_hdr)" \
  -H "x-device-id: social-e2e-device-c" \
  -d "{\"user_id\":\"${USER_C}\",\"market_ref\":\"${SLUG}\",\"side\":\"no\",\"crowd_guess_pct\":40,\"idempotency_key\":\"${IDEM_VC}\"}" >/dev/null

poll_until "$RESOLVE_DEADLINE_MS" state_in "$MARKET_ID" resolved paid \
  || fail "market never settled (state=$(market_state "$MARKET_ID"))"
echo "   state=$(market_state "$MARKET_ID")"

# Wait for resolution notifications
poll_until $(( $(now_ms) + 30000 )) bash -c "
  n=\$(psql -h localhost -p 15434 -U opinions -d opinions -X -A -t -c \
    \"select count(*) from notifications where user_id='${DEMO_USER}' and type='resolution_trade'\" | tr -d '[:space:]')
  [ \"\$n\" -ge 1 ]
" || fail "demo holder never got resolution_trade"

DEMO_N=$(list_notifs "$DEMO_USER")
echo "   demo_notifs=$(echo "$DEMO_N" | jq -c '[.notifications[] | {type, payload}]')"
RT=$(echo "$DEMO_N" | jq '[.notifications[] | select(.type=="resolution_trade")] | .[0]')
[ "$(echo "$RT" | jq -r .type)" = "resolution_trade" ] || fail "missing resolution_trade for holder"
SCORE=$(echo "$RT" | jq -r '.payload.score_bp')
[ "$SCORE" != "null" ] && [ -n "$SCORE" ] || fail "holder∧voter missing score_bp in resolution_trade"
PAYOUT=$(echo "$RT" | jq -r '.payload.payout_total_micro')
[ "$PAYOUT" != "null" ] || fail "missing payout_total_micro"

# Match ledger: sum realization/settlement for user on market
LEDGER_PAY=$(psql_demo "
  select coalesce(sum(realized_delta_micro),0) from realizations
   where user_id='${DEMO_USER}' and market_id='${MARKET_ID}' and source in ('settlement','void')")
# payout_total may equal settlement redemption total; compare realized_delta if present
REAL=$(echo "$RT" | jq -r '.payload.realized_delta_micro')
echo "   payout_total_micro=${PAYOUT} realized_delta=${REAL} ledger_realized=${LEDGER_PAY} score_bp=${SCORE}"
# Prefer realized_delta match to ledger sum
if [ "$REAL" != "null" ]; then
  [ "$REAL" = "$LEDGER_PAY" ] || fail "realized_delta ${REAL} != ledger ${LEDGER_PAY}"
fi
# payout_total_micro should be non-negative number present
python3 -c "int('${PAYOUT}')" >/dev/null 2>&1 || fail "payout_total_micro not int: ${PAYOUT}"

# Voter-only C
poll_until $(( $(now_ms) + 20000 )) bash -c "
  n=\$(psql -h localhost -p 15434 -U opinions -d opinions -X -A -t -c \
    \"select count(*) from notifications where user_id='${USER_C}' and type='resolution_vote'\" | tr -d '[:space:]')
  [ \"\$n\" -ge 1 ]
" || fail "voter-only C missing resolution_vote"
CN=$(list_notifs "$USER_C")
RV=$(echo "$CN" | jq '[.notifications[] | select(.type=="resolution_vote")] | .[0]')
[ "$(echo "$RV" | jq -r .type)" = "resolution_vote" ] || fail "C missing resolution_vote"
CSCORE=$(echo "$RV" | jq -r '.payload.score_bp')
[ "$CSCORE" != "null" ] || fail "resolution_vote missing score_bp"
# Holder should NOT also get resolution_vote
DUP=$(echo "$DEMO_N" | jq '[.notifications[] | select(.type=="resolution_vote")] | length')
[ "$DUP" = "0" ] || fail "holder also got resolution_vote (should dedupe to trade only)"
echo "SECTION 4 GREEN resolution_trade score=${SCORE} payout=${PAYOUT} voter_score=${CSCORE}"

# ---------------------------------------------------------------------------
echo "== [5] mention flood drops beyond MENTION_NOTIFS_PER_HOUR =="
# Fresh market for flood so rate window is clean — reuse same market still open? it's paid.
# Post on a NEW live market
export DEMO_SLUG="social-flood"
export DEMO_CHAIN_SIG="social-chain-flood"
SEED2=$(cargo run -q -p main -- --seed-demo)
M2=$(echo "$SEED2" | sed -n 's/^market_id=\([^ ]*\).*/\1/p')
[ -n "$M2" ] || fail "flood seed failed"
TARGET_U=$(create_user "$(rand_handle mentionme)" 86400 0)
TH=$(psql_demo "select handle from users where id='${TARGET_U}'")
echo "   mention_target=@${TH} id=${TARGET_U}"
# Before flood
BEFORE=$(psql_demo "select count(*) from notifications where user_id='${TARGET_U}' and type='mention'")

for i in $(seq 1 $((MENTION_NOTIFS_PER_HOUR + 3))); do
  u=$(create_user "$(rand_handle fl${i})")
  body="hey @${TH} flood ${i}"
  resp=$(post_comment "$M2" "$u" "$body")
  comment_id=$(echo "$resp" | jq -er '.id') || fail "mention flood post ${i} returned no comment id: ${resp}"
  echo "   flood_${i} author=${u} comment_id=${comment_id}"
done

poll_until $(( $(now_ms) + 30000 )) bash -c "
  n=\$(psql -h localhost -p 15434 -U opinions -d opinions -X -A -t -c \
    \"select count(*) from notifications where user_id='${TARGET_U}' and type='mention'\" | tr -d '[:space:]')
  [ \"\$n\" -ge 1 ]
" || fail "no mention notifs materialized"

AFTER=$(psql_demo "select count(*) from notifications where user_id='${TARGET_U}' and type='mention'")
echo "   mention_notifs=${AFTER} cap=${MENTION_NOTIFS_PER_HOUR} (attempted $((MENTION_NOTIFS_PER_HOUR + 3)))"
# Cap: at most MENTION_NOTIFS_PER_HOUR (materializer checks count before insert)
[ "$AFTER" -le "$MENTION_NOTIFS_PER_HOUR" ] || fail "mention flood exceeded cap: ${AFTER} > ${MENTION_NOTIFS_PER_HOUR}"
# And drops happened: after < attempts
[ "$AFTER" -lt $((MENTION_NOTIFS_PER_HOUR + 3)) ] || fail "expected some drops"
# Badge/unread ≤ cap
U_FLOOD=$(unread "$TARGET_U")
echo "   unread_badge=${U_FLOOD}"
[ "$U_FLOOD" -le $((MENTION_NOTIFS_PER_HOUR + 5)) ] || fail "unread badge suspiciously high"
echo "SECTION 5 GREEN mention_cap=${AFTER} badge=${U_FLOOD}"

# ---------------------------------------------------------------------------
echo "== [6] mark-read zeroes badge =="
IDS=$(list_notifs "$USER_B" | jq -c '[.notifications[].id]')
curl -sf -X POST "${CORE_API_URL}/users/${USER_B}/notifications/read" \
  -H "content-type: application/json" \
  -H "$(auth_hdr)" \
  -d "{\"ids\":${IDS}}" >/dev/null
poll_until $(( $(now_ms) + 10000 )) bash -c "
  u=\$(curl -sf -H 'x-demo-token: ${DEMO_TOKEN}' ${CORE_API_URL}/users/${USER_B}/notifications/unread_count | jq -r .unread_count)
  [ \"\$u\" = 0 ]
" || fail "mark-read did not zero badge (unread=$(unread "$USER_B"))"
echo "SECTION 6 GREEN mark_read_unread=0"

# ---------------------------------------------------------------------------
echo "== [7] hot pagination as_of cursor stable =="
# Create enough root comments on flood market for pagination
for i in $(seq 1 12); do
  u=$(create_user "$(rand_handle pg${i})")
  post_comment "$M2" "$u" "pagination filler ${i} $(uuid | tr -d '-')" >/dev/null
done
PAGE1=$(curl -sf "${CORE_API_URL}/markets/${M2}/comments?sort=hot&limit=5" -H "$(auth_hdr)")
CUR=$(echo "$PAGE1" | jq -r '.next_cursor // empty')
[ -n "$CUR" ] || fail "expected next_cursor on hot page1: $PAGE1"
# Cursor is URL-safe base64 of as_of|hot_score|created_at|id
DECODED=$(python3 - <<PY
import base64,sys
c="${CUR}"
pad="="*((4-len(c)%4)%4)
raw=base64.urlsafe_b64decode(c+pad).decode()
print(raw)
parts=raw.split("|")
assert len(parts)==4, parts
print("AS_OF="+parts[0], file=sys.stderr)
PY
) || fail "cursor not valid base64 hot shape: $CUR"
AS_OF=$(echo "$DECODED" | awk -F'|' '{print $1}')
echo "   as_of=${AS_OF} decoded=${DECODED}"
PAGE2=$(curl -sf --get "${CORE_API_URL}/markets/${M2}/comments" \
  --data-urlencode "sort=hot" \
  --data-urlencode "limit=5" \
  --data-urlencode "cursor=${CUR}" \
  -H "$(auth_hdr)")
IDS1=$(echo "$PAGE1" | jq -r '[.comments[].id] | join(",")')
IDS2=$(echo "$PAGE2" | jq -r '[.comments[].id] | join(",")')
echo "   page1=${IDS1}"
echo "   page2=${IDS2}"
# No overlap
python3 - <<PY
ids1=set("${IDS1}".split(",")) if "${IDS1}" else set()
ids2=set("${IDS2}".split(",")) if "${IDS2}" else set()
ids1.discard(""); ids2.discard("")
assert ids1.isdisjoint(ids2), f"overlap {ids1&ids2}"
assert len(ids2)>0, "page2 empty"
print("ok")
PY
# Re-fetch page2 with same as_of cursor — stable across "hour boundary" (frozen as_of)
PAGE2b=$(curl -sf --get "${CORE_API_URL}/markets/${M2}/comments" \
  --data-urlencode "sort=hot" \
  --data-urlencode "limit=5" \
  --data-urlencode "cursor=${CUR}" \
  -H "$(auth_hdr)")
IDS2b=$(echo "$PAGE2b" | jq -r '[.comments[].id] | join(",")')
[ "$IDS2" = "$IDS2b" ] || fail "as_of page2 unstable: ${IDS2} vs ${IDS2b}"
echo "SECTION 7 GREEN as_of_stable page2_n=$(echo "$PAGE2" | jq '.comments|length')"

echo "PHASE 4 E2E GREEN"
