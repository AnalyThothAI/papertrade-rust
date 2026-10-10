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
    let error = h.bot_ended().await.unwrap_err().to_string();
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
    let error = h.bot_ended().await.unwrap_err().to_string();
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

/// Funds moving between the two accounts, and the wallet keys that make it possible.
mod funding {
    use std::time::Duration;

    use std::sync::Arc;

    use alloy::primitives::{Address, U256};
    use alloy::signers::local::PrivateKeySigner;
    use papertrade::keystore::{AccountKeys, UnlockedKeys, WalletKey};
    use papertrade::protocol::deployment::usd;
    use papertrade::status::Snapshot;
    use papertrade::store::Abandoned;
    use papertrade::strategy::decide::{Mode, Phase, Side};
    use papertrade::strategy::transfer::Stage;

    use super::support::{
        Behavior, ENTRY, Harness, Received, SendBehavior, Setup, Sweep, TARGET, TTL, eventually,
        eventually_stored, steadily,
    };
    use super::{A_WHILE, SOON};

    /// The long account holds 2,000 and the short 300. Half the difference
    /// levels them at 1,150 each; the round that opens then takes 500 from both.
    const MOVED: u64 = 850;
    const LEVELLED: u64 = 1_150;
    const AFTER_OPENING: u64 = 650;
    /// The bot counts in whole milliseconds, and the simulator sees a request a
    /// moment after the bot looked at its clock.
    const SLACK: Duration = Duration::from_millis(50);
    /// The settle time of the scenarios in which an attempt is repeated.
    const SETTLE: Duration = Duration::from_millis(600);

    /// The wallet automation is on and both wallets' own keys were unlocked.
    /// Rebalancing is still off.
    fn automated() -> Setup {
        Setup { wallet_keys: true, ..Setup::default() }
    }

    /// Rebalancing is switched on as well.
    fn rebalancing() -> Setup {
        Setup { rebalance: true, ..automated() }
    }

    fn lopsided() -> Setup {
        Setup { long_usd: 2_000, short_usd: 300, ..rebalancing() }
    }

    /// A harness whose bot gives an attempt only a short settle time.
    async fn repeating(setup: Setup) -> Harness {
        let mut h = Harness::prepare_with(setup).await;
        h.timing.funding.transfer.settle = SETTLE;
        h
    }

    fn withdrawals(h: &Harness) -> Vec<Received> {
        h.sim.received().into_iter().filter(|r| r.action == "withdrawToCore").collect()
    }

    async fn transfer_stage(h: &Harness) -> Option<Stage> {
        h.store.active_transfer().await.unwrap().map(|transfer| transfer.stage)
    }

    async fn transfer_finished(h: &Harness) {
        eventually_stored("the transfer finishes", SOON, || async {
            h.store.active_transfer().await.unwrap().is_none()
        })
        .await;
    }

    /// What the long leg is paid back when a round closes at the target in its
    /// favour: its 500 margin, and 88% of the 100 it won less the 2% fee.
    fn won() -> U256 {
        usd(586) + usd(24) / U256::from(100)
    }

    async fn in_stage(h: &Harness, stage: Stage) {
        in_stage_within(h, stage, SOON).await;
    }

    async fn in_stage_within(h: &Harness, stage: Stage, within: Duration) {
        let what = format!("the transfer is in {stage}");
        eventually_stored(&what, within, || async { transfer_stage(h).await == Some(stage) }).await;
    }

    /// The transfer's steps on the timeline, oldest first.
    async fn timeline(h: &Harness) -> Vec<String> {
        let mut events = h.store.recent_events(200).await.unwrap();
        events.reverse();
        events.into_iter().filter(|event| event.kind == "transfer").map(|event| event.summary).collect()
    }

    async fn status_when(h: &Harness, what: &str, wanted: impl Fn(&Snapshot) -> bool) -> Snapshot {
        let started = std::time::Instant::now();
        loop {
            if let Some(status) = h.store.read_status().await.unwrap().filter(&wanted) {
                return status;
            }
            assert!(started.elapsed() < SOON, "timed out after {SOON:?} waiting for: {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The first status that shows a transfer the bot would make, held back by this.
    async fn planned_but_held_by(h: &Harness, reason: &str) -> Snapshot {
        let what = format!("a transfer planned and not started: {reason}");
        status_when(h, &what, |status| {
            let planned = status.funds.as_ref().and_then(|funds| funds.planned.as_ref());
            planned.is_some_and(|planned| planned.held_by.as_deref() == Some(reason))
        })
        .await
    }

    /// The round opened on level accounts, and nothing is left anywhere on the way.
    fn assert_levelled_and_trading(h: &Harness) {
        assert_eq!((h.sim.balance(h.long), h.sim.balance(h.short)), (usd(AFTER_OPENING), usd(AFTER_OPENING)));
        for wallet in [h.long, h.short] {
            assert_eq!(h.sim.core_usdc(wallet), U256::ZERO);
            assert_eq!(h.sim.core_usdc(h.sim.deposit_address(wallet)), U256::ZERO);
        }
        assert_eq!(h.opens(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lopsided_accounts_are_levelled_through_hypercore_and_then_trade() {
        let mut h = Harness::prepare_with(lopsided()).await;
        // The relayer credits a deposit only when asked to look, and when it answers
        // it names an address that is not the short account's deposit address.
        let decoy = Address::repeat_byte(0xde);
        h.sim.set_sweep(Sweep::OnCheck);
        h.sim.answer_deposit_checks_with(decoy);
        h.start_bot();
        h.hedged().await;

        // One withdrawal, by the richer account. One send, to the poorer account's
        // deposit address as the chain derives it.
        assert_eq!((h.sim.count(h.long, "withdrawToCore"), h.sim.count(h.short, "withdrawToCore")), (1, 0));
        let sends = h.sim.sends();
        assert_eq!(sends.len(), 1);
        assert_eq!(
            (sends[0].from, sends[0].destination, sends[0].amount_usdc),
            (h.long, h.sim.deposit_address(h.short), MOVED)
        );
        assert!(h.sim.deposit_checks(h.short) >= 1);
        assert_eq!(h.sim.deposit_checks(h.long), 0);
        assert_eq!(h.sim.core_usdc(decoy), U256::ZERO);
        assert_levelled_and_trading(&h);
        // The sender's HyperCore balance was followed while the transfer needed it, and no longer.
        let reads = h.sim.hypercore_reads();
        assert!(reads > 0);
        steadily("Hyperliquid is left alone once the transfer is done", A_WHILE, || {
            h.sim.hypercore_reads() == reads
        })
        .await;

        let address = h.sim.deposit_address(h.short);
        assert_eq!(
            timeline(&h).await[..5],
            [
                "transfer #1 planned: 850 USDC from the long account to the short account".to_owned(),
                "transfer #1 withdrawing 850 USDC from the long account to its wallet's HyperCore account \
                 (attempt 1)"
                    .to_owned(),
                "transfer #1 the withdrawal arrived: 850 USDC are in the long wallet's HyperCore account".to_owned(),
                format!(
                    "transfer #1 sending 850 USDC on Hyperliquid from the long wallet to the short account's \
                     deposit address {address} (attempt 1)"
                ),
                "transfer #1 850 USDC left the long wallet on Hyperliquid; waiting for the short account's credit"
                    .to_owned(),
            ]
        );
        let steps = timeline(&h).await;
        assert_eq!(steps.len(), 6, "{steps:?}");
        assert!(steps[5].starts_with("transfer #1 done: the short account was credited"), "{steps:?}");
        assert_eq!(h.store.active_transfer().await.unwrap(), None);

        // The withdrawal went through the journal like every other intent.
        eventually_stored("the withdrawal is settled in the journal", TTL + SOON, || async {
            h.store.unresolved_intents().await.unwrap().is_empty()
        })
        .await;
        let journal = h.store.recent_intents(10).await.unwrap();
        let withdrawal = journal.iter().find(|row| row.action == "withdrawToCore").unwrap();
        assert_eq!((withdrawal.side.as_str(), withdrawal.status.as_str()), ("long", "settled"));
        // The relayer's word on how it ended is on record unless the stream was
        // between two connections when it was sent. The transfer never needed it.
        assert!(matches!(withdrawal.outcome.as_deref(), None | Some("succeeded")), "{withdrawal:?}");

        let status =
            status_when(&h, "the status shows the round", |status| status.phase == Phase::Hedged).await;
        assert_eq!(status.transfer, None);
        let funds = status.funds.unwrap();
        assert_eq!((funds.verdict.as_str(), funds.can_rebalance, funds.planned), ("enough", true, None));
        assert_eq!((funds.total_usd.as_str(), funds.difference_usd.as_str()), ("2300.00", "0.00"));
        assert!(status.legs.iter().all(|leg| leg.has_wallet_key));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_status_shows_a_transfer_while_it_is_under_way() {
        let mut h = Harness::prepare_with(lopsided()).await;
        h.sim.set_sweep(Sweep::Never);
        h.start_bot();
        in_stage(&h, Stage::Credit).await;
        let status = status_when(&h, "the status shows the transfer", |status| {
            status
                .transfer
                .as_ref()
                .is_some_and(|transfer| transfer.stage == "credit" && transfer.note.is_some())
        })
        .await;
        // The transfer is no reason to wait. The short account's balance is.
        assert_eq!(status.summary, "flat; the short account's balance is below the margin");
        let transfer = status.transfer.unwrap();
        assert_eq!((transfer.from.as_str(), transfer.to.as_str()), ("long", "short"));
        assert_eq!(transfer.amount_usd, "850.00");
        assert_eq!(transfer.note.as_deref(), Some("waiting for the relayer to credit the short account"));
        // What has left one account and not reached the other is missing from the total.
        let funds = status.funds.unwrap();
        assert_eq!((funds.total_usd.as_str(), funds.verdict.as_str()), ("1450.00", "rebalance"));
        // One transfer at a time: the one under way is shown, no second one is planned.
        assert_eq!(funds.planned, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_account_that_can_open_but_could_not_absorb_a_loss_opens_and_is_topped_up_all_the_same() {
        // 550 covers the 500 margin, but not the 100 a losing round takes on top.
        let mut h = Harness::prepare_with(Setup { short_usd: 550, ..lopsided() }).await;
        h.start_bot();
        // Neither waits for the other: the round opens, and the funds are moved.
        h.hedged().await;
        eventually("the funds are on their way", SOON, || !h.sim.sends().is_empty()).await;
        transfer_finished(&h).await;
        assert_eq!(withdrawals(&h).len(), 1);
        assert_eq!(h.sim.sends()[0].amount_usdc, 725);
        // 1,275 each, less the margin of the round that is open.
        assert_eq!((h.sim.balance(h.long), h.sim.balance(h.short)), (usd(775), usd(775)));
        assert_eq!((h.opens(), h.closes()), (2, 0));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn while_both_accounts_hold_the_margin_and_a_loss_nothing_is_moved_however_uneven_they_are() {
        let mut h = Harness::prepare_with(Setup { long_usd: 2_000, short_usd: 1_000, ..rebalancing() }).await;
        h.start_bot();
        h.hedged().await;
        let status = status_when(&h, "the open round", |status| status.phase == Phase::Hedged).await;
        let funds = status.funds.unwrap();
        assert_eq!((funds.verdict.as_str(), funds.difference_usd.as_str()), ("enough", "1000.00"));
        assert_eq!(funds.planned, None);
        // The short leg loses the round: 900 left, still the margin and a loss.
        h.sim.set_mid(ENTRY + TARGET);
        h.flat().await;
        steadily("nothing is moved", A_WHILE, || withdrawals(&h).is_empty() && h.sim.sends().is_empty())
            .await;
        assert_eq!(h.store.active_transfer().await.unwrap(), None);
        assert_eq!(h.sim.hypercore_reads(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_difference_too_small_to_move_does_not_hold_up_the_round() {
        // The short account is 5 short of margin plus loss, but half the difference
        // is 7 USDC: less than the relayer would take. Both can post the margin.
        let mut h = Harness::prepare_with(Setup { long_usd: 610, short_usd: 595, ..lopsided() }).await;
        h.start_bot();
        h.hedged().await;
        steadily("nothing is moved", A_WHILE, || withdrawals(&h).is_empty() && h.sim.sends().is_empty())
            .await;
        assert_eq!(h.store.active_transfer().await.unwrap(), None);
    }

    // --- killed in each stage ------------------------------------------------------
    // The bot is killed where it stands, not stopped: whatever it was in the
    // middle of does not get to finish. The next process has the record and the
    // balances, nothing else.

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bot_killed_while_the_withdrawal_is_queued_does_not_withdraw_twice() {
        let mut h = Harness::prepare_with(lopsided()).await;
        h.sim.script(h.long, "withdrawToCore", &[Behavior::Hold]);
        h.start_bot();
        eventually("the withdrawal is queued", SOON, || h.sim.queued(h.long) == 1).await;
        h.kill_bot().await;
        assert_eq!(transfer_stage(&h).await, Some(Stage::Withdraw));

        // The new process finds the withdrawal in its journal and waits for it.
        h.start_bot();
        steadily("no second withdrawal", Duration::from_millis(400), || {
            withdrawals(&h).len() == 1 && h.sim.sends().is_empty()
        })
        .await;
        h.sim.release();
        h.hedged().await;
        assert_eq!((withdrawals(&h).len(), h.sim.sends().len()), (1, 1));
        assert_levelled_and_trading(&h);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bot_killed_while_hyperliquid_is_moving_the_funds_does_not_send_twice() {
        let mut h = Harness::prepare_with(lopsided()).await;
        h.sim.hold_sends();
        h.start_bot();
        eventually("the send is made", SOON, || h.sim.sends().len() == 1).await;
        h.kill_bot().await;
        assert_eq!(transfer_stage(&h).await, Some(Stage::Send));
        assert_eq!(h.sim.core_usdc(h.long), usd(MOVED));

        h.start_bot();
        steadily("the send is not repeated", A_WHILE, || h.sim.sends().len() == 1).await;
        h.sim.release_sends();
        h.hedged().await;
        assert_eq!((withdrawals(&h).len(), h.sim.sends().len()), (1, 1));
        assert_levelled_and_trading(&h);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bot_killed_while_the_credit_is_awaited_neither_withdraws_nor_sends_again() {
        let mut h = Harness::prepare_with(lopsided()).await;
        h.sim.set_sweep(Sweep::Never);
        h.start_bot();
        in_stage(&h, Stage::Credit).await;
        h.kill_bot().await;

        h.start_bot();
        steadily("nothing is withdrawn or sent again", A_WHILE, || {
            withdrawals(&h).len() == 1 && h.sim.sends().len() == 1 && h.opens() == 0
        })
        .await;
        h.sim.set_sweep(Sweep::After(Duration::ZERO));
        h.hedged().await;
        assert_eq!((withdrawals(&h).len(), h.sim.sends().len()), (1, 1));
        assert_levelled_and_trading(&h);
    }

    // --- the withdrawal ------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_withdrawal_is_signed_again_once_the_balances_show_nothing_moved() {
        let mut h = repeating(lopsided()).await;
        h.sim.script(h.long, "withdrawToCore", &[Behavior::Reject("rate_limited", "")]);
        h.start_bot();
        h.hedged().await;
        let attempts = withdrawals(&h);
        assert_eq!(attempts.len(), 2);
        assert_ne!(attempts[0].intent_id, attempts[1].intent_id);
        // Not at once: the balances have to be read after the settle time first.
        // By the bot's own journal, which is where it measures from.
        let journal = h.store.recent_intents(10).await.unwrap();
        let signed_at: Vec<i64> = journal
            .iter()
            .filter(|row| row.action == "withdrawToCore")
            .map(|row| row.created_at_ms)
            .collect();
        let [second, first] = signed_at[..] else { panic!("{journal:?}") };
        let settle = i64::try_from(h.timing.funding.transfer.settle.as_millis()).unwrap();
        assert!(second - first >= settle, "signed again after {} ms", second - first);
        assert_eq!(h.sim.sends().len(), 1);
        assert_levelled_and_trading(&h);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unanswered_withdrawal_is_not_signed_again_before_its_deadline_has_passed() {
        let mut h = repeating(lopsided()).await;
        h.sim.script(h.long, "withdrawToCore", &[Behavior::Drop]);
        h.start_bot();
        eventually("the withdrawal is sent", SOON, || withdrawals(&h).len() == 1).await;
        // It never reached the relayer, but the bot cannot know: it may still execute.
        steadily(
            "no second withdrawal while the first could still execute",
            Duration::from_millis(1_500),
            || withdrawals(&h).len() == 1,
        )
        .await;
        eventually("both legs open once it has expired and been repeated", TTL + SOON, || {
            h.sim.positions(h.long).len() == 1 && h.sim.positions(h.short).len() == 1
        })
        .await;
        let attempts = withdrawals(&h);
        assert_eq!(attempts.len(), 2);
        let waited = attempts[1].at - attempts[0].at;
        assert!(waited + SLACK >= TTL, "{waited:?}");
        assert_eq!(h.sim.sends().len(), 1);
        assert_levelled_and_trading(&h);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unanswered_withdrawal_that_did_execute_is_not_repeated() {
        let mut h = Harness::prepare_with(lopsided()).await;
        h.sim.script(h.long, "withdrawToCore", &[Behavior::AcceptSilently]);
        h.start_bot();
        h.hedged().await;
        assert_eq!((withdrawals(&h).len(), h.sim.sends().len()), (1, 1));
        assert_levelled_and_trading(&h);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_withdrawal_whose_outcome_was_never_seen_is_not_repeated_while_the_balance_shows_it_left() {
        let mut h = repeating(lopsided()).await;
        // The withdrawal executes while the account streams are down, so the bot
        // never hears how it ended. It also takes long to reach HyperCore: well
        // past its deadline and the settle time after it.
        h.sim.hold_arrivals();
        h.sim.script(h.long, "withdrawToCore", &[Behavior::Hold]);
        h.start_bot();
        eventually("the withdrawal is queued", SOON, || h.sim.queued(h.long) == 1).await;
        h.sim.set_stream_up(false);
        h.sim.release();
        eventually("the withdrawal executes unseen", SOON, || h.sim.balance(h.long) == usd(LEVELLED)).await;
        h.sim.set_stream_up(true);

        // The deadline passes with nothing on HyperCore. But Papertrade no longer
        // holds the funds, so the withdrawal happened: it is not signed again.
        steadily("no second withdrawal", TTL + Duration::from_millis(2_000), || {
            withdrawals(&h).len() == 1 && h.sim.core_usdc(h.long) == U256::ZERO
        })
        .await;
        h.sim.release_arrivals();
        h.hedged().await;
        assert_eq!((withdrawals(&h).len(), h.sim.sends().len()), (1, 1));
        assert_levelled_and_trading(&h);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_withdrawal_that_arrives_short_is_not_sent_on() {
        let mut h = Harness::prepare_with(lopsided()).await;
        h.timing.funding.transfer.stuck_after = Duration::from_millis(500);
        // HyperCore keeps 1 USDC, as it does to activate an account that is new to it.
        h.sim.set_arrival_fee(1);
        h.start_bot();
        // Stuck once the withdrawal can no longer execute and the time allowed is over.
        in_stage_within(&h, Stage::Stuck, TTL + SOON).await;
        let stuck = h.store.active_transfer().await.unwrap().unwrap();
        assert_eq!(
            stuck.note.as_deref(),
            Some(
                "stuck in withdraw: the withdrawal of 850 USDC ended, but the long wallet's HyperCore balance \
                 did not rise by that much; compare the long account's Papertrade balance with its HyperCore \
                 account"
            )
        );
        // What arrived stays where it is: no send of an amount that is not there, no second withdrawal.
        assert_eq!(h.sim.core_usdc(h.long), usd(MOVED - 1));
        assert!(h.sim.sends().is_empty());
        assert_eq!(withdrawals(&h).len(), 1);
        assert_eq!(h.opens(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_withdrawal_that_is_refused_three_times_leaves_the_transfer_stuck_with_nothing_moved() {
        let mut h = repeating(lopsided()).await;
        h.sim.script(h.long, "withdrawToCore", &[Behavior::Reject("rate_limited", ""); 3]);
        h.start_bot();
        in_stage(&h, Stage::Stuck).await;
        let stuck = h.store.active_transfer().await.unwrap().unwrap();
        assert_eq!(
            stuck.note.as_deref(),
            Some(
                "stuck in withdraw: the withdrawal did not go through in 3 attempts; nothing has left the long \
                 account"
            )
        );
        assert_eq!(withdrawals(&h).len(), 3);
        steadily("nothing is tried a fourth time", A_WHILE, || withdrawals(&h).len() == 3).await;
        assert_eq!((h.sim.balance(h.long), h.sim.balance(h.short)), (usd(2_000), usd(300)));
        assert!(h.sim.sends().is_empty());

        // Abandoned, the accounts are judged afresh: still lopsided, so a new transfer starts.
        assert!(matches!(h.store.abandon_stuck_transfer().await.unwrap(), Abandoned::Ended(_)));
        h.hedged().await;
        assert_eq!((withdrawals(&h).len(), h.sim.sends().len()), (4, 1));
        assert_levelled_and_trading(&h);
        assert!(timeline(&h).await.iter().any(|step| step.starts_with("transfer #2 done")));
    }

    // --- the Hyperliquid send ------------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn a_send_that_never_arrived_is_repeated_after_the_settle_time() {
        let mut h = repeating(lopsided()).await;
        h.sim.script_sends(&[SendBehavior::Lose]);
        h.start_bot();
        h.hedged().await;
        // The first was lost, the second went through. On a slow machine a third
        // may follow before the balance is read again; it changes nothing.
        let sends = h.sim.sends();
        assert!(!sends[0].taken && sends[1].taken, "{sends:?}");
        assert_eq!(sends.iter().filter(|send| send.taken).count(), 1);
        assert!(sends.iter().all(|send| send.nonce == sends[0].nonce));
        // Not at once. Seen from the receiving end the two are a settle time apart
        // give or take what the bot does before each send, so the bar is half of it.
        let waited = sends[1].at - sends[0].at;
        assert!(waited >= h.timing.funding.transfer.settle / 2, "{waited:?}");
        // The repeat is the same transfer: same nonce (above), same destination, same amount.
        assert_eq!(sends[1].destination, h.sim.deposit_address(h.short));
        assert_eq!(sends[1].amount_usdc, MOVED);
        assert_eq!(withdrawals(&h).len(), 1);
        assert_levelled_and_trading(&h);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_send_that_fails_three_times_leaves_the_transfer_stuck_with_the_funds_on_hypercore() {
        let mut h = repeating(lopsided()).await;
        h.sim.script_sends(&[SendBehavior::Refuse("Insufficient spot balance"); 3]);
        h.start_bot();
        in_stage(&h, Stage::Stuck).await;
        let stuck = h.store.active_transfer().await.unwrap().unwrap();
        assert_eq!(
            stuck.note.as_deref(),
            Some(
                "stuck in send: Hyperliquid did not move the 850 USDC in 3 attempts; they are in the long \
                 wallet's HyperCore account (last error: Insufficient spot balance)"
            )
        );
        assert_eq!(h.sim.sends().len(), 3);
        steadily("nothing is tried a fourth time", A_WHILE, || h.sim.sends().len() == 3).await;
        // The funds are where the note says, and the withdrawal was not repeated.
        assert_eq!(h.sim.core_usdc(h.long), usd(MOVED));
        assert_eq!((h.sim.balance(h.long), h.sim.balance(h.short)), (usd(LEVELLED), usd(300)));
        assert_eq!(withdrawals(&h).len(), 1);
        assert_eq!(h.opens(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_send_that_went_through_unanswered_is_not_repeated() {
        let mut h = Harness::prepare_with(lopsided()).await;
        h.sim.script_sends(&[SendBehavior::DeliverSilently]);
        h.start_bot();
        h.hedged().await;
        // The answer was an error, but the balance showed the funds leaving.
        assert_eq!(h.sim.sends().len(), 1);
        assert_eq!(withdrawals(&h).len(), 1);
        assert_levelled_and_trading(&h);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_send_repeated_while_the_first_is_still_under_way_cannot_pay_twice() {
        let mut h = repeating(lopsided()).await;
        // Hyperliquid took the send but has not moved the funds by the settle time,
        // so the bot repeats it. The repeat carries the same nonce and is refused.
        h.sim.hold_sends();
        h.start_bot();
        eventually("the send is repeated", SOON, || h.sim.sends().len() == 2).await;
        h.sim.release_sends();
        h.hedged().await;
        let sends = h.sim.sends();
        assert!(sends[0].taken && sends[1..].iter().all(|send| !send.taken), "{sends:?}");
        assert!(sends.iter().all(|send| send.nonce == sends[0].nonce));
        // The short account was credited once: 300 + 850, less the margin now in its position.
        assert_levelled_and_trading(&h);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_hypercore_balance_that_fell_by_less_than_the_amount_stops_the_transfer() {
        let mut h = repeating(lopsided()).await;
        h.sim.hold_sends();
        h.start_bot();
        eventually("the send is made", SOON, || h.sim.sends().len() == 1).await;
        // The send has not moved anything yet, and something else takes 1 USDC.
        h.sim.take_from_core(h.long, 1);
        in_stage(&h, Stage::Stuck).await;
        let stuck = h.store.active_transfer().await.unwrap().unwrap();
        assert_eq!(
            stuck.note.as_deref(),
            Some(
                "stuck in send: the long wallet's HyperCore balance fell, but by less than the 850 USDC that \
                 were sent; check where the funds went"
            )
        );
        // Neither taken for the send having gone through, nor answered with another send.
        assert_eq!(h.sim.sends().len(), 1);
        assert_eq!(h.sim.deposit_checks(h.short), 0);
        assert_eq!(h.opens(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nothing_is_sent_while_the_deposit_address_cannot_be_read_from_the_chain() {
        let mut h = Harness::prepare_with(lopsided()).await;
        h.sim.set_proxies_readable(false);
        // The relayer would name an address. It is not asked where to send.
        h.sim.answer_deposit_checks_with(Address::repeat_byte(0xde));
        h.start_bot();
        in_stage(&h, Stage::Send).await;
        steadily("nothing is sent to an address the chain did not give", A_WHILE, || {
            h.sim.sends().is_empty()
        })
        .await;
        // Nothing was tried, so no attempt is used up.
        assert_eq!(h.store.active_transfer().await.unwrap().unwrap().attempts, 0);
        assert_eq!(h.sim.core_usdc(h.long), usd(MOVED));

        h.sim.set_proxies_readable(true);
        h.hedged().await;
        let sends = h.sim.sends();
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].destination, h.sim.deposit_address(h.short));
        assert_levelled_and_trading(&h);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_transfer_the_other_way_goes_to_the_long_accounts_deposit_address() {
        let setup = Setup { long_usd: 300, short_usd: 2_000, ..lopsided() };
        let h = {
            let mut h = Harness::prepare_with(setup).await;
            h.start_bot();
            h
        };
        h.hedged().await;
        assert_eq!((h.sim.count(h.long, "withdrawToCore"), h.sim.count(h.short, "withdrawToCore")), (0, 1));
        let sends = h.sim.sends();
        assert_eq!(sends.len(), 1);
        assert_eq!((sends[0].from, sends[0].destination), (h.short, h.sim.deposit_address(h.long)));
        assert_levelled_and_trading(&h);
    }

    // --- the credit ----------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn a_late_credit_is_waited_for_and_asked_after() {
        let mut h = Harness::prepare_with(lopsided()).await;
        h.sim.set_sweep(Sweep::Never);
        h.start_bot();
        // One check on entering the stage, then one per interval for as long as nothing comes.
        eventually("the relayer is asked again and again", SOON, || h.sim.deposit_checks(h.short) >= 3).await;
        assert_eq!(h.opens(), 0);
        h.sim.set_sweep(Sweep::After(Duration::ZERO));
        h.hedged().await;
        assert_eq!((withdrawals(&h).len(), h.sim.sends().len()), (1, 1));
        assert_levelled_and_trading(&h);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_credit_one_usdc_short_completes_the_transfer() {
        let mut h = Harness::prepare_with(lopsided()).await;
        // What activating a deposit address costs.
        h.sim.set_sweep_fee(1);
        h.start_bot();
        h.hedged().await;
        assert_eq!((withdrawals(&h).len(), h.sim.sends().len()), (1, 1));
        assert_eq!(
            (h.sim.balance(h.long), h.sim.balance(h.short)),
            (usd(AFTER_OPENING), usd(AFTER_OPENING - 1))
        );
        assert_eq!(h.store.active_transfer().await.unwrap(), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_credit_that_never_comes_leaves_the_transfer_stuck_and_rounds_go_on_all_the_same() {
        let mut h = Harness::prepare_with(lopsided()).await;
        h.timing.funding.transfer.stuck_after = Duration::from_millis(800);
        h.sim.set_sweep(Sweep::Never);
        h.start_bot();
        in_stage(&h, Stage::Stuck).await;
        let stuck = h.store.active_transfer().await.unwrap().unwrap();
        let note = "stuck in credit: 850 USDC left the long wallet but the short account was not credited; they \
                    should be at the short account's deposit address on HyperCore";
        assert_eq!(stuck.note.as_deref(), Some(note));
        // The funds are where the note says.
        assert_eq!(h.sim.core_usdc(h.sim.deposit_address(h.short)), usd(MOVED));
        assert_eq!((h.sim.balance(h.long), h.sim.balance(h.short)), (usd(LEVELLED), usd(300)));

        let status = status_when(&h, "the status shows it stuck", |status| {
            status.transfer.as_ref().is_some_and(|transfer| transfer.stage == "stuck")
        })
        .await;
        assert_eq!(status.transfer.unwrap().note.as_deref(), Some(note));
        // Nothing opens, but for the short account's balance and not for the transfer.
        assert_eq!(status.summary, "flat; the short account's balance is below the margin");
        assert_eq!(h.opens(), 0);
        assert!(timeline(&h).await.contains(&format!("transfer #1 {note}")));

        // The operator gets the deposit credited. The transfer stays stuck until
        // the bot is told that it has been dealt with. A round is opened and
        // closed meanwhile, and no second transfer is started.
        h.sim.set_sweep(Sweep::After(Duration::ZERO));
        h.hedged().await;
        assert_eq!((h.sim.balance(h.long), h.sim.balance(h.short)), (usd(AFTER_OPENING), usd(AFTER_OPENING)));
        h.sim.set_mid(ENTRY + TARGET);
        h.flat().await;
        assert_eq!((h.opens(), h.closes()), (2, 2));
        assert_eq!(transfer_stage(&h).await, Some(Stage::Stuck));
        steadily("nothing more is moved", A_WHILE, || withdrawals(&h).len() == 1 && h.sim.sends().len() == 1)
            .await;

        assert_eq!(h.store.abandon_stuck_transfer().await.unwrap(), Abandoned::Ended(stuck));
        // Judged afresh, both accounts hold the margin and a loss: nothing to move.
        steadily("nothing more is moved", A_WHILE, || withdrawals(&h).len() == 1 && h.sim.sends().len() == 1)
            .await;
        assert_eq!(h.store.active_transfer().await.unwrap(), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_transfer_under_way_is_seen_through_while_the_bot_is_paused() {
        let mut h = Harness::prepare_with(lopsided()).await;
        h.sim.set_sweep(Sweep::Never);
        h.start_bot();
        in_stage(&h, Stage::Credit).await;
        h.store.set_mode(Mode::Pause).await.unwrap();
        h.sim.set_sweep(Sweep::After(Duration::ZERO));
        transfer_finished(&h).await;
        assert_eq!((h.sim.balance(h.long), h.sim.balance(h.short)), (usd(LEVELLED), usd(LEVELLED)));
        steadily("paused: no round is opened", A_WHILE, || h.opens() == 0).await;

        // A paused bot finishes what it began and begins nothing: the short
        // account runs short again, and no second transfer is started.
        h.sim.set_balance(h.short, 300);
        let status = planned_but_held_by(&h, "the bot is in pause mode").await;
        let planned = status.funds.unwrap().planned.unwrap();
        assert_eq!(
            (planned.from, planned.to, planned.amount_usd.as_str()),
            (Side::Long, Side::Short, "425.00")
        );
        steadily("paused: no transfer is started", A_WHILE, || withdrawals(&h).len() == 1).await;
        assert_eq!(h.store.active_transfer().await.unwrap(), None);

        h.store.set_mode(Mode::Run).await.unwrap();
        eventually("running again, the transfer is started", SOON, || withdrawals(&h).len() == 2).await;
    }

    // --- when a transfer may start -------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn a_transfer_starts_and_finishes_while_a_round_is_open_and_leaves_the_round_alone() {
        let mut h = Harness::prepare_with(rebalancing()).await;
        h.start_bot();
        h.hedged().await;
        assert!(withdrawals(&h).is_empty());
        // The short account loses what it had outside its position. Its posted
        // margin is all it holds now: not enough to lose a round as well.
        h.sim.set_balance(h.short, 0);
        transfer_finished_after(&h, 1).await;
        // Half of what the long account held more: 2,000 against 500.
        let sends = h.sim.sends();
        assert_eq!(sends.len(), 1);
        assert_eq!((sends[0].from, sends[0].amount_usdc), (h.long, 750));
        assert_eq!((h.sim.balance(h.long), h.sim.balance(h.short)), (usd(750), usd(750)));
        // Both legs are still open, and nothing was done to them.
        assert_eq!((h.sim.positions(h.long).len(), h.sim.positions(h.short).len()), (1, 1));
        assert_eq!((h.opens(), h.closes()), (2, 0));

        // The round closes at its target as it would have.
        h.sim.set_mid(ENTRY + TARGET);
        h.flat().await;
        assert_eq!(h.closes(), 2);
        assert_eq!(withdrawals(&h).len(), 1);
    }

    /// Waits until this many transfers have been started and none is unfinished.
    async fn transfer_finished_after(h: &Harness, started: usize) {
        eventually("the transfer is started", SOON, || withdrawals(h).len() == started).await;
        eventually("the funds are sent on", SOON, || h.sim.sends().len() == started).await;
        transfer_finished(h).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_round_opens_and_closes_while_a_transfer_is_in_progress() {
        // Both accounts can post the margin, and the short one is short of a loss.
        let mut h = Harness::prepare_with(Setup { short_usd: 550, ..lopsided() }).await;
        // The relayer takes no opens for now, so the transfer is the first thing
        // to happen; and its withdrawal takes its time to reach HyperCore.
        h.sim.pause("openPosition");
        h.sim.hold_arrivals();
        h.start_bot();
        eventually("the withdrawal has left the long account", SOON, || h.sim.balance(h.long) == usd(1_275))
            .await;
        assert_eq!(h.opens(), 0);

        // Opens are taken again: the round opens with the transfer under way.
        h.sim.unpause("openPosition");
        h.hedged().await;
        assert_eq!(transfer_stage(&h).await, Some(Stage::Withdraw));
        // One round is all this scenario is about.
        h.sim.pause("openPosition");
        h.sim.set_mid(ENTRY + TARGET);
        h.flat().await;
        assert_eq!((h.opens(), h.closes()), (2, 2));
        assert_eq!(transfer_stage(&h).await, Some(Stage::Withdraw));
        assert!(h.sim.sends().is_empty());

        // The transfer goes on from where it was, and arrives.
        h.sim.release_arrivals();
        transfer_finished_after(&h, 1).await;
        assert_eq!(h.sim.sends()[0].amount_usdc, 725);
        // The long leg won the round, the short leg lost 100 of its margin.
        assert_eq!(h.sim.balance(h.long), usd(1_275 - 500) + won());
        assert_eq!(h.sim.balance(h.short), usd(550 - 100 + 725));
        // Everything about the round came between the withdrawal and the send.
        let received = h.sim.received();
        let withdrawal = received.iter().position(|r| r.action == "withdrawToCore").unwrap();
        let first_open = received.iter().position(|r| r.action == "openPosition").unwrap();
        let last_close = received.iter().rfind(|r| r.action == "closePositions").unwrap();
        assert!(withdrawal < first_open, "{received:?}");
        assert!(last_close.at < h.sim.sends()[0].at, "{received:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_round_that_closes_while_a_credit_is_awaited_does_not_pass_for_the_credit() {
        let mut h = Harness::prepare_with(Setup { long_usd: 1_150, short_usd: 550, ..rebalancing() }).await;
        // The funds leave the long wallet while the round is open, and are not
        // credited to the short account for now.
        h.sim.hold_arrivals();
        h.sim.set_sweep(Sweep::Never);
        h.start_bot();
        h.hedged().await;
        h.sim.pause("openPosition");
        h.sim.release_arrivals();
        in_stage(&h, Stage::Credit).await;
        assert_eq!(h.sim.sends()[0].amount_usdc, 300);

        // The round closes in the short account's favour: its margin comes back
        // with its winnings, more than the 300 it is waiting for. That is not the credit.
        h.sim.set_mid(ENTRY - TARGET);
        h.flat().await;
        assert_eq!(h.sim.balance(h.short), usd(50) + won());
        tokio::time::sleep(A_WHILE).await;
        assert_eq!(transfer_stage(&h).await, Some(Stage::Credit));

        h.sim.set_sweep(Sweep::After(Duration::ZERO));
        transfer_finished(&h).await;
        assert_eq!(h.sim.balance(h.short), usd(350) + won());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn what_a_pending_withdrawal_will_take_is_not_opened_with() {
        let mut h = Harness::prepare_with(Setup { long_usd: 680, short_usd: 680, ..rebalancing() }).await;
        // Long enough for the round to close and the pause between two opens to
        // pass while the withdrawal is still in the relayer's queue.
        h.config.intent_ttl_seconds = 3;
        h.sim.script(h.long, "withdrawToCore", &[Behavior::Hold]);
        h.start_bot();
        h.hedged().await;
        tokio::time::sleep(Duration::from_millis(1_500)).await;

        // The short account is left with its posted margin, so the long account
        // sends half of the 180 it holds more. Its own margin is posted: it
        // keeps nothing back. The withdrawal stays in the relayer's queue.
        h.sim.set_balance(h.short, 0);
        eventually("the withdrawal is queued", SOON, || h.sim.queued(h.long) == 1).await;
        assert_eq!(h.store.active_transfer().await.unwrap().unwrap().amount_usdc, 90);
        // One transfer is all this scenario is about: the relayer takes no further withdrawal.
        h.sim.pause("withdrawToCore");

        // The round closes against the long leg: 580 in the account, of which 90
        // are about to leave. 490 do not make a margin, 580 would.
        h.sim.set_mid(ENTRY - TARGET);
        eventually("both legs are closed", SOON, || {
            h.sim.positions(h.long).is_empty() && h.sim.positions(h.short).is_empty()
        })
        .await;
        assert_eq!((h.sim.balance(h.long), h.sim.balance(h.short)), (usd(580), won()));
        status_when(&h, "the open waits for the long account's balance", |status| {
            status.summary == "flat; the long account's balance is below the margin"
        })
        .await;
        assert_eq!(h.sim.queued(h.long), 1, "the withdrawal is still pending");
        steadily("nothing is opened with money that is about to leave", Duration::from_millis(300), || {
            h.opens() == 2
        })
        .await;

        // The withdrawal goes through. Had the round been opened, the two would
        // have competed for the same money and the relayer would have refused one.
        h.sim.release();
        transfer_finished_after(&h, 1).await;
        assert_eq!((h.sim.balance(h.long), h.sim.balance(h.short)), (usd(490), usd(90) + won()));
        assert_eq!(h.opens(), 2);
        let journal = h.store.recent_intents(20).await.unwrap();
        assert!(journal.iter().all(|row| row.status != "rejected"), "{journal:?}");
        assert!(
            journal.iter().all(|row| !matches!(row.outcome.as_deref(), Some("failed" | "expired"))),
            "{journal:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_transfer_starts_while_the_sender_has_an_open_in_flight() {
        let mut h = Harness::prepare_with(rebalancing()).await;
        h.sim.script(h.long, "openPosition", &[Behavior::Drop]);
        h.sim.script(h.short, "openPosition", &[Behavior::Drop]);
        h.start_bot();
        eventually("both opens are sent", SOON, || h.opens() == 2).await;
        h.sim.set_balance(h.short, 300);
        // Both accounts are flat and lopsided, but the unanswered opens are still in flight.
        steadily("nothing is withdrawn from under an open", Duration::from_millis(1_200), || {
            withdrawals(&h).is_empty()
        })
        .await;
        eventually("levelled once the opens have expired", TTL + SOON, || {
            h.sim.count(h.long, "withdrawToCore") == 1
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn underfunded_accounts_are_only_reported_and_what_can_open_still_opens() {
        let mut h = Harness::prepare_with(Setup { long_usd: 600, short_usd: 399, ..lopsided() }).await;
        h.start_bot();
        let status = status_when(&h, "the status says so", |status| status.funds.is_some()).await;
        let funds = status.funds.unwrap();
        assert_eq!(
            (funds.verdict.as_str(), funds.rounds_affordable, funds.planned),
            ("underfunded", 0, None)
        );
        // What keeps the round from opening is the short account's own balance.
        let waiting =
            status_when(&h, "the reason to wait", |status| status.summary.starts_with("flat; the")).await;
        assert_eq!(waiting.summary, "flat; the short account's balance is below the margin");
        steadily("nothing is moved or opened", A_WHILE, || {
            withdrawals(&h).is_empty() && h.sim.sends().is_empty() && h.opens() == 0
        })
        .await;

        // A deposit: the short account can post its margin. It is still short
        // of a round's loss, and the 5 USDC that would level the two are not
        // worth a transfer. The round opens all the same.
        h.sim.set_balance(h.short, 590);
        h.hedged().await;
        assert!(withdrawals(&h).is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rebalancing_is_off_by_default_so_wallet_keys_alone_never_start_a_transfer() {
        // The wallet automation is on and both keys are unlocked. That is all.
        let setup = Setup { long_usd: 2_000, short_usd: 300, ..automated() };
        assert!(!setup.rebalance && !Setup::default().rebalance);
        let mut h = Harness::prepare_with(setup).await;
        h.start_bot();
        let status = status_when(&h, "the status shows the funds", |status| {
            status.funds.is_some() && status.summary.starts_with("flat; the")
        })
        .await;
        assert_eq!(status.summary, "flat; the short account's balance is below the margin");
        assert_eq!(status.decision, "wait");
        let funds = status.funds.unwrap();
        assert_eq!((funds.verdict.as_str(), funds.can_rebalance, funds.planned), ("off", true, None));
        assert_eq!((funds.total_usd.as_str(), funds.difference_usd.as_str()), ("2300.00", "1700.00"));
        assert!(status.legs.iter().all(|leg| leg.has_wallet_key));
        assert_eq!(status.transfer, None);
        steadily("nothing is moved or opened", A_WHILE, || {
            withdrawals(&h).is_empty() && h.sim.sends().is_empty() && h.opens() == 0
        })
        .await;
        assert_eq!(h.store.active_transfer().await.unwrap(), None);
        assert!(timeline(&h).await.is_empty());
        // With no transfer to watch, Hyperliquid is not asked for anything.
        assert_eq!(h.sim.hypercore_reads(), 0);

        // The operator moves the funds by hand; the bot goes on.
        h.sim.set_balance(h.long, LEVELLED);
        h.sim.set_balance(h.short, LEVELLED);
        h.hedged().await;
        assert!(withdrawals(&h).is_empty() && h.sim.sends().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_wallet_key_is_not_enough_to_rebalance() {
        let mut h = Harness::prepare_with(lopsided()).await;
        // The sender's key was unlocked; the receiver's could not be.
        let reason = "no encrypted wallet key";
        Arc::get_mut(&mut h.keys).unwrap().short.wallet = WalletKey::Unavailable(reason.into());
        h.start_bot();
        let status = planned_but_held_by(&h, "the wallets' own keys are not both unlocked").await;
        let keys: Vec<bool> = status.legs.iter().map(|leg| leg.has_wallet_key).collect();
        assert_eq!(keys, [true, false]);
        let funds = status.funds.unwrap();
        assert_eq!((funds.verdict.as_str(), funds.can_rebalance), ("rebalance", false));
        let planned = funds.planned.unwrap();
        assert_eq!(
            (planned.from, planned.to, planned.amount_usd.as_str()),
            (Side::Long, Side::Short, "850.00")
        );
        steadily("nothing is moved", A_WHILE, || withdrawals(&h).is_empty() && h.sim.sends().is_empty())
            .await;
        assert_eq!(h.store.active_transfer().await.unwrap(), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_dry_run_only_reports_the_transfer_it_would_make() {
        let mut h = Harness::prepare_with(lopsided()).await;
        h.config.dry_run = true;
        h.start_bot();
        let status = planned_but_held_by(&h, "dry run").await;
        // Moving funds is not a decision about the legs.
        assert_eq!(status.decision, "wait");
        let funds = status.funds.unwrap();
        assert_eq!(funds.verdict, "rebalance");
        let planned = funds.planned.unwrap();
        assert_eq!(
            (planned.from, planned.to, planned.amount_usd.as_str()),
            (Side::Long, Side::Short, "850.00")
        );
        steadily("nothing is recorded as started, signed or sent", A_WHILE, || {
            h.sim.received().is_empty() && h.sim.sends().is_empty()
        })
        .await;
        assert_eq!(h.store.active_transfer().await.unwrap(), None);
        // The plan is on the timeline once, and says that it was not started.
        assert_eq!(
            timeline(&h).await,
            ["transfer planned, not started (dry run): 850 USDC from the long account to the short account"]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_dry_run_registers_no_session_key() {
        let mut h = Harness::prepare_with(Setup { registered: false, ..automated() }).await;
        h.config.dry_run = true;
        h.start_bot();
        status_when(&h, "the bot is running", |status| status.funds.is_some()).await;
        steadily("nothing is submitted", A_WHILE, || h.sim.received().is_empty()).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nothing_is_withdrawn_while_the_relayer_refuses_withdrawals() {
        let mut h = Harness::prepare_with(lopsided()).await;
        h.sim.pause("withdrawToCore");
        // Opens being refused as well does not matter to a transfer.
        h.sim.pause("openPosition");
        h.start_bot();
        let status = planned_but_held_by(&h, "the relayer has withdrawToCore paused").await;
        // The legs wait for something else: the relayer refuses opens as well.
        assert_eq!(status.summary, "flat; the relayer has openPosition paused");
        steadily("no withdrawal is sent into the pause", A_WHILE, || withdrawals(&h).is_empty()).await;
        assert_eq!(h.store.active_transfer().await.unwrap(), None);

        h.sim.unpause("withdrawToCore");
        eventually_stored("the accounts are levelled although opens are still refused", SOON, || async {
            h.sim.balance(h.short) == usd(LEVELLED) && h.store.active_transfer().await.unwrap().is_none()
        })
        .await;
        assert_eq!(h.opens(), 0);
        h.sim.unpause("openPosition");
        h.hedged().await;
        assert_levelled_and_trading(&h);
    }

    // --- wallet keys and session keys ----------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn a_wallet_key_registers_the_session_key_by_itself() {
        let mut h = Harness::prepare_with(Setup { registered: false, ..automated() }).await;
        h.start_bot();
        h.hedged().await;
        for account in [&h.keys.long, &h.keys.short] {
            assert_eq!(h.sim.count(account.owner, "registerSessionKey"), 1);
            assert!(h.sim.session_key_expiry(account.owner, account.session.address()).is_some());
        }
        steadily("it is registered once", A_WHILE, || {
            h.sim.count(h.long, "registerSessionKey") + h.sim.count(h.short, "registerSessionKey") == 2
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_key_about_to_expire_is_renewed_and_a_refused_renewal_is_not_hammered() {
        let mut h = Harness::prepare_with(automated()).await;
        let (key, now) = (h.keys.long.session.address(), papertrade::store::now_ms() as u64 / 1000);
        // Two days left: enough to trade on, too little to keep.
        h.sim.set_session_key_expiry(h.long, key, now + 2 * 86_400);
        h.sim.script(h.long, "registerSessionKey", &[Behavior::Reject("rate_limited", "")]);
        h.start_bot();
        h.hedged().await;
        eventually("the key is registered for another thirty days", SOON, || {
            h.sim.session_key_expiry(h.long, key).is_some_and(|expiry| expiry > now + 29 * 86_400)
        })
        .await;
        let attempts: Vec<Received> =
            h.sim.received().into_iter().filter(|r| r.action == "registerSessionKey").collect();
        assert_eq!(attempts.iter().map(|r| r.wallet).collect::<Vec<_>>(), [h.long, h.long]);
        let waited = attempts[1].at - attempts[0].at;
        assert!(waited + SLACK >= h.timing.funding.register_retry, "{waited:?}");
        // The round was not held up by any of it.
        assert_eq!(h.opens(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_transfer_whose_senders_key_is_gone_at_the_next_start_stops_as_stuck() {
        let mut h = Harness::prepare_with(lopsided()).await;
        h.sim.script(h.long, "withdrawToCore", &[Behavior::Hold]);
        h.start_bot();
        eventually("the withdrawal is queued", SOON, || h.sim.queued(h.long) == 1).await;
        h.stop_bot().await.unwrap();
        // The next run is started with the wallet automation off, and so with rebalancing off.
        (h.config.wallet_automation, h.config.rebalance) = (false, false);
        let keys = Arc::get_mut(&mut h.keys).unwrap();
        (keys.long.wallet, keys.short.wallet) = (WalletKey::Disabled, WalletKey::Disabled);

        // The withdrawal arrives, and the bot cannot sign the next step.
        h.start_bot();
        h.sim.release();
        in_stage(&h, Stage::Stuck).await;
        let stuck = h.store.active_transfer().await.unwrap().unwrap();
        assert_eq!(
            stuck.note.as_deref(),
            Some("stuck in send: the long wallet's own key is not available to this run")
        );
        assert_eq!(h.sim.core_usdc(h.long), usd(MOVED));
        assert!(h.sim.sends().is_empty());
        assert_eq!(h.opens(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_key_that_is_not_the_wallets_own_is_never_signed_with() {
        // The keystore refuses such a key. Should one ever be handed over all the
        // same, the bot keeps trading on the session keys and signs nothing with it.
        let mut h = Harness::prepare_with(Setup { registered: false, ..lopsided() }).await;
        Arc::get_mut(&mut h.keys).unwrap().long.wallet = WalletKey::Loaded(PrivateKeySigner::random());
        h.start_bot();
        let status = status_when(&h, "the status shows the funds", |status| status.funds.is_some()).await;
        assert!(!status.funds.unwrap().can_rebalance);
        // The short wallet's key is its own: its session key gets registered.
        eventually("the short account's session key is registered", SOON, || {
            h.sim.session_key_expiry(h.short, h.keys.short.session.address()).is_some()
        })
        .await;
        steadily("nothing is signed for the long wallet, and nothing is moved", A_WHILE, || {
            h.sim.received().iter().all(|r| r.wallet == h.short) && h.sim.sends().is_empty()
        })
        .await;
        assert_eq!(h.sim.count(h.short, "registerSessionKey"), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unrelated_withdrawal_receipt_keeps_the_original_deadline_and_does_not_pay_twice() {
        let mut h = Harness::prepare_with(Setup { long_usd: 1_150, short_usd: 550, ..rebalancing() }).await;
        h.config.intent_ttl_seconds = 1;
        h.timing.funding.transfer.settle = Duration::from_millis(50);
        h.sim.script(h.long, "withdrawToCore", &[Behavior::WrongReceipt]);
        h.start_bot();
        h.hedged().await;
        eventually_stored("the foreign receipt is recorded as unknown", SOON, || async {
            h.store
                .recent_intents(10)
                .await
                .unwrap()
                .iter()
                .any(|r| r.action == "withdrawToCore" && r.status == "unknown")
        })
        .await;
        let expected = withdrawals(&h)[0].intent_id;
        assert!(h.store.unresolved_intents().await.unwrap().iter().any(|r| r.intent_id == expected));
        steadily("no second withdrawal before the original deadline", A_WHILE, || withdrawals(&h).len() == 1)
            .await;
        eventually("the expired original request is retried", SOON, || withdrawals(&h).len() == 2).await;
        eventually("only the second withdrawal funds a send", SOON, || h.sim.sends().len() == 1).await;
        transfer_finished(&h).await;
        // First request never executed; only the retried withdrawal funded one send.
        assert_eq!(h.sim.sends().len(), 1);
        assert_eq!(h.sim.sends()[0].amount_usdc, 300);
    }

    fn small_transfer() -> Setup {
        Setup { long_usd: 610, short_usd: 590, ..rebalancing() }
    }

    fn swap_wallet_roles(h: &mut Harness) {
        let copy = |account: &AccountKeys| AccountKeys {
            owner: account.owner,
            session: account.session.clone(),
            wallet: WalletKey::Loaded(account.wallet.signer().unwrap().clone()),
        };
        h.keys = Arc::new(UnlockedKeys { long: copy(&h.keys.short), short: copy(&h.keys.long) });
        std::mem::swap(&mut h.config.long_account, &mut h.config.short_account);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn swapping_wallet_roles_cannot_rebind_a_credit_baseline() {
        let mut h = Harness::prepare_with(lopsided()).await;
        h.sim.pause("openPosition");
        h.sim.set_sweep(Sweep::Never);
        h.start_bot();
        in_stage(&h, Stage::Credit).await;
        let original = h.store.active_transfer().await.unwrap().unwrap();
        assert_eq!((original.from_wallet, original.to_wallet), (Some(h.long), Some(h.short)));
        h.kill_bot().await;
        swap_wallet_roles(&mut h);
        h.start_bot();
        status_when(&h, "the old wallet binding prevents recovery with swapped roles", |status| {
            status.transfer.as_ref().is_some_and(|transfer| {
                transfer.stage == "credit"
                    && transfer.note.as_ref().is_some_and(|note| note.contains("restore"))
            })
        })
        .await;
        tokio::time::sleep(A_WHILE).await;
        assert_eq!(h.store.active_transfer().await.unwrap(), Some(original));
        assert_eq!(h.sim.sends().len(), 1);
        // Actual credit while the roles are swapped still cannot finish the old
        // transfer; restoring its recorded owners resumes the same payment.
        h.sim.set_sweep(Sweep::After(Duration::ZERO));
        eventually("the original receiver is credited", SOON, || h.sim.balance(h.short) == usd(LEVELLED))
            .await;
        h.kill_bot().await;
        swap_wallet_roles(&mut h);
        h.start_bot();
        transfer_finished(&h).await;
        assert_eq!(withdrawals(&h).len(), 1);
        assert_eq!(h.sim.sends().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hypercore_arrival_does_not_release_an_unindexed_withdrawal() {
        let mut h = Harness::prepare_with(lopsided()).await;
        h.timing.stream.resync = Duration::from_secs(60);
        h.sim.pause("openPosition");
        h.sim.hold_account_frames(h.long);
        h.start_bot();
        eventually("withdrawal has arrived on Core ahead of its account patch", SOON, || {
            h.sim.core_usdc(h.long) == usd(MOVED)
        })
        .await;
        tokio::time::sleep(A_WHILE).await;
        assert_eq!(transfer_stage(&h).await, Some(Stage::Withdraw));
        assert_eq!(h.sim.sends().len(), 0);
        h.sim.release_account_frames(h.long);
        transfer_finished(&h).await;
        assert_eq!(withdrawals(&h).len(), 1);
        assert_eq!(h.sim.sends().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_small_credit_is_not_completed_by_realized_profit() {
        let mut h = Harness::prepare_with(small_transfer()).await;
        h.sim.hold_arrivals();
        h.sim.set_sweep(Sweep::Never);
        h.start_bot();
        h.hedged().await;
        h.sim.pause("openPosition");
        h.sim.release_arrivals();
        in_stage(&h, Stage::Credit).await;
        assert_eq!(h.sim.sends()[0].amount_usdc, 10);
        h.sim.pause("withdrawToCore");
        h.sim.set_mid(ENTRY - TARGET);
        h.flat().await;
        assert!(h.sim.balance(h.short) > usd(590 + 10));
        tokio::time::sleep(A_WHILE).await;
        assert_eq!(transfer_stage(&h).await, Some(Stage::Credit));
        h.sim.set_sweep(Sweep::After(Duration::ZERO));
        transfer_finished(&h).await;
        assert_eq!(h.sim.balance(h.short), usd(100) + won());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_credit_received_with_realized_loss_survives_a_restart() {
        let mut h = Harness::prepare_with(small_transfer()).await;
        h.sim.hold_arrivals();
        h.sim.set_sweep(Sweep::Never);
        h.start_bot();
        h.hedged().await;
        h.sim.pause("openPosition");
        h.sim.release_arrivals();
        in_stage(&h, Stage::Credit).await;
        h.kill_bot().await;
        h.sim.pause("withdrawToCore");
        // Both the cash credit and a losing settlement happen while it is down.
        h.sim.set_mid(ENTRY + TARGET);
        h.sim.settle_positions(h.long);
        h.sim.settle_positions(h.short);
        h.sim.set_sweep(Sweep::After(Duration::ZERO));
        eventually("10 USDC credited despite the receiver's 100 USDC loss", SOON, || {
            h.sim.balance(h.short) == usd(500)
        })
        .await;
        h.timing.funding.transfer.stuck_after = Duration::from_millis(300);
        h.start_bot();
        transfer_finished(&h).await;
        assert_eq!(withdrawals(&h).len(), 1);
        assert_eq!(h.sim.sends().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_withdrawal_can_retry_after_the_sender_realizes_a_loss() {
        let mut h = Harness::prepare_with(Setup { long_usd: 1_150, short_usd: 550, ..rebalancing() }).await;
        h.timing.funding.transfer.settle = Duration::from_secs(1);
        h.sim.script(
            h.long,
            "withdrawToCore",
            &[Behavior::Reject("admission_rejected", "temporary"), Behavior::Hold],
        );
        h.start_bot();
        h.hedged().await;
        eventually("the first withdrawal was refused", SOON, || withdrawals(&h).len() == 1).await;
        h.sim.pause("openPosition");
        h.sim.set_mid(ENTRY - TARGET);
        h.flat().await;
        assert_eq!(h.sim.balance(h.long), usd(1_050));
        eventually("the unchanged cash withdrawal total permits retry", SOON, || withdrawals(&h).len() == 2)
            .await;
        assert_eq!(transfer_stage(&h).await, Some(Stage::Withdraw));
        assert_eq!(h.sim.sends().len(), 0);
    }

    async fn slow_funding_reply_does_not_delay_a_close(action: &str) {
        let mut h = Harness::prepare_with(small_transfer()).await;
        h.sim.hold_replies(action);
        h.sim.set_sweep(Sweep::Never);
        h.start_bot();
        h.hedged().await;
        eventually("the funding request is waiting on its reply", SOON, || match action {
            "withdrawToCore" => !withdrawals(&h).is_empty(),
            "hlSend" => !h.sim.sends().is_empty(),
            "checkDeposit" => h.sim.deposit_checks(h.short) > 0,
            _ => unreachable!(),
        })
        .await;
        h.sim.pause("openPosition");
        h.sim.pause("withdrawToCore");
        h.sim.set_mid(ENTRY + TARGET);
        // The withheld reply never returns until after both legs are flat.
        h.flat().await;
        assert_eq!(h.closes(), 2);
        assert_eq!(withdrawals(&h).len(), 1);
        assert!(h.sim.sends().len() <= 1);
        h.sim.release_replies(action);
        h.sim.set_sweep(Sweep::After(Duration::ZERO));
        transfer_finished(&h).await;
        assert_eq!(h.sim.sends().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_slow_withdrawal_reply_does_not_delay_a_close() {
        slow_funding_reply_does_not_delay_a_close("withdrawToCore").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_slow_hyperliquid_reply_does_not_delay_a_close() {
        slow_funding_reply_does_not_delay_a_close("hlSend").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_slow_deposit_check_reply_does_not_delay_a_close() {
        slow_funding_reply_does_not_delay_a_close("checkDeposit").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_slow_registration_reply_does_not_delay_a_close() {
        let mut h = Harness::prepare_with(rebalancing()).await;
        h.start_bot();
        h.hedged().await;
        h.sim.hold_replies("registerSessionKey");
        let soon = papertrade::store::now_ms() as u64 / 1000 + 2 * 86_400;
        h.sim.set_session_key_expiry(h.long, h.keys.long.session.address(), soon);
        eventually("renewal is waiting on its reply", SOON, || {
            h.sim.count(h.long, "registerSessionKey") == 1
        })
        .await;
        h.sim.pause("openPosition");
        h.sim.set_mid(ENTRY + TARGET);
        h.flat().await;
        assert_eq!(h.closes(), 2);
        h.sim.release_replies("registerSessionKey");
    }

    async fn a_kill_during_a_funding_reply_keeps_one_payment(action: &str) {
        let mut h = Harness::prepare_with(small_transfer()).await;
        h.sim.hold_replies(action);
        h.sim.set_sweep(Sweep::Never);
        h.start_bot();
        h.hedged().await;
        h.sim.pause("openPosition");
        eventually("the signed request left before its reply", SOON, || {
            if action == "hlSend" { !h.sim.sends().is_empty() } else { !withdrawals(&h).is_empty() }
        })
        .await;
        h.kill_bot().await;
        h.sim.release_replies(action);
        h.sim.pause("withdrawToCore");
        h.sim.set_sweep(Sweep::After(Duration::ZERO));
        h.start_bot();
        transfer_finished(&h).await;
        assert_eq!(withdrawals(&h).len(), 1);
        assert_eq!(h.sim.sends().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_kill_during_an_unanswered_withdrawal_does_not_withdraw_twice() {
        a_kill_during_a_funding_reply_keeps_one_payment("withdrawToCore").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_kill_during_an_unanswered_send_does_not_pay_twice() {
        a_kill_during_a_funding_reply_keeps_one_payment("hlSend").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn missing_cashflow_evidence_holds_only_rebalancing() {
        let mut h = Harness::prepare_with(small_transfer()).await;
        h.sim.set_cashflows_visible(h.short, false);
        h.start_bot();
        h.hedged().await;
        let status = planned_but_held_by(&h, "account cashflow totals unavailable").await;
        assert_eq!(status.phase, Phase::Hedged);
        assert!(withdrawals(&h).is_empty());
        h.sim.pause("openPosition");
        h.sim.set_mid(ENTRY + TARGET);
        h.flat().await;
        assert_eq!(h.closes(), 2);
        assert!(withdrawals(&h).is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_history_is_unknown_then_recovers_and_becomes_stale_without_holding_closes() {
    let mut h = Harness::prepare().await;
    h.sim.set_history_readable(false);
    h.start_bot();
    h.hedged().await;
    let wallet = format!("{:#x}", h.long);
    eventually_stored("the failed first history fetch is recorded", SOON, || async {
        h.store.ledger_sync(&wallet).await.unwrap().is_some_and(|s| s.latest_error.is_some())
    })
    .await;
    let report = papertrade::tui::report(&h.store).await.unwrap();
    assert!(report.contains("history unknown"), "{report}");
    assert!(!report.contains("actual cost"), "{report}");
    h.sim.pause("openPosition");
    h.sim.set_mid(ENTRY + TARGET);
    h.flat().await;
    assert_eq!(h.closes(), 2);
    h.sim.set_history_readable(true);
    eventually_stored("both settled positions are fully synchronized", SOON, || async {
        let long = h.store.ledger_sync(&wallet).await.unwrap();
        let short = h.store.ledger_sync(&format!("{:#x}", h.short)).await.unwrap();
        [long, short]
            .iter()
            .all(|s| s.as_ref().is_some_and(|s| s.complete && s.is_fresh(papertrade::store::now_ms())))
            && h.store.totals().await.unwrap().trades == 2
    })
    .await;
    assert!(papertrade::tui::report(&h.store).await.unwrap().contains("actual cost"));
    h.sim.set_history_readable(false);
    eventually_stored("an error marks the last successful history stale", SOON, || async {
        h.store.ledger_sync(&wallet).await.unwrap().is_some_and(|s| s.latest_error.is_some())
    })
    .await;
    let report = papertrade::tui::report(&h.store).await.unwrap();
    assert!(report.contains("stale: request failed") && report.contains("recorded cost"), "{report}");
    assert!(!report.contains("actual cost"), "{report}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_history_longer_than_one_page_is_fully_imported_and_not_duplicated() {
    let mut h = Harness::prepare().await;
    h.sim.seed_settled_trades(h.long, 76);
    h.sim.pause("openPosition");
    h.start_bot();
    eventually_stored("all 76 positions are in the ledger", SOON, || async {
        h.store.totals().await.unwrap().trades == 76
    })
    .await;
    tokio::time::sleep(A_WHILE).await;
    assert_eq!(h.store.totals().await.unwrap().trades, 76);
    let sync = h.store.ledger_sync(&format!("{:#x}", h.long)).await.unwrap().unwrap();
    assert!(sync.complete && sync.is_fresh(papertrade::store::now_ms()));
}
