//! The local database: operator intent, live status, timeline, intent journal, ledger.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, B256, U256};
use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

use crate::protocol::intents::SignedIntent;
use crate::protocol::wire::Trade;
use crate::status::Snapshot;
use crate::strategy::decide::{Mode, Side};
use crate::strategy::inflight::Inflight;

pub fn now_ms() -> i64 {
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct EventRow {
    pub at_ms: i64,
    pub kind: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct IntentRow {
    pub created_at_ms: i64,
    pub side: String,
    pub action: String,
    pub status: String,
    pub outcome: Option<String>,
    pub detail: Option<String>,
}

/// Ledger totals for display. Floating point: not for accounting.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Totals {
    pub trades: i64,
    pub net_pnl_usd: f64,
    pub paper_minted: f64,
}

impl Totals {
    /// What one PAPER has cost so far, in USD.
    pub fn cost_per_paper(&self) -> Option<f64> {
        (self.paper_minted > 0.0).then(|| -self.net_pnl_usd / self.paper_minted)
    }
}

#[derive(Debug, Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// Opens the database, creating and migrating it as needed.
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            // WAL lets viewers read while the bot writes.
            .journal_mode(SqliteJournalMode::Wal);
        Self::connect(SqlitePoolOptions::new().connect_with(options).await?).await
    }

    #[cfg(test)]
    pub async fn in_memory() -> Result<Self> {
        // One connection: every connection to `:memory:` is its own database.
        Self::connect(SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await?).await
    }

    async fn connect(pool: SqlitePool) -> Result<Self> {
        sqlx::migrate!().run(&pool).await.context("migrating the database")?;
        Ok(Self { pool })
    }

    // --- operator intent ------------------------------------------------------

    pub async fn mode(&self) -> Result<Mode> {
        let mode: String =
            sqlx::query_scalar("SELECT mode FROM control WHERE id = 1").fetch_one(&self.pool).await?;
        Ok(serde_json::from_value(Value::String(mode))?)
    }

    pub async fn set_mode(&self, mode: Mode) -> Result<()> {
        sqlx::query("UPDATE control SET mode = ?, updated_at_ms = ? WHERE id = 1")
            .bind(mode.as_str())
            .bind(now_ms())
            .execute(&self.pool)
            .await?;
        self.record_event("mode", &format!("operator set mode to {}", mode.as_str()), None).await
    }

    // --- live status ------------------------------------------------------------

    pub async fn write_status(&self, snapshot: &Snapshot) -> Result<()> {
        sqlx::query(
            "INSERT INTO status (id, updated_at_ms, snapshot) VALUES (1, ?, ?)
             ON CONFLICT (id) DO UPDATE SET updated_at_ms = excluded.updated_at_ms, snapshot = excluded.snapshot",
        )
        .bind(snapshot.at_ms)
        .bind(serde_json::to_string(snapshot)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn read_status(&self) -> Result<Option<Snapshot>> {
        let row: Option<String> =
            sqlx::query_scalar("SELECT snapshot FROM status WHERE id = 1").fetch_optional(&self.pool).await?;
        row.map(|json| serde_json::from_str(&json).context("reading the status snapshot")).transpose()
    }

    // --- timeline ---------------------------------------------------------------

    pub async fn record_event(&self, kind: &str, summary: &str, detail: Option<&Value>) -> Result<()> {
        sqlx::query("INSERT INTO events (at_ms, kind, summary, detail) VALUES (?, ?, ?, ?)")
            .bind(now_ms())
            .bind(kind)
            .bind(summary)
            .bind(detail.map(Value::to_string))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The latest events, newest first.
    pub async fn recent_events(&self, limit: u32) -> Result<Vec<EventRow>> {
        Ok(sqlx::query_as("SELECT at_ms, kind, summary FROM events ORDER BY id DESC LIMIT ?")
            .bind(limit)
            .fetch_all(&self.pool)
            .await?)
    }

    // --- intent journal -----------------------------------------------------------

    /// Records an intent before it is sent. If this fails, the intent must not go out.
    pub async fn journal_intent(&self, side: Side, intent: &SignedIntent) -> Result<()> {
        sqlx::query(
            "INSERT INTO intents (intent_id, created_at_ms, side, action, deadline, body, status)
             VALUES (?, ?, ?, ?, ?, ?, 'signed')",
        )
        .bind(intent.intent_id.to_string())
        .bind(now_ms())
        .bind(side.as_str())
        .bind(intent.action.as_str())
        .bind(i64::try_from(intent.deadline)?)
        .bind(intent.body().to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_accepted(&self, intent_id: B256, queue_revision: U256) -> Result<()> {
        sqlx::query("UPDATE intents SET status = 'accepted', queue_revision = ? WHERE intent_id = ?")
            .bind(queue_revision.to_string())
            .bind(intent_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// `status` is `rejected` (it will not execute) or `unknown` (it still might).
    pub async fn mark_not_accepted(&self, intent_id: B256, status: &str, detail: &str) -> Result<()> {
        sqlx::query("UPDATE intents SET status = ?, detail = ? WHERE intent_id = ?")
            .bind(status)
            .bind(detail)
            .bind(intent_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn mark_seen_queued(&self, intent_id: B256) -> Result<()> {
        sqlx::query("UPDATE intents SET seen_queued = 1 WHERE intent_id = ?")
            .bind(intent_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn mark_settled(&self, intent_id: B256) -> Result<()> {
        sqlx::query("UPDATE intents SET status = 'settled', settled_at_ms = ? WHERE intent_id = ?")
            .bind(now_ms())
            .bind(intent_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_outcome(&self, intent_id: B256, outcome: &str) -> Result<()> {
        sqlx::query("UPDATE intents SET outcome = ? WHERE intent_id = ?")
            .bind(outcome)
            .bind(intent_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Intents that could still change an account. An intent that was journaled
    /// but never confirmed as sent counts: it may have left before a crash.
    pub async fn unresolved_intents(&self) -> Result<Vec<Inflight>> {
        let rows: Vec<(String, String, i64, Option<String>, bool)> = sqlx::query_as(
            "SELECT intent_id, side, deadline, queue_revision, seen_queued FROM intents
             WHERE status IN ('signed', 'accepted', 'unknown') ORDER BY created_at_ms",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(intent_id, side, deadline, queue_revision, seen_queued)| {
                Ok(Inflight {
                    side: serde_json::from_value(Value::String(side))?,
                    intent_id: intent_id.parse()?,
                    deadline: u64::try_from(deadline)?,
                    queue_revision: queue_revision.map(|text| text.parse()).transpose()?,
                    seen_queued,
                })
            })
            .collect()
    }

    /// When the last open was attempted, successful or not.
    pub async fn last_open_attempt_ms(&self) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar("SELECT MAX(created_at_ms) FROM intents WHERE action = 'openPosition'")
            .fetch_one(&self.pool)
            .await?)
    }

    /// The latest intents, newest first.
    pub async fn recent_intents(&self, limit: u32) -> Result<Vec<IntentRow>> {
        Ok(sqlx::query_as(
            "SELECT created_at_ms, side, action, status, outcome, detail FROM intents
             ORDER BY created_at_ms DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    // --- ledger -------------------------------------------------------------------

    /// Adds trades the ledger does not hold yet and returns them.
    pub async fn record_trades(&self, wallet: Address, side: Side, trades: &[Trade]) -> Result<Vec<Trade>> {
        let mut added = Vec::new();
        for trade in trades {
            let inserted = sqlx::query(
                "INSERT INTO trades (wallet, position_id, side, closed_at_ms, tx_hash, symbol, kind, leverage,
                                     margin, price_scale, entry_price, exit_price, adjusted_pnl, user_paid_fee,
                                     paper_minted, paper_mint_basis)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT (wallet, position_id) DO NOTHING",
            )
            .bind(format!("{wallet:#x}"))
            .bind(trade.position_id.0.to_string())
            .bind(side.as_str())
            .bind(i64::try_from(trade.timestamp_ms)?)
            .bind(trade.tx_hash.to_string())
            .bind(&trade.symbol)
            .bind(&trade.kind)
            .bind(trade.leverage)
            .bind(trade.margin_raw.0.to_string())
            .bind(trade.price_scale_raw.0.to_string())
            .bind(trade.entry_price_raw.0.to_string())
            .bind(trade.exit_price_raw.0.to_string())
            .bind(trade.adjusted_pnl_raw.0.to_string())
            .bind(trade.user_paid_fee_raw.0.to_string())
            .bind(trade.paper_minted_raw.0.to_string())
            .bind(trade.paper_mint_basis_raw.0.to_string())
            .execute(&self.pool)
            .await?;
            if inserted.rows_affected() > 0 {
                added.push(trade.clone());
            }
        }
        Ok(added)
    }

    pub async fn totals(&self) -> Result<Totals> {
        let (trades, net_pnl_usd, paper_minted): (i64, f64, f64) = sqlx::query_as(
            "SELECT COUNT(*), COALESCE(SUM(net_pnl_usd), 0.0), COALESCE(SUM(paper_minted), 0.0) FROM trades_usd",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(Totals { trades, net_pnl_usd, paper_minted })
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;
    use alloy::signers::local::PrivateKeySigner;

    use super::*;
    use crate::protocol::deployment::usd;
    use crate::protocol::intents::{OpenPosition, sign_open_position};
    use crate::protocol::wire::{Dec, SignedDec};

    const WALLET: Address = address!("0x000000000000000000000000000000000000dead");

    fn intent(nonce: u64) -> SignedIntent {
        let message = OpenPosition {
            user: WALLET,
            instrumentId: 0,
            isLong: true,
            size: 500 * 10u128.pow(18),
            leverage: 20,
            useDebt: false,
            nonce: U256::from(nonce),
            deadline: 1_800_000_120,
        };
        sign_open_position(&PrivateKeySigner::random(), &message).unwrap()
    }

    fn trade(position_id: u64, pnl_usd: i64, paper: u64) -> Trade {
        let pnl = alloy::primitives::I256::try_from(pnl_usd).unwrap()
            * alloy::primitives::I256::try_from(usd(1)).unwrap();
        Trade {
            timestamp_ms: 1_791_700_000_000,
            tx_hash: B256::with_last_byte(1),
            position_id: Dec(U256::from(position_id)),
            symbol: "BTC".into(),
            price_scale_raw: Dec(U256::from(10)),
            is_long: true,
            entry_price_raw: Dec(U256::from(827_935)),
            margin_raw: Dec(usd(500)),
            leverage: 20,
            kind: "PositionClosed".into(),
            exit_price_raw: Dec(U256::from(819_656)),
            adjusted_pnl_raw: SignedDec(pnl),
            user_paid_fee_raw: Dec(U256::ZERO),
            paper_minted_raw: Dec(usd(paper)),
            paper_mint_basis_raw: Dec(U256::ZERO),
        }
    }

    #[tokio::test]
    async fn the_operator_mode_starts_at_run_and_changes_are_on_the_timeline() {
        let store = Store::in_memory().await.unwrap();
        assert_eq!(store.mode().await.unwrap(), Mode::Run);
        store.set_mode(Mode::Flatten).await.unwrap();
        assert_eq!(store.mode().await.unwrap(), Mode::Flatten);
        let events = store.recent_events(10).await.unwrap();
        assert_eq!(
            (events[0].kind.as_str(), events[0].summary.as_str()),
            ("mode", "operator set mode to flatten")
        );
    }

    #[tokio::test]
    async fn status_is_one_row_that_gets_replaced() {
        let store = Store::in_memory().await.unwrap();
        assert_eq!(store.read_status().await.unwrap(), None);
        let mut snapshot = Snapshot {
            at_ms: 1,
            dry_run: true,
            mode: Mode::Run,
            coin: "BTC".into(),
            close_bps: 100,
            decision: "wait".into(),
            summary: "flat; settling before the next open".into(),
            mid: Some("82754.5".into()),
            move_bps: None,
            trading: None,
            queue_total_usd: None,
            legs: Vec::new(),
        };
        store.write_status(&snapshot).await.unwrap();
        snapshot.at_ms = 2;
        snapshot.summary = "open both legs".into();
        store.write_status(&snapshot).await.unwrap();
        assert_eq!(store.read_status().await.unwrap(), Some(snapshot));
    }

    #[tokio::test]
    async fn events_come_back_newest_first() {
        let store = Store::in_memory().await.unwrap();
        for summary in ["first", "second", "third"] {
            store.record_event("decision", summary, None).await.unwrap();
        }
        let summaries: Vec<String> =
            store.recent_events(2).await.unwrap().into_iter().map(|event| event.summary).collect();
        assert_eq!(summaries, ["third", "second"]);
    }

    #[tokio::test]
    async fn a_journaled_intent_stays_unresolved_until_it_settles_or_is_rejected() {
        let store = Store::in_memory().await.unwrap();
        let (accepted, rejected, lost) = (intent(1), intent(2), intent(3));
        for (side, intent) in [(Side::Long, &accepted), (Side::Short, &rejected), (Side::Short, &lost)] {
            store.journal_intent(side, intent).await.unwrap();
        }
        // Journaled but not confirmed as sent: all three may have left before a crash.
        assert_eq!(store.unresolved_intents().await.unwrap().len(), 3);

        store.mark_accepted(accepted.intent_id, U256::from(7)).await.unwrap();
        store.mark_seen_queued(accepted.intent_id).await.unwrap();
        store
            .mark_not_accepted(rejected.intent_id, "rejected", "admission_rejected: openInterest")
            .await
            .unwrap();
        store.mark_not_accepted(lost.intent_id, "unknown", "connection reset").await.unwrap();

        let unresolved = store.unresolved_intents().await.unwrap();
        assert_eq!(
            unresolved,
            [
                Inflight {
                    side: Side::Long,
                    intent_id: accepted.intent_id,
                    deadline: 1_800_000_120,
                    queue_revision: Some(U256::from(7)),
                    seen_queued: true,
                },
                Inflight {
                    side: Side::Short,
                    intent_id: lost.intent_id,
                    deadline: 1_800_000_120,
                    queue_revision: None,
                    seen_queued: false,
                },
            ]
        );

        store.mark_settled(accepted.intent_id).await.unwrap();
        store.mark_settled(lost.intent_id).await.unwrap();
        store.set_outcome(accepted.intent_id, "succeeded").await.unwrap();
        assert!(store.unresolved_intents().await.unwrap().is_empty());

        let journal = store.recent_intents(10).await.unwrap();
        assert_eq!(journal.len(), 3);
        let settled = journal.iter().find(|row| row.outcome.is_some()).unwrap();
        assert_eq!((settled.status.as_str(), settled.outcome.as_deref()), ("settled", Some("succeeded")));
        let refused = journal.iter().find(|row| row.status == "rejected").unwrap();
        assert_eq!(refused.detail.as_deref(), Some("admission_rejected: openInterest"));
    }

    #[tokio::test]
    async fn the_same_intent_cannot_be_journaled_twice() {
        let store = Store::in_memory().await.unwrap();
        let intent = intent(1);
        store.journal_intent(Side::Long, &intent).await.unwrap();
        assert!(store.journal_intent(Side::Long, &intent).await.is_err());
    }

    #[tokio::test]
    async fn the_last_open_attempt_is_remembered_across_restarts() {
        let store = Store::in_memory().await.unwrap();
        assert_eq!(store.last_open_attempt_ms().await.unwrap(), None);
        let before = now_ms();
        store.journal_intent(Side::Long, &intent(1)).await.unwrap();
        let attempt = store.last_open_attempt_ms().await.unwrap().unwrap();
        assert!((before..=now_ms()).contains(&attempt));
    }

    #[tokio::test]
    async fn the_ledger_records_each_settled_position_once() {
        let store = Store::in_memory().await.unwrap();
        assert_eq!(store.totals().await.unwrap(), Totals::default());
        assert_eq!(store.totals().await.unwrap().cost_per_paper(), None);

        let (loser, winner) = (trade(1, -100, 9_800), trade(2, 86, 0));
        let added = store.record_trades(WALLET, Side::Long, std::slice::from_ref(&loser)).await.unwrap();
        assert_eq!(added, std::slice::from_ref(&loser));
        // The history endpoint returns the same trades on every poll.
        let added = store.record_trades(WALLET, Side::Long, &[loser, winner.clone()]).await.unwrap();
        assert_eq!(added, [winner]);

        let totals = store.totals().await.unwrap();
        assert_eq!(totals.trades, 2);
        assert!((totals.net_pnl_usd - -14.0).abs() < 1e-9);
        assert!((totals.paper_minted - 9_800.0).abs() < 1e-9);
        assert!((totals.cost_per_paper().unwrap() - 14.0 / 9_800.0).abs() < 1e-12);
    }

    #[tokio::test]
    async fn the_win_fee_comes_off_the_winners_net_result() {
        let store = Store::in_memory().await.unwrap();
        let mut winner = trade(1, 88, 0);
        winner.user_paid_fee_raw = Dec(usd(2));
        store.record_trades(WALLET, Side::Short, &[winner]).await.unwrap();
        assert!((store.totals().await.unwrap().net_pnl_usd - 86.0).abs() < 1e-9);
    }
}
