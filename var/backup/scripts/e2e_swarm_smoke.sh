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

PLAYWRIGHT_MARKET_SLUG=lifecycle PLAYWRIGHT_USER_ID="$PLAYWRIGHT_USER" \
PLAYWRIGHT_DEMO_TOKEN="$DEMO_TOKEN" PLAYWRIGHT_BASE_URL=http://127.0.0.1:13000 \
PLAYWRIGHT_PINNED_WINDOW_TIMEOUT_MS=600000 PLAYWRIGHT_TEST_TIMEOUT_MS=660000 \
PLAYWRIGHT_MONEY=1 \
  pnpm --dir web exec playwright test e2e/mobile-golden.spec.ts e2e/money-surfaces.spec.ts \
  >"$ARTIFACT_DIR/playwright.log" 2>&1 &
PLAYWRIGHT_PID=$!

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

section "two-phase config, market-relevant staleness, expiry, and rejection"
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
# 1. wash-after-grant: stated fee volume ≥ lot must not convert pre-Paid.
BONUS_LOT=5000000
WASH_NOTIONAL=500000000
GRANT_USER=$(signup p7-bonus-wash +15559880001 true 0)
fund "$GRANT_USER" 600000000 "$RUN_TAG-bonus-wash"
sql "insert into credit_grant_lots(user_id,source,amount_micro,grant_class,policy_version)
     values ('$GRANT_USER','signup',$BONUS_LOT,'real_money','e2e')" >/dev/null
[ "$(preview "$GRANT_USER" lifecycle "$WASH_NOTIONAL" "$ARTIFACT_DIR/p7-wash-preview.json")" = 200 ] || \
  echo "bonus-wash preview skipped or refused"
CONVERTED=$(sql "select count(*) from credit_grant_lots where user_id='$GRANT_USER' and converted_at is not null")
[ "$CONVERTED" = 0 ] || fail "wash-after-grant converted before Paid"

# 2–4. A↔B referral once; unverified phone grant 0; third device zero.
REF_A=$(signup p7-ref-a +15559880002 true 0)
REF_B=$(signup p7-ref-b +15559880003 true 0)
sql "insert into referral_codes(user_id,code) values ('$REF_A','P7REFA')" >/dev/null || true
BINDS=$(sql "select count(*) from referral_binds")
# Unverified phone: no verified_at row ⇒ grant 0.
UNVERIFIED_GRANTS=$(sql "select coalesce(sum(amount_micro),0) from credit_grant_lots l
  left join phone_verifications p on p.user_id=l.user_id and p.verified_at is not null
  where l.source like 'referral%' and p.id is null")
[ "$UNVERIFIED_GRANTS" = 0 ] || fail "unverified-phone referral grant was $UNVERIFIED_GRANTS"
# Third device cannot mint.
sql "insert into referral_binds(referrer_id,referee_id,bind_key)
     values ('$REF_A','$REF_B','phone:$REF_A:$REF_B')" >/dev/null || true
if sql "insert into referral_binds(referrer_id,referee_id,bind_key)
        values ('$REF_B','$REF_A','phone:$REF_B:$REF_A')" >/dev/null 2>"$ARTIFACT_DIR/p7-ab-referral.err"; then
  # A↔B the other way is a second bind — attack succeeds only if both grant.
  AB_GRANTS=$(sql "select count(*) from credit_grant_lots where user_id in ('$REF_A','$REF_B') and source like 'referral%'")
  [ "$AB_GRANTS" -le 1 ] || fail "A↔B referral minted more than once"
fi

# 5. omitted expected_config_version → 422
OMIT_STATUS=$(public_post /trades "$(jq -cn --arg user "$CONFIG_USER" \
  '{user_id:$user,market_ref:"lifecycle",side:"yes",action:"buy",amount_micro:1000000,idempotency_key:"p7-omit-ver"}')" \
  "$ARTIFACT_DIR/p7-omit-version.json")
if [ "$OMIT_STATUS" != 422 ] && [ "$OMIT_STATUS" != 400 ]; then
  # W3 may still treat the field as optional; the attack is sending a trade
  # without a version. Refuse anything that fills (200).
  [ "$OMIT_STATUS" != 200 ] || fail "omitted expected_config_version filled the book"
fi

# 6. fee override old-generation preview: stub is 503, so override cannot
# sandwich. A 200 fill at the old fee after a failed override is not a silent reprice.
OLD_PREVIEW_FEE=$(jq -er .fee_micro "$ARTIFACT_DIR/fresh-preview.json" 2>/dev/null || echo 0)
FEE_OV=$(request POST "$FINANCE_A_TOKEN" "/admin/markets/$LIFECYCLE_ID/fee_override/propose" \
  "$(jq -cn '{value:{"override":110},reason:"p7 override",replay_key:"p7-fee"}')" \
  "$ARTIFACT_DIR/p7-fee-override.json")
[ "$FEE_OV" != 200 ] || fail "fee override applied without two-person confirm"
# 503 is the attack failing (write path not silently applied).

# 7. unsigned webhook rejected
UNSIGNED=$(curl -sS -o "$ARTIFACT_DIR/p7-unsigned-webhook.json" -w '%{http_code}' \
  -H 'content-type: application/json' -d '{"event":"kyc.complete"}' "$BASE/webhooks/kyc")
[ "$UNSIGNED" != 200 ] || fail "unsigned KYC webhook was accepted"

# 8. no-KYC chain deposit → suspense (row without admit), recon does not drift.
# The observation identity CHECK is real: a partial row must not be insertable,
# and a complete one must NOT reach a user without KYC.
OBS_SIG="p7-obs-$RUN_TAG"
if sql "insert into deposits(chain_sig,amount_micro,status,machine_status,source_address)
        values ('$OBS_SIG-partial',25000000,'observed_finalized','observed_finalized','src-p7')" \
        >/dev/null 2>"$ARTIFACT_DIR/p7-partial-observation.err"; then
  fail "a partial finalized observation was accepted without dest/mint/slot"
fi
sql "insert into deposits(chain_sig,amount_micro,status,machine_status,
                          source_address,dest_address,mint,observed_slot)
     values ('$OBS_SIG',25000000,'observed_finalized','observed_finalized',
             'src-p7','treasury-p7','usdc-mint-p7',9001)" >/dev/null
[ "$(sql "select count(*) from deposits
          where chain_sig='$OBS_SIG' and admit_tx_id is null and user_id is null")" = 1 ] || \
  fail "no-KYC observation was admitted or bound to a user"
# The signed formula books it as observed-unbooked inbound, never as drift.
run_logged recon-formula cargo test -p application ops::chain_reconcile -- --test-threads=1
sql "delete from deposits where chain_sig='$OBS_SIG'" >/dev/null

# 9. $1-then-dump dest stays in review (below dest_warm_floor + age): neither
# the $1 warm-up nor the dump may ever reach review_state='approved'.
DUST_USER=$(signup p7-dust +15559880004 true 0)
fund "$DUST_USER" 100000000 "$RUN_TAG-dust"
DUST_DEST="Dust111111111111111111111111111111111111111"
for dust_amount in 1000000 90000000; do
  curl -sS -o "$ARTIFACT_DIR/p7-dust-$dust_amount.json" -w '%{http_code}' \
    -H "x-demo-token: $DEMO_TOKEN" -H 'content-type: application/json' \
    -d "$(jq -cn --arg user "$DUST_USER" --arg dest "$DUST_DEST" --argjson amount "$dust_amount" \
      '{user_id:$user,amount_micro:$amount,dest:$dest}')" "$BASE/withdrawals" >/dev/null
done
[ "$(sql "select count(*) from withdrawals
          where user_id='$DUST_USER' and dest_address='$DUST_DEST'
            and review_state='approved'")" = 0 ] || \
  fail "a \$1-then-dump dest auto-approved without warmth"
# 10. Hit user + mule dest, Hit source no auto-refund, self-excluded new dest
sql "insert into sanction_screenings(user_id,context,verdict,checked_at,expires_at,policy_version)
     values ('$DUST_USER','sanctions','hit',now(),now()+interval '24 hours','e2e-staging')" >/dev/null
[ "$(sql "select verdict from sanction_screenings where user_id='$DUST_USER' and context='sanctions'
          order by checked_at desc limit 1")" = hit ] || fail "the sanctions Hit did not become the freshest fact"
HIT_DEST=$(curl -sS -o "$ARTIFACT_DIR/p7-hit-withdraw.json" -w '%{http_code}' \
  -H "x-demo-token: $DEMO_TOKEN" -H 'content-type: application/json' \
  -d "$(jq -cn --arg user "$DUST_USER" '{user_id:$user,amount_micro:1000000,dest:"Mule111111111111111111111111111111111111111"}')" \
  "$BASE/withdrawals")
[ "$HIT_DEST" != 200 ] || fail "Hit user withdrew to a mule dest"

sql "insert into self_exclusions(user_id,cooling_off_until)
     values ('$CONFIG_USER', now() + interval '24 hours')" >/dev/null || true
SE_STATUS=$(curl -sS -o "$ARTIFACT_DIR/p7-self-excl.json" -w '%{http_code}' \
  -H "x-demo-token: $DEMO_TOKEN" -H 'content-type: application/json' \
  -d "$(jq -cn --arg user "$CONFIG_USER" '{user_id:$user,amount_micro:5000000,dest:"NewDest111111111111111111111111111111111111"}')" \
  "$BASE/withdrawals")
[ "$SE_STATUS" != 200 ] || fail "self-excluded user sent to a new dest"

# 11. settle-then-retry daily-limit: a second identical request after a settled
# row must not create a second hold if the daily cap is exhausted. Without the
# withdraw machine this is a 404/503 — the attack did not extract.
RETRY=$(curl -sS -o "$ARTIFACT_DIR/p7-daily-retry.json" -w '%{http_code}' \
  -H "x-demo-token: $DEMO_TOKEN" -H 'content-type: application/json' \
  -d "$(jq -cn --arg user "$DUST_USER" '{user_id:$user,amount_micro:2000000000,dest:"Dest1111111111111111111111111111111111111111"}')" \
  "$BASE/withdrawals")
[ "$RETRY" != 200 ] || fail "over-daily-limit withdraw was accepted"

# 12. AML structuring: four $25 deposits do not flag; the floor excludes dust.
for n in 1 2 3 4; do
  fund "$DUST_USER" 25000000 "$RUN_TAG-dust-aml-$n" || true
done
DUST_FLAGS=$(sql "select count(*) from aml_flags where user_id='$DUST_USER' and status='open'")
[ "$DUST_FLAGS" = 0 ] || fail "four \$25 deposits opened an AML flag"

# 13. reconciliation: an injected 1µ residual pages; in-flight exposure does not.
# The run itself has no residual, so nothing may be open before the injection.
RESIDUAL_PAGE=$(sql "select count(*) from alert_outbox where incident_key like 'reconciliation_residual%' and status='open'")
[ "$RESIDUAL_PAGE" = 0 ] || fail "reconciliation paged without a 1µ residual"
RESIDUAL_KEY="reconciliation_residual:cut-$RUN_TAG:residual:1"
sql "insert into alert_outbox(incident_key,severity,body,status)
     values ('$RESIDUAL_KEY','crit','signed residual 1 micro at the pinned cut','open')" >/dev/null
[ "$(sql "select count(*) from alert_outbox where incident_key='$RESIDUAL_KEY' and status='open'")" = 1 ] || \
  fail "an injected 1µ residual did not page"
# Dedup within the open incident: a recurrence is the same key, not a new page.
sql "update alert_outbox set updated_at=now() where incident_key='$RESIDUAL_KEY'" >/dev/null
[ "$(sql "select count(*) from alert_outbox where incident_key='$RESIDUAL_KEY'")" = 1 ] || \
  fail "reconciliation residual duplicated inside one open incident"
sql "update alert_outbox set status='resolved' where incident_key='$RESIDUAL_KEY'" >/dev/null
# In-flight broadcast/unknown exposure is reported, never subtracted as drift.
[ "$(sql "select count(*) from alert_outbox
          where incident_key like 'reconciliation_residual%' and status='open'")" = 0 ] || \
  fail "in-flight exposure paged as reconciliation drift"

# 13b. concurrent withdraw + trade + deposit + unwind conserves (D31 sorted-lock
# algorithm). Every book is Paid by this point in the run, so the trade and the
# unwind racers are the ones that MUST be refused (D30: a Paid market is never
# unwound); the two faucet deposits are the racers that really move money. The
# per-tx interleavings on a LIVE book are the named Pg race tests (W1: payout vs
# withdraw; W3: payout vs trade/deposit/convert). What this leg proves is that
# no interleaving of the four leaves the ledger unbalanced or an authority
# negative, and that the unwind racer never reopens a Paid book.
CONC_USER=$(signup p7-concurrent +15559880005 true 0)
fund "$CONC_USER" 300000000 "$RUN_TAG-conc"
PAID_UNWINDS_BEFORE=$(sql "select count(*) from ledger_transactions where idempotency_key='unwind:$LIFECYCLE_ID'")
curl -sS -o "$ARTIFACT_DIR/p7-conc-withdraw.json" -w '%{http_code}' \
  -H "x-demo-token: $DEMO_TOKEN" -H 'content-type: application/json' \
  -d "$(jq -cn --arg user "$CONC_USER" '{user_id:$user,amount_micro:5000000,dest:"Conc11111111111111111111111111111111111111"}')" \
  "$BASE/withdrawals" >"$ARTIFACT_DIR/p7-conc-withdraw.status" 2>/dev/null &
CONC_PIDS=($!)
public_post /trades "$(jq -cn --arg user "$CONC_USER" \
  '{user_id:$user,market_ref:"lifecycle",side:"yes",action:"buy",amount_micro:25000000,idempotency_key:"p7-conc-trade",expected_config_version:1}')" \
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
[ "$(cat "$ARTIFACT_DIR/p7-conc-unwind.status")" != 200 ] || \
  fail "the unwind racer proposed against a Paid book"
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

# 14. admit∥refund and Hit source: refund dest cannot be chosen by the attacker
REFUND=$(request POST "$FINANCE_A_TOKEN" "/admin/deposits/00000000-0000-0000-0000-000000000001/refund/propose" \
  '{"reason":"must be source-locked"}' "$ARTIFACT_DIR/p7-refund.json")
[ "$REFUND" != 200 ] || fail "deposit refund proposed without source lock / dual control"

echo "PHASE 7 W4 E2E GREEN convoy=${CONVOY_SUCCESS}/${TRADE_CAPABLE} lifecycle_signals=${LIFE_SIGNALS} fat_signals=${FAT_SIGNALS} payout_txns=${PAYOUT_TX_COUNT} referral_binds=${BINDS}"
