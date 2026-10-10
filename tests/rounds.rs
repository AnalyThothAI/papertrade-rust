//! The real bot against the simulated exchange: whole rounds, and what happens
//! when a leg, the relayer, the stream, the database or the process misbehaves.

mod support;

use std::sync::Arc;
use std::time::Duration;

use alloy::signers::local::PrivateKeySigner;
use papertrade::keystore::{self, WalletKey};
use papertrade::protocol::deployment::usd;
use papertrade::session_keys;
use papertrade::status::{EstimateSnapshot, FeesSnapshot, RoundEstimate, RoundSnapshot};
use papertrade::strategy::decide::{Mode, Phase};
use support::{Behavior, ENTRY, Harness, TARGET, TTL, eventually, eventually_stored, status_when, steadily};

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

    // The liquidated long is booked at minus its whole margin, although its record
    // carries the PnL at the price it was liquidated at (here none: the mid never
    // moved). The short was closed where it was opened.
    let booked = " liquidated: net -500.00 USD, 50000.00 PAPER";
    eventually_stored("the liquidation is on the timeline", SOON, || async {
        let timeline = h.store.recent_events(50).await.unwrap();
        timeline.iter().any(|event| {
            event.kind == "trade"
                && event.summary.starts_with("long position ")
                && event.summary.ends_with(booked)
        })
    })
    .await;
    eventually_stored("both settled positions are in the ledger", SOON, || async {
        h.store.totals().await.unwrap().trades == 2
    })
    .await;
    let totals = h.store.totals().await.unwrap();
    assert!((totals.net_pnl_usd - -500.0).abs() < 1e-6, "{totals:?}");
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
    h.config.coin = "NOPE".into();
    h.start_self_healing_bot(0);
    let error = h.stop_bot().await.unwrap_err().to_string();
    assert!(error.contains("NOPE is not listed on Papertrade"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn keys_unlocked_once_serve_every_restart_with_nothing_left_to_read() {
    const PASSWORD: &str = "correct horse battery staple";
    let mut h = Harness::prepare().await;
    // The way `run` comes by its keys: files made by `keys init`, unlocked with
    // the password once, before the first start.
    let keys_dir = h.config.keys_dir.clone();
    for wallet in [h.long, h.short] {
        session_keys::create(&keys_dir, wallet, PASSWORD).unwrap();
    }
    h.keys = Arc::new(keystore::unlock(&h.config, PASSWORD).unwrap());
    for account in [&h.keys.long, &h.keys.short] {
        h.sim.set_session_key(account.owner, Some(account.session.address()));
    }
    // From here on there is no file to read again and nobody to ask.
    std::fs::remove_dir_all(&keys_dir).unwrap();

    // Two runs lose their feed and are started again in the same process.
    h.start_self_healing_bot(2);
    h.hedged().await;
    h.sim.set_mid(ENTRY + TARGET);
    h.flat().await;
    // The exchange takes an intent from a registered session key only, so every
    // one of these was signed with the keys unlocked at the start.
    assert_eq!((h.opens(), h.closes()), (2, 2));
    let status = h.store.read_status().await.unwrap().unwrap();
    assert_eq!(status.legs[0].session_key, format!("{:#x}", h.keys.long.session.address()));
    assert_eq!(status.legs[1].session_key, format!("{:#x}", h.keys.short.session.address()));
    assert!(!keys_dir.exists(), "something wrote to the keys directory");
    h.stop_bot().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_handed_the_keys_of_other_accounts_does_not_start() {
    let mut h = Harness::prepare().await;
    h.keys = Arc::new(support::session_keys(h.short, h.long));
    h.start_self_healing_bot(0);
    let error = h.stop_bot().await.unwrap_err().to_string();
    assert!(error.contains("but its keys were unlocked for"), "{error}");
    assert!(h.sim.received().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wallet_key_that_did_not_load_is_reported_and_the_round_goes_on() {
    let mut h = Harness::prepare().await;
    // The wallet automation is on. The long wallet's key did not load; the short one's did.
    let reason = "no encrypted wallet key at ./secrets/0x01.wallet.json; `papertrade keys init` sets it up";
    let keys = Arc::get_mut(&mut h.keys).unwrap();
    keys.long.wallet = WalletKey::Unavailable(reason.into());
    keys.short.wallet = WalletKey::Loaded(PrivateKeySigner::random());
    h.start_bot();

    h.hedged().await;
    let status = status_when(&h, "the open round", |status| status.phase == Phase::Hedged).await;
    let wallet_keys: Vec<_> =
        status.legs.iter().map(|leg| (leg.has_wallet_key, leg.wallet_key_problem.as_deref())).collect();
    assert_eq!(wallet_keys, [(false, Some(reason)), (true, None)]);
    // The session keys still manage the round and take it out.
    h.sim.set_mid(ENTRY + TARGET);
    h.flat().await;
    assert_eq!((h.opens(), h.closes()), (2, 2));
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_wallet_automation_the_status_claims_no_wallet_key() {
    let h = Harness::start().await;
    h.hedged().await;
    let status = status_when(&h, "the open round", |status| status.phase == Phase::Hedged).await;
    assert!(status.legs.iter().all(|leg| !leg.has_wallet_key && leg.wallet_key_problem.is_none()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dry_run_decides_and_shows_it_but_signs_and_sends_nothing() {
    let mut h = Harness::prepare().await;
    h.config.dry_run = true;
    h.start_bot();
    let status = status_when(&h, "the decision to open", |status| status.decision == "open").await;
    assert!(status.dry_run);
    steadily("nothing reaches the relayer", A_WHILE, || h.sim.received().is_empty()).await;
    // Not signed either: an intent is journaled the moment it is signed.
    assert!(h.store.recent_intents(10).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_key_its_wallet_has_not_registered_waits_instead_of_trading() {
    let mut h = Harness::prepare().await;
    // The key exists on this machine and is unlocked. The exchange has never heard of it.
    h.sim.set_session_key(h.short, None);
    h.start_bot();
    let waiting = status_when(&h, "the wait for a registration", |status| {
        status.summary == "flat; the short account's session key is not registered or expires soon"
    })
    .await;
    assert_eq!(waiting.decision, "wait");
    let expiry = |side: usize| waiting.legs[side].account.as_ref().unwrap().session_key_expiry;
    assert!(expiry(0).is_some() && expiry(1).is_none(), "{:?}", waiting.legs);
    steadily("nothing is sent on a key that is not registered", A_WHILE, || h.sim.received().is_empty())
        .await;

    // Once the wallet has registered that same key, the round opens.
    h.sim.set_session_key(h.short, Some(h.keys.short.session.address()));
    h.hedged().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_status_carries_an_estimate_for_the_next_round_and_for_the_open_one() {
    let figures = |[loser, winner, net, paper, cost]: [&str; 5]| RoundEstimate {
        loser_usd: loser.into(),
        winner_usd: winner.into(),
        net_usd: net.into(),
        paper: paper.into(),
        usd_per_paper: Some(cost.into()),
    };
    // What the simulator serves is what production served on 2026-10-10: BTC's
    // impact parameters, 2% fees, 100 PAPER per USD of mint basis, an empty queue.
    // For 10,000 USD a leg closed 1% from an entry of 80,000.0: the loser pays 100
    // and mints on 98 of it; the winner's 99.80 (the deadband takes 0.20) keeps
    // 88.3% through the haircut and then pays the 2% win fee.
    let at_target = figures(["100.00", "86.38", "-13.61", "9800.00", "0.001389"]);
    let fees = Some(FeesSnapshot {
        win_percent: "2.00".into(),
        loss_percent: "2.00".into(),
        liquidation_percent: "2.00".into(),
    });

    let mut h = Harness::prepare().await;
    // Flat for as long as the relayer refuses opens.
    h.sim.pause("openPosition");
    h.start_bot();
    let flat = status_when(&h, "an estimate while flat", |status| {
        status.phase == Phase::Flat && status.estimate.is_some()
    })
    .await;
    assert_eq!(
        flat.estimate,
        Some(EstimateSnapshot {
            mint_rate: "100.00".into(),
            at_target: at_target.clone(),
            now: None,
            fees: fees.clone(),
        })
    );
    assert_eq!(flat.round, None);

    // While winners are queued a losing close pays no loss fee, so the same loss
    // mints on all 100 and each PAPER comes out cheaper.
    h.sim.set_queue_total(usd(250));
    let queued = status_when(&h, "the estimate for a queued payout", |status| {
        status.estimate.as_ref().is_some_and(|estimate| estimate.at_target.paper == "10000.00")
    })
    .await;
    assert_eq!(
        queued.estimate.unwrap().at_target,
        figures(["100.00", "86.38", "-13.61", "10000.00", "0.001361"])
    );
    h.sim.set_queue_total(usd(0));

    h.sim.unpause("openPosition");
    h.hedged().await;
    // Half way to the target: the loser is down 50 and would mint on 49; a smaller
    // move keeps less of the gain (86.7% of 49.80), so each PAPER costs more.
    h.sim.set_mid(ENTRY + TARGET / 2);
    let hedged = status_when(&h, "the round valued at the new mid", |status| {
        status.phase == Phase::Hedged && status.mid.as_deref() == Some("80400.0") && status.estimate.is_some()
    })
    .await;
    assert_eq!(
        hedged.estimate,
        Some(EstimateSnapshot {
            mint_rate: "100.00".into(),
            at_target,
            now: Some(figures(["50.00", "42.31", "-7.68", "4900.00", "0.001568"])),
            fees,
        })
    );
    // The status also says where this round closes and since when it is held.
    assert_eq!(
        hedged.round,
        Some(RoundSnapshot {
            entry: "80000.0".into(),
            close_below: "79200.0".into(),
            close_above: "80800.0".into(),
        })
    );
    assert!(hedged.held_since_ms().is_some());
    // And the reported trigger is where it really closes: not one price unit earlier.
    h.sim.set_mid(ENTRY + TARGET - 1);
    steadily("nothing closes just inside the reported trigger", A_WHILE, || h.closes() == 0).await;
    h.sim.set_mid(ENTRY + TARGET);
    h.flat().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn fee_rates_that_cannot_be_read_cost_the_estimate_and_nothing_else() {
    let mut h = Harness::prepare().await;
    h.sim.set_fees_readable(false);
    h.start_bot();
    // The round opens all the same.
    h.hedged().await;
    let blind = status_when(&h, "the open round", |status| status.phase == Phase::Hedged).await;
    assert_eq!(blind.estimate, None);

    h.sim.set_fees_readable(true);
    status_when(&h, "an estimate once the rates are read", |status| status.estimate.is_some()).await;
    // Rates that can no longer be confirmed are not kept on show.
    h.sim.set_fees_readable(false);
    status_when(&h, "no estimate once the rates are stale", |status| status.estimate.is_none()).await;

    h.sim.set_mid(ENTRY + TARGET);
    h.flat().await;
    assert_eq!((h.opens(), h.closes()), (2, 2));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_contract_call_that_reverts_is_an_error() {
    let h = Harness::prepare().await;
    let reply =
        h.sim.chain().call(h.long, alloy::primitives::Bytes::from_static(&[0x12, 0x34, 0x56, 0x78])).await;
    assert!(reply.unwrap_err().to_string().contains("execution reverted"));
}
