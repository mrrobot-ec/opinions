//! Small operator/e2e entry point for one Phase 5 video-worker action.

use std::env;

use adapters::pg::PgStore;
use application::ensure_genesis::{EnsureGenesis, EnsureGenesisCmd};
use application::model::ContentConfig;
use application::ports::{Clock, Store};
use domain::ledger::Currency;
use domain::money::MicroUsd;

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> time::OffsetDateTime {
        time::OffsetDateTime::now_utc()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = env::var("DATABASE_URL")?;
    let store = PgStore::connect(&url).await?;
    let mut config = ContentConfig::default();
    if let Some(path) = env::var_os("CONTENT_RENDER_DIR") {
        config.render_dir = path.into();
    }
    match env::args().nth(1).as_deref() {
        Some("render-one") => {
            application::video::worker_tick(
                &store,
                &SystemClock,
                adapters::render::renderer().as_ref(),
                &config,
            )
            .await?;
            println!("rendered");
        }
        Some("claim-one") => {
            let now = SystemClock.now();
            let mut tx = store.video_tx().await?;
            let claimed = tx.claim(now, time::Duration::seconds(60), 1).await?;
            tx.commit().await?;
            println!(
                "{}",
                claimed
                    .first()
                    .map_or_else(|| "none".to_string(), |job| job.id.0.to_string())
            );
        }
        Some("ensure-genesis") => {
            let amount = env::var("GENESIS_HOUSE_MICRO")
                .unwrap_or_else(|_| "20000000000".to_string())
                .parse::<i64>()?;
            let receipt = EnsureGenesis { store: &store }
                .execute(EnsureGenesisCmd {
                    currency: Currency::Usdc,
                    amount: MicroUsd(amount),
                })
                .await?;
            println!("{}", receipt.ledger_txn);
        }
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "usage: content_worker render-one|claim-one|ensure-genesis",
            )
            .into())
        }
    }
    Ok(())
}
