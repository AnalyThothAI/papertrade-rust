//! Live view of one wallet over the state API's server-sent events.

use std::time::Duration;

use alloy::primitives::Address;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, sleep, sleep_until, timeout};
use tracing::warn;

use super::account::{AccountState, apply_frame};
use super::wire::IntentOutcome;

/// Upper bound for the silence allowed before the connection is treated as dead.
const MAX_SILENCE: Duration = Duration::from_secs(45);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy)]
pub struct StreamTiming {
    /// The server only sends a frame when the wallet changes, so an idle wallet's
    /// indexed-through time stands still. Reconnecting this often yields a full
    /// snapshot taken at the indexer's current position, which also covers any
    /// patch that was lost.
    pub resync: Duration,
    /// Wait before reconnecting after a failure; doubles up to `retry_max` while failures repeat.
    pub retry_min: Duration,
    pub retry_max: Duration,
}

impl Default for StreamTiming {
    fn default() -> Self {
        Self {
            resync: Duration::from_secs(30),
            retry_min: Duration::from_secs(1),
            retry_max: Duration::from_secs(30),
        }
    }
}

enum End {
    /// Planned: time for a fresh snapshot. The held state stays valid meanwhile.
    Resync,
    /// The connection failed, went silent or sent something unusable.
    Lost(String),
}

/// Keeps `state` at the wallet's current account state, forever.
///
/// The published value is `None` whenever the view cannot be trusted: before the
/// first snapshot, after a connection error, or when the server has gone
/// silent. Readers treat `None` as "unknown" and wait instead of acting on old
/// data.
pub async fn follow_account(
    url: String,
    expected_wallet: Address,
    timing: StreamTiming,
    state: watch::Sender<Option<AccountState>>,
    outcomes: mpsc::Sender<IntentOutcome>,
) {
    // No overall timeout: the response body is the stream.
    let http = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .expect("default HTTP client configuration is valid");
    let mut retry = timing.retry_min;
    loop {
        match connection(&http, &url, expected_wallet, timing.resync, &state, &outcomes).await {
            // A connection that lasted until its resync was healthy.
            End::Resync => retry = timing.retry_min,
            End::Lost(reason) => {
                state.send_replace(None);
                warn!(%reason, retry_in = ?retry, "account stream lost");
                sleep(retry).await;
                retry = (retry * 2).min(timing.retry_max);
            }
        }
    }
}

async fn connection(
    http: &reqwest::Client,
    url: &str,
    expected_wallet: Address,
    resync: Duration,
    state: &watch::Sender<Option<AccountState>>,
    outcomes: &mpsc::Sender<IntentOutcome>,
) -> End {
    let response = http
        .get(url)
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .send()
        .await
        .and_then(reqwest::Response::error_for_status);
    let mut events = match response {
        Ok(response) => response.bytes_stream().eventsource(),
        Err(error) => return End::Lost(error.to_string()),
    };
    let resync_at = Instant::now() + resync;
    let mut silence = MAX_SILENCE;
    // Every connection starts over from a full snapshot.
    let mut held: Option<AccountState> = None;
    loop {
        let event = tokio::select! {
            () = sleep_until(resync_at) => return End::Resync,
            event = timeout(silence, events.next()) => event,
        };
        let event = match event {
            Err(_) => return End::Lost(format!("silent for {silence:?}")),
            Ok(None) => return End::Lost("closed by the server".into()),
            Ok(Some(Err(error))) => return End::Lost(error.to_string()),
            Ok(Some(Ok(event))) => event,
        };
        match event.event.as_str() {
            "liveness" => {
                if let Some(interval) = heartbeat_interval(&event.data) {
                    silence = MAX_SILENCE.min(3 * interval);
                }
            }
            "wallet" => {
                let update = serde_json::from_str::<Value>(&event.data)
                    .map_err(|error| error.to_string())
                    .and_then(|frame| {
                        // Revisions restart on reconnect. Bind even the first snapshot to the
                        // requested wallet before any balance or outcome reaches its owner.
                        let wallet: Address =
                            serde_json::from_value(frame.get("address").cloned().unwrap_or(Value::Null))
                                .map_err(|error| error.to_string())?;
                        if wallet != expected_wallet {
                            return Err(format!("wallet frame is for {wallet}, expected {expected_wallet}"));
                        }
                        apply_frame(held.as_ref(), frame).map_err(|e| e.to_string())
                    });
                match update {
                    Ok(Some(update)) => {
                        held = Some(update.state.clone());
                        state.send_replace(Some(update.state));
                        for outcome in update.outcomes {
                            // Outcomes are informational; a full channel only loses a journal note.
                            let _ = outcomes.try_send(outcome);
                        }
                    }
                    Ok(None) => {}
                    Err(reason) => return End::Lost(reason),
                }
            }
            // Heartbeats and anything else only prove the connection is alive.
            _ => {}
        }
    }
}

fn heartbeat_interval(data: &str) -> Option<Duration> {
    let millis = serde_json::from_str::<Value>(data).ok()?.get("heartbeatIntervalMs")?.as_u64()?;
    (millis > 0).then(|| Duration::from_millis(millis))
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::extract::State;
    use axum::response::sse::{Event, Sse};
    use axum::routing::get;
    use futures::{Stream, stream};
    use tokio::sync::Semaphore;
    use tokio::task::JoinHandle;

    use super::*;
    use crate::testing::WALLET_SNAPSHOT;

    const EXPECTED: Address = Address::with_last_byte(1);
    const FOREIGN: Address = Address::with_last_byte(2);
    const TEST_TIMEOUT: Duration = Duration::from_secs(2);

    struct Responses {
        frames: Vec<String>,
        next: AtomicUsize,
        requested: mpsc::UnboundedSender<usize>,
        gated_request: usize,
        release: Arc<Semaphore>,
    }

    async fn wallet_stream(
        State(responses): State<Arc<Responses>>,
    ) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
        let request = responses.next.fetch_add(1, Ordering::SeqCst);
        responses.requested.send(request).unwrap();
        if request >= responses.gated_request {
            responses.release.acquire().await.unwrap().forget();
        }
        let frame = responses.frames[request.min(responses.frames.len() - 1)].clone();
        Sse::new(
            stream::once(async move { Ok(Event::default().event("wallet").data(frame)) })
                .chain(stream::pending()),
        )
    }

    struct TestServer {
        url: String,
        requests: mpsc::UnboundedReceiver<usize>,
        release: Arc<Semaphore>,
        task: JoinHandle<()>,
    }

    async fn serve(wallets: &[Address], gated_request: usize) -> TestServer {
        let frames = wallets
            .iter()
            .map(|wallet| {
                let mut frame: Value = serde_json::from_str(WALLET_SNAPSHOT).unwrap();
                frame["address"] = serde_json::to_value(wallet).unwrap();
                // Each connection has its own revision sequence.
                frame["accountRevision"] = Value::from(1);
                frame.to_string()
            })
            .collect();
        let (requested, requests) = mpsc::unbounded_channel();
        let release = Arc::new(Semaphore::new(0));
        let responses = Arc::new(Responses {
            frames,
            next: AtomicUsize::new(0),
            requested,
            gated_request,
            release: release.clone(),
        });
        let app = Router::new().route("/wallet", get(wallet_stream)).with_state(responses);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/wallet", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        TestServer { url, requests, release, task }
    }

    fn timing() -> StreamTiming {
        StreamTiming {
            resync: Duration::from_millis(100),
            retry_min: Duration::from_millis(1),
            retry_max: Duration::from_millis(1),
        }
    }

    async fn request(server: &mut TestServer, expected: usize) {
        assert_eq!(timeout(TEST_TIMEOUT, server.requests.recv()).await.unwrap(), Some(expected));
    }

    async fn own_snapshot(state: &mut watch::Receiver<Option<AccountState>>) {
        timeout(TEST_TIMEOUT, async {
            loop {
                state.changed().await.unwrap();
                if let Some(account) = state.borrow_and_update().as_ref() {
                    assert_eq!(account.address, EXPECTED, "foreign balances were published");
                    return;
                }
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn foreign_first_snapshot_is_rejected_and_own_wallet_recovers() {
        let mut server = serve(&[FOREIGN, EXPECTED], 1).await;
        let (sender, mut state) = watch::channel(None);
        let (outcomes, mut received) = mpsc::channel(8);
        let follower = tokio::spawn(follow_account(server.url.clone(), EXPECTED, timing(), sender, outcomes));

        request(&mut server, 0).await;
        request(&mut server, 1).await;
        assert!(state.borrow_and_update().is_none(), "foreign first snapshot was trusted");
        assert!(received.try_recv().is_err());
        server.release.add_permits(1);
        own_snapshot(&mut state).await;

        follower.abort();
        server.task.abort();
    }

    #[tokio::test]
    async fn foreign_reconnect_snapshot_invalidates_old_state_then_own_wallet_recovers() {
        let mut server = serve(&[EXPECTED, FOREIGN, EXPECTED], 1).await;
        let (sender, mut state) = watch::channel(None);
        let (outcomes, mut received) = mpsc::channel(8);
        let follower = tokio::spawn(follow_account(server.url.clone(), EXPECTED, timing(), sender, outcomes));

        request(&mut server, 0).await;
        own_snapshot(&mut state).await;
        request(&mut server, 1).await;
        server.release.add_permits(1);
        request(&mut server, 2).await;
        assert!(state.borrow_and_update().is_none(), "foreign reconnect snapshot was trusted");
        assert!(received.try_recv().is_err());
        server.release.add_permits(1);
        own_snapshot(&mut state).await;

        follower.abort();
        server.task.abort();
    }
}
