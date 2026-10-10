//! Stopping the bot and starting it again: a new process has nothing but the
//! exchange's account state and its own journal, and has to take over whatever
//! the old one left, whether it stopped cleanly or was killed.

mod support;

use std::time::Duration;

use support::{Behavior, ENTRY, Harness, TARGET, eventually, eventually_stored, steadily};

const SOON: Duration = Duration::from_secs(5);
const A_WHILE: Duration = Duration::from_millis(700);

#[tokio::test(flavor = "multi_thread")]
async fn a_killed_bot_takes_over_the_open_round_and_closes_it_at_the_target() {
    let mut h = Harness::start().await;
    h.hedged().await;
    h.kill_bot().await;
    // Nothing watches the round while the bot is down.
    h.sim.set_mid(ENTRY + TARGET);
    steadily("nobody closes while the bot is down", A_WHILE, || h.closes() == 0).await;

    h.start_bot();
    h.flat().await;
    assert_eq!((h.opens(), h.closes()), (2, 2));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_inside_the_target_just_keeps_holding() {
    let mut h = Harness::start().await;
    h.hedged().await;
    h.stop_bot().await.unwrap();
    h.start_bot();
    steadily("the round is neither closed nor opened again", A_WHILE, || (h.opens(), h.closes()) == (2, 0))
        .await;
    h.hedged().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_killed_right_after_sending_its_opens_does_not_send_them_again() {
    let mut h = Harness::prepare().await;
    // Both opens reach the relayer and sit in its queue.
    h.sim.script(h.long, "openPosition", &[Behavior::Hold]);
    h.sim.script(h.short, "openPosition", &[Behavior::Hold]);
    h.start_bot();
    eventually("both opens are queued", SOON, || h.sim.queued(h.long) == 1 && h.sim.queued(h.short) == 1)
        .await;
    h.kill_bot().await;
    assert_eq!(h.store.unresolved_intents().await.unwrap().len(), 2, "the journal survives the kill");

    h.start_bot();
    steadily("the new process waits for them instead of opening again", A_WHILE, || h.opens() == 2).await;
    h.sim.release();
    h.hedged().await;
    assert_eq!((h.opens(), h.closes()), (2, 0));
    eventually_stored("the journal is settled", SOON, || async {
        h.store.unresolved_intents().await.unwrap().is_empty()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_leg_liquidated_while_the_bot_was_down_gets_the_survivor_closed_on_restart() {
    let mut h = Harness::start().await;
    h.hedged().await;
    h.kill_bot().await;
    h.sim.liquidate(h.short);

    h.start_bot();
    h.flat().await;
    assert_eq!((h.sim.count(h.long, "closePositions"), h.sim.count(h.short, "closePositions")), (1, 0));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_killed_between_the_two_closes_finishes_the_job() {
    let mut h = Harness::start().await;
    h.hedged().await;
    // Price rises: the short is closed first; the long's close stays in the relayer's queue.
    h.sim.script(h.long, "closePositions", &[Behavior::Hold]);
    h.sim.set_mid(ENTRY + TARGET);
    eventually("the short is closed and the long's close is queued", SOON, || {
        h.sim.positions(h.short).is_empty() && h.sim.queued(h.long) == 1
    })
    .await;
    h.kill_bot().await;

    h.start_bot();
    steadily("the queued close is not sent a second time", A_WHILE, || {
        h.sim.count(h.long, "closePositions") == 1
    })
    .await;
    h.sim.release();
    h.flat().await;
    assert_eq!(h.closes(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_clean_stop_exits_without_error_and_leaves_the_round_for_the_next_start() {
    let mut h = Harness::start().await;
    h.hedged().await;
    // A clean stop is not an error, and it does not touch the open round.
    h.stop_bot().await.unwrap();
    steadily("stopping does not close anything", A_WHILE, || h.closes() == 0).await;
    assert_eq!(h.sim.positions(h.long).len() + h.sim.positions(h.short).len(), 2);

    h.sim.set_mid(ENTRY - TARGET);
    h.start_bot();
    h.flat().await;
    // Price fell, so the long lost and is closed first.
    let closes: Vec<_> = h.sim.received().into_iter().filter(|r| r.action == "closePositions").collect();
    assert_eq!(closes.iter().map(|r| r.wallet).collect::<Vec<_>>(), [h.long, h.short]);
}
