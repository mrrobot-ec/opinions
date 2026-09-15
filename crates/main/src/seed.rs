//! `--seed-demo`: the demo world, built through use cases only (raw SQL for
//! money/markets/reserves is banned). Idempotent end to end — re-running the
//! seed replays every step: genesis and deposit replay on their deterministic
//! keys, the market id is derived from the slug (uuid v5) so `SeedMarket`
//! replays, `GoLive` is skipped once the market left `Scheduled`, and the
//! gate vote replays on its key.

use std::env;

use adapters::pg::PgStore;
use application::advance_market::{AdvanceMarket, AdvanceMarketCmd};
use application::cast_vote::{CastVote, CastVoteCmd};
use application::create_user::{CreateUser, CreateUserCmd};
use application::credit_deposit::{CreditDeposit, CreditDepositCmd};
use application::ensure_genesis::{EnsureGenesis, EnsureGenesisCmd};
use application::model::{LpKillConfig, MarketId, RepConfig, UserId};
use application::ports::MarketQueries;
use application::seed_market::{SeedMarket, SeedMarketCmd};
use domain::amm::Side;
use domain::ledger::Currency;
use domain::market::{MarketEvent, MarketState};
use domain::money::{BasisPoints, MicroUsd};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::SystemClock;

const DEMO_SLUG: &str = "demo-coffee";
const DEMO_CHAIN_SIG: &str = "demo-chain-sig-0001";

fn env_i64(name: &str, default: i64) -> i64 {
    env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_truthy(name: &str) -> bool {
    env::var(name).is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

fn env_positive_i64(name: &str) -> Option<i64> {
    env::var(name)
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
}

/// Flash-market windows for Task 2.5 live-loop; defaults match Phase 1 e2e.
fn market_windows(now: OffsetDateTime) -> (OffsetDateTime, OffsetDateTime) {
    let flash_closes = env_positive_i64("FLASH_CLOSES_SECS");
    let flash_hidden = env_positive_i64("FLASH_TALLY_HIDDEN_SECS");
    match (flash_closes, flash_hidden) {
        (Some(closes), Some(hidden)) => {
            let closes_at = now + Duration::seconds(closes);
            let tally_hidden_at = now + Duration::seconds(hidden.min(closes));
            (closes_at, tally_hidden_at)
        }
        (Some(closes), None) => {
            let closes_at = now + Duration::seconds(closes);
            let tally_hidden_at = closes_at - Duration::seconds(closes.min(30));
            (closes_at, tally_hidden_at)
        }
        _ => {
            let closes_at = now + Duration::hours(2);
            (closes_at, closes_at - Duration::minutes(10))
        }
    }
}

async fn ensure_demo_user(store: &PgStore, phone: &str) -> anyhow::Result<UserId> {
    if let Some(existing) = store.user_by_channel("imessage", phone).await? {
        return Ok(existing);
    }
    Ok(CreateUser { store }
        .execute(CreateUserCmd::plain(
            "demo",
            Some(("imessage".to_string(), phone.to_string())),
            &format!("create-user:demo:{phone}"),
        ))
        .await?)
}

async fn maybe_gate_vote(
    store: &PgStore,
    market: MarketId,
    user: UserId,
    slug: &str,
    skip: bool,
) -> anyhow::Result<()> {
    if skip {
        println!("vote_id=skipped");
        return Ok(());
    }
    let vote = CastVote {
        store,
        clock: &SystemClock,
        config: application::model::VoteIntegrityConfig::default(),
    }
    .execute(CastVoteCmd {
        market,
        user,
        side: Side::Yes,
        crowd_guess_pct: 60,
        idempotency_key: format!("vote:{slug}:{}", user.0),
        cast_ip: None,
        device_hash: None,
    })
    .await?;
    println!(
        "vote_id={} seq={:?} replayed={}",
        vote.vote_id.0, vote.seq, vote.replayed
    );
    Ok(())
}

pub async fn seed_demo(
    store: &PgStore,
    rep_config: RepConfig,
    lp_kill_config: LpKillConfig,
) -> anyhow::Result<()> {
    let phone = env::var("DEMO_PHONE").unwrap_or_else(|_| "+15550100".to_string());
    let genesis_micro = env_i64("GENESIS_HOUSE_MICRO", 10_000_000_000); // $10,000
    let min_votes = i32::try_from(env_i64("MIN_VOTES_TO_RESOLVE", 1)).unwrap_or(1);
    let skip_vote = env_truthy("SEED_SKIP_VOTE");
    let slug = env::var("DEMO_SLUG").unwrap_or_else(|_| DEMO_SLUG.to_string());
    let chain_sig = env::var("DEMO_CHAIN_SIG").unwrap_or_else(|_| DEMO_CHAIN_SIG.to_string());

    let genesis = EnsureGenesis { store }
        .execute(EnsureGenesisCmd {
            currency: Currency::Usdc,
            amount: MicroUsd(genesis_micro),
        })
        .await?;
    let user = ensure_demo_user(store, &phone).await?;

    // Deterministic market id: same slug → same id on every run.
    let market_id = MarketId(Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("opinions:market:{slug}").as_bytes(),
    ));
    let (closes_at, tally_hidden_at) = market_windows(OffsetDateTime::now_utc());
    let seeded = SeedMarket {
        store,
        clock: &SystemClock,
        rep_config,
        lp_kill_config,
    }
    .execute(SeedMarketCmd {
        market_id,
        slug: slug.clone(),
        min_votes_to_resolve: min_votes,
        closes_at,
        tally_hidden_at,
        fee: BasisPoints(100),
        seed: MicroUsd(1_000_000_000), // $1,000 at 50/50
        idempotency_key: format!("seed:{slug}"),
        force: false,
    })
    .await?;

    let market = store.market_by_ref(&slug).await?;
    if market.state == MarketState::Scheduled {
        AdvanceMarket { store }
            .execute(AdvanceMarketCmd {
                market: market.id,
                event: MarketEvent::GoLive,
                idempotency_key: format!("golive:{slug}"),
            })
            .await?;
    }

    let deposit = CreditDeposit { store }
        .execute(CreditDepositCmd {
            user,
            amount: MicroUsd(50_000_000), // $50
            chain_sig: chain_sig.clone(),
            idempotency_key: format!("deposit:{chain_sig}"),
        })
        .await?;

    println!(
        "genesis_txn={} replayed={}",
        genesis.ledger_txn, genesis.replayed
    );
    println!("user_id={}", user.0);
    println!("market_id={} replayed={}", seeded.market.0, seeded.replayed);
    println!("slug={slug}");
    println!("closes_at={closes_at}");
    println!("tally_hidden_at={tally_hidden_at}");
    println!(
        "deposit_id={} replayed={}",
        deposit.deposit_id.0, deposit.replayed
    );
    maybe_gate_vote(store, market.id, user, &slug, skip_vote).await?;
    println!("demo_phone={phone}");
    Ok(())
}
