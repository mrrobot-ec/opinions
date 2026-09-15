#!/usr/bin/env bash
# Executable refutation harness for the claim that the Phase 7 red-team section
# of scripts/e2e_swarm_smoke.sh (lines 785-1000) is verification theatre.
#
# READ-ONLY with respect to the repository. Writes nothing outside
# var/p9-smoke-refutation*. Runs no Cargo, no VCS, no network, no Postgres.
#
# Two independent proofs:
#   PROOF A  status-predicate space: evaluate each leg's literal acceptance
#            predicate against the full plausible status space and show which
#            statuses the leg accepts as "attack failed".
#   PROOF B  null-implementation replay: run the leg's exact SQL against an
#            EMPTY schema (zero production code, zero workers, zero routes) in
#            sqlite3 and show the leg's assertions still pass.
#   PROOF C  deposits observation-identity CHECK replay: run the script's own
#            leg-8 fixture INSERTs against the constraint text from
#            migrations/0011_money.sql:162-170.
set -uo pipefail

OUT_DIR=$(cd "$(dirname "$0")" && pwd)
DB="$OUT_DIR/p9-smoke-refutation-nullimpl.sqlite"
rm -f "$DB"

pass_count=0
fail_count=0
note() { printf '%s\n' "$*"; }
rule() { printf '%s\n' "------------------------------------------------------------"; }

# ---------------------------------------------------------------------------
# PROOF A — status-predicate space
# ---------------------------------------------------------------------------
# Each function is a verbatim transcription of the leg's acceptance test from
# scripts/e2e_swarm_smoke.sh. Return 0 == "the leg treats the attack as failed"
# == the script keeps going == green.

# line 793-794: `[ "$(preview ...)" = 200 ] || echo "bonus-wash preview skipped or refused"`
leg01_wash_preview() { local s=$1; [ "$s" = 200 ] || echo >/dev/null; return 0; }

# line 819-826
leg05_omit_version() {
  local s=$1
  if [ "$s" != 422 ] && [ "$s" != 400 ]; then
    [ "$s" != 200 ] || return 1
  fi
  return 0
}

# line 834
leg06_fee_override() { local s=$1; [ "$s" != 200 ]; }
# line 840
leg07_unsigned_webhook() { local s=$1; [ "$s" != 200 ]; }
# lines 867-872: the two /withdrawals statuses are captured by `-w` into
# /dev/null and never compared to anything.
leg09_dust_withdraw() { local s=$1; return 0; }
# line 886
leg10a_hit_dest() { local s=$1; [ "$s" != 200 ]; }
# line 894
leg10b_self_excluded() { local s=$1; [ "$s" != 200 ]; }
# line 903
leg11_daily_retry() { local s=$1; [ "$s" != 200 ]; }
# line 966
leg13b_unwind_racer() { local s=$1; [ "$s" != 200 ]; }
# line 998
leg14_refund() { local s=$1; [ "$s" != 200 ]; }

STATUS_SPACE=(200 400 403 404 409 422 423 500 503)

note "PROOF A — accepted status space per red-team leg"
note "(A = leg accepts this status as 'the attack failed' and the run stays green)"
rule
printf '%-34s' "leg"
for s in "${STATUS_SPACE[@]}"; do printf '%5s' "$s"; done
printf '\n'
rule
for leg in \
  "leg01_wash_preview:1 wash-after-grant preview" \
  "leg05_omit_version:5 omitted config version" \
  "leg06_fee_override:6 fee override propose" \
  "leg07_unsigned_webhook:7 unsigned KYC webhook" \
  "leg09_dust_withdraw:9 dust warm-up + dump" \
  "leg10a_hit_dest:10a Hit user -> mule dest" \
  "leg10b_self_excluded:10b self-excluded -> new dest" \
  "leg11_daily_retry:11 settle-then-retry daily cap" \
  "leg13b_unwind_racer:13b unwind racer vs Paid" \
  "leg14_refund:14 admit/refund source lock"; do
  fn=${leg%%:*}; label=${leg#*:}
  printf '%-34s' "$label"
  for s in "${STATUS_SPACE[@]}"; do
    if "$fn" "$s"; then printf '%5s' "A"; else printf '%5s' "."; fi
  done
  printf '\n'
done
rule
note "Every leg above accepts 404 (route absent) and 503 (hardcoded stub)."
note "Only leg 5 discriminates at all, and only against 200."
note

# Observed statuses from the last recorded green run,
# artifacts/phase7-w4-p7w4-1786686052-95850/*.status and empty-body responses.
note "PROOF A2 — statuses actually observed in the last 'PHASE 7 W4 E2E GREEN' run"
rule
for obs in \
  "leg06_fee_override:6 fee override:503 (phase7_admin_stubs)" \
  "leg07_unsigned_webhook:7 unsigned webhook:404 (kyc_webhook::router never merged)" \
  "leg09_dust_withdraw:9 dust withdraw:404 (empty body artifact)" \
  "leg10a_hit_dest:10a Hit dest:404 (empty body artifact)" \
  "leg10b_self_excluded:10b self-excluded:404 (empty body artifact)" \
  "leg11_daily_retry:11 daily retry:404 (empty body artifact)" \
  "leg13b_unwind_racer:13b unwind racer:409 (IllegalTransition, market Paid)" \
  "leg14_refund:14 refund propose:404 (empty body artifact)"; do
  fn=${obs%%:*}; rest=${obs#*:}; label=${rest%%:*}; detail=${rest#*:}
  code=${detail%% *}
  if "$fn" "$code"; then verdict="LEG PASSED"; else verdict="LEG FAILED"; fi
  printf '%-30s %-52s %s\n' "$label" "$detail" "$verdict"
done
rule
note "Six of eight money/compliance legs 'passed' against an ABSENT route (404)."
note "One passed against a hardcoded 503 stub. One passed on a lifecycle refusal"
note "(market already Paid) that has nothing to do with the money path."
note

# ---------------------------------------------------------------------------
# PROOF B — null-implementation replay
# ---------------------------------------------------------------------------
note "PROOF B — null-implementation replay (sqlite3, zero production code)"
rule

sqlite3 "$DB" <<'SQL'
-- The Phase 7 tables the red-team legs read, created EMPTY. No watcher, no
-- referral engine, no AML scanner, no reconciliation detector, no withdraw
-- machine, no credit converter exists in this world.
create table credit_grant_lots(
  id integer primary key, user_id text, source text, amount_micro integer,
  grant_class text, policy_version text, converted_at text);
create table referral_codes(user_id text, code text);
create table referral_binds(referrer_id text, referee_id text, bind_key text unique);
create table phone_verifications(id integer primary key, user_id text, verified_at text);
create table withdrawals(user_id text, dest_address text, review_state text);
create table sanction_screenings(user_id text, context text, verdict text,
  checked_at text, expires_at text, policy_version text);
create table self_exclusions(user_id text, cooling_off_until text);
create table aml_flags(user_id text, status text);
create table alert_outbox(incident_key text, severity text, body text,
  status text, updated_at text);
create table deposits(chain_sig text, amount_micro integer, status text,
  machine_status text, source_address text, dest_address text, mint text,
  observed_slot integer, rail_fingerprint text, admit_tx_id text, user_id text);
SQL

q() { sqlite3 "$DB" "$1"; }
check() {
  local label=$1 got=$2 want=$3
  if [ "$got" = "$want" ]; then
    printf 'GREEN  %-58s got=%s want=%s\n' "$label" "$got" "$want"
    pass_count=$((pass_count + 1))
  else
    printf 'RED    %-58s got=%s want=%s\n' "$label" "$got" "$want"
    fail_count=$((fail_count + 1))
  fi
}

GRANT_USER='p7-bonus-wash'; REF_A='p7-ref-a'; REF_B='p7-ref-b'
DUST_USER='p7-dust'; CONFIG_USER='p6-config-user'
RUN_TAG='nullimpl'; OBS_SIG="p7-obs-$RUN_TAG"
DUST_DEST='Dust111111111111111111111111111111111111111'

# Leg 1 (script lines 791-796)
q "insert into credit_grant_lots(user_id,source,amount_micro,grant_class,policy_version)
   values ('$GRANT_USER','signup',5000000,'real_money','e2e')"
check "leg 1  wash-after-grant: converted lots" \
  "$(q "select count(*) from credit_grant_lots where user_id='$GRANT_USER' and converted_at is not null")" 0

# Legs 2-4 (script lines 799-816)
q "insert into referral_codes(user_id,code) values ('$REF_A','P7REFA')"
BINDS=$(q "select count(*) from referral_binds")
check "legs 2-4  unverified-phone referral grant sum" \
  "$(q "select coalesce(sum(amount_micro),0) from credit_grant_lots l
        left join phone_verifications p on p.user_id=l.user_id and p.verified_at is not null
        where l.source like 'referral%' and p.id is null")" 0
q "insert into referral_binds(referrer_id,referee_id,bind_key)
   values ('$REF_A','$REF_B','phone:$REF_A:$REF_B')"
if q "insert into referral_binds(referrer_id,referee_id,bind_key)
      values ('$REF_B','$REF_A','phone:$REF_B:$REF_A')" 2>/dev/null; then
  AB=$(q "select count(*) from credit_grant_lots where user_id in ('$REF_A','$REF_B') and source like 'referral%'")
  if [ "$AB" -le 1 ]; then
    printf 'GREEN  %-58s got=%s want<=1\n' "legs 2-4  A<->B referral grants" "$AB"
    pass_count=$((pass_count + 1))
  else
    printf 'RED    %-58s got=%s want<=1\n' "legs 2-4  A<->B referral grants" "$AB"
    fail_count=$((fail_count + 1))
  fi
fi

# Leg 8 second half (script lines 851-857) — the row the script itself inserted.
q "insert into deposits(chain_sig,amount_micro,status,machine_status,
                        source_address,dest_address,mint,observed_slot)
   values ('$OBS_SIG',25000000,'observed_finalized','observed_finalized',
           'src-p7','treasury-p7','usdc-mint-p7',9001)"
check "leg 8  no-KYC observation unadmitted/unbound" \
  "$(q "select count(*) from deposits where chain_sig='$OBS_SIG' and admit_tx_id is null and user_id is null")" 1

# Leg 9 (script lines 873-876) — no withdrawal row ever created.
check "leg 9  dust dest auto-approved rows" \
  "$(q "select count(*) from withdrawals where user_id='$DUST_USER' and dest_address='$DUST_DEST' and review_state='approved'")" 0

# Leg 10 (script lines 878-881) — the script asserts its own INSERT.
q "insert into sanction_screenings(user_id,context,verdict,checked_at,expires_at,policy_version)
   values ('$DUST_USER','sanctions','hit',datetime('now'),datetime('now','+24 hours'),'e2e-staging')"
check "leg 10  freshest sanctions verdict" \
  "$(q "select verdict from sanction_screenings where user_id='$DUST_USER' and context='sanctions' order by checked_at desc limit 1")" hit

# Leg 12 (script lines 906-910)
check "leg 12  four \$25 deposits open AML flags" \
  "$(q "select count(*) from aml_flags where user_id='$DUST_USER' and status='open'")" 0

# Leg 13 (script lines 914-929) — the script injects the alert row itself.
check "leg 13  no residual page before injection" \
  "$(q "select count(*) from alert_outbox where incident_key like 'reconciliation_residual%' and status='open'")" 0
RESIDUAL_KEY="reconciliation_residual:cut-$RUN_TAG:residual:1"
q "insert into alert_outbox(incident_key,severity,body,status)
   values ('$RESIDUAL_KEY','crit','signed residual 1 micro at the pinned cut','open')"
check "leg 13  injected residual pages" \
  "$(q "select count(*) from alert_outbox where incident_key='$RESIDUAL_KEY' and status='open'")" 1
q "update alert_outbox set updated_at=datetime('now') where incident_key='$RESIDUAL_KEY'"
check "leg 13  recurrence dedups inside open incident" \
  "$(q "select count(*) from alert_outbox where incident_key='$RESIDUAL_KEY'")" 1
q "update alert_outbox set status='resolved' where incident_key='$RESIDUAL_KEY'"
check "leg 13  in-flight exposure not paged as drift" \
  "$(q "select count(*) from alert_outbox where incident_key like 'reconciliation_residual%' and status='open'")" 0

rule
note "Null-implementation replay: $pass_count leg assertions GREEN, $fail_count RED."
note "referral_binds counter echoed by the script's final GREEN line: $BINDS"
note

# ---------------------------------------------------------------------------
# PROOF C — deposits_observation_identity CHECK replay
# ---------------------------------------------------------------------------
note "PROOF C — migrations/0011_money.sql:162-170 CHECK vs the script's own fixtures"
rule
CDB="$OUT_DIR/p9-smoke-refutation-check.sqlite"
rm -f "$CDB"
sqlite3 "$CDB" <<'SQL'
create table deposits(
  chain_sig text not null unique,
  amount_micro integer not null check (amount_micro > 0),
  status text not null,
  machine_status text,
  source_address text, dest_address text, mint text,
  observed_slot integer, rail_fingerprint text,
  constraint deposits_observation_identity check (
    machine_status is null
    or machine_status in ('admitted_legacy', 'quarantined_legacy')
    or (source_address is not null and dest_address is not null
        and mint is not null and observed_slot is not null
        and rail_fingerprint is not null)
  )
);
SQL

# script line 846-850: the partial observation. Expected to be REJECTED.
if sqlite3 "$CDB" "insert into deposits(chain_sig,amount_micro,status,machine_status,source_address)
     values ('p7-obs-partial',25000000,'observed_finalized','observed_finalized','src-p7')" 2>&1 >/dev/null | head -1; then :; fi
PARTIAL_RC=$(sqlite3 "$CDB" "select count(*) from deposits where chain_sig='p7-obs-partial'")
note "line 846 partial observation rows inserted: $PARTIAL_RC (script expects rejection => 0)"

# script line 851-854: the "complete" observation. The script assumes it lands.
COMPLETE_ERR=$(sqlite3 "$CDB" "insert into deposits(chain_sig,amount_micro,status,machine_status,
                          source_address,dest_address,mint,observed_slot)
     values ('p7-obs',25000000,'observed_finalized','observed_finalized',
             'src-p7','treasury-p7','usdc-mint-p7',9001)" 2>&1 >/dev/null)
COMPLETE_RC=$(sqlite3 "$CDB" "select count(*) from deposits where chain_sig='p7-obs'")
note "line 851 'complete' observation rows inserted: $COMPLETE_RC (script assumes 1)"
note "line 851 error: ${COMPLETE_ERR:-<none>}"
rule
if [ "$COMPLETE_RC" = 0 ]; then
  note "The script's own leg-8 fixture omits rail_fingerprint, which the CHECK"
  note "requires. Under 'sql()' + ON_ERROR_STOP=1 + 'set -e' with no '|| true',"
  note "line 851 aborts the entire run before any assertion after it."
fi
note
note "Artifacts: $DB, $CDB"
