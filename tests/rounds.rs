//! The real bot against the simulated exchange: whole rounds, and what happens
//! when a leg, the relayer, the stream, the database or the process misbehaves.

mod support;

use std::time::Duration;

use papertrade::protocol::deployment::usd;
use papertrade::strategy::decide::Mode;
use support::{Behavior, ENTRY, Harness, TARGET, TTL, eventually, eventually_stored, steadily};

const SOON: Duration = Duration::from_secs(5);
const A_WHILE: Duration = Duration::from_millis(600);

/// Time between the wallet's first open and its first close reaching the exchange.
fn exposure(harness: &Harness, wallet: alloy::primitives::Address) -> Duration {
    let received = harness.sim.received();
    let at = |action: &str| received.iter().find(|r| r.wallet == wallet && r.action == action).unwrap().at;
    at("closePositions") - at("openPosition")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_round_opens_both_legs_closes_them_at_the_target_and_books_the_result() {
    let h = Harness::start().await;
    h.hedged().await;
    assert_eq!(h.opens(), 2);
    // The long account holds a long, the short account a short.
    assert!(h.sim.positions(h.long)[0].1 && !h.sim.positions(h.short)[0].1);

    h.sim.set_mid(ENTRY + TARGET - 100);
    steadily("nothing closes inside the target", A_WHILE, || h.closes() == 0).await;

    h.sim.set_mid(ENTRY + TARGET);
    h.flat().await;
    // Price rose, so the short lost; it is closed first.
    let closes: Vec<_> = h.sim.received().into_iter().filter(|r| r.action == "closePositions").collect();
    assert_eq!(closes.iter().map(|r| r.wallet).collect::<Vec<_>>(), [h.short, h.long]);
    // 10,000 notional, 1% move: the loser pays 100, the winner keeps 88 less a 2% fee.
    assert_eq!(h.sim.balance(h.short), usd(1_900));
    assert_eq!(h.sim.balance(h.long), usd(2_086) + usd(24) / alloy::primitives::U256::from(100));

    // The ledger and the journal are written beside the loop; give them a moment.
    eventually_stored("both settled positions are in the ledger", SOON, || async {
        h.store.totals().await.unwrap().trades == 2
    })
    .await;
    let totals = h.store.totals().await.unwrap();
    assert!((totals.net_pnl_usd - -13.76).abs() < 1e-6, "{totals:?}");
    assert!((totals.paper_minted - 9_800.0).abs() < 1e-6, "{totals:?}");
    eventually_stored("every intent of the round is settled", SOON, || async {
        h.store.unresolved_intents().await.unwrap().is_empty()
    })
    .await;
    let journal = h.store.recent_intents(10).await.unwrap();
    assert_eq!(journal.len(), 4);
    assert!(
        journal.iter().all(|row| row.status == "settled" && row.outcome.as_deref() == Some("succeeded")),
        "{journal:?}"
    );

    eventually("the next round opens after the cooldown", TTL + SOON, || h.opens() == 4).await;
    h.hedged().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_leg_the_relayer_refuses_leaves_the_other_leg_open_only_for_a_moment() {
    let mut h = Harness::prepare().await;
    h.sim.script(h.short, "openPosition", &[Behavior::Reject("admission_rejected", "openInterest")]);
    h.start_bot();
    eventually("the lone long leg is closed", SOON, || {
        h.sim.count(h.long, "closePositions") == 1 && h.sim.positions(h.long).is_empty()
    })
    .await;
    assert!(exposure(&h, h.long) < Duration::from_millis(400), "{:?}", exposure(&h, h.long));
    assert_eq!(h.sim.count(h.short, "closePositions"), 0);
    steadily("no new round before the cooldown is over", A_WHILE, || h.opens() == 2).await;
    eventually("the next round is attempted", TTL + SOON, || h.opens() == 4).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_leg_that_fails_on_execution_is_noticed_from_the_reported_outcome() {
    let mut h = Harness::prepare().await;
    h.sim.script(h.short, "openPosition", &[Behavior::AcceptThenFail]);
    h.start_bot();
    eventually("the lone long leg is closed", SOON, || {
        h.sim.count(h.long, "closePositions") == 1 && h.sim.positions(h.long).is_empty()
    })
    .await;
    // The failure is reported, so there is no need to wait out the grace period.
    assert!(exposure(&h, h.long) < Duration::from_millis(400), "{:?}", exposure(&h, h.long));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unanswered_open_does_not_keep_the_other_leg_unhedged_until_its_deadline() {
    let mut h = Harness::prepare().await;
    h.sim.script(h.short, "openPosition", &[Behavior::Drop]);
    h.start_bot();
    eventually("the lone long leg is closed", SOON, || {
        h.sim.count(h.long, "closePositions") == 1 && h.sim.positions(h.long).is_empty()
    })
    .await;
    // Held for the grace period in case the short is merely slow, but nowhere near
    // the two seconds the unanswered intent could still take to execute.
    let exposed = exposure(&h, h.long);
    assert!(exposed >= Duration::from_millis(350) && exposed < Duration::from_millis(1_200), "{exposed:?}");
    steadily("no second open while the unanswered one could still execute", A_WHILE, || h.opens() == 2).await;
    eventually("a new round once it has expired", TTL + SOON, || h.opens() == 4).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_leg_stuck_in_the_relayer_queue_is_waited_for_until_it_expires() {
    let mut h = Harness::prepare().await;
    h.sim.script(h.short, "openPosition", &[Behavior::Hold]);
    h.start_bot();
    eventually("the long is open and the short is queued", SOON, || {
        h.sim.positions(h.long).len() == 1 && h.sim.queued(h.short) == 1
    })
    .await;
    // The short is visibly on its way, so the long is not closed after the grace period.
    steadily("the long is kept while the short is queued", Duration::from_millis(900), || h.closes() == 0)
        .await;
    // It never executes. Once it has expired the long is alone for good.
    eventually("the lone long leg is closed", TTL + SOON, || {
        h.sim.count(h.long, "closePositions") == 1 && h.sim.positions(h.long).is_empty()
    })
    .await;
    assert!(h.sim.positions(h.short).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unanswered_open_blocks_new_opens_until_the_chain_clock_passes_its_deadline() {
    let mut h = Harness::prepare().await;
    // The indexer is three seconds behind: by its clock the unanswered opens stay
    // executable well after their two seconds have passed on ours.
    h.sim.set_clock_lag(3);
    h.sim.script(h.long, "openPosition", &[Behavior::Drop]);
    h.sim.script(h.short, "openPosition", &[Behavior::Drop]);
    h.start_bot();
    eventually("both opens are sent", SOON, || h.opens() == 2).await;
    steadily("no second round by our own clock alone", TTL + Duration::from_millis(1_500), || h.opens() == 2)
        .await;
    eventually("a new round once the chain clock has passed the deadline", SOON, || h.opens() == 4).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unanswered_open_that_did_execute_becomes_a_normal_hedged_round() {
    let mut h = Harness::prepare().await;
    h.sim.script(h.short, "openPosition", &[Behavior::AcceptSilently]);
    h.start_bot();
    h.hedged().await;
    steadily("the round is left alone", TTL + A_WHILE, || h.closes() == 0 && h.opens() == 2).await;
    h.sim.set_mid(ENTRY - TARGET);
    h.flat().await;
    assert_eq!(h.closes(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_position_that_is_indexed_late_is_not_mistaken_for_a_failed_leg() {
    let mut h = Harness::prepare().await;
    // The short's open leaves the relayer's queue a while before its position
    // shows up. For that while the long looks like the only leg.
    h.sim.set_position_lag(h.short, Duration::from_millis(250));
    h.start_bot();
    h.hedged().await;
    steadily("neither leg is closed as unhedged", A_WHILE, || h.closes() == 0).await;
    assert_eq!(h.opens(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unanswered_close_is_sent_again() {
    let h = Harness::start().await;
    h.hedged().await;
    // Price rises: the short is closed first, then the long, whose first close is lost.
    h.sim.script(h.long, "closePositions", &[Behavior::Drop]);
    h.sim.set_mid(ENTRY + TARGET);
    h.flat().await;
    assert_eq!(h.sim.count(h.short, "closePositions"), 1);
    assert!(h.sim.count(h.long, "closePositions") >= 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_close_is_retried_until_it_goes_through() {
    let h = Harness::start().await;
    h.hedged().await;
    let refused = Behavior::Reject("rate_limited", "");
    h.sim.script(h.short, "closePositions", &[refused, refused]);
    h.sim.set_mid(ENTRY + TARGET);
    h.flat().await;
    assert_eq!(h.sim.count(h.short, "closePositions"), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_close_that_keeps_being_refused_is_not_hammered() {
    let h = Harness::start().await;
    h.hedged().await;
    h.sim.script(h.short, "closePositions", &[Behavior::Reject("rate_limited", ""); 100]);
    h.sim.set_mid(ENTRY + TARGET);
    tokio::time::sleep(Duration::from_millis(900)).await;
    // One attempt per retry interval (150 ms here), not one per tick (15 ms).
    let attempts = h.sim.count(h.short, "closePositions");
    assert!((3..=8).contains(&attempts), "{attempts} close attempts in 900 ms");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_liquidated_leg_gets_the_survivor_closed() {
    let h = Harness::start().await;
    h.hedged().await;
    h.sim.liquidate(h.long);
    h.flat().await;
    assert_eq!((h.sim.count(h.long, "closePositions"), h.sim.count(h.short, "closePositions")), (0, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_remembers_unanswered_opens_and_does_not_open_twice() {
    let mut h = Harness::prepare().await;
    h.sim.script(h.long, "openPosition", &[Behavior::Drop]);
    h.sim.script(h.short, "openPosition", &[Behavior::Drop]);
    h.start_bot();
    eventually("both opens are sent", SOON, || h.opens() == 2).await;
    h.stop_bot().await.unwrap();
    assert_eq!(h.store.unresolved_intents().await.unwrap().len(), 2);

    // A fresh process sees two flat accounts. Only the journal knows about the opens.
    h.start_bot();
    steadily("no new open while the old ones could still execute", Duration::from_millis(1_200), || {
        h.opens() == 2
    })
    .await;
    eventually("a new round once they have expired", TTL + SOON, || h.opens() == 4).await;
    h.hedged().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_picks_up_a_round_that_is_already_open() {
    let mut h = Harness::start().await;
    h.hedged().await;
    h.stop_bot().await.unwrap();
    h.sim.set_mid(ENTRY - TARGET);
    h.start_bot();
    h.flat().await;
    assert_eq!((h.opens(), h.closes()), (2, 2));
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_opens_while_the_relayer_refuses_closes() {
    let mut h = Harness::prepare().await;
    h.sim.pause("closePositions");
    h.start_bot();
    steadily("no open that could not be closed", A_WHILE, || h.opens() == 0).await;
    h.sim.unpause("closePositions");
    h.hedged().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_waits_out_a_relayer_pause_without_hammering_it() {
    let h = Harness::start().await;
    h.hedged().await;
    h.sim.pause("closePositions");
    tokio::time::sleep(Duration::from_millis(250)).await;
    h.sim.set_mid(ENTRY + TARGET);
    steadily("no close is sent into the pause", A_WHILE, || h.closes() == 0).await;
    h.sim.unpause("closePositions");
    h.flat().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_bot_does_not_act_blind_while_the_account_streams_are_down() {
    let h = Harness::start().await;
    h.hedged().await;
    h.sim.set_stream_up(false);
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.sim.set_mid(ENTRY + TARGET);
    steadily("nothing is sent without account state", A_WHILE, || h.closes() == 0).await;
    h.sim.set_stream_up(true);
    h.flat().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_price_the_round_is_held_but_a_lone_leg_is_still_closed() {
    let h = Harness::start().await;
    h.hedged().await;
    h.sim.silence_quotes();
    steadily("hedged and waiting for a price", A_WHILE, || h.closes() == 0).await;
    h.sim.liquidate(h.short);
    h.flat().await;
    assert_eq!(h.sim.count(h.long, "closePositions"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn flatten_closes_the_round_now_and_resume_starts_the_next() {
    let h = Harness::start().await;
    h.hedged().await;
    h.store.set_mode(Mode::Flatten).await.unwrap();
    h.flat().await;
    assert_eq!(h.closes(), 2);
    steadily("flatten opens nothing", TTL + A_WHILE, || h.opens() == 2).await;
    h.store.set_mode(Mode::Run).await.unwrap();
    h.hedged().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pause_finishes_the_open_round_and_starts_no_new_one() {
    let h = Harness::start().await;
    h.hedged().await;
    h.store.set_mode(Mode::Pause).await.unwrap();
    steadily("the open round is kept", A_WHILE, || h.closes() == 0).await;
    h.sim.set_mid(ENTRY + TARGET);
    h.flat().await;
    steadily("no new round while paused", TTL + A_WHILE, || h.opens() == 2).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_database_that_stops_working_does_not_stop_a_close() {
    let h = Harness::start().await;
    h.hedged().await;
    h.store.close().await;
    h.sim.set_mid(ENTRY + TARGET);
    h.flat().await;
    assert_eq!(h.closes(), 2);
    // But with no journal to write to, no new round is opened.
    steadily("no open without a journal", TTL + A_WHILE, || h.opens() == 2).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_bot_stops_when_one_of_its_feeds_dies() {
    let h = Harness::prepare().await;
    let ended = tokio::time::timeout(SOON, h.run_with_a_dead_feed()).await.expect("the bot should stop");
    let error = ended.unwrap_err().to_string();
    assert!(error.contains("a background task stopped"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_whose_feed_dies_starts_itself_again_and_carries_on() {
    let mut h = Harness::prepare().await;
    // The first two runs lose their price feed straight away.
    h.start_self_healing_bot(2);
    h.hedged().await;
    h.sim.set_mid(ENTRY + TARGET);
    h.flat().await;
    assert_eq!((h.opens(), h.closes()), (2, 2));
    // Stopping it is a clean exit, not another restart.
    h.stop_bot().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_that_cannot_start_the_first_time_says_so_instead_of_retrying() {
    let mut h = Harness::prepare().await;
    h.forget_session_keys();
    h.start_self_healing_bot(0);
    let error = h.stop_bot().await.unwrap_err().to_string();
    assert!(error.contains("no session key"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_contract_call_that_reverts_is_an_error() {
    let h = Harness::prepare().await;
    let reply =
        h.sim.chain().call(h.long, alloy::primitives::Bytes::from_static(&[0x12, 0x34, 0x56, 0x78])).await;
    assert!(reply.unwrap_err().to_string().contains("execution reverted"));
}
