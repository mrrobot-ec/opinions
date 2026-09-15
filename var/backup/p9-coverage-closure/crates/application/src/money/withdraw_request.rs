//! D31 request protocol: fingerprint FIRST, lock-free screenings, then
//! serialize → `lock_user` → revalidate → auto-collect → limits → hold.

use std::net::IpAddr;

use serde_json::json;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::error::{AppError, StoreError};
use crate::model::{Event, UserId, UserStatus};
use crate::ports::{
    canonicalize_dest, intent_fingerprint, Clock, Combo, GeoResolver, RequestWithdrawCmd,
    SanctionsScreen, ScreenVerdict, WithdrawLimits, WithdrawStore, WithdrawTx, WithdrawalId,
    WithdrawalReceipt, WithdrawalRow,
};

/// Request a withdrawal. Inserts W1 (`queued/screening/unsent`) on accept.
pub struct RequestWithdraw<'a, S: WithdrawStore, C: Clock> {
    pub store: &'a S,
    pub clock: &'a C,
    pub geo: &'a dyn GeoResolver,
    pub sanctions: &'a dyn SanctionsScreen,
}

impl<S: WithdrawStore, C: Clock> RequestWithdraw<'_, S, C> {
    /// # Errors
    /// Typed refusal (replay-stable), fingerprint conflict, or store failure.
    pub async fn execute(&self, cmd: RequestWithdrawCmd) -> Result<WithdrawalReceipt, AppError> {
        let dest = canonicalize_dest(&cmd.dest).ok_or(AppError::ConfigInvalid {
            key: "dest".into(),
            reason: "dest must be a 32-byte solana pubkey",
        })?;
        if cmd.amount_micro <= 0 {
            return Err(AppError::InsufficientFunds);
        }
        let fingerprint = intent_fingerprint(cmd.user, cmd.amount_micro, &dest);
        if let Some(mut hit) = self.store.lookup_fingerprint(&fingerprint).await? {
            hit.replayed = true;
            return Ok(hit);
        }
        if let Some(key) = cmd.idempotency_key.as_deref() {
            // Mismatch is checked under the write lock; a lock-free miss is fine.
            let _ = key;
        }

        let (geo, sanctions) = tokio::join!(
            screen_geo(self.geo, cmd.client_ip),
            self.sanctions.screen(cmd.user, "withdraw")
        );

        let mut tx = self.store.withdraw_tx().await?;
        tx.serialize_key(&format!("withdraw-fp:{fingerprint}"))
            .await?;
        if let Some(mut hit) = tx.lookup_fingerprint_tx(&fingerprint).await? {
            hit.replayed = true;
            return Ok(hit);
        }
        if let Some(key) = cmd.idempotency_key.as_deref() {
            let slot = idempotency_slot(cmd.user, key);
            tx.serialize_key(&slot).await?;
            if let Some(existing) = tx.lookup_idempotency(&slot).await? {
                if existing != fingerprint {
                    return Err(AppError::IdempotencyConflict);
                }
            }
        }

        if let Err(receipt) =
            persist_remote_or_refuse(tx.as_mut(), &cmd, &dest, &fingerprint, &geo, &sanctions)
                .await?
        {
            tx.commit().await?;
            return Ok(receipt);
        }

        let now = self.clock.now();
        tx.lock_user(cmd.user).await?;
        tx.convert_then_collect(cmd.user, now, &format!("withdraw-lock:{fingerprint}"))
            .await?;
        let receipt = match revalidate_and_hold(tx.as_mut(), &cmd, &dest, &fingerprint, now).await?
        {
            Outcome::Accepted(receipt) | Outcome::Refused(receipt) => receipt,
        };
        tx.commit().await?;
        Ok(receipt)
    }
}

enum Outcome {
    Accepted(WithdrawalReceipt),
    Refused(WithdrawalReceipt),
}

async fn screen_geo(
    geo: &dyn GeoResolver,
    ip: Option<IpAddr>,
) -> Result<ScreenVerdict, StoreError> {
    let Some(ip) = ip else {
        return Ok(ScreenVerdict::Indeterminate);
    };
    geo.resolve(ip).await
}

fn refusal(user: UserId, dest: &str, amount: i64, code: &str, message: &str) -> WithdrawalReceipt {
    WithdrawalReceipt {
        id: None,
        user,
        dest: dest.to_string(),
        amount_micro: amount,
        combo: None,
        hold_tx_id: None,
        replayed: false,
        refused: true,
        refuse_code: Some(code.to_string()),
        refuse_message: Some(message.to_string()),
    }
}

async fn persist_remote_or_refuse(
    tx: &mut dyn WithdrawTx,
    cmd: &RequestWithdrawCmd,
    dest: &str,
    fingerprint: &str,
    geo: &Result<ScreenVerdict, StoreError>,
    sanctions: &Result<ScreenVerdict, StoreError>,
) -> Result<Result<(), WithdrawalReceipt>, AppError> {
    tx.persist_screening(
        cmd.user,
        &geo_context(fingerprint),
        geo.as_ref().unwrap_or(&ScreenVerdict::Indeterminate),
    )
    .await?;
    tx.persist_screening(
        cmd.user,
        "withdraw",
        sanctions.as_ref().unwrap_or(&ScreenVerdict::Indeterminate),
    )
    .await?;
    let refusal_fact = if geo.is_err() {
        Some(("geo_unavailable", "geo screening unavailable"))
    } else if sanctions.is_err() {
        Some(("sanctions_unavailable", "sanctions screening unavailable"))
    } else {
        None
    };
    if let Some((code, message)) = refusal_fact {
        let receipt = refusal(cmd.user, dest, cmd.amount_micro, code, message);
        tx.persist_intent(&receipt).await?;
        persist_idemp(tx, cmd, fingerprint).await?;
        return Ok(Err(receipt));
    }
    Ok(Ok(()))
}

async fn persist_idemp(
    tx: &mut dyn WithdrawTx,
    cmd: &RequestWithdrawCmd,
    fingerprint: &str,
) -> Result<(), AppError> {
    if let Some(key) = cmd.idempotency_key.as_deref() {
        tx.persist_idempotency(&idempotency_slot(cmd.user, key), fingerprint)
            .await?;
    }
    Ok(())
}

fn idempotency_slot(user: UserId, client_key: &str) -> String {
    format!("withdraw-idem:{}:{client_key}", user.0)
}

async fn revalidate_and_hold(
    tx: &mut dyn WithdrawTx,
    cmd: &RequestWithdrawCmd,
    dest: &str,
    fingerprint: &str,
    now: OffsetDateTime,
) -> Result<Outcome, AppError> {
    let limits = tx.limits().await?;
    let view = tx.user_money_view(cmd.user).await?;
    if let Some(receipt) = admission_refusal(cmd, dest, fingerprint, &limits, &view, tx).await? {
        return Ok(Outcome::Refused(receipt));
    }

    let collected = auto_collect(tx, cmd.user, fingerprint).await?;
    let _ = collected;
    let cash = tx.user_cash(cmd.user).await?;
    if cash < cmd.amount_micro {
        let receipt = refusal(
            cmd.user,
            dest,
            cmd.amount_micro,
            "insufficient_funds",
            "insufficient available cash",
        );
        tx.persist_intent(&receipt).await?;
        persist_idemp(tx, cmd, fingerprint).await?;
        return Ok(Outcome::Refused(receipt));
    }

    tx.lock_cap("withdraw-daily").await?;
    let since = now - Duration::hours(24);
    if !within_limits(tx, cmd, dest, &limits, since).await? {
        let receipt = refusal(
            cmd.user,
            dest,
            cmd.amount_micro,
            "limit",
            "withdrawal exceeds daily or destination cap",
        );
        tx.persist_intent(&receipt).await?;
        persist_idemp(tx, cmd, fingerprint).await?;
        return Ok(Outcome::Refused(receipt));
    }

    let id = WithdrawalId(Uuid::new_v4());
    tx.evaluate_withdraw_aml_candidate(id, cmd.user, dest, cmd.amount_micro, now)
        .await?;
    let reasons = current_reasons(tx, cmd, dest, fingerprint, &limits, &view, now).await?;
    let hold_key = format!("withdraw-hold:{fingerprint}");
    let hold_tx = tx.apply_hold(cmd.user, cmd.amount_micro, &hold_key).await?;
    let row = WithdrawalRow {
        id,
        user: cmd.user,
        dest: dest.to_string(),
        amount_micro: cmd.amount_micro,
        combo: Combo::W1,
        hold_tx_id: hold_tx,
        release_tx_id: None,
        settle_tx_id: None,
        request_fingerprint: fingerprint.to_string(),
        risk_reasons: reasons,
        requested_at: now,
        decided_at: None,
        sent_at: None,
        settled_at: None,
    };
    tx.insert_withdrawal(&row).await?;
    tx.append_withdrawal_event(id, "insert", "machine", json!({"combo": "W1"}))
        .await?;
    let _seq = tx
        .record_event(Event {
            event_type: "WithdrawalRequested",
            aggregate_type: "withdrawal",
            aggregate_id: id.0,
            payload: json!({
                "user_id": cmd.user.0.to_string(),
                "amount_micro": cmd.amount_micro,
                "dest": dest,
            }),
        })
        .await?;
    let receipt = WithdrawalReceipt {
        id: Some(id),
        user: cmd.user,
        dest: dest.to_string(),
        amount_micro: cmd.amount_micro,
        combo: Some(Combo::W1),
        hold_tx_id: Some(hold_tx),
        replayed: false,
        refused: false,
        refuse_code: None,
        refuse_message: None,
    };
    tx.persist_intent(&receipt).await?;
    persist_idemp(tx, cmd, fingerprint).await?;
    Ok(Outcome::Accepted(receipt))
}

async fn admission_refusal(
    cmd: &RequestWithdrawCmd,
    dest: &str,
    fingerprint: &str,
    limits: &WithdrawLimits,
    view: &crate::ports::UserMoneyView,
    tx: &mut dyn WithdrawTx,
) -> Result<Option<WithdrawalReceipt>, AppError> {
    let mut code = None;
    let mut message = None;
    if limits.pause_withdrawals {
        code = Some("paused");
        message = Some("withdrawals are paused");
    } else if view.status == UserStatus::Banned {
        code = Some("banned");
        message = Some("account is banned");
    } else if i64::from(view.kyc_tier) < limits.withdraw_kyc_tier {
        code = Some("kyc");
        message = Some("kyc tier too low");
    } else if cmd.amount_micro < limits.min_micro || cmd.amount_micro > limits.max_micro {
        code = Some("amount");
        message = Some("amount outside per-tx bounds");
    } else if cmd.client_ip.is_none() {
        code = Some("geo_missing_ip");
        message = Some("client ip is required");
    }
    if let (Some(code), Some(message)) = (code, message) {
        let receipt = refusal(cmd.user, dest, cmd.amount_micro, code, message);
        tx.persist_intent(&receipt).await?;
        persist_idemp(tx, cmd, fingerprint).await?;
        return Ok(Some(receipt));
    }
    Ok(None)
}

async fn auto_collect(
    tx: &mut dyn WithdrawTx,
    user: UserId,
    fingerprint: &str,
) -> Result<i64, AppError> {
    let open = tx.open_receivables(user).await?;
    let owed = open.iter().try_fold(0_i64, |sum, row| {
        sum.checked_add(row.outstanding_micro)
            .ok_or(AppError::Overflow)
    })?;
    if owed == 0 {
        return Ok(0);
    }
    let cash = tx.user_cash(user).await?;
    let collect = owed.min(cash);
    if collect == 0 {
        return Ok(0);
    }
    let key = format!("recv-collect:{fingerprint}");
    let cash_txn = tx.apply_user_to_house(user, collect, &key).await?;
    let mut remaining = collect;
    for row in open {
        if remaining == 0 {
            break;
        }
        let take = remaining.min(row.outstanding_micro);
        remaining -= take;
        tx.insert_receivable_movement(
            row.id,
            take,
            "machine:withdraw",
            cash_txn,
            &format!("recv-collect:{fingerprint}:{}", row.id),
        )
        .await?;
    }
    Ok(collect)
}

async fn within_limits(
    tx: &mut dyn WithdrawTx,
    cmd: &RequestWithdrawCmd,
    dest: &str,
    limits: &WithdrawLimits,
    since: OffsetDateTime,
) -> Result<bool, AppError> {
    let user_sum = tx.window_sum_user(cmd.user, since).await?;
    let dest_sum = tx.window_sum_dest(dest, since).await?;
    let hot_sum = tx.window_sum_hot(since).await?;
    Ok(
        user_sum.saturating_add(cmd.amount_micro) <= limits.daily_micro
            && dest_sum.saturating_add(cmd.amount_micro) <= limits.dest_daily_micro
            && hot_sum.saturating_add(cmd.amount_micro) <= limits.hot_daily_micro,
    )
}

async fn current_reasons(
    tx: &mut dyn WithdrawTx,
    cmd: &RequestWithdrawCmd,
    dest: &str,
    fingerprint: &str,
    limits: &WithdrawLimits,
    view: &crate::ports::UserMoneyView,
    now: OffsetDateTime,
) -> Result<Vec<String>, AppError> {
    let mut reasons = Vec::new();
    if cmd.amount_micro >= limits.auto_approve_micro {
        reasons.push("amount_ge_auto".into());
    }
    if cmd.amount_micro >= limits.dual_control_micro {
        reasons.push("amount_ge_dual".into());
    }
    if view.status == UserStatus::ShadowLimited {
        reasons.push("shadow_limited".into());
    }
    if tx.open_aml_flag_count(cmd.user).await? > 0 {
        reasons.push("aml_open".into());
    }
    let sanctions = tx.latest_screening(cmd.user, "withdraw").await?;
    if !is_fresh_clear(sanctions.as_ref(), now) {
        reasons.push("sanctions_not_clear".into());
    }
    let geo = tx
        .latest_screening(cmd.user, &geo_context(fingerprint))
        .await?;
    if !is_fresh_clear(geo.as_ref(), now) {
        reasons.push("geo_not_clear".into());
    }
    let warmth = tx.dest_warmth(dest).await?;
    if !warmth.is_warm(*limits, now) {
        reasons.push("dest_not_warm".into());
    }
    if warmth.distinct_users >= 2 {
        reasons.push("dest_shared".into());
    }
    if warmth.is_refund_dest {
        reasons.push("dest_is_refund".into());
    }
    if let Some(until) = view.self_excluded_until {
        if until > now && !is_self_exclusion_egress(tx, cmd.user, dest).await? {
            reasons.push("self_exclusion_new_dest".into());
        }
    }
    Ok(reasons)
}

pub(super) async fn is_self_exclusion_egress(
    tx: &mut dyn WithdrawTx,
    user: UserId,
    dest: &str,
) -> Result<bool, AppError> {
    if tx.dest_was_settled_for_user(user, dest).await? {
        return Ok(true);
    }
    Ok(tx.dest_is_observation_source_for_user(user, dest).await?)
}

/// D33 freshness predicate shared by request, decision, and send.
#[must_use]
pub(super) fn is_fresh_clear(verdict: Option<&ScreenVerdict>, now: OffsetDateTime) -> bool {
    matches!(
        verdict,
        Some(ScreenVerdict::Clear {
            checked_at,
            expires_at,
            policy_version,
        }) if *checked_at <= now && *expires_at > now && !policy_version.is_empty()
    )
}

pub(super) fn geo_context(fingerprint: &str) -> String {
    format!("geo:withdraw:{fingerprint}")
}

/// Controllable clock for request tests.
pub struct RequestClock(pub OffsetDateTime);

impl Clock for RequestClock {
    fn now(&self) -> OffsetDateTime {
        self.0
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines, clippy::unwrap_used)]

    use super::*;
    use crate::model::UserStatus;
    use crate::ports::identity_a_holds;
    use crate::ports::withdraw_fakes::{dest_a, dest_b, FakeScreen, FakeWithdrawStore};
    use crate::ports::WithdrawStore;

    fn clear() -> FakeScreen {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        FakeScreen {
            verdict: ScreenVerdict::Clear {
                checked_at: now,
                expires_at: now + Duration::hours(24),
                policy_version: "1".into(),
            },
            fail: false,
        }
    }

    fn cmd(user: UserId, amount: i64, dest: &str) -> RequestWithdrawCmd {
        RequestWithdrawCmd {
            user,
            amount_micro: amount,
            dest: dest.to_string(),
            client_ip: Some(IpAddr::from([203, 0, 113, 10])),
            idempotency_key: Some(format!("idem-{amount}-{}", user.0)),
        }
    }

    async fn run(
        store: &FakeWithdrawStore,
        screen: &FakeScreen,
        cmd: RequestWithdrawCmd,
    ) -> Result<WithdrawalReceipt, AppError> {
        run_with_screens(store, screen, screen, cmd).await
    }

    async fn run_with_screens(
        store: &FakeWithdrawStore,
        geo: &FakeScreen,
        sanctions: &FakeScreen,
        cmd: RequestWithdrawCmd,
    ) -> Result<WithdrawalReceipt, AppError> {
        RequestWithdraw {
            store,
            clock: &RequestClock(store.now()),
            geo,
            sanctions,
        }
        .execute(cmd)
        .await
    }

    #[tokio::test]
    async fn request_holds_cash_as_w1_and_replays_fingerprint() {
        let store = FakeWithdrawStore::new();
        let user = store.seed_user(UserStatus::Active, 2);
        store.credit(user, 20_000_000);
        let screen = clear();
        let first = run(&store, &screen, cmd(user, 5_000_000, &dest_a()))
            .await
            .unwrap();
        assert!(!first.refused);
        assert_eq!(first.combo, Some(Combo::W1));
        assert_eq!(store.withheld(), 5_000_000);
        assert_eq!(store.lazy_conversion_calls(), 1);
        let replay = run(&store, &screen, cmd(user, 5_000_000, &dest_a()))
            .await
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.id, first.id);
        assert_eq!(store.withheld(), 5_000_000);
        assert_eq!(store.lazy_conversion_calls(), 1);
        assert!(identity_a_holds(store.withheld(), &store.withdrawals()));
    }

    #[tokio::test]
    async fn fingerprint_mismatch_on_idempotency_key_is_conflict() {
        let store = FakeWithdrawStore::new();
        let user = store.seed_user(UserStatus::Active, 2);
        store.credit(user, 50_000_000);
        let screen = clear();
        let mut first = cmd(user, 5_000_000, &dest_a());
        first.idempotency_key = Some("same".into());
        run(&store, &screen, first).await.unwrap();
        let mut second = cmd(user, 6_000_000, &dest_a());
        second.idempotency_key = Some("same".into());
        let err = run(&store, &screen, second).await.unwrap_err();
        assert_eq!(err, AppError::IdempotencyConflict);
    }

    #[tokio::test]
    async fn remote_errors_refuse_but_hits_hold_for_review() {
        let store = FakeWithdrawStore::new();
        let user = store.seed_user(UserStatus::Active, 2);
        store.credit(user, 20_000_000);
        let mut down = clear();
        down.fail = true;
        let refused = run(&store, &down, cmd(user, 5_000_000, &dest_a()))
            .await
            .unwrap();
        assert!(refused.refused);
        assert_eq!(store.withheld(), 0);
        down.fail = false;
        let replay = run(&store, &down, cmd(user, 5_000_000, &dest_a()))
            .await
            .unwrap();
        assert!(replay.replayed);
        assert!(replay.refused);

        let user2 = store.seed_user(UserStatus::Active, 2);
        store.credit(user2, 20_000_000);
        let sanctions_hit = FakeScreen {
            verdict: ScreenVerdict::Hit,
            fail: false,
        };
        let held = run_with_screens(
            &store,
            &clear(),
            &sanctions_hit,
            cmd(user2, 5_000_000, &dest_b()),
        )
        .await
        .unwrap();
        assert!(!held.refused);
        assert_eq!(held.combo, Some(Combo::W1));
        let sanctions_row = store
            .withdrawals()
            .into_iter()
            .find(|row| Some(row.id) == held.id)
            .unwrap();
        assert!(sanctions_row
            .risk_reasons
            .iter()
            .any(|reason| reason == "sanctions_not_clear"));

        let user3 = store.seed_user(UserStatus::Active, 2);
        store.credit(user3, 20_000_000);
        let held = run_with_screens(
            &store,
            &sanctions_hit,
            &clear(),
            cmd(user3, 5_000_000, &dest_b()),
        )
        .await
        .unwrap();
        assert!(!held.refused);
        let geo_row = store
            .withdrawals()
            .into_iter()
            .find(|row| Some(row.id) == held.id)
            .unwrap();
        assert!(geo_row
            .risk_reasons
            .iter()
            .any(|reason| reason == "geo_not_clear"));
    }

    #[tokio::test]
    async fn pause_ban_kyc_amount_and_missing_ip_refuse() {
        let store = FakeWithdrawStore::new();
        let screen = clear();
        let user = store.seed_user(UserStatus::Active, 2);
        store.credit(user, 100_000_000);
        store.set_pause(true);
        assert!(
            run(&store, &screen, cmd(user, 5_000_000, &dest_a()))
                .await
                .unwrap()
                .refused
        );
        store.set_pause(false);

        let banned = store.seed_user(UserStatus::Banned, 2);
        store.credit(banned, 100_000_000);
        assert_eq!(
            run(&store, &screen, cmd(banned, 5_000_000, &dest_a()))
                .await
                .unwrap()
                .refuse_code
                .as_deref(),
            Some("banned")
        );

        let raw = store.seed_user(UserStatus::Active, 0);
        store.credit(raw, 100_000_000);
        assert_eq!(
            run(&store, &screen, cmd(raw, 5_000_000, &dest_a()))
                .await
                .unwrap()
                .refuse_code
                .as_deref(),
            Some("kyc")
        );

        let ok = store.seed_user(UserStatus::Active, 2);
        store.credit(ok, 100_000_000);
        assert_eq!(
            run(&store, &screen, cmd(ok, 1, &dest_a()))
                .await
                .unwrap()
                .refuse_code
                .as_deref(),
            Some("amount")
        );
        let mut no_ip = cmd(ok, 5_000_000, &dest_b());
        no_ip.client_ip = None;
        assert_eq!(
            run(&store, &screen, no_ip)
                .await
                .unwrap()
                .refuse_code
                .as_deref(),
            Some("geo_missing_ip")
        );
        assert!(RequestWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            geo: &screen,
            sanctions: &screen,
        }
        .execute(cmd(ok, 5_000_000, "nope"))
        .await
        .is_err());
    }

    #[tokio::test]
    async fn auto_collect_and_daily_window_count_settled() {
        let store = FakeWithdrawStore::new();
        let user = store.seed_user(UserStatus::Active, 2);
        store.credit(user, 3_100_000_000);
        let _ = store.add_receivable(user, 10_000_000);
        let screen = clear();
        let accepted = run(&store, &screen, cmd(user, 5_000_000, &dest_a()))
            .await
            .unwrap();
        assert!(!accepted.refused);
        // 30 cash − 10 collect − 5 hold = 15 remaining, withheld = 5
        assert_eq!(store.withheld(), 5_000_000);

        // Force the first row into settled so it still counts in the window.
        {
            let mut tx = store.withdraw_tx().await.unwrap();
            let mut row = tx
                .withdrawal_for_update(accepted.id.unwrap())
                .await
                .unwrap();
            row.combo = Combo::W10;
            row.settle_tx_id = Some(Uuid::new_v4());
            tx.cas_withdrawal(row.id, Combo::W1, &row).await.unwrap();
            tx.commit().await.unwrap();
        }
        let second = run(&store, &screen, cmd(user, 1_000_000_000, &dest_b()))
            .await
            .unwrap();
        assert!(!second.refused);
        let over = run(
            &store,
            &screen,
            RequestWithdrawCmd {
                idempotency_key: Some("over-daily".into()),
                ..cmd(user, 1_000_000_000, "11111111111111111111111111111112")
            },
        )
        .await
        .unwrap();
        assert!(over.refused);
        assert_eq!(over.refuse_code.as_deref(), Some("limit"));
    }

    #[tokio::test]
    async fn auto_collect_rejects_an_overflowed_receivable_total() {
        let store = FakeWithdrawStore::new();
        let user = store.seed_user(UserStatus::Active, 2);
        let _ = store.add_receivable(user, i64::MAX);
        let _ = store.add_receivable(user, 1);
        let mut tx = store.withdraw_tx().await.unwrap();
        tx.lock_user(user).await.unwrap();

        let err = auto_collect(tx.as_mut(), user, "overflow")
            .await
            .unwrap_err();

        assert_eq!(err, AppError::Overflow);
    }

    #[tokio::test]
    async fn self_exclusion_to_new_dest_still_accepts_with_reason() {
        let store = FakeWithdrawStore::new();
        let user = store.seed_user(UserStatus::Active, 2);
        store.credit(user, 20_000_000);
        store.set_self_excluded(user, store.now() + Duration::days(30));
        let screen = clear();
        let receipt = run(&store, &screen, cmd(user, 5_000_000, &dest_a()))
            .await
            .unwrap();
        assert!(!receipt.refused);
        let row = store
            .withdrawals()
            .into_iter()
            .find(|row| Some(row.id) == receipt.id)
            .unwrap();
        assert!(row
            .risk_reasons
            .iter()
            .any(|reason| reason == "self_exclusion_new_dest"));
    }

    #[tokio::test]
    async fn self_exclusion_observation_source_is_an_allowed_review_path() {
        let store = FakeWithdrawStore::new();
        let user = store.seed_user(UserStatus::Active, 2);
        store.credit(user, 20_000_000);
        store.set_self_excluded(user, store.now() + Duration::days(30));
        store.mark_observation_source(user, &dest_a());
        let screen = clear();

        let receipt = run(&store, &screen, cmd(user, 5_000_000, &dest_a()))
            .await
            .unwrap();
        let row = store
            .withdrawals()
            .into_iter()
            .find(|row| Some(row.id) == receipt.id)
            .unwrap();

        assert!(!row
            .risk_reasons
            .iter()
            .any(|reason| reason == "self_exclusion_new_dest"));
        assert!(row
            .risk_reasons
            .iter()
            .any(|reason| reason == "dest_is_refund"));
    }

    #[tokio::test]
    async fn fourth_in_band_withdrawal_opens_aml_at_request() {
        let store = FakeWithdrawStore::new();
        let user = store.seed_user(UserStatus::Active, 2);
        store.credit(user, 1_000_000_000);
        let screen = clear();

        let mut fourth = None;
        for (index, amount) in [100_000_000, 101_000_000, 102_000_000, 103_000_000]
            .into_iter()
            .enumerate()
        {
            let receipt = run(
                &store,
                &screen,
                RequestWithdrawCmd {
                    user,
                    amount_micro: amount,
                    dest: dest_a(),
                    client_ip: Some(IpAddr::from([203, 0, 113, 10])),
                    idempotency_key: Some(format!("aml-band-{index}")),
                },
            )
            .await
            .unwrap();
            fourth = Some(receipt.id.unwrap());
        }

        let row = store
            .withdrawals()
            .into_iter()
            .find(|row| row.id == fourth.unwrap())
            .unwrap();
        assert!(row.risk_reasons.iter().any(|reason| reason == "aml_open"));
    }

    #[tokio::test]
    async fn request_edges_are_replay_stable_and_fail_closed() {
        let store = FakeWithdrawStore::new();
        let user = store.seed_user(UserStatus::Active, 2);
        let screen = clear();
        let err = run(&store, &screen, cmd(user, 0, &dest_a()))
            .await
            .unwrap_err();
        assert_eq!(err, AppError::InsufficientFunds);

        let no_cash = run(&store, &screen, cmd(user, 5_000_000, &dest_a()))
            .await
            .unwrap();
        assert_eq!(no_cash.refuse_code.as_deref(), Some("insufficient_funds"));

        let sanctions_down = FakeScreen {
            verdict: ScreenVerdict::Indeterminate,
            fail: true,
        };
        let second = store.seed_user(UserStatus::Active, 2);
        store.credit(second, 20_000_000);
        let refused = run_with_screens(
            &store,
            &screen,
            &sanctions_down,
            cmd(second, 5_000_000, &dest_a()),
        )
        .await
        .unwrap();
        assert_eq!(
            refused.refuse_code.as_deref(),
            Some("sanctions_unavailable")
        );

        let partial = store.seed_user(UserStatus::Active, 2);
        store.credit(partial, 3_000_000);
        let _ = store.add_receivable(partial, 2_000_000);
        let _ = store.add_receivable(partial, 2_000_000);
        let _ = store.add_receivable(partial, 2_000_000);
        let refused = run(&store, &screen, cmd(partial, 5_000_000, &dest_a()))
            .await
            .unwrap();
        assert_eq!(refused.refuse_code.as_deref(), Some("insufficient_funds"));

        let shadow = store.seed_user(UserStatus::ShadowLimited, 2);
        store.credit(shadow, 1_000_000_000);
        let mut no_client_key = cmd(shadow, 500_000_000, &dest_b());
        no_client_key.idempotency_key = None;
        let accepted = run(&store, &screen, no_client_key).await.unwrap();
        let row = store
            .withdrawals()
            .into_iter()
            .find(|row| Some(row.id) == accepted.id)
            .unwrap();
        assert!(row
            .risk_reasons
            .iter()
            .any(|reason| reason == "amount_ge_auto"));
        assert!(row
            .risk_reasons
            .iter()
            .any(|reason| reason == "amount_ge_dual"));
        assert!(row
            .risk_reasons
            .iter()
            .any(|reason| reason == "shadow_limited"));

        assert!(!is_fresh_clear(
            Some(&ScreenVerdict::Clear {
                checked_at: store.now() + Duration::seconds(1),
                expires_at: store.now() + Duration::hours(1),
                policy_version: "1".into(),
            }),
            store.now(),
        ));
        assert!(!is_fresh_clear(
            Some(&ScreenVerdict::Clear {
                checked_at: store.now(),
                expires_at: store.now() + Duration::hours(1),
                policy_version: String::new(),
            }),
            store.now(),
        ));
    }

    #[tokio::test]
    async fn request_race_zero_collection_and_egress_edges_are_explicit() {
        let store = FakeWithdrawStore::new();
        let user = store.seed_user(UserStatus::Active, 2);
        store.credit(user, 200_000_000);
        let screen = clear();
        let first_cmd = cmd(user, 5_000_000, &dest_a());
        let first = run(&store, &screen, first_cmd.clone()).await.unwrap();

        store.suppress_lock_free_fingerprint();
        let replay = run(&store, &screen, first_cmd).await.unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.id, first.id);

        let dest = dest_b();
        let amount = 6_000_000;
        let fingerprint = intent_fingerprint(user, amount, &dest);
        let key = "preseeded-equal-key";
        let mut tx = store.withdraw_tx().await.unwrap();
        tx.persist_idempotency(&idempotency_slot(user, key), &fingerprint)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let accepted = run(
            &store,
            &screen,
            RequestWithdrawCmd {
                idempotency_key: Some(key.into()),
                ..cmd(user, amount, &dest)
            },
        )
        .await
        .unwrap();
        assert!(!accepted.refused);

        let no_cash = store.seed_user(UserStatus::Active, 2);
        let _ = store.add_receivable(no_cash, 1);
        let mut tx = store.withdraw_tx().await.unwrap();
        tx.lock_user(no_cash).await.unwrap();
        assert_eq!(
            auto_collect(tx.as_mut(), no_cash, "zero-cash")
                .await
                .unwrap(),
            0
        );

        let shared = store.seed_user(UserStatus::Active, 2);
        store.credit(shared, 20_000_000);
        store.set_dest_distinct_users(&dest_a(), 2);
        let receipt = run(&store, &screen, cmd(shared, 5_000_000, &dest_a()))
            .await
            .unwrap();
        let row = store
            .withdrawals()
            .into_iter()
            .find(|row| Some(row.id) == receipt.id)
            .unwrap();
        assert!(row
            .risk_reasons
            .iter()
            .any(|reason| reason == "dest_shared"));

        let egress = store.seed_user(UserStatus::Active, 2);
        store.credit(egress, 30_000_000);
        let prior_id = WithdrawalId(Uuid::new_v4());
        let mut tx = store.withdraw_tx().await.unwrap();
        tx.lock_user(egress).await.unwrap();
        let hold = tx
            .apply_hold(egress, 5_000_000, "egress-hold")
            .await
            .unwrap();
        let settle = tx.apply_settle(5_000_000, "egress-settle").await.unwrap();
        tx.insert_withdrawal(&WithdrawalRow {
            id: prior_id,
            user: egress,
            dest: dest_b(),
            amount_micro: 5_000_000,
            combo: Combo::W10,
            hold_tx_id: hold,
            release_tx_id: None,
            settle_tx_id: Some(settle),
            request_fingerprint: "prior-egress".into(),
            risk_reasons: vec![],
            requested_at: store.now() - Duration::days(10),
            decided_at: Some(store.now() - Duration::days(10)),
            sent_at: Some(store.now() - Duration::days(10)),
            settled_at: Some(store.now() - Duration::days(10)),
        })
        .await
        .unwrap();
        tx.commit().await.unwrap();
        store.set_self_excluded(egress, store.now() + Duration::days(30));
        let receipt = run(&store, &screen, cmd(egress, 5_000_000, &dest_b()))
            .await
            .unwrap();
        let row = store
            .withdrawals()
            .into_iter()
            .find(|row| Some(row.id) == receipt.id)
            .unwrap();
        assert!(!row
            .risk_reasons
            .iter()
            .any(|reason| reason == "self_exclusion_new_dest"));
    }
}
