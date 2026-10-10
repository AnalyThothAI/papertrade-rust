use papertrade::protocol::deployment::usd;
use papertrade::strategy::decide::Side;
use papertrade::strategy::transfer::{Limits, Observed, Stage, Step, Transfer, next_step};

fn awaiting_credit() -> Transfer {
    Transfer {
        stage: Stage::Credit,
        receiver_baseline: Some(usd(100)),
        ..Transfer::new(1, Side::Long, Side::Short, 10, 1)
    }
}

fn receiver(now_ms: i64, balance_usdc: u64) -> Observed {
    Observed {
        now_ms,
        sender_core: None,
        sender_withdrawn: None,
        sender_indexed_at_ms: None,
        receiver_deposited: Some(usd(balance_usdc)),
        receiver_indexed_at_ms: Some(now_ms),
        withdrawal: None,
    }
}

#[test]
fn realized_profit_is_not_a_deposit() {
    // Equity rose to 109 from trading; cumulative deposits are still 100.
    let result = next_step(&awaiting_credit(), &receiver(2, 100), &Limits::default());
    assert!(!matches!(result, Step::Advance(record) if record.stage == Stage::Done));
}

#[test]
fn realized_loss_does_not_erase_a_deposit() {
    // Equity fell to 108 after the deposit; cumulative deposits are still 110.
    let result = next_step(&awaiting_credit(), &receiver(600_001, 110), &Limits::default());
    assert!(matches!(result, Step::Advance(record) if record.stage == Stage::Done));
}
