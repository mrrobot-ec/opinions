#!/usr/bin/env bash
# Phase 6 exit barrier. Native PostgreSQL only; all waits are bounded polls.
# FLASH_TALLY_HIDDEN_SECS=120 is an offset from market creation. With
# FLASH_CLOSES_SECS=180, the hidden WINDOW is therefore exactly 60 seconds.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

PGHOST=127.0.0.1
PGPORT=15434
PGUSER=opinions
PGDATABASE=${PGDATABASE:-opinions_w4}
export PGPASSWORD=opinions
export DATABASE_URL="${DATABASE_URL:-postgres://opinions:opinions@${PGHOST}:${PGPORT}/${PGDATABASE}}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target-w4}"
BIN="${CARGO_TARGET_DIR}/debug"
export BIND_ADDR=127.0.0.1:18086
export DEMO_TOKEN=phase6-demo
export OPINIONS_ENV=staging
export STAGING_FAUCET=1
export SCHEDULER_TICK_MS=100
export DRAFT_PUBLISHER_ENABLED=true
export VIDEO_WORKER_ENABLED=false
export MODERATION_RUNNER_ENABLED=false
export FLASH_CADENCE_SECS=3600
export DAILY_SLOTS=2
export DRAFT_TTL_SECS=1800
export MAX_SLOT_HORIZON_SECS=21600
export DAILY_SEED_BUDGET_MICRO=1000000000
export FLASH_CLOSES_SECS=180
export FLASH_TALLY_HIDDEN_SECS=120
export FLASH_OPEN_SECS=180
export FLASH_HIDDEN_WINDOW_SECS=60
export FLASH_SEED_MICRO=100000000
export FLASH_SEED_FLOOR_MICRO=100000000
export FLASH_FEE_BPS=100
export FLASH_MIN_VOTES_TO_RESOLVE=3
export FLASH_MIN_VOTES_FLOOR=3
export MIN_FEE_BPS=10
export REP_TIER_THRESHOLDS_MICRO=200000,400000,600000,800000
export POSITION_CAP_MICRO_BY_TIER=25000000,50000000,100000000,250000000,500000000
export FEE_DISCOUNT_BP_BY_TIER=0,0,10,20,30
export DISCOUNT_FLIP_WINDOW_SECS=3600
export REP_SCORE_MIN_POT_MICRO=50000000
export PAYOUT_HOLD_THRESHOLD_MICRO=500000000
export SWEEP_DELAY_SECS=180
export BURST_WINDOW_SECS=60
export PRIOR_HORIZON_WINDOWS=4
export BURST_MULTIPLIER_PPM=2000000
export YOUNG_ACCOUNT_AGE_SECS=259200
export YOUNG_ACCOUNT_SHARE_MAX_PPM=500000
export SUBNET_SHARE_MAX_PPM=600000
export DEVICE_SHARE_MAX_PPM=600000
export MIN_VOTES_FOR_RATIOS=4
export MIN_METADATA_COVERAGE_PPM=500000
export MAX_VOTES_PER_WINDOW=30
export VOTE_WINDOW_SECS=3600
export VOTE_NEAR_CLOSE_SECS=600
export VOTE_MIN_ACCOUNT_AGE_SECS=259200
export OI_FLOOR_MICRO=50000000
export DEVICE_HASH_SECRET=phase6-pinned-device-hmac
export TRUSTED_PROXY_CIDRS=127.0.0.0/8,10.0.0.0/8
# D35: targets live in the simswarm lib; --slo-gate flips enforce.

CURATOR_TOKEN=phase6-curator
OPS_TOKEN=phase6-ops
FINANCE_A_TOKEN=phase6-finance-a
FINANCE_B_TOKEN=phase6-finance-b
SUPER_A_TOKEN=phase6-super-a
SUPER_B_TOKEN=phase6-super-b

digest() {
  printf '%s' "$1" | shasum -a 256 | awk '{print $1}'
}

CURATOR_DIGEST=$(digest "$CURATOR_TOKEN")
OPS_DIGEST=$(digest "$OPS_TOKEN")
FINANCE_A_DIGEST=$(digest "$FINANCE_A_TOKEN")
FINANCE_B_DIGEST=$(digest "$FINANCE_B_TOKEN")
SUPER_A_DIGEST=$(digest "$SUPER_A_TOKEN")
SUPER_B_DIGEST=$(digest "$SUPER_B_TOKEN")
export ADMIN_TOKENS_JSON
ADMIN_TOKENS_JSON=$(jq -cn \
  --arg cd "$CURATOR_DIGEST" --arg od "$OPS_DIGEST" \
  --arg fa "$FINANCE_A_DIGEST" --arg fb "$FINANCE_B_DIGEST" \
  --arg sa "$SUPER_A_DIGEST" --arg sb "$SUPER_B_DIGEST" \
  '[
    {id:"curator",roles:["curator"],sha256:$cd},
    {id:"ops",roles:["ops"],sha256:$od},
    {id:"finance-a",roles:["finance"],sha256:$fa},
    {id:"finance-b",roles:["finance"],sha256:$fb},
    {id:"super-a",roles:["superadmin"],sha256:$sa},
    {id:"super-b",roles:["superadmin"],sha256:$sb}
  ]')

BASE=http://127.0.0.1:18086
WS=ws://127.0.0.1:18086/ws
RUN_TAG="p7w4-$(date +%s)-$$"
ARTIFACT_DIR="artifacts/phase7-w4-${RUN_TAG}"
mkdir -p "$ARTIFACT_DIR"
SERVER_PID=
WEB_PID=
SWARM_PID=
PLAYWRIGHT_PID=
WS_PID=

fail() {
  echo "PHASE 6 E2E FAILED: $*" >&2
  exit 1
}

section() {
  echo "== $* =="
}

cleanup() {
  for pid in "${WS_PID:-}" "${PLAYWRIGHT_PID:-}" "${SWARM_PID:-}" "${WEB_PID:-}" "${SERVER_PID:-}"; do
    if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; fi
  done
  wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

sql() {
  psql -X -v ON_ERROR_STOP=1 -At -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d "$PGDATABASE" -c "$1"
}

now_epoch() { date +%s; }

wait_until() {
  local label=$1 budget=$2
  shift 2
  local deadline=$(( $(now_epoch) + budget ))
  until "$@"; do
    if [ "$(now_epoch)" -ge "$deadline" ]; then fail "deadline exceeded: $label"; fi
    sleep 0.2
  done
}

health_ok() { curl -fsS "$BASE/healthz" >/dev/null 2>&1; }
web_ok() { curl -fsS http://127.0.0.1:13000 >/dev/null 2>&1; }

start_server() {
  local mode=$1
  local log="$ARTIFACT_DIR/server-${mode}.log"
  if [ "$mode" = armed ]; then
    env CHAOS_ENABLED=1 CHAOS_RESOLUTION_READY_FILE="$ARTIFACT_DIR/resolution.ready" \
      "$BIN/main" --continuous >"$log" 2>&1 &
  else
    env -u CHAOS_ENABLED -u CHAOS_RESOLUTION_READY_FILE \
      "$BIN/main" --continuous >"$log" 2>&1 &
  fi
  SERVER_PID=$!
  wait_until "server health ($mode)" 30 health_ok
}

stop_server() {
  if [ -n "${SERVER_PID:-}" ]; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
    SERVER_PID=
  fi
}

request() {
  local method=$1 token=$2 path=$3 body=${4:-}
  local output=$5
  local args=(-sS -o "$output" -w '%{http_code}' -X "$method")
  if [ -n "$token" ]; then args+=(-H "x-admin-token: $token"); fi
  if [ -n "$body" ]; then args+=(-H 'content-type: application/json' -d "$body"); fi
  curl "${args[@]}" "$BASE$path"
}

expect_status() {
  local expected=$1 method=$2 token=$3 path=$4 body=${5:-}
  local output="$ARTIFACT_DIR/http-response.json"
  local status
  status=$(request "$method" "$token" "$path" "$body" "$output")
  if [ "$status" != "$expected" ]; then
    fail "$method $path returned $status, expected $expected: $(cat "$output")"
  fi
  cat "$output"
}

public_post() {
  local path=$1 body=$2 output=$3
  curl -sS -o "$output" -w '%{http_code}' -H "x-demo-token: $DEMO_TOKEN" \
    -H 'content-type: application/json' -d "$body" "$BASE$path"
}

# Phase 7 money/compliance legs decide pass/fail HERE, and only here. A 404 is
# an unmounted route and a 503 is an unimplemented stub; neither is an attack
# that failed. The previous `!= 200` predicates accepted both, which is how six
# money legs reported "attack refused" against routes that did not exist.
expect_money_status() {
  local expected=$1 label=$2 actual=$3 body=${4:-}
  case "$expected" in
    404|503)
      fail "$label: 404/503 may never be the EXPECTED outcome of a money leg" ;;
  esac
  case "$actual" in
    404)
      fail "$label: route is not mounted (404); a missing money route is not a refused attack" ;;
    503)
      fail "$label: route is an unimplemented stub (503); a stub is not a refused attack" ;;
  esac
  if [ "$actual" != "$expected" ]; then
    fail "$label: got $actual, expected $expected${body:+ -- $(cat "$body" 2>/dev/null)}"
  fi
}

# Cross-owner production surfaces that do not exist yet. Recording one prints it
# immediately and fails the run at the end (see the gate before the GREEN line):
# a missing prerequisite is never laundered into a pass.
PREREQ_FAILURES=()
prereq_missing() {
  PREREQ_FAILURES+=("$1")
  echo "MISSING PRODUCTION PREREQUISITE: $1" >&2
}

# Returns 0 when the route answered, so the caller must assert exactly.
route_is_live() {
  local label=$1 status=$2
  case "$status" in
    404) prereq_missing "$label: route is not mounted (404)"; return 1 ;;
    503) prereq_missing "$label: route is an unimplemented stub (503)"; return 1 ;;
  esac
  return 0
}

# `canonicalize_dest` (application/src/money/types.rs) accepts a base58 string
# that decodes to EXACTLY 32 bytes. Two literals this script used to send were
# not canonical at all — "Mule1..." carries a non-base58 'l' and "Conc1..."
# decodes to 31 bytes — so those legs could only ever have produced a 422
# ConfigInvalid, never a compliance verdict. Every dest is checked before use.
assert_dest_canonical() {
  local dest=$1 label=$2 decoded
  decoded=$(node -e '
    const A = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    const input = process.argv[1];
    let n = 0n;
    for (const ch of input) {
      const v = A.indexOf(ch);
      if (v < 0) { console.log(-1); process.exit(0); }
      n = n * 58n + BigInt(v);
    }
    let bytes = 0;
    while (n > 0n) { n >>= 8n; bytes += 1; }
    let zeros = 0;
    while (zeros < input.length && input[zeros] === "1") zeros += 1;
    console.log(bytes + zeros);
  ' "$dest")
  [ "$decoded" = 32 ] || \
    fail "$label dest is not canonical: $dest decodes to $decoded bytes, canonicalize_dest requires 32"
}

# Every withdrawal request carries `confirm_dest` — a REQUIRED
# WithdrawRequestDto field (adapters/src/http/dto/withdraw.rs). Omitting it is
# an axum deserialization rejection that never reaches the handler, so geo,
# sanctions, dest warmth, daily limits and self-exclusion are all skipped. The
# idempotency_key is replay-stable so the retry legs mean something.
withdraw_request() {
  local user=$1 amount=$2 dest=$3 key=$4 output=$5
  assert_dest_canonical "$dest" "withdraw($key)"
  curl -sS -o "$output" -w '%{http_code}' -H "x-demo-token: $DEMO_TOKEN" \
    -H 'content-type: application/json' \
    -d "$(jq -cn --arg user "$user" --arg dest "$dest" --arg key "$key" \
      --argjson amount "$amount" \
      '{user_id:$user,amount_micro:$amount,dest:$dest,confirm_dest:$dest,idempotency_key:$key}')" \
    "$BASE/withdrawals"
}

# D33 is fail-closed: a fresh Clear geo AND sanctions fact plus a KYC tier are
# preconditions for every money mutation. Staging seeds them per user, exactly
# like `created_at_override`/`rep_seed_micro`. `checked_at` is backdated one
# minute so a red-team leg's `now()` Hit always sorts newer and wins.
seed_compliance() {
  local user=$1 tier=${2:-2} context
  sql "update users set kyc_tier=$tier where id='$user'" >/dev/null
  for context in geo sanctions; do
    sql "insert into sanction_screenings(user_id,context,verdict,checked_at,expires_at,policy_version)
         values ('$user','$context','clear',now()-interval '1 minute',
                 now()+interval '24 hours','e2e-staging')" >/dev/null
  done
}

signup_raw() {
  local handle=$1 address=$2 aged=$3 rep=$4
  local body
  if [ "$aged" = true ]; then
    body=$(jq -cn --arg handle "$handle" --arg address "$address" --arg at "$AGED_AT" \
      --argjson rep "$rep" '{handle:$handle,channel:"imessage",address:$address,created_at_override:$at,rep_seed_micro:$rep}')
  elif [ "$rep" -gt 0 ]; then
    body=$(jq -cn --arg handle "$handle" --arg address "$address" --argjson rep "$rep" \
      '{handle:$handle,channel:"imessage",address:$address,rep_seed_micro:$rep}')
  else
    body=$(jq -cn --arg handle "$handle" --arg address "$address" \
      '{handle:$handle,channel:"imessage",address:$address}')
  fi
  local response="$ARTIFACT_DIR/signup.json" status
  status=$(curl -sS -o "$response" -w '%{http_code}' -H 'content-type: application/json' \
    -d "$body" "$BASE/users")
  [ "$status" = 200 ] || fail "signup $handle returned $status: $(cat "$response")"
  jq -er .user_id "$response"
}

signup() {
  local user
  user=$(signup_raw "$@")
  seed_compliance "$user"
  printf '%s\n' "$user"
}

fund() {
  local user=$1 amount=$2 key=$3
  expect_status 200 POST "$FINANCE_A_TOKEN" /admin/deposits \
    "$(jq -cn --arg user "$user" --arg key "$key" --argjson amount "$amount" \
      '{user_id:$user,amount_micro:$amount,reason:"phase6 exit fixture",idempotency_key:$key}')" >/dev/null
}

vote() {
  local user=$1 market=$2 key=$3 device=$4 ip=$5
  local body output="$ARTIFACT_DIR/vote.json"
  body=$(jq -cn --arg user "$user" --arg market "$market" --arg key "$key" \
    '{user_id:$user,market_ref:$market,side:"yes",crowd_guess_pct:63,idempotency_key:$key}')
  curl -sS -o "$output" -w '%{http_code}' -H "x-demo-token: $DEMO_TOKEN" \
    -H "x-device-id: $device" -H "x-forwarded-for: $ip" \
    -H 'content-type: application/json' -d "$body" "$BASE/votes"
}

preview() {
  local user=$1 market=$2 amount=$3 output=$4
  public_post /trades/preview "$(jq -cn --arg user "$user" --arg market "$market" --argjson amount "$amount" \
    '{user_id:$user,market_ref:$market,side:"yes",action:"buy",amount_micro:$amount}')" "$output"
}

place() {
  local user=$1 market=$2 amount=$3 version=$4 key=$5 output=$6
  public_post /trades "$(jq -cn --arg user "$user" --arg market "$market" --arg key "$key" \
    --argjson amount "$amount" --argjson version "$version" \
    '{user_id:$user,market_ref:$market,side:"yes",action:"buy",amount_micro:$amount,idempotency_key:$key,expected_config_version:$version}')" "$output"
}

preview_is_current() {
  local user=$1 market=$2 amount=$3 output=$4 status version generation
  status=$(preview "$user" "$market" "$amount" "$output")
  [ "$status" = 200 ] || return 1
  version=$(jq -er .config_version "$output")
  generation=$(sql 'select generation from config_generation where singleton=1')
  [ "$version" = "$generation" ]
}

preview_status_is() {
  local expected=$1 user=$2 market=$3 amount=$4 output=$5
  [ "$(preview "$user" "$market" "$amount" "$output")" = "$expected" ]
}

create_market() {
  local slug=$1 open_secs=$2 hidden_window_secs=$3 seed=$4 fee=$5 min_votes=$6
  local created draft spec edited market
  created=$(expect_status 200 POST "$CURATOR_TOKEN" /admin/drafts \
    "$(jq -cn --arg topic "$RUN_TAG-$slug" '{topics:[$topic],tier:"flash",source:"template",allow_fallback:false}')")
  draft=$(jq -er '.[0].id' <<<"$created")
  spec=$(jq -c --arg slug "$slug" --arg question "Will $slug prove Phase 6?" \
    --argjson open "$open_secs" --argjson hidden "$hidden_window_secs" \
    --argjson seed "$seed" --argjson fee "$fee" --argjson votes "$min_votes" \
    '.[0].spec | .slug=$slug | .question=$question | .open_secs=$open |
      .hidden_window_secs=$hidden | .seed_micro=$seed | .fee_bps=$fee |
      .min_votes_to_resolve=$votes' <<<"$created")
  edited=$(expect_status 200 PATCH "$CURATOR_TOKEN" "/admin/drafts/$draft" \
    "$(jq -cn --arg reviewer "$REVIEWER_ID" --argjson spec "$spec" '{reviewer_id:$reviewer,spec:$spec}')")
  [ "$(jq -r .spec.slug <<<"$edited")" = "$slug" ] || fail "draft edit missed $slug"
  expect_status 200 POST "$CURATOR_TOKEN" "/admin/drafts/$draft/approve" \
    "$(jq -cn --arg reviewer "$REVIEWER_ID" '{reviewer_id:$reviewer}')" >/dev/null
  expect_status 202 POST "$CURATOR_TOKEN" "/admin/drafts/$draft/publish_now" '' >/dev/null
  wait_until "publish $slug" 30 bash -c \
    "[ \"\$(psql -X -At -h '$PGHOST' -p '$PGPORT' -U '$PGUSER' -d '$PGDATABASE' -c \"select status from market_drafts where id='$draft'\")\" = published ]"
  market=$(sql "select published_market_id from market_drafts where id='$draft'")
  [ -n "$market" ] || fail "published $slug has no market id"
  printf '%s\n' "$market"
}

market_state_is() {
  [ "$(curl -fsS "$BASE/markets/$1" | jq -r .state)" = "$2" ]
}

market_state_in() {
  local state
  state=$(curl -fsS "$BASE/markets/$1" | jq -r .state)
  shift
  local wanted
  for wanted in "$@"; do [ "$state" = "$wanted" ] && return 0; done
  return 1
}

run_logged() {
  local name=$1
  shift
  local log="$ARTIFACT_DIR/$name.log"
  if ! "$@" >"$log" 2>&1; then
    tail -200 "$log" >&2
    fail "$name failed (full log: $log)"
  fi
}

section "fresh native PostgreSQL + globbed migrations"
dropdb --if-exists --force -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" "$PGDATABASE"
createdb -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" "$PGDATABASE"
for migration in migrations/*.sql; do
  psql -X -v ON_ERROR_STOP=1 -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d "$PGDATABASE" -f "$migration" >/dev/null
done
# Raw psql is the exit-contract migration path. Record the same SHA-384
# history sqlx embeds so main's startup verifier recognizes, rather than
# attempts to reapply, the already executed glob.
sql 'create table _sqlx_migrations (
  version bigint primary key, description text not null,
  installed_on timestamptz not null default now(), success boolean not null,
  checksum bytea not null, execution_time bigint not null
)' >/dev/null
MIGRATION_COUNT=0
for migration in migrations/*.sql; do
  filename=${migration##*/}
  prefix=${filename%%_*}
  version=$((10#$prefix))
  description=${filename#*_}
  description=${description%.sql}
  description=${description//_/ }
  checksum=$(shasum -a 384 "$migration" | awk '{print $1}')
  sql "insert into _sqlx_migrations(version,description,success,checksum,execution_time)
       values ($version,'$description',true,decode('$checksum','hex'),0)" >/dev/null
  MIGRATION_COUNT=$((MIGRATION_COUNT + 1))
done
[ "$(sql 'select count(*) from _sqlx_migrations')" = "$MIGRATION_COUNT" ] || fail "migration glob history incomplete"

section "build drivers and start ordinary profile"
cargo build -q -p main -p simswarm
cargo build -q -p adapters --example content_worker
GENESIS_HOUSE_MICRO=20000000000 "$CARGO_TARGET_DIR/debug/examples/content_worker" ensure-genesis \
  >"$ARTIFACT_DIR/genesis.log"
start_server ordinary
HEALTH=$(curl -fsS "$BASE/healthz")
jq -e '.status=="ok" and .faults.active==[] and .faults.relay_delay_ms==null and .faults.ws_drop_every_n==null' \
  <<<"$HEALTH" >/dev/null || fail "ordinary /healthz disclosed a fault: $HEALTH"

# `time`'s serde-human-readable wire shape (the DTO's actual contract).
AGED_AT=$(date -u -v-80d '+%Y-%m-%d %H:%M:%S.0 +00:00:00')

section "D33 fail-closed compliance and the pinned staging region allowset"
# Two separate D33 facts, proven separately:
#   1. `region_allowset` ships ABSENT (= deny-all). The staging setup applies a
#      pinned NON-PRODUCTION array; production content is the counsel proposal.
#   2. The gate itself is fail-closed on the FACTS: a user with no fresh Clear
#      geo screening cannot mutate money, allowset or not. That leg needs a live
#      book, so it runs beside the config proof vote below.
[ -z "$(sql "select value from config_entries where key='region_allowset'")" ] || \
  fail "region_allowset was seeded; the deny-all default is the D33 contract"
REGION_PROPOSAL=$(expect_status 200 POST "$SUPER_A_TOKEN" /admin/config/proposals \
  "$(jq -cn '{patch:{region_allowset:["CA","NY","TX","WA"],region_allowset_version:2},
              reason:"pinned non-production staging allowset",
              idempotency_key:"p7-region-allowset"}')")
REGION_ID=$(jq -er .id <<<"$REGION_PROPOSAL")
expect_status 403 POST "$SUPER_A_TOKEN" "/admin/config/proposals/$REGION_ID/confirm" \
  '{"reason":"same principal must fail"}' >/dev/null
expect_status 200 POST "$SUPER_B_TOKEN" "/admin/config/proposals/$REGION_ID/confirm" \
  '{"reason":"independent superadmin confirmation"}' >/dev/null
[ -n "$(sql "select value from config_entries where key='region_allowset'")" ] || \
  fail "the pinned staging allowset was not applied"
[ "$(sql "select value from config_entries where key='region_allowset_version'")" = 2 ] || \
  fail "region_allowset_version did not advance by exactly one"

REVIEWER_ID=$(signup p6-reviewer +15559990001 true 0)
fund "$REVIEWER_ID" 100000000 "$RUN_TAG-reviewer"

section "canonical manifest, idempotent public signup, staged identities, and funding"
MANIFEST="$ARTIFACT_DIR/manifest.json"
"$BIN/simswarm" --profile smoke --seed 604 --manifest-output "$MANIFEST" --manifest-only
"$BIN/simswarm" --profile 2k --seed 604 --manifest-output "$ARTIFACT_DIR/manifest-2k.json" --manifest-only
jq -e '.profile=="full" and (.agents|length)==2000 and (.roster.lifecycle_voters|length)>=1000' \
  "$ARTIFACT_DIR/manifest-2k.json" >/dev/null || fail "pinned 2k release profile is incomplete"
# D35: the release profile carries a 10% money path; the smoke profile does not.
jq -e '([.agents[]|select(.money_capable)]|length) * 10 == (.agents|length)' \
  "$ARTIFACT_DIR/manifest-2k.json" >/dev/null || fail "pinned 2k profile is missing its 10% money path"
jq -e '[.agents[]|select(.money_capable)]|length == 0' "$MANIFEST" >/dev/null || \
  fail "the smoke profile grew a money path"
# The published five-market proof uses the lifecycle book for the wash pair.
jq '(.rings[] | select(.kind=="wash_pair") | .target_market)="lifecycle" |
    (.agents[] | select(.target_market=="wash") | .target_market)="lifecycle"' \
  "$MANIFEST" >"$ARTIFACT_DIR/manifest-patched.json"
mv "$ARTIFACT_DIR/manifest-patched.json" "$MANIFEST"

while IFS= read -r agent; do
  id=$(jq -r .id <<<"$agent")
  address=$(jq -r .channel_address <<<"$agent")
  aged=$(jq -r .aged <<<"$agent")
  rep=$(jq -r .rep_seed_micro <<<"$agent")
  uid=$(signup "p6-$id" "$address" "$aged" "$rep")
  replay_uid=$(signup "ignored-$id" "$address" false 0)
  [ "$uid" = "$replay_uid" ] || fail "channel-key signup replay changed identity for $id"
  fund "$uid" 500000000 "$RUN_TAG-fund-$id"
  jq --arg id "$id" --arg uid "$uid" \
    '(.agents[] | select(.id==$id) | .user_id)=$uid' "$MANIFEST" >"$ARTIFACT_DIR/manifest-next.json"
  mv "$ARTIFACT_DIR/manifest-next.json" "$MANIFEST"
done < <(jq -c '.agents[]' "$MANIFEST")

[ "$(sql "select count(*) from admin_actions where action='create_user_override'")" -ge 80 ] || \
  fail "staging identity overrides were not audited"

CONFIG_USER=$(signup p6-config-user +15559990002 true 0)
SWITCH_USER=$(signup p6-switch-user +15559990003 true 0)
PAD_A=$(signup p6-pad-a +15559990004 true 0)
PAD_B=$(signup p6-pad-b +15559990005 true 0)
NEAR_YOUNG_CONTROL=$(signup p6-near-young-control +15559990006 false 0)
NEAR_YOUNG_LATE=$(signup p6-near-young-late +15559990007 false 0)
NEAR_AGED=$(signup p6-near-aged +15559990008 true 0)
PLAYWRIGHT_USER=$(signup p6-mobile +15559990009 true 0)
LINKED_USER=$(signup "$FINANCE_A_DIGEST" +15559990010 true 0)
for pair in \
  "$CONFIG_USER config" "$SWITCH_USER switch" "$PAD_A pad-a" "$PAD_B pad-b" \
  "$NEAR_YOUNG_CONTROL near-control" "$NEAR_YOUNG_LATE near-late" "$NEAR_AGED near-aged" \
  "$PLAYWRIGHT_USER mobile" "$LINKED_USER linked"; do
  set -- $pair
  fund "$1" 500000000 "$RUN_TAG-$2"
done
BALLAST_USERS=()
for index in $(seq 1 12); do
  user=$(signup "p6-ballast-$index" "+15559991$(printf '%04d' "$index")" true 0)
  fund "$user" 500000000 "$RUN_TAG-ballast-$index"
  BALLAST_USERS+=("$user")
done
expect_status 422 POST "$FINANCE_A_TOKEN" /admin/deposits \
  "$(jq -cn --arg user "$REVIEWER_ID" '{user_id:$user,amount_micro:1000000001,reason:"cap proof",idempotency_key:"faucet-over-cap"}')" >/dev/null

section "determinism proofs"
"$BIN/simswarm" --manifest "$MANIFEST" --dry-run --output "$ARTIFACT_DIR/dry-a.jsonl"
"$BIN/simswarm" --manifest "$MANIFEST" --dry-run --output "$ARTIFACT_DIR/dry-b.jsonl"
cmp -s "$ARTIFACT_DIR/dry-a.jsonl" "$ARTIFACT_DIR/dry-b.jsonl" || fail "dry runs differ"
jq '.schema_version=999' "$MANIFEST" >"$ARTIFACT_DIR/manifest-version-mismatch.json"
if "$BIN/simswarm" --manifest "$ARTIFACT_DIR/manifest-version-mismatch.json" --dry-run \
  --output "$ARTIFACT_DIR/should-not-exist.jsonl" >"$ARTIFACT_DIR/version-mismatch.log" 2>&1; then
  fail "version-mismatched manifest was accepted"
fi

section "web server ready before the pinned lifecycle clock starts"
CORE_PROXY_TARGET="$BASE" NEXT_PUBLIC_CORE_URL=/core-api NEXT_PUBLIC_WS_URL="$WS" \
  pnpm --dir web dev --hostname 127.0.0.1 --port 13000 >"$ARTIFACT_DIR/web-dev.log" 2>&1 &
WEB_PID=$!
wait_until "web dev server" 45 web_ok

section "explicit proof-market stamps and live swarm start"
# The three ring-only books start first and run for 200s. Their tick-179 ring
# actions are consequently inside the final minute, while no actor can race a
# not-yet-published target during the first ticks of the swarm.
FAT_ID=$(create_market fat-pot 200 60 500000000 100 3)
SUPPRESS_ID=$(create_market void-suppress 200 60 100000000 100 3)
PAD_ID=$(create_market threshold-pad 200 60 100000000 100 4)
LIFECYCLE_ID=$(create_market lifecycle 180 60 200000000 100 3)
[ "$(vote "$CONFIG_USER" lifecycle "$RUN_TAG-config-vote" config-device 10.244.1.1)" = 200 ] || fail "config proof vote failed"
# D33 fail-closed on the facts: same book, same moment, same aged account —
# the only difference is that this user carries no fresh Clear geo screening.
DENIED_USER=$(signup_raw p7-geo-denied +15559970001 true 0)
[ "$(vote "$DENIED_USER" lifecycle "$RUN_TAG-geo-denied" geo-denied 10.243.9.1)" != 200 ] || \
  fail "a user with no fresh Clear geo fact mutated money on a live book"
while IFS= read -r wash_agent; do
  row=$(jq -c --arg id "$wash_agent" '.agents[] | select(.id==$id)' "$MANIFEST")
  uid=$(jq -r .user_id <<<"$row")
  device=$(jq -r .device_id <<<"$row")
  ip=$(jq -r .forwarded_for <<<"$row")
  [ "$(vote "$uid" lifecycle "$RUN_TAG-wash-vote-$wash_agent" "$device" "$ip")" = 200 ] || \
    fail "wash member prerequisite vote failed"
done < <(jq -r '.rings[] | select(.kind=="wash_pair") | .members[]' "$MANIFEST")
OLD_PREVIEW="$ARTIFACT_DIR/existing-book-preview.json"
[ "$(preview "$CONFIG_USER" lifecycle 1000000 "$OLD_PREVIEW")" = 200 ] || fail "existing-book preview failed"
OLD_VERSION=$(jq -er .config_version "$OLD_PREVIEW")
OLD_FEE=$(jq -er .fee_micro "$OLD_PREVIEW")

section "new-market fee config and complete swarm target set"
FEE_PROPOSAL=$(expect_status 200 POST "$FINANCE_A_TOKEN" /admin/config/proposals \
  "$(jq -cn '{patch:{trade_fee_bps:120},reason:"new-market fee proof",idempotency_key:"p6-fee-120"}')")
FEE_PROPOSAL_ID=$(jq -er .id <<<"$FEE_PROPOSAL")
expect_status 403 POST "$FINANCE_A_TOKEN" "/admin/config/proposals/$FEE_PROPOSAL_ID/confirm" \
  '{"reason":"same principal must fail"}' >/dev/null
expect_status 200 POST "$FINANCE_B_TOKEN" "/admin/config/proposals/$FEE_PROPOSAL_ID/confirm" \
  '{"reason":"second finance principal"}' >/dev/null

[ "$(place "$CONFIG_USER" lifecycle 1000000 "$OLD_VERSION" "$RUN_TAG-existing-old-fee" "$ARTIFACT_DIR/existing-place.json")" = 200 ] || \
  fail "new-market-only fee drift stale-rejected the existing book"
[ "$(jq -r .fee_micro "$ARTIFACT_DIR/existing-place.json")" = "$OLD_FEE" ] || fail "existing pool fee repriced"

NEAR_ID=$(create_market near-close 720 60 100000000 120 3)
[ "$(preview "$CONFIG_USER" near-close 1000000 "$ARTIFACT_DIR/new-fee-preview.json")" = 200 ] || fail "new fee preview failed"
[ "$(jq -r .fee_micro "$ARTIFACT_DIR/new-fee-preview.json")" = 12000 ] || fail "new market did not quote 120bp"

PLAYWRIGHT_MARKET_SLUG=lifecycle PLAYWRIGHT_USER_ID="$PLAYWRIGHT_USER" \
PLAYWRIGHT_DEMO_TOKEN="$DEMO_TOKEN" PLAYWRIGHT_BASE_URL=http://127.0.0.1:13000 \
PLAYWRIGHT_PINNED_WINDOW_TIMEOUT_MS=600000 PLAYWRIGHT_TEST_TIMEOUT_MS=660000 \
PLAYWRIGHT_MONEY=1 \
  pnpm --dir web exec playwright test e2e/mobile-golden.spec.ts e2e/money-surfaces.spec.ts \
  >"$ARTIFACT_DIR/playwright.log" 2>&1 &
PLAYWRIGHT_PID=$!

# Every manifest target must exist before the WebSocket subscriber resolves
# their public IDs. Starting the swarm before `near-close` was published made
# this a scheduler race: a fast subscriber parsed the 404 body and failed with
# "response omitted id", while a slower run happened to pass.
"$BIN/simswarm" --manifest "$MANIFEST" --base-url "$BASE" --demo-token "$DEMO_TOKEN" \
  --slo-gate \
  --output "$ARTIFACT_DIR/live-trace.json" --latency-log "$ARTIFACT_DIR/latency.jsonl" \
  --slo-report "$ARTIFACT_DIR/slo-report.json" >"$ARTIFACT_DIR/swarm.stdout" \
  2>"$ARTIFACT_DIR/swarm.stderr" &
SWARM_PID=$!

# Guarantee the lifecycle book crosses the pinned $500 hold threshold without
# consuming any manifest trade-capable participant's position cap.
ballast_index=0
for user in "${BALLAST_USERS[@]}"; do
  ballast_index=$((ballast_index + 1))
  [ "$(vote "$user" lifecycle "$RUN_TAG-ballast-vote-$ballast_index" "ballast-$ballast_index" "10.245.$ballast_index.1")" = 200 ] || \
    fail "lifecycle ballast vote failed"
  out="$ARTIFACT_DIR/ballast-$ballast_index-preview.json"
  [ "$(preview "$user" lifecycle 25000000 "$out")" = 200 ] || fail "lifecycle ballast preview failed"
  version=$(jq -er .config_version "$out")
  [ "$(place "$user" lifecycle 25000000 "$version" "$RUN_TAG-ballast-$ballast_index" "$ARTIFACT_DIR/ballast-$ballast_index-place.json")" = 200 ] || \
    fail "lifecycle ballast trade failed"
done

section "roster-isolated early baselines and exact \$50 OI padding"
[ "$(vote "$PAD_A" threshold-pad "$RUN_TAG-pad-a-vote" pad-a-device 10.242.1.1)" = 200 ] || fail "pad-a vote failed"
[ "$(vote "$PAD_B" threshold-pad "$RUN_TAG-pad-b-vote" pad-b-device 10.243.1.1)" = 200 ] || fail "pad-b vote failed"
while IFS= read -r agent_id; do
  row=$(jq -c --arg id "$agent_id" '.agents[] | select(.id==$id)' "$MANIFEST")
  uid=$(jq -r .user_id <<<"$row")
  device=$(jq -r .device_id <<<"$row")
  ip=$(jq -r .forwarded_for <<<"$row")
  aged=$(jq -r .aged <<<"$row")
  expected=422; [ "$aged" = true ] && expected=200
  [ "$(vote "$uid" fat-pot "$RUN_TAG-fat-honest-$agent_id" "$device" "$ip")" = "$expected" ] || \
    fail "fat-pot honest baseline age contract failed for $agent_id"
done < <(jq -r '.roster.fat_pot_honest_voters[]' "$MANIFEST")

while IFS= read -r agent_id; do
  row=$(jq -c --arg id "$agent_id" '.agents[] | select(.id==$id)' "$MANIFEST")
  uid=$(jq -r .user_id <<<"$row")
  device=$(jq -r .device_id <<<"$row")
  ip=$(jq -r .forwarded_for <<<"$row")
  aged=$(jq -r .aged <<<"$row")
  expected=422; [ "$aged" = true ] && expected=200
  [ "$(vote "$uid" lifecycle "$RUN_TAG-life-vote-$agent_id" "$device" "$ip")" = "$expected" ] || \
    fail "lifecycle baseline age contract failed for $agent_id"
done < <(jq -r '.roster.lifecycle_voters[]' "$MANIFEST")

for pair in "$PAD_A pad-a" "$PAD_B pad-b"; do
  set -- $pair
  out="$ARTIFACT_DIR/$2-preview.json"
  [ "$(preview "$1" threshold-pad 25000000 "$out")" = 200 ] || fail "pad preview failed"
  version=$(jq -er .config_version "$out")
  [ "$(place "$1" threshold-pad 25000000 "$version" "$RUN_TAG-$2-trade" "$ARTIFACT_DIR/$2-place.json")" = 200 ] || \
    fail "pad trade failed"
done
[ "$(sql "select coalesce(sum(collateral_micro),0) from trades where market_id='$PAD_ID' and side='buy'")" = 50000000 ] || \
  fail "threshold-pad did not pin exactly \$50 OI"

section "remaining two-phase config, staleness, expiry, and rejection"
[ "$(preview "$CONFIG_USER" lifecycle 1000000 "$ARTIFACT_DIR/stale-preview.json")" = 200 ] || fail "stale preview setup failed"
STALE_VERSION=$(jq -er .config_version "$ARTIFACT_DIR/stale-preview.json")
CAP_PROPOSAL=$(expect_status 200 POST "$FINANCE_A_TOKEN" /admin/config/proposals \
  "$(jq -cn '{patch:{position_cap_micro_by_tier:[50000000,50000000,100000000,250000000,500000000]},reason:"cap staleness proof",idempotency_key:"p6-cap-rise"}')")
CAP_ID=$(jq -er .id <<<"$CAP_PROPOSAL")
expect_status 200 POST "$FINANCE_B_TOKEN" "/admin/config/proposals/$CAP_ID/confirm" '{"reason":"confirm cap"}' >/dev/null
[ "$(place "$CONFIG_USER" lifecycle 1000000 "$STALE_VERSION" "$RUN_TAG-stale-place" "$ARTIFACT_DIR/stale-place.json")" = 409 ] || \
  fail "cap drift did not produce 409 StaleConfig"
[ "$(jq -r .code "$ARTIFACT_DIR/stale-place.json")" = StaleConfig ] || fail "409 was not StaleConfig"
wait_until "config watch catches up for fresh preview" 10 preview_is_current \
  "$CONFIG_USER" lifecycle 1000000 "$ARTIFACT_DIR/fresh-preview.json"
FRESH_VERSION=$(jq -er .config_version "$ARTIFACT_DIR/fresh-preview.json")
[ "$(place "$CONFIG_USER" lifecycle 1000000 "$FRESH_VERSION" "$RUN_TAG-fresh-place" "$ARTIFACT_DIR/fresh-place.json")" = 200 ] || \
  fail "fresh preview + new lexical confirmation did not place"

expect_status 422 POST "$FINANCE_A_TOKEN" /admin/config/proposals \
  "$(jq -cn '{patch:{trade_fee_bps:500},reason:"bounds proof",idempotency_key:"p6-oob"}')" >/dev/null
EXPIRED=$(expect_status 200 POST "$FINANCE_A_TOKEN" /admin/config/proposals \
  "$(jq -cn '{patch:{min_fee_bps:20},reason:"expiry proof",idempotency_key:"p6-expired"}')")
EXPIRED_ID=$(jq -er .id <<<"$EXPIRED")
sql "update config_change_proposals set expires_at=now()-interval '1 second' where id='$EXPIRED_ID'" >/dev/null
expect_status 409 POST "$FINANCE_B_TOKEN" "/admin/config/proposals/$EXPIRED_ID/confirm" '{"reason":"must expire"}' >/dev/null
REJECTED=$(expect_status 200 POST "$FINANCE_A_TOKEN" /admin/config/proposals \
  "$(jq -cn '{patch:{min_fee_bps:20},reason:"reject proof",idempotency_key:"p6-reject"}')")
REJECTED_ID=$(jq -er .id <<<"$REJECTED")
expect_status 200 POST "$FINANCE_B_TOKEN" "/admin/config/proposals/$REJECTED_ID/reject" '{"reason":"reject and unblock"}' >/dev/null
UNBLOCKED=$(expect_status 200 POST "$FINANCE_A_TOKEN" /admin/config/proposals \
  "$(jq -cn '{patch:{min_fee_bps:20},reason:"unblocked proof",idempotency_key:"p6-unblocked"}')")
UNBLOCKED_ID=$(jq -er .id <<<"$UNBLOCKED")
expect_status 200 POST "$FINANCE_B_TOKEN" "/admin/config/proposals/$UNBLOCKED_ID/reject" '{"reason":"cleanup"}' >/dev/null
[ "$(sql "select count(*) from config_changes where generation>1")" -ge 2 ] || fail "config_changes were not written"
[ "$(sql "select count(*) from admin_actions where action like '%config%'")" -ge 8 ] || fail "config audit facts missing"
[ "$(sql "select extract(epoch from (closes_at-tally_hidden_at))::bigint from markets where id='$LIFECYCLE_ID'")" = 60 ] || \
  fail "live tally_hidden_at moved"

section "fenced switches and public WebSocket frames"
node --input-type=module - "$WS" "$LIFECYCLE_ID" "$ARTIFACT_DIR/switch-frames.json" <<'NODE' &
import { writeFileSync } from "node:fs";
import { WebSocket } from "./scripts/node_modules/ws/wrapper.mjs";
const [, , url, market, output] = process.argv;
const seen = [];
const ws = new WebSocket(url);
const timer = setTimeout(() => process.exit(2), 20000);
ws.on("open", () => ws.send(JSON.stringify({op:"subscribe", market_id:market})));
ws.on("message", raw => {
  const frame = JSON.parse(String(raw));
  if (["trading_paused","trading_resumed"].includes(frame.type)) seen.push(frame.type);
  if (seen.includes("trading_paused") && seen.includes("trading_resumed")) {
    clearTimeout(timer); writeFileSync(output, JSON.stringify(seen)); ws.close(); process.exit(0);
  }
});
NODE
WS_PID=$!
sleep 0.5

[ "$(preview "$CONFIG_USER" lifecycle 500000 "$ARTIFACT_DIR/pause-preview.json")" = 200 ] || fail "pause preview failed"
PAUSE_VERSION=$(jq -er .config_version "$ARTIFACT_DIR/pause-preview.json")
expect_status 200 POST "$OPS_TOKEN" /admin/config \
  "$(jq -cn '{patch:{trading_paused:true},reason:"drain proof",idempotency_key:"p6-trading-pause"}')" >/dev/null
[ "$(place "$CONFIG_USER" lifecycle 500000 "$PAUSE_VERSION" "$RUN_TAG-paused-trade" "$ARTIFACT_DIR/paused-trade.json")" = 423 ] || \
  fail "trade was not 423 during trading pause"
[ "$(vote "$SWITCH_USER" lifecycle "$RUN_TAG-vote-during-trading-pause" switch-device 10.240.1.1)" = 200 ] || \
  fail "vote did not remain 200 during trading pause"
expect_status 200 POST "$OPS_TOKEN" /admin/config \
  "$(jq -cn '{patch:{trading_paused:false},reason:"drain proof complete",idempotency_key:"p6-trading-resume"}')" >/dev/null
wait "$WS_PID" || fail "public pause/resume WS frames were not observed"
WS_PID=

expect_status 200 POST "$OPS_TOKEN" /admin/config \
  "$(jq -cn --arg key "market_paused:$SUPPRESS_ID" '{patch:{($key):true},reason:"hidden resume fence",idempotency_key:"p6-market-pause"}')" >/dev/null

[ "$(vote "$NEAR_YOUNG_CONTROL" near-close "$RUN_TAG-near-young-control" near-control 10.241.1.1)" = 200 ] || \
  fail "young t0 control vote was not 200"
VOTE_PAUSE=$(expect_status 200 POST "$SUPER_A_TOKEN" /admin/config/proposals \
  "$(jq -cn --arg key "voting_paused:$NEAR_ID" '{patch:{($key):true},reason:"voting pause proof",idempotency_key:"p6-voting-pause"}')")
VOTE_PAUSE_ID=$(jq -er .id <<<"$VOTE_PAUSE")
expect_status 403 POST "$SUPER_A_TOKEN" "/admin/config/proposals/$VOTE_PAUSE_ID/confirm" '{"reason":"same token"}' >/dev/null
expect_status 200 POST "$SUPER_B_TOKEN" "/admin/config/proposals/$VOTE_PAUSE_ID/confirm" '{"reason":"independent superadmin"}' >/dev/null
[ "$(vote "$NEAR_AGED" near-close "$RUN_TAG-near-paused" near-aged 10.241.2.1)" = 423 ] || fail "voting pause did not fence votes"
wait_until "voting pause reaches preview watch" 10 preview_status_is 423 \
  "$NEAR_AGED" near-close 500000 "$ARTIFACT_DIR/vote-pause-trade.json"
[ "$(sql "select count(*) from events_outbox where event_type='MarketVotingPaused' and aggregate_id='$NEAR_ID'")" = 1 ] || \
  fail "MarketVotingPaused public event missing"

section "wait for live swarm and assert convoy/ring branch inputs"
wait_until "suppress hidden boundary" 150 market_state_is void-suppress closing
expect_status 409 POST "$OPS_TOKEN" /admin/config \
  "$(jq -cn --arg key "market_paused:$SUPPRESS_ID" '{patch:{($key):false},reason:"must fail in hidden",idempotency_key:"p6-hidden-resume"}')" >/dev/null
wait "$SWARM_PID" || { tail -100 "$ARTIFACT_DIR/swarm.stderr" >&2; fail "simswarm live run failed"; }
SWARM_PID=
[ -s "$ARTIFACT_DIR/slo-report.json" ] || fail "SLO report was not persisted"
jq -e '.slo_enforce==true and .trade_confirm_p95_target_ms==300 and .close_to_paid_p99_target_ms==1000 and .ws_delivery_p95_target_ms==100 and .passed==true' \
  "$ARTIFACT_DIR/slo-report.json" >/dev/null || fail "gated SLO report did not pass: $(cat "$ARTIFACT_DIR/slo-report.json")"

TRADE_CAPABLE=$(jq '[.agents[] | select(.trade_capable and .target_market=="lifecycle")] | length' "$MANIFEST")
CONVOY_SUCCESS=$(jq '[.actions[] |
  select(.opportunity.sim_tick>=60 and .opportunity.sim_tick<120) |
  .decision.actions as $actions | .outcome.outcomes as $outcomes |
  range(0; ($actions|length)) as $i |
  select($actions[$i].action.kind=="trade" and $outcomes[$i].kind=="http" and
         $outcomes[$i].status>=200 and $outcomes[$i].status<300) |
  $actions[$i].agent_id] | unique | length' "$ARTIFACT_DIR/live-trace.json")
[ $((CONVOY_SUCCESS * 2)) -ge "$TRADE_CAPABLE" ] || \
  fail "convoy proof below 50%: $CONVOY_SUCCESS/$TRADE_CAPABLE"

WASH_AGENT=$(jq -r '.rings[] | select(.kind=="wash_pair") | .members[0]' "$MANIFEST")
WASH_USER=$(jq -r --arg id "$WASH_AGENT" '.agents[] | select(.id==$id) | .user_id' "$MANIFEST")
WASH_REP=$(sql "select rep_micro from reputation where user_id='$WASH_USER'")
[ "$WASH_REP" -ge 400000 ] || fail "wash seller was not rep-seeded"
WASH_GROSS=$(sql "select collateral_micro from trades where market_id='$LIFECYCLE_ID' and user_id='$WASH_USER' and side='sell' order by created_at limit 1")
WASH_FEE=$(sql "select fee_micro from trades where market_id='$LIFECYCLE_ID' and user_id='$WASH_USER' and side='sell' order by created_at limit 1")
[ -n "$WASH_GROSS" ] && [ -n "$WASH_FEE" ] || fail "rep-seeded wash sell was not recorded"
EXPECTED_100BP=$(( (WASH_GROSS * 100 + 9999) / 10000 ))
EXPECTED_90BP=$(( (WASH_GROSS * 90 + 9999) / 10000 ))
[ "$WASH_FEE" = "$EXPECTED_100BP" ] && [ "$WASH_FEE" != "$EXPECTED_90BP" ] || \
  fail "wash fee was not 100bp-not-90bp: gross=$WASH_GROSS fee=$WASH_FEE"

wait_until "suppress void branch" 60 market_state_is void-suppress voided
wait_until "pad forced-tally branch" 60 market_state_is threshold-pad resolving
[ -n "$(sql "select curator_flagged_at from markets where id='$PAD_ID'")" ] || fail "pad market was not curator-flagged"
SUPPRESS_OI=$(sql "select coalesce(sum(collateral_micro),0) from trades where market_id='$SUPPRESS_ID'")
[ "$SUPPRESS_OI" -lt 50000000 ] || fail "suppress market unexpectedly crossed OI floor"

section "resume refusal in hidden window and armed resolution crash barrier"
stop_server
rm -f "$ARTIFACT_DIR/resolution.ready"
start_server armed
wait_until "resolution crash readiness" 260 test -f "$ARTIFACT_DIR/resolution.ready"
kill -9 "$SERVER_PID"
wait "$SERVER_PID" 2>/dev/null || true
SERVER_PID=
start_server ordinary
wait_until "lifecycle paid after crash restart" 60 market_state_is lifecycle paid
PAYOUT_TX_COUNT=$(sql "select count(*) from ledger_transactions where idempotency_key='resolve:$LIFECYCLE_ID'")
[ "$PAYOUT_TX_COUNT" = 1 ] || fail "crash recovery payout count was $PAYOUT_TX_COUNT, expected exactly one"

INVARIANTS=$(expect_status 200 GET "$OPS_TOKEN" /admin/invariants '')
jq -e '.pass and ([.identities[].pass] | all)' <<<"$INVARIANTS" >/dev/null || fail "post-crash invariants failed"

wait_until "fat-pot integrity flag" 60 bash -c \
  "[ \"\$(psql -X -At -h '$PGHOST' -p '$PGPORT' -U '$PGUSER' -d '$PGDATABASE' -c \"select verdict from integrity_reports where market_id='$FAT_ID'\")\" = flag ]"
[ "$(sql "select verdict from integrity_reports where market_id='$LIFECYCLE_ID'")" = pass ] || fail "lifecycle integrity verdict was not Pass"
LIFE_SIGNALS=$(sql "select count(*) from integrity_reports r, jsonb_array_elements(r.checks) c where r.market_id='$LIFECYCLE_ID' and (c->>'flagged')::boolean")
FAT_SIGNALS=$(sql "select count(*) from integrity_reports r, jsonb_array_elements(r.checks) c where r.market_id='$FAT_ID' and (c->>'flagged')::boolean")
[ "$LIFE_SIGNALS" -le 1 ] || fail "lifecycle Pass had more than one signal"
[ "$FAT_SIGNALS" -ge 2 ] || fail "fat-pot Flag had fewer than two signals"

section "RBAC, honest fraud-void unwind, remedial credit, and faucet controls"
expect_status 200 POST "$CURATOR_TOKEN" "/admin/markets/$FAT_ID/resolve" '{"decision":"void"}' >/dev/null
expect_status 403 POST "$FINANCE_A_TOKEN" "/admin/markets/$FAT_ID/unwind/propose" \
  '{"reason":"wrong role","idempotency_key":"bad-role"}' >/dev/null
UNWIND=$(expect_status 200 POST "$SUPER_A_TOKEN" "/admin/markets/$FAT_ID/unwind/propose" \
  '{"reason":"the FRAUD void was wrong","idempotency_key":"p6-fat-unwind"}')
[ "$(jq -r .stage <<<"$UNWIND")" = proposed ] || fail "unwind was not proposed"
expect_status 403 POST "$SUPER_A_TOKEN" "/admin/markets/$FAT_ID/unwind/confirm" \
  '{"reason":"wrong confirming role","idempotency_key":"p6-fat-unwind"}' >/dev/null
expect_status 409 POST "$SUPER_A_TOKEN" "/admin/markets/$LIFECYCLE_ID/unwind/propose" \
  '{"reason":"paid must refuse","idempotency_key":"p6-paid-refuse"}' >/dev/null

expect_status 422 POST "$FINANCE_A_TOKEN" "/admin/markets/$LIFECYCLE_ID/remedial_credit/propose" \
  "$(jq -cn --arg user "$CONFIG_USER" '{user_id:$user,amount_micro:500000001,reason:"cap proof",idempotency_key:"p6-remedial-over"}')" >/dev/null
expect_status 409 POST "$FINANCE_A_TOKEN" "/admin/markets/$LIFECYCLE_ID/remedial_credit/propose" \
  "$(jq -cn --arg user "$LINKED_USER" '{user_id:$user,amount_micro:1000000,reason:"self credit proof",idempotency_key:"p6-remedial-self"}')" >/dev/null
expect_status 200 POST "$FINANCE_A_TOKEN" "/admin/markets/$LIFECYCLE_ID/remedial_credit/propose" \
  "$(jq -cn --arg user "$CONFIG_USER" '{user_id:$user,amount_micro:1000000,reason:"discretionary credit, not refund",idempotency_key:"p6-remedial-ok"}')" >/dev/null

sleep 61
expect_status 200 POST "$FINANCE_A_TOKEN" "/admin/markets/$FAT_ID/unwind/confirm" \
  '{"reason":"independent finance confirmation","idempotency_key":"p6-fat-unwind"}' >/dev/null
expect_status 200 POST "$FINANCE_B_TOKEN" "/admin/markets/$LIFECYCLE_ID/remedial_credit/confirm" \
  '{"reason":"independent finance confirmation","idempotency_key":"p6-remedial-ok"}' >/dev/null
[ "$(sql "select count(*) from ledger_transactions where idempotency_key='unwind:$FAT_ID'")" = 1 ] || fail "unwind reversal not exactly once"
[ "$(sql "select count(*) from ledger_transactions where idempotency_key='unwind:$FAT_ID' and kind='reversal'")" = 1 ] || fail "unwind did not use Reversal"

section "named chaos set and Pg manual-ops contracts"
run_logged chaos-named cargo test -p simswarm chaos::tests -- --test-threads=1
run_logged reconciler-heal cargo test -p application ops::reconciler::tests::a_missed_wake_up_heals_from_the_authoritative_generation -- --exact
run_logged ops-pg-contract cargo test -p adapters --test w2_ops_contract -- --test-threads=1
run_logged unwind-contract cargo test -p application ops::unwind_market::tests -- --test-threads=1

section "near-close age rule and voting-pause auto-expiry"
wait_until "near-close hidden boundary" 360 market_state_is near-close closing
[ "$(vote "$NEAR_AGED" near-close "$RUN_TAG-near-aged-at-hidden" near-aged 10.241.2.1)" = 200 ] || \
  fail "aged vote was not 200 after voting pause auto-expired"
[ "$(vote "$NEAR_YOUNG_LATE" near-close "$RUN_TAG-near-young-at-hidden" near-young 10.241.3.1)" = 422 ] || \
  fail "young near-close vote was not 422"
VOTE_LATE=$(expect_status 200 POST "$SUPER_A_TOKEN" /admin/config/proposals \
  "$(jq -cn --arg key "voting_paused:$NEAR_ID" '{patch:{($key):true},reason:"hidden write refusal",idempotency_key:"p6-voting-too-late"}')")
VOTE_LATE_ID=$(jq -er .id <<<"$VOTE_LATE")
VOTE_LATE_REFUSAL=$(expect_status 409 POST "$SUPER_B_TOKEN" "/admin/config/proposals/$VOTE_LATE_ID/confirm" \
  '{"reason":"hidden write must refuse"}')
jq -e '.code != null' <<<"$VOTE_LATE_REFUSAL" >/dev/null || fail "late voting pause refusal was untyped"

section "trace replay, final invariants, and thin mobile Playwright"
"$BIN/simswarm" --manifest "$MANIFEST" --replay-log "$ARTIFACT_DIR/live-trace.json" \
  --output "$ARTIFACT_DIR/replayed-trace.json"
cmp -s "$ARTIFACT_DIR/live-trace.json" "$ARTIFACT_DIR/replayed-trace.json" || fail "DecidedAction replay differed"
INVARIANTS=$(expect_status 200 GET "$OPS_TOKEN" /admin/invariants '')
jq -e '.pass and ([.identities[].pass] | all)' <<<"$INVARIANTS" >/dev/null || fail "final invariants failed"
wait "$PLAYWRIGHT_PID" || { tail -160 "$ARTIFACT_DIR/playwright.log" >&2; fail "mobile Playwright smoke failed"; }
PLAYWRIGHT_PID=

[ -f docs/copy/ops.md ] || fail "docs/copy/ops.md missing"
for phrase in "not D22" "not a config hatch" "void was the wrong FRAUD decision" \
  "not a refund" "dual control" 'handle == digest'; do
  grep -Fqi "$phrase" docs/copy/ops.md || fail "ops.md missing honesty phrase: $phrase"
done

section "Phase 7 red-team (each attack must fail)"
# Every leg asserts an EXACT status or an EXACT database fact. Where a
# production surface does not exist yet the leg records a NAMED prerequisite
# and the run fails at the gate before the GREEN line. Nothing here may pass
# because a route was missing (404) or stubbed (503).

# A live book. Every Phase 6 market is Paid or Voided by this point, so a
# conversion boundary or a money-path concurrency race against them proves
# nothing: the old concurrency leg raced a Paid market, and the lifecycle
# refusal that came back was counted as a success.
P7_MARKET=$(create_market p7-live 420 60 200000000 100 3)
# Three votes clear min_votes_to_resolve; the pad trade keeps the book above
# OI_FLOOR_MICRO so it resolves Paid instead of auto-voiding thin.
[ "$(vote "$PAD_A" p7-live "$RUN_TAG-p7-pad-a-vote" pad-a-device 10.242.2.1)" = 200 ] || \
  fail "p7-live pad-a vote failed"
[ "$(vote "$PAD_B" p7-live "$RUN_TAG-p7-pad-b-vote" pad-b-device 10.243.2.1)" = 200 ] || \
  fail "p7-live pad-b vote failed"
P7_PAD_PREVIEW="$ARTIFACT_DIR/p7-pad-preview.json"
expect_money_status 200 "p7-live pad preview" \
  "$(preview "$PAD_A" p7-live 25000000 "$P7_PAD_PREVIEW")" "$P7_PAD_PREVIEW"
expect_money_status 200 "p7-live pad trade" \
  "$(place "$PAD_A" p7-live 25000000 "$(jq -er .config_version "$P7_PAD_PREVIEW")" \
    "$RUN_TAG-p7-pad-trade" "$ARTIFACT_DIR/p7-pad-place.json")" "$ARTIFACT_DIR/p7-pad-place.json"

# 1. wash-after-grant: real fee volume >= lot must not convert before Paid, and
# MUST convert after Paid. The old leg only PREVIEWED, which allocates no fees
# and converts no lot, so its no-convert assertion held vacuously.
BONUS_LOT=200000
WASH_NOTIONAL=25000000
GRANT_USER=$(signup p7-bonus-wash +15559880001 true 0)
fund "$GRANT_USER" 600000000 "$RUN_TAG-bonus-wash"
# Grant ISSUANCE has no production surface (/admin/credits/grant/* is a 503
# stub), so the lot is a fixture. Everything asserted about it below is
# production behaviour: fee allocation, the pre-Paid no-convert rule, and the
# post-Paid lazy convert.
sql "insert into credit_grant_lots(user_id,source,amount_micro,grant_class,policy_version)
     values ('$GRANT_USER','signup',$BONUS_LOT,'real_money','e2e')" >/dev/null
prereq_missing "credit grant issuance: /admin/credits/grant/{propose,confirm} are phase7_admin_stubs 503s, so the reserve-at-grant encumbrance cannot be exercised and the lot must be seeded"
[ "$(vote "$GRANT_USER" p7-live "$RUN_TAG-wash-vote" wash-device 10.246.1.1)" = 200 ] || \
  fail "wash-after-grant prerequisite vote failed"
WASH_PREVIEW="$ARTIFACT_DIR/p7-wash-preview.json"
expect_money_status 200 "wash-after-grant preview" \
  "$(preview "$GRANT_USER" p7-live "$WASH_NOTIONAL" "$WASH_PREVIEW")" "$WASH_PREVIEW"
WASH_VERSION=$(jq -er .config_version "$WASH_PREVIEW")
WASH_FEE_QUOTED=$(jq -er .fee_micro "$WASH_PREVIEW")
[ "$WASH_FEE_QUOTED" -ge "$BONUS_LOT" ] || \
  fail "wash-after-grant fee volume $WASH_FEE_QUOTED is below the lot $BONUS_LOT: the Paid-finalization rule would not be under test"
expect_money_status 200 "wash-after-grant trade" \
  "$(place "$GRANT_USER" p7-live "$WASH_NOTIONAL" "$WASH_VERSION" "$RUN_TAG-wash-trade" "$ARTIFACT_DIR/p7-wash-place.json")" \
  "$ARTIFACT_DIR/p7-wash-place.json"
# Positive control: the fee really was allocated against the lot. Delete the
# credits allocation code and this is 0, so the no-convert assertion can no
# longer pass for the wrong reason.
WASH_ALLOCATIONS=$(sql "select count(*) from credit_fee_allocations a
  join credit_grant_lots l on l.id = a.lot_id where l.user_id='$GRANT_USER'")
[ "$WASH_ALLOCATIONS" -ge 1 ] || \
  fail "no credit_fee_allocations rows for the granted user: fee volume was never allocated against the lot"
[ "$(sql "select count(*) from credit_grant_lots
          where user_id='$GRANT_USER' and converted_at is null")" = 1 ] || \
  fail "wash-after-grant converted before Paid"

# 2-4. A<->B referral once; unverified-phone grant 0; third device zero.
# The bind use cases (BindReferral / BindReferralCode) have NO mounted route,
# so no referral attack can be driven through production code. The old leg
# INSERTed the referral_binds rows itself and then counted grants that nothing
# could ever have created — it passed against an empty implementation.
REF_A=$(signup p7-ref-a +15559880002 true 0)
REF_B=$(signup p7-ref-b +15559880003 true 0)
prereq_missing "referral bind surface: application::money::referrals::{BindReferral,BindReferralCode} have no mounted route (rg finds no caller outside tests), so the A<->B-once, unverified-phone and third-device attacks cannot be driven through production code. This leg refuses to fabricate the binds it is supposed to test."
# No bind can exist yet. If one appears, it was written by something other than
# a production code path and this run must not certify.
BINDS=$(sql "select count(*) from referral_binds")
[ "$BINDS" = 0 ] || \
  fail "referral_binds is non-empty ($BINDS) with no bind surface mounted: a fixture is standing in for the behaviour under test"
# Invariant that holds regardless of how a bind was created: the P7 book pays
# below, and grant_referral_on_paid_in_tx is wired into resolve, so a referral
# grant reaching an unverified phone would show up here.
UNVERIFIED_GRANTS=$(sql "select coalesce(sum(amount_micro),0) from credit_grant_lots l
  left join phone_verifications p on p.user_id=l.user_id and p.verified_at is not null
  where l.source like 'referral%' and p.id is null")
[ "$UNVERIFIED_GRANTS" = 0 ] || fail "unverified-phone referral grant was $UNVERIFIED_GRANTS"

# 5. omitted expected_config_version: exactly 422. `TradeRequest`'s field is a
# required i64, so serde refuses the body before the book is touched. The old
# leg also accepted 400 and every other non-200, which a missing route satisfies.
OMIT_STATUS=$(public_post /trades "$(jq -cn --arg user "$CONFIG_USER" \
  '{user_id:$user,market_ref:"p7-live",side:"yes",action:"buy",amount_micro:1000000,idempotency_key:"p7-omit-ver"}')" \
  "$ARTIFACT_DIR/p7-omit-version.json")
expect_money_status 422 "trade with omitted expected_config_version" \
  "$OMIT_STATUS" "$ARTIFACT_DIR/p7-omit-version.json"
grep -q expected_config_version "$ARTIFACT_DIR/p7-omit-version.json" || \
  fail "the 422 did not name the missing expected_config_version field"

# 6. fee override sandwich (D36): a preview taken at the pre-override generation
# must 409 StaleConfig once the override lands, so no one can quote one fee and
# fill at another. The old leg accepted the 503 stub as the attack failing.
# PAD_A already voted on p7-live; PlaceTrade refuses an unvoted user with
# VoteRequired, which would mask the StaleConfig this leg is looking for.
OVERRIDE_PREVIEW="$ARTIFACT_DIR/p7-override-preview.json"
expect_money_status 200 "fee override pre-preview" \
  "$(preview "$PAD_A" p7-live 1000000 "$OVERRIDE_PREVIEW")" "$OVERRIDE_PREVIEW"
OVERRIDE_VERSION=$(jq -er .config_version "$OVERRIDE_PREVIEW")
FEE_OV=$(request POST "$FINANCE_A_TOKEN" "/admin/markets/$P7_MARKET/fee_override/propose" \
  "$(jq -cn '{value:{"override":110},reason:"p7 override",replay_key:"p7-fee"}')" \
  "$ARTIFACT_DIR/p7-fee-override.json")
if route_is_live "market fee_override" "$FEE_OV"; then
  expect_money_status 200 "fee override propose" "$FEE_OV" "$ARTIFACT_DIR/p7-fee-override.json"
  FEE_OV_ID=$(jq -er .id "$ARTIFACT_DIR/p7-fee-override.json")
  expect_status 403 POST "$FINANCE_A_TOKEN" \
    "/admin/markets/$P7_MARKET/fee_override/confirm" \
    "$(jq -cn --arg id "$FEE_OV_ID" '{proposal_id:$id,reason:"same principal must fail"}')" >/dev/null
  expect_status 200 POST "$FINANCE_B_TOKEN" \
    "/admin/markets/$P7_MARKET/fee_override/confirm" \
    "$(jq -cn --arg id "$FEE_OV_ID" '{proposal_id:$id,reason:"independent finance confirmation"}')" >/dev/null
  expect_money_status 409 "fee override sandwich fill at the pre-override version" \
    "$(place "$PAD_A" p7-live 1000000 "$OVERRIDE_VERSION" "$RUN_TAG-override-sandwich" \
      "$ARTIFACT_DIR/p7-override-sandwich.json")" "$ARTIFACT_DIR/p7-override-sandwich.json"
  [ "$(jq -r .code "$ARTIFACT_DIR/p7-override-sandwich.json")" = StaleConfig ] || \
    fail "the fee override sandwich was refused, but not as StaleConfig"
fi

# 7. KYC webhook authentication. `ingest_kyc` validates event_id and
# provider_ref BEFORE the signature, so the old probe body ({"event":...}) could
# only ever have produced a 422 InvalidWebhook — it never reached the auth gate.
# A well-formed unsigned delivery is exactly 403 AdminForbidden, and the signed
# delivery is the positive control without which "rejected" is indistinguishable
# from "route absent".
KYC_WEBHOOK_SECRET=${KYC_WEBHOOK_SECRET:-p7-kyc-webhook-secret}
KYC_USER=$(signup p7-kyc +15559880011 true 0)
KYC_REF="p7-provider-ref-$RUN_TAG"
sql "insert into config_entries(key,value)
     values ('kyc_provider_ref:$KYC_REF', to_jsonb('$KYC_USER'::text))
     on conflict (key) do update set value = excluded.value" >/dev/null
KYC_BODY=$(jq -cn --arg ref "$KYC_REF" --arg event "p7-kyc-event-$RUN_TAG" \
  '{event_id:$event,provider:"kyc",provider_ref:$ref,to_tier:3,policy_version:"e2e-staging"}')
UNSIGNED=$(curl -sS -o "$ARTIFACT_DIR/p7-unsigned-webhook.json" -w '%{http_code}' \
  -H 'content-type: application/json' -d "$KYC_BODY" "$BASE/webhooks/kyc")
if route_is_live "KYC provider webhook" "$UNSIGNED"; then
  expect_money_status 403 "unsigned KYC webhook" "$UNSIGNED" \
    "$ARTIFACT_DIR/p7-unsigned-webhook.json"
  KYC_SIG=$(printf '%s' "$KYC_BODY" | \
    openssl dgst -sha256 -hmac "$KYC_WEBHOOK_SECRET" -hex | awk '{print $NF}')
  SIGNED=$(curl -sS -o "$ARTIFACT_DIR/p7-signed-webhook.json" -w '%{http_code}' \
    -H 'content-type: application/json' -H "x-kyc-signature: $KYC_SIG" \
    -d "$KYC_BODY" "$BASE/webhooks/kyc")
  expect_money_status 200 "signed KYC webhook positive control" "$SIGNED" \
    "$ARTIFACT_DIR/p7-signed-webhook.json"
  [ "$(sql "select kyc_tier from users where id='$KYC_USER'")" = 3 ] || \
    fail "the signed KYC webhook was accepted but applied no tier"
fi

# 8. no-KYC chain deposit → suspense (row without admit), recon does not drift.
# The observation identity CHECK is real: a partial row must not be insertable,
# and a complete one must NOT reach a user without KYC.
OBS_SIG="p7-obs-$RUN_TAG"
# PRESERVED CONTROL: the observation identity CHECK really refuses a partial
# row. The .err file carries the Postgres violation.
if sql "insert into deposits(chain_sig,amount_micro,status,machine_status,source_address)
        values ('$OBS_SIG-partial',25000000,'observed_finalized','observed_finalized','src-p7')" \
        >/dev/null 2>"$ARTIFACT_DIR/p7-partial-observation.err"; then
  fail "a partial finalized observation was accepted without dest/mint/slot"
fi
grep -q deposits_observation_identity "$ARTIFACT_DIR/p7-partial-observation.err" || \
  fail "the partial observation was refused, but not by deposits_observation_identity"
# `rail_fingerprint` is the fifth column the CHECK requires. Omitting it made
# this INSERT abort the whole run under ON_ERROR_STOP + set -e.
sql "insert into deposits(chain_sig,amount_micro,status,machine_status,
                          source_address,dest_address,mint,observed_slot,rail_fingerprint)
     values ('$OBS_SIG',25000000,'observed_finalized','observed_finalized',
             'src-p7','treasury-p7','usdc-mint-p7',9001,'e2e-staging-rail')" >/dev/null
OBS_ID=$(sql "select id from deposits where chain_sig='$OBS_SIG'")
# Drive the ADMISSION MACHINE instead of re-reading the columns this script
# omitted from its own INSERT. An unheld observation is not admissible, so the
# dual-control propose is exactly 409; delete that guard and this goes green.
expect_money_status 409 "manual admit propose against an unheld observation" \
  "$(request POST "$FINANCE_A_TOKEN" "/admin/deposits/$OBS_ID/admit/propose" \
    '{"reason":"no-KYC observation must not be admissible"}' \
    "$ARTIFACT_DIR/p7-obs-admit.json")" "$ARTIFACT_DIR/p7-obs-admit.json"
[ "$(sql "select count(*) from deposits where chain_sig='$OBS_SIG' and admit_tx_id is not null")" = 0 ] || \
  fail "the refused admit proposal still credited the observation"
prereq_missing "inbound chain watcher: adapters/src/rails/watcher.rs is never constructed in main.rs, so no genuine compliance_hold deposit with suspense backing can exist; the no-KYC suspense path and the admit-versus-refund one-winner race are both fixtures away from production"
# The signed formula books it as observed-unbooked inbound, never as drift.
run_logged recon-formula cargo test -p application ops::chain_reconcile -- --test-threads=1
sql "delete from deposits where chain_sig='$OBS_SIG'" >/dev/null

# 9. $5-then-dump dest stays in review. Both legs are below dest_warm_floor
# ($100), so the dest can never warm; both must be ACCEPTED as review-required
# holds carrying `dest_not_warm`, and neither may reach approved. The old leg
# threw both status codes away, so it also passed when the route was absent.
# ($1 is below withdraw_min_micro, which would refuse on `amount` and never
# reach the warmth rule at all.)
DUST_USER=$(signup p7-dust +15559880004 true 0)
fund "$DUST_USER" 200000000 "$RUN_TAG-dust"
DUST_DEST="97voHZSezZSSCz1BsWsicAsnuz5vU8BRE8kjL3BHLokv"
for dust_amount in 5000000 90000000; do
  expect_money_status 200 "dest-warming withdrawal ($dust_amount)" \
    "$(withdraw_request "$DUST_USER" "$dust_amount" "$DUST_DEST" \
      "$RUN_TAG-dust-$dust_amount" "$ARTIFACT_DIR/p7-dust-$dust_amount.json")" \
    "$ARTIFACT_DIR/p7-dust-$dust_amount.json"
done
[ "$(sql "select count(*) from withdrawals
          where user_id='$DUST_USER' and dest_address='$DUST_DEST'
            and review_state='approved'")" = 0 ] || \
  fail "a \$5-then-dump dest auto-approved without warmth"
# Positive control: the warmth rule actually ran. Delete `dest_not_warm` from
# current_reasons and the assertion above still passes; this one does not.
[ "$(sql "select count(*) from withdrawals
          where user_id='$DUST_USER' and dest_address='$DUST_DEST'
            and risk_reasons @> '[\"dest_not_warm\"]'::jsonb")" = 2 ] || \
  fail "the dest-warming holds do not carry the dest_not_warm risk reason"

# 10. Hit user + mule dest, Hit source no auto-refund, self-excluded new dest.
# A sanctions Hit can only be produced by SandboxCompliance::plant_hit, which is
# an in-process fixture with no HTTP surface, and the withdraw path re-screens
# through that provider on every request — so a `sanction_screenings` INSERT is
# overwritten and read by nothing. The old leg inserted the Hit and then
# asserted the verdict it had just inserted. There is nothing honest to assert
# here until the staging fixture is reachable.
prereq_missing "sanctions Hit fixture: SandboxCompliance::plant_hit has no route and RequestWithdraw re-screens via the provider on every request, so the Hit-user/mule-dest and Hit-source-no-auto-refund legs cannot be driven; a staging-armed compliance fixture endpoint is required"
prereq_missing "self-exclusion surface: application::money::self_exclusion::start_self_exclusion has no mounted route (compliance_admin::public_router is not merged), so the self-excluded-to-new-dest hold cannot be driven"

# 11. settle-then-retry: an exhausted daily cap refuses, and the identical
# retry replays the SAME refusal instead of minting a second hold. The old leg
# sent one request, at an amount above withdraw_max_micro, and accepted any
# non-200. Distinct dests keep dest_daily_limit_micro out of the way so the
# refusal is the USER daily cap.
RETRY_USER=$(signup p7-retry +15559880006 true 0)
for retry_fund in 1 2 3; do
  fund "$RETRY_USER" 1000000000 "$RUN_TAG-retry-fund-$retry_fund"
done
RETRY_DEST_A="F2tB68m4XXtNPWMpUwWwePJerqUsKSNWmtdNTb93smW"
RETRY_DEST_B="MtizwN2bowUw4r7ziyZeyzFbxPEKpMspJmH69bjJXS3"
RETRY_DEST_C="8P7JmHvAM3swW3ZrwppQ3bRUNemYv8VxNBUpqG3mfXp1"
expect_money_status 200 "daily-cap leg A" \
  "$(withdraw_request "$RETRY_USER" 1000000000 "$RETRY_DEST_A" "$RUN_TAG-retry-a" \
    "$ARTIFACT_DIR/p7-retry-a.json")" "$ARTIFACT_DIR/p7-retry-a.json"
expect_money_status 200 "daily-cap leg B" \
  "$(withdraw_request "$RETRY_USER" 1000000000 "$RETRY_DEST_B" "$RUN_TAG-retry-b" \
    "$ARTIFACT_DIR/p7-retry-b.json")" "$ARTIFACT_DIR/p7-retry-b.json"
HOLDS_BEFORE_RETRY=$(sql "select count(*) from withdrawals where user_id='$RETRY_USER'")
RETRY_KEY="$RUN_TAG-retry-c"
expect_money_status 429 "over-daily-cap withdrawal" \
  "$(withdraw_request "$RETRY_USER" 1000000000 "$RETRY_DEST_C" "$RETRY_KEY" \
    "$ARTIFACT_DIR/p7-daily-retry.json")" "$ARTIFACT_DIR/p7-daily-retry.json"
[ "$(jq -r .refuse_code "$ARTIFACT_DIR/p7-daily-retry.json")" = limit ] || \
  fail "the over-cap withdrawal was refused, but not on the daily limit"
expect_money_status 429 "identical over-cap retry" \
  "$(withdraw_request "$RETRY_USER" 1000000000 "$RETRY_DEST_C" "$RETRY_KEY" \
    "$ARTIFACT_DIR/p7-daily-retry-replay.json")" "$ARTIFACT_DIR/p7-daily-retry-replay.json"
[ "$(jq -r .replayed "$ARTIFACT_DIR/p7-daily-retry-replay.json")" = true ] || \
  fail "the identical retry was re-evaluated instead of replaying the refusal"
[ "$(sql "select count(*) from withdrawals where user_id='$RETRY_USER'")" = "$HOLDS_BEFORE_RETRY" ] || \
  fail "the retry minted a second hold after the daily cap was exhausted"
# Replaying the key under a different amount is a typed conflict, not a fill.
expect_money_status 409 "retry key reused for a different amount" \
  "$(withdraw_request "$RETRY_USER" 900000000 "$RETRY_DEST_C" "$RETRY_KEY" \
    "$ARTIFACT_DIR/p7-retry-conflict.json")" "$ARTIFACT_DIR/p7-retry-conflict.json"
[ "$(jq -r .code "$ARTIFACT_DIR/p7-retry-conflict.json")" = IdempotencyConflict ] || \
  fail "reusing the retry key for a different amount was not an IdempotencyConflict"

# 12. AML structuring, both pinned series. Only the WITHDRAW path evaluates
# candidates (evaluate_withdraw_aml_candidate); the old leg fired four faucet
# DEPOSITS, which nothing evaluates, so "no flag" was true with no detector at
# all. Four in-band $499 legs must open a structuring flag; four below-floor
# $25 legs must not.
AML_HIT_USER=$(signup p7-aml-hit +15559880007 true 0)
AML_CLEAN_USER=$(signup p7-aml-clean +15559880008 true 0)
for aml_fund in 1 2; do
  fund "$AML_HIT_USER" 1000000000 "$RUN_TAG-aml-hit-fund-$aml_fund"
done
fund "$AML_CLEAN_USER" 200000000 "$RUN_TAG-aml-clean-fund"
AML_DESTS=(
  "H7Chr6MbaQxBSpNwquGMJBFkGNVWSLWiLc5TaY3FQft3"
  "wd2BnRD5KF8A1MGMqqJzyKiJ7hG1prpS2TFthRot7P5"
  "Gyk4cbbHr3i7kKzJcnsb8xnmuwND3i2REtmuoDv9hP5h"
  "Gg8oPYDnNmDd882SCkqWXb7y4PzfeNDBPekzRaVKHDyn"
)
aml_leg=0
for aml_dest in "${AML_DESTS[@]}"; do
  aml_leg=$((aml_leg + 1))
  expect_money_status 200 "in-band structuring leg $aml_leg" \
    "$(withdraw_request "$AML_HIT_USER" 499000000 "$aml_dest" "$RUN_TAG-aml-band-$aml_leg" \
      "$ARTIFACT_DIR/p7-aml-band-$aml_leg.json")" "$ARTIFACT_DIR/p7-aml-band-$aml_leg.json"
done
[ "$(sql "select count(*) from aml_flags
          where user_id='$AML_HIT_USER' and rule='structuring' and status='open'")" -ge 1 ] || \
  fail "four in-band \$499 withdrawals did not open a structuring AML flag"
AML_DUST_DEST="5Q1A82RqTdG2Hbj648xWBdup4CaBRw1XHULxuqQiHPQZ"
for aml_dust in 1 2 3 4; do
  expect_money_status 200 "below-floor structuring leg $aml_dust" \
    "$(withdraw_request "$AML_CLEAN_USER" 25000000 "$AML_DUST_DEST" "$RUN_TAG-aml-dust-$aml_dust" \
      "$ARTIFACT_DIR/p7-aml-dust-$aml_dust.json")" "$ARTIFACT_DIR/p7-aml-dust-$aml_dust.json"
done
[ "$(sql "select count(*) from aml_flags
          where user_id='$AML_CLEAN_USER' and rule='structuring' and status='open'")" = 0 ] || \
  fail "four below-floor \$25 withdrawals opened a structuring AML flag"

# 13. reconciliation: nothing may page without a residual. The old leg INSERTed
# the alert row itself and then asserted that row existed, so it tested
# PostgreSQL, not the detector: `residual_incident` has no caller outside its
# own module, and no scheduled cut runs in main.rs.
[ "$(sql "select count(*) from alert_outbox
          where incident_key like 'reconciliation_residual%' and status='open'")" = 0 ] || \
  fail "a reconciliation_residual incident is open on a run with no residual"
prereq_missing "reconciliation detector: application::ops::chain_reconcile::residual_incident has no caller and no scheduled cut runs in main.rs, so 'an injected 1u residual pages, in-flight does not' cannot be proven end to end; only the pure formula test above covers it"

# 13b. concurrent withdraw + trade + deposit + unwind conserves (D31 sorted-lock
# algorithm). The trade and withdraw racers run against the LIVE p7-live book at
# its CURRENT config generation, so they are real money mutations rather than
# lifecycle refusals; the previous version raced a Paid book, which bounced the
# trade before any lock was taken and proved nothing about the sorted-lock
# algorithm. The unwind racer still targets the Paid lifecycle book, because
# D30's "a Paid market is never unwound" is the invariant it exists to test.
CONC_USER=$(signup p7-concurrent +15559880005 true 0)
fund "$CONC_USER" 300000000 "$RUN_TAG-conc"
CONC_DEST="Hk1gjLRzoKjCvnH4ubFmYN6qK85TJ3UEPqhQDV2vBUCz"
[ "$(vote "$CONC_USER" p7-live "$RUN_TAG-conc-vote" conc-device 10.247.1.1)" = 200 ] || \
  fail "concurrency racer prerequisite vote failed"
CONC_PREVIEW="$ARTIFACT_DIR/p7-conc-preview.json"
expect_money_status 200 "concurrency racer preview" \
  "$(preview "$CONC_USER" p7-live 25000000 "$CONC_PREVIEW")" "$CONC_PREVIEW"
CONC_VERSION=$(jq -er .config_version "$CONC_PREVIEW")
market_state_is p7-live live || \
  fail "the concurrency leg needs p7-live still open for trading; it is $(curl -fsS "$BASE/markets/p7-live" | jq -r .state)"
PAID_UNWINDS_BEFORE=$(sql "select count(*) from ledger_transactions where idempotency_key='unwind:$LIFECYCLE_ID'")
withdraw_request "$CONC_USER" 5000000 "$CONC_DEST" "$RUN_TAG-conc-withdraw" \
  "$ARTIFACT_DIR/p7-conc-withdraw.json" >"$ARTIFACT_DIR/p7-conc-withdraw.status" 2>/dev/null &
CONC_PIDS=($!)
public_post /trades "$(jq -cn --arg user "$CONC_USER" --arg key "$RUN_TAG-conc-trade" \
  --argjson version "$CONC_VERSION" \
  '{user_id:$user,market_ref:"p7-live",side:"yes",action:"buy",amount_micro:25000000,idempotency_key:$key,expected_config_version:$version}')" \
  "$ARTIFACT_DIR/p7-conc-trade.json" >"$ARTIFACT_DIR/p7-conc-trade.status" 2>/dev/null &
CONC_PIDS+=($!)
for deposit_leg in a b; do
  request POST "$FINANCE_A_TOKEN" /admin/deposits \
    "$(jq -cn --arg user "$CONC_USER" --arg key "$RUN_TAG-conc-deposit-$deposit_leg" \
      '{user_id:$user,amount_micro:10000000,reason:"concurrency leg",idempotency_key:$key}')" \
    "$ARTIFACT_DIR/p7-conc-deposit-$deposit_leg.json" \
    >"$ARTIFACT_DIR/p7-conc-deposit-$deposit_leg.status" 2>/dev/null &
  CONC_PIDS+=($!)
done
request POST "$SUPER_A_TOKEN" "/admin/markets/$LIFECYCLE_ID/unwind/propose" \
  '{"reason":"concurrency racer","idempotency_key":"p7-conc-unwind"}' \
  "$ARTIFACT_DIR/p7-conc-unwind.json" >"$ARTIFACT_DIR/p7-conc-unwind.status" 2>/dev/null &
CONC_PIDS+=($!)
# Never a bare `wait` here: the core server and the web dev server are still
# background jobs of this shell and would never exit.
for conc_pid in "${CONC_PIDS[@]}"; do wait "$conc_pid" || true; done
expect_money_status 200 "concurrent trade on the live book" \
  "$(cat "$ARTIFACT_DIR/p7-conc-trade.status")" "$ARTIFACT_DIR/p7-conc-trade.json"
expect_money_status 200 "concurrent withdrawal on the live book" \
  "$(cat "$ARTIFACT_DIR/p7-conc-withdraw.status")" "$ARTIFACT_DIR/p7-conc-withdraw.json"
expect_money_status 409 "unwind racer against the Paid lifecycle book" \
  "$(cat "$ARTIFACT_DIR/p7-conc-unwind.status")" "$ARTIFACT_DIR/p7-conc-unwind.json"
[ "$(sql "select count(*) from ledger_transactions where idempotency_key='unwind:$LIFECYCLE_ID'")" = "$PAID_UNWINDS_BEFORE" ] || \
  fail "a Paid book was unwound by the concurrency racer"
# Each faucet deposit landed exactly as often as it was accepted: `deposits`
# is keyed by chain_sig, so a doubled credit shows up as a second row.
CONC_ACCEPTED=0
for deposit_leg in a b; do
  if [ "$(cat "$ARTIFACT_DIR/p7-conc-deposit-$deposit_leg.status")" = 200 ]; then
    CONC_ACCEPTED=$((CONC_ACCEPTED + 1))
  fi
  [ "$(sql "select count(*) from deposits where chain_sig like '%conc-deposit-$deposit_leg%'")" -le 1 ] || \
    fail "concurrent faucet deposit $deposit_leg was credited more than once"
done
# +1 for this user's opening `fund` row.
[ "$(sql "select count(*) from deposits where user_id='$CONC_USER'")" = "$((CONC_ACCEPTED + 1))" ] || \
  fail "concurrent faucet deposits did not land exactly once per accepted request"
[ "$(sql "select coalesce(sum(amount_micro),0) from ledger_entries")" = 0 ] || \
  fail "the concurrent money racers did not conserve value"
[ "$(sql "select count(*) from (select txn_id from ledger_entries
          group by txn_id having sum(amount_micro) <> 0) unbalanced")" = 0 ] || \
  fail "a concurrent transaction landed unbalanced"
[ "$(sql "select count(*) from ledger_accounts a
          join (select account_id, sum(amount_micro) as bal from ledger_entries
                group by account_id) s on s.account_id = a.id
          where a.owner_type in ('user','withheld','deposit_suspense','bonus_reserve')
            and s.bal < 0")" = 0 ] || \
  fail "a non-negative authority went negative under the concurrent racers"

# 14. admit versus refund: neither command is available on a deposit that is not
# in compliance_hold, and the refund DTO carries no destination at all — the
# dest is the observation's source_address, so an attacker cannot choose it. The
# old leg probed a made-up deposit id and accepted the resulting 404.
CONC_DEPOSIT_ID=$(sql "select id from deposits where user_id='$CONC_USER' order by created_at limit 1")
[ -n "$CONC_DEPOSIT_ID" ] || fail "no real deposit row to drive the admit/refund commands against"
expect_money_status 409 "refund propose against a deposit that is not held" \
  "$(request POST "$FINANCE_A_TOKEN" "/admin/deposits/$CONC_DEPOSIT_ID/refund/propose" \
    '{"reason":"must be source-locked"}' "$ARTIFACT_DIR/p7-refund.json")" \
  "$ARTIFACT_DIR/p7-refund.json"
expect_money_status 409 "admit propose against a deposit that is not held" \
  "$(request POST "$FINANCE_A_TOKEN" "/admin/deposits/$CONC_DEPOSIT_ID/admit/propose" \
    '{"reason":"must require compliance_hold"}' "$ARTIFACT_DIR/p7-admit.json")" \
  "$ARTIFACT_DIR/p7-admit.json"
# Curator is not finance: the money-command matrix is enforced before the state
# check, so a wrong role cannot even learn whether the deposit is held.
expect_money_status 403 "refund propose from a non-finance role" \
  "$(request POST "$CURATOR_TOKEN" "/admin/deposits/$CONC_DEPOSIT_ID/refund/propose" \
    '{"reason":"wrong role"}' "$ARTIFACT_DIR/p7-refund-role.json")" \
  "$ARTIFACT_DIR/p7-refund-role.json"

# 1 (continued). The Paid boundary. Allocations finalize at resolve and the lot
# converts lazily on the granted user's next `lock_user` transaction, so the
# withdrawal request below is what triggers it. Without this half the leg passes
# when conversion is deleted outright.
wait_until "p7-live paid" 600 market_state_is p7-live paid
GRANT_TRIGGER_DEST="EVbNNUQgjNnCEQppiarF4usufFDDnpornDEaMFWAWZry"
expect_money_status 200 "post-Paid lock_user trigger for the granted user" \
  "$(withdraw_request "$GRANT_USER" 5000000 "$GRANT_TRIGGER_DEST" "$RUN_TAG-grant-convert" \
    "$ARTIFACT_DIR/p7-grant-convert.json")" "$ARTIFACT_DIR/p7-grant-convert.json"
[ "$(sql "select count(*) from credit_grant_lots
          where user_id='$GRANT_USER' and converted_at is not null")" = 1 ] || \
  fail "the granted lot did not convert after the market Paid and the user was locked"

# A missing production surface never becomes a green line.
if [ ${#PREREQ_FAILURES[@]} -ne 0 ]; then
  echo "PHASE 7 RED-TEAM INCOMPLETE — ${#PREREQ_FAILURES[@]} missing production prerequisites:" >&2
  printf '  - %s\n' "${PREREQ_FAILURES[@]}" >&2
  fail "the Phase 7 red-team cannot certify: the prerequisites above are unbuilt, so those attacks were never executed"
fi

echo "PHASE 7 W4 E2E GREEN convoy=${CONVOY_SUCCESS}/${TRADE_CAPABLE} lifecycle_signals=${LIFE_SIGNALS} fat_signals=${FAT_SIGNALS} payout_txns=${PAYOUT_TX_COUNT} referral_binds=${BINDS}"
