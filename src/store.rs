//! The local database: operator intent, live status, timeline, intent journal, ledger.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, B256, U256};
use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

use crate::protocol::intents::SignedIntent;
use crate::protocol::wire::Trade;
use crate::status::Snapshot;
use crate::strategy::decide::{Mode, Side};
use crate::strategy::inflight::{Inflight, Kind, is_failure};
use crate::strategy::transfer::{Stage, Transfer};

/// How often opening the database is tried, and the pause in between.
const OPEN_ATTEMPTS: u32 = 10;
const OPEN_RETRY: std::time::Duration = std::time::Duration::from_millis(100);

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

#[derive(sqlx::FromRow)]
struct UnresolvedRow {
    intent_id: String,
    side: String,
    action: String,
    created_at_ms: i64,
    deadline: i64,
    queue_revision: Option<String>,
    seen_queued: bool,
    outcome: Option<String>,
}

/// What the journal knows about one intent.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct IntentFate {
    pub status: String,
    pub outcome: Option<String>,
    pub created_at_ms: i64,
    pub settled_at_ms: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct TransferRow {
    id: i64,
    started_at_ms: i64,
    from_side: String,
    to_side: String,
    amount_usdc: i64,
    stage: String,
    attempts: i64,
    attempt_at_ms: i64,
    core_baseline: Option<String>,
    sender_baseline: Option<String>,
    receiver_baseline: Option<String>,
    withdraw_intent: Option<String>,
    send_nonce: Option<i64>,
    checked_at_ms: Option<i64>,
    note: Option<String>,
}

impl TryFrom<TransferRow> for Transfer {
    type Error = anyhow::Error;

    fn try_from(row: TransferRow) -> Result<Self> {
        let amount = |text: Option<String>| text.map(|text| text.parse::<U256>()).transpose();
        Ok(Transfer {
            id: row.id,
            from: serde_json::from_value(Value::String(row.from_side))?,
            to: serde_json::from_value(Value::String(row.to_side))?,
            amount_usdc: u64::try_from(row.amount_usdc)?,
            stage: Stage::parse(&row.stage)
                .with_context(|| format!("unknown transfer stage {:?}", row.stage))?,
            started_at_ms: row.started_at_ms,
            attempts: u32::try_from(row.attempts)?,
            attempt_at_ms: row.attempt_at_ms,
            core_baseline: amount(row.core_baseline)?,
            sender_baseline: amount(row.sender_baseline)?,
            receiver_baseline: amount(row.receiver_baseline)?,
            withdraw_intent: row.withdraw_intent.map(|text| text.parse()).transpose()?,
            send_nonce: row.send_nonce.map(u64::try_from).transpose()?,
            checked_at_ms: row.checked_at_ms,
            note: row.note,
        })
    }
}

/// What `rebalance abandon` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Abandoned {
    /// The stuck transfer, now ended.
    Ended(Transfer),
    /// The transfer is still under way. Only a stuck one can be abandoned.
    NotStuck(Transfer),
    /// No transfer is unfinished.
    Nothing,
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
            .journal_mode(SqliteJournalMode::Wal)
            // No fsync per commit: on a slow disk one could stall for a second or more.
            // A process crash still loses nothing. Only a power cut can drop the last
            // commits, and an intent journaled that late has expired before the host is back.
            .synchronous(SqliteSynchronous::Normal);
        // The bot and a viewer are often started together. On a database that is
        // new or needs migrating, both set it up at once and SQLite refuses one of
        // them outright instead of making it wait. By the time that one tries
        // again the other has finished, so trying again is all it takes.
        let mut attempt = 1;
        loop {
            let opened = match SqlitePoolOptions::new().connect_with(options.clone()).await {
                Ok(pool) => Self::connect(pool).await,
                Err(error) => Err(error.into()),
            };
            match opened {
                Err(_) if attempt < OPEN_ATTEMPTS => {
                    attempt += 1;
                    tokio::time::sleep(OPEN_RETRY).await;
                }
                opened => return opened,
            }
        }
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

    /// Closes the database. Every later call fails.
    pub async fn close(&self) {
        self.pool.close().await;
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

    /// The latest snapshot, or `None` if there is none this version can read.
    /// A snapshot only lives until the running bot writes the next one, so one
    /// left behind by another version is simply not a status.
    pub async fn read_status(&self) -> Result<Option<Snapshot>> {
        let row: Option<String> =
            sqlx::query_scalar("SELECT snapshot FROM status WHERE id = 1").fetch_optional(&self.pool).await?;
        Ok(row.and_then(|json| serde_json::from_str(&json).ok()))
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
        insert_intent(&self.pool, side, intent).await
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
        let rows: Vec<UnresolvedRow> = sqlx::query_as(
            "SELECT intent_id, side, action, created_at_ms, deadline, queue_revision, seen_queued, outcome
             FROM intents WHERE status IN ('signed', 'accepted', 'unknown') ORDER BY created_at_ms",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(Inflight {
                    side: serde_json::from_value(Value::String(row.side))?,
                    kind: Kind::of_action(&row.action),
                    intent_id: row.intent_id.parse()?,
                    deadline: u64::try_from(row.deadline)?,
                    sent_at_ms: row.created_at_ms,
                    queue_revision: row.queue_revision.map(|text| text.parse()).transpose()?,
                    seen_queued: row.seen_queued,
                    failed: row.outcome.as_deref().is_some_and(is_failure),
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

    /// When the account last signed an intent of this kind, whatever became of it.
    pub async fn last_attempt_ms(&self, side: Side, action: &str) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar("SELECT MAX(created_at_ms) FROM intents WHERE side = ? AND action = ?")
            .bind(side.as_str())
            .bind(action)
            .fetch_one(&self.pool)
            .await?)
    }

    /// What the journal holds about one intent, if it was ever journaled.
    pub async fn intent_fate(&self, intent_id: B256) -> Result<Option<IntentFate>> {
        Ok(sqlx::query_as(
            "SELECT status, outcome, created_at_ms, settled_at_ms FROM intents WHERE intent_id = ?",
        )
        .bind(intent_id.to_string())
        .fetch_optional(&self.pool)
        .await?)
    }

    // --- transfers ----------------------------------------------------------------

    /// Starts a transfer of whole USDC. Fails while another one is unfinished.
    pub async fn create_transfer(&self, from: Side, to: Side, amount_usdc: u64) -> Result<Transfer> {
        let now = now_ms();
        let created = sqlx::query(
            "INSERT INTO transfers (started_at_ms, from_side, to_side, amount_usdc, stage, attempts, attempt_at_ms)
             VALUES (?, ?, ?, ?, 'withdraw', 0, ?)",
        )
        .bind(now)
        .bind(from.as_str())
        .bind(to.as_str())
        .bind(i64::try_from(amount_usdc)?)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(Transfer::new(created.last_insert_rowid(), from, to, amount_usdc, now))
    }

    /// The transfer that is under way or stuck, if there is one.
    pub async fn active_transfer(&self) -> Result<Option<Transfer>> {
        let row: Option<TransferRow> = sqlx::query_as(
            "SELECT id, started_at_ms, from_side, to_side, amount_usdc, stage, attempts, attempt_at_ms,
                    core_baseline, sender_baseline, receiver_baseline, withdraw_intent, send_nonce,
                    checked_at_ms, note
             FROM transfers WHERE finished_at_ms IS NULL",
        )
        .fetch_optional(&self.pool)
        .await?;
        row.map(Transfer::try_from).transpose()
    }

    /// Writes the transfer's record: before the action it describes, never after.
    /// Fails if the transfer is no longer unfinished, which only happens when
    /// the operator has abandoned it.
    pub async fn save_transfer(&self, transfer: &Transfer) -> Result<()> {
        update_transfer(&self.pool, transfer).await
    }

    /// Records a withdrawal attempt together with its intent: both or neither,
    /// so there is never an attempt without the intent a restart has to track.
    pub async fn begin_withdrawal(&self, transfer: &Transfer, intent: &SignedIntent) -> Result<()> {
        let mut both = self.pool.begin().await?;
        update_transfer(&mut *both, transfer).await?;
        insert_intent(&mut *both, transfer.from, intent).await?;
        both.commit().await?;
        Ok(())
    }

    /// Keeps the latest error of a transfer's action with its record.
    pub async fn note_transfer(&self, id: i64, note: &str) -> Result<()> {
        sqlx::query("UPDATE transfers SET note = ? WHERE id = ? AND finished_at_ms IS NULL")
            .bind(note)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Ends the stuck transfer, once the operator has dealt with the funds.
    /// A transfer that is still under way is left alone.
    pub async fn abandon_stuck_transfer(&self) -> Result<Abandoned> {
        let Some(transfer) = self.active_transfer().await? else { return Ok(Abandoned::Nothing) };
        if transfer.stage != Stage::Stuck {
            return Ok(Abandoned::NotStuck(transfer));
        }
        let ended = sqlx::query(
            "UPDATE transfers SET stage = 'abandoned', finished_at_ms = ? WHERE id = ? AND stage = 'stuck'",
        )
        .bind(now_ms())
        .bind(transfer.id)
        .execute(&self.pool)
        .await?;
        if ended.rows_affected() == 0 {
            return Ok(Abandoned::Nothing);
        }
        let summary = format!(
            "operator abandoned transfer #{} ({} USDC from {} to {}); it was {}",
            transfer.id,
            transfer.amount_usdc,
            transfer.from,
            transfer.to,
            transfer.note.as_deref().unwrap_or("stuck"),
        );
        self.record_event("transfer", &summary, None).await?;
        Ok(Abandoned::Ended(transfer))
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

async fn insert_intent<'e>(
    executor: impl sqlx::SqliteExecutor<'e>,
    side: Side,
    intent: &SignedIntent,
) -> Result<()> {
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
    .execute(executor)
    .await?;
    Ok(())
}

async fn update_transfer<'e>(executor: impl sqlx::SqliteExecutor<'e>, transfer: &Transfer) -> Result<()> {
    let amount = |amount: Option<U256>| amount.map(|amount| amount.to_string());
    let written = sqlx::query(
        "UPDATE transfers
         SET stage = ?, finished_at_ms = ?, attempts = ?, attempt_at_ms = ?, core_baseline = ?,
             sender_baseline = ?, receiver_baseline = ?, withdraw_intent = ?, send_nonce = ?,
             checked_at_ms = ?, note = ?
         WHERE id = ? AND finished_at_ms IS NULL",
    )
    .bind(transfer.stage.as_str())
    .bind((transfer.stage == Stage::Done).then(now_ms))
    .bind(transfer.attempts)
    .bind(transfer.attempt_at_ms)
    .bind(amount(transfer.core_baseline))
    .bind(amount(transfer.sender_baseline))
    .bind(amount(transfer.receiver_baseline))
    .bind(transfer.withdraw_intent.map(|id| id.to_string()))
    .bind(transfer.send_nonce.map(i64::try_from).transpose()?)
    .bind(transfer.checked_at_ms)
    .bind(&transfer.note)
    .bind(transfer.id)
    .execute(executor)
    .await?;
    anyhow::ensure!(written.rows_affected() == 1, "transfer #{} is no longer under way", transfer.id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;
    use alloy::signers::local::PrivateKeySigner;

    use super::*;
    use crate::protocol::deployment::usd;
    use crate::protocol::intents::{OpenPosition, WithdrawToCore, sign_open_position, sign_withdraw_to_core};
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

    #[tokio::test(flavor = "multi_thread")]
    async fn two_processes_can_open_a_new_database_at_the_same_time() {
        // Separate pools stand in for the bot and a viewer started together.
        for _ in 0..20 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("papertrade.db");
            let (first, second, third) =
                tokio::join!(Store::open(&path), Store::open(&path), Store::open(&path));
            for store in [first, second, third] {
                assert_eq!(store.unwrap().mode().await.unwrap(), Mode::Run);
            }
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
            phase: crate::strategy::decide::Phase::Flat,
            decision: "wait".into(),
            summary: "flat; settling before the next open".into(),
            mid: Some("82754.5".into()),
            move_bps: None,
            round: None,
            trading: None,
            queue_total_usd: None,
            legs: Vec::new(),
            estimate: None,
            funds: None,
            transfer: None,
        };
        store.write_status(&snapshot).await.unwrap();
        snapshot.at_ms = 2;
        snapshot.summary = "open both legs".into();
        store.write_status(&snapshot).await.unwrap();
        assert_eq!(store.read_status().await.unwrap(), Some(snapshot));
    }

    #[tokio::test]
    async fn a_snapshot_left_by_another_version_is_not_a_status() {
        let store = Store::in_memory().await.unwrap();
        sqlx::query("INSERT INTO status (id, updated_at_ms, snapshot) VALUES (1, 1, '{\"at_ms\": 1}')")
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(store.read_status().await.unwrap(), None);
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
                    kind: Kind::Open,
                    intent_id: accepted.intent_id,
                    deadline: 1_800_000_120,
                    sent_at_ms: unresolved[0].sent_at_ms,
                    queue_revision: Some(U256::from(7)),
                    seen_queued: true,
                    failed: false,
                },
                Inflight {
                    side: Side::Short,
                    kind: Kind::Open,
                    intent_id: lost.intent_id,
                    deadline: 1_800_000_120,
                    sent_at_ms: unresolved[1].sent_at_ms,
                    queue_revision: None,
                    seen_queued: false,
                    failed: false,
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

    fn withdrawal(nonce: u64) -> SignedIntent {
        let wallet = PrivateKeySigner::random();
        let message = WithdrawToCore {
            user: wallet.address(),
            amount: U256::from(850_000_000u64),
            nonce: U256::from(nonce),
            deadline: 1_800_000_120,
        };
        sign_withdraw_to_core(&wallet, &message).unwrap()
    }

    #[tokio::test]
    async fn the_journal_tells_what_became_of_an_intent_and_when_each_kind_was_last_tried() {
        let store = Store::in_memory().await.unwrap();
        let (open, withdrawn) = (intent(1), withdrawal(2));
        assert_eq!(store.intent_fate(withdrawn.intent_id).await.unwrap(), None);
        assert_eq!(store.last_attempt_ms(Side::Long, "withdrawToCore").await.unwrap(), None);
        store.journal_intent(Side::Short, &open).await.unwrap();
        store.journal_intent(Side::Long, &withdrawn).await.unwrap();

        // The withdrawal is tracked like any other intent, under its own kind.
        let unresolved = store.unresolved_intents().await.unwrap();
        assert_eq!(
            unresolved.iter().map(|entry| entry.kind).collect::<Vec<_>>(),
            [Kind::Open, Kind::Withdraw]
        );
        let fate = store.intent_fate(withdrawn.intent_id).await.unwrap().unwrap();
        assert_eq!((fate.status.as_str(), fate.outcome, fate.settled_at_ms), ("signed", None, None));
        assert_eq!(
            store.last_attempt_ms(Side::Long, "withdrawToCore").await.unwrap(),
            Some(fate.created_at_ms)
        );
        assert_eq!(store.last_attempt_ms(Side::Short, "withdrawToCore").await.unwrap(), None);
        assert_eq!(store.last_attempt_ms(Side::Long, "openPosition").await.unwrap(), None);

        store.set_outcome(withdrawn.intent_id, "expired").await.unwrap();
        store.mark_settled(withdrawn.intent_id).await.unwrap();
        let fate = store.intent_fate(withdrawn.intent_id).await.unwrap().unwrap();
        assert_eq!((fate.status.as_str(), fate.outcome.as_deref()), ("settled", Some("expired")));
        assert!(fate.settled_at_ms.is_some_and(|at| at >= fate.created_at_ms));
    }

    #[tokio::test]
    async fn one_transfer_at_a_time_and_its_record_survives_as_written() {
        let store = Store::in_memory().await.unwrap();
        assert_eq!(store.active_transfer().await.unwrap(), None);
        let created = store.create_transfer(Side::Long, Side::Short, 850).await.unwrap();
        assert_eq!(created, Transfer::new(created.id, Side::Long, Side::Short, 850, created.started_at_ms));
        assert_eq!(store.active_transfer().await.unwrap(), Some(created.clone()));
        // While it is unfinished there is no second one, in either direction.
        assert!(store.create_transfer(Side::Short, Side::Long, 10).await.is_err());

        let written = Transfer {
            stage: Stage::Credit,
            attempts: 2,
            attempt_at_ms: 1_791_700_000_456,
            core_baseline: Some(usd(855)),
            sender_baseline: Some(usd(2_000)),
            receiver_baseline: Some(U256::from(300_123_456_789_012_345_678u128)),
            withdraw_intent: Some(B256::with_last_byte(9)),
            send_nonce: Some(1_791_700_000_123),
            checked_at_ms: Some(1_791_700_000_789),
            note: Some("HTTP 502".into()),
            ..created.clone()
        };
        store.save_transfer(&written).await.unwrap();
        assert_eq!(store.active_transfer().await.unwrap(), Some(written.clone()));
        store.note_transfer(written.id, "timed out").await.unwrap();
        let noted = Transfer { note: Some("timed out".into()), ..written.clone() };
        assert_eq!(store.active_transfer().await.unwrap(), Some(noted));

        // Done ends it, and the next one can start.
        store.save_transfer(&Transfer { stage: Stage::Done, ..written.clone() }).await.unwrap();
        assert_eq!(store.active_transfer().await.unwrap(), None);
        assert!(store.save_transfer(&written).await.unwrap_err().to_string().contains("no longer under way"));
        let next = store.create_transfer(Side::Short, Side::Long, 10).await.unwrap();
        assert_ne!(next.id, created.id);
    }

    #[tokio::test]
    async fn a_transfer_between_one_account_and_itself_cannot_be_recorded() {
        let store = Store::in_memory().await.unwrap();
        assert!(store.create_transfer(Side::Long, Side::Long, 850).await.is_err());
        assert!(store.create_transfer(Side::Long, Side::Short, 0).await.is_err());
    }

    #[tokio::test]
    async fn a_withdrawal_attempt_and_its_intent_are_written_together_or_not_at_all() {
        let store = Store::in_memory().await.unwrap();
        let created = store.create_transfer(Side::Long, Side::Short, 850).await.unwrap();
        let intent = withdrawal(1);
        let attempt = Transfer {
            attempts: 1,
            core_baseline: Some(usd(5)),
            sender_baseline: Some(usd(2_000)),
            withdraw_intent: Some(intent.intent_id),
            ..created.clone()
        };
        store.begin_withdrawal(&attempt, &intent).await.unwrap();
        assert_eq!(store.active_transfer().await.unwrap(), Some(attempt.clone()));
        let journaled = store.unresolved_intents().await.unwrap();
        assert_eq!(
            journaled.iter().map(|entry| (entry.side, entry.kind, entry.intent_id)).collect::<Vec<_>>(),
            [(Side::Long, Kind::Withdraw, intent.intent_id)]
        );

        // The same intent cannot be journaled twice, so this attempt is not recorded either.
        let again = Transfer { attempts: 2, ..attempt.clone() };
        assert!(store.begin_withdrawal(&again, &intent).await.is_err());
        assert_eq!(store.active_transfer().await.unwrap(), Some(attempt));
        assert_eq!(store.unresolved_intents().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn only_a_stuck_transfer_can_be_abandoned() {
        let store = Store::in_memory().await.unwrap();
        assert_eq!(store.abandon_stuck_transfer().await.unwrap(), Abandoned::Nothing);

        let moving = store.create_transfer(Side::Long, Side::Short, 850).await.unwrap();
        assert_eq!(store.abandon_stuck_transfer().await.unwrap(), Abandoned::NotStuck(moving.clone()));
        assert_eq!(store.active_transfer().await.unwrap(), Some(moving.clone()));

        let stuck = Transfer {
            stage: Stage::Stuck,
            note: Some("stuck in credit: the short account was not credited".into()),
            ..moving
        };
        store.save_transfer(&stuck).await.unwrap();
        // Stuck is not finished: it still stands in the way of the next transfer.
        assert_eq!(store.active_transfer().await.unwrap(), Some(stuck.clone()));
        assert!(store.create_transfer(Side::Long, Side::Short, 850).await.is_err());

        assert_eq!(store.abandon_stuck_transfer().await.unwrap(), Abandoned::Ended(stuck.clone()));
        assert_eq!(store.active_transfer().await.unwrap(), None);
        assert_eq!(store.abandon_stuck_transfer().await.unwrap(), Abandoned::Nothing);
        // The bot can no longer write to it, and the timeline says who ended it.
        assert!(store.save_transfer(&stuck).await.is_err());
        let events = store.recent_events(1).await.unwrap();
        assert_eq!(events[0].kind, "transfer");
        assert_eq!(
            events[0].summary,
            format!(
                "operator abandoned transfer #{} (850 USDC from long to short); it was stuck in credit: the \
                 short account was not credited",
                stuck.id
            )
        );
        store.create_transfer(Side::Long, Side::Short, 850).await.unwrap();
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

    /// A liquidation as the trade history might report it: the PnL at the price
    /// it fired at and a fee, neither of which is what the trader lost.
    fn liquidation(position_id: u64) -> Trade {
        let mut liquidated = trade(position_id, -480, 50_000);
        liquidated.kind = "Liquidated".into();
        liquidated.user_paid_fee_raw = Dec(usd(10));
        liquidated
    }

    #[tokio::test]
    async fn a_liquidation_is_booked_at_minus_its_whole_margin() {
        let store = Store::in_memory().await.unwrap();
        let (liquidated, closed) = (liquidation(1), trade(2, -100, 9_800));
        store.record_trades(WALLET, Side::Long, &[liquidated.clone(), closed.clone()]).await.unwrap();
        let totals = store.totals().await.unwrap();
        assert!((totals.net_pnl_usd - -600.0).abs() < 1e-9, "{totals:?}");
        // The view books each trade exactly as the bot's own rule does.
        let by_rule = crate::economics::net_result(&liquidated) + crate::economics::net_result(&closed);
        assert_eq!(crate::status::format_signed_amount(by_rule), "-600.00");
    }

    #[tokio::test]
    async fn a_ledger_from_before_the_liquidation_rule_keeps_its_rows_and_is_rebooked() {
        // A database as the first version left it: only the first migration applied.
        let pool = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!().run_to(1, &pool).await.unwrap();
        let old = Store { pool: pool.clone() };
        old.record_trades(WALLET, Side::Short, &[liquidation(1)]).await.unwrap();
        assert!((old.totals().await.unwrap().net_pnl_usd - -490.0).abs() < 1e-9);

        let upgraded = Store::connect(pool).await.unwrap();
        let totals = upgraded.totals().await.unwrap();
        assert_eq!(totals.trades, 1);
        assert!((totals.net_pnl_usd - -500.0).abs() < 1e-9, "{totals:?}");
        assert!((totals.paper_minted - 50_000.0).abs() < 1e-9, "{totals:?}");
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
