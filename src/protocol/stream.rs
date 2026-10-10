//! Live view of one wallet over the state API's server-sent events.

use std::time::Duration;

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
/// The server only sends a frame when the wallet changes, so an idle wallet's
/// indexed-through time stands still. Reconnecting yields a full snapshot taken
/// at the indexer's current position, which also covers any patch that was lost.
const RESYNC: Duration = Duration::from_secs(30);
const RETRY: Duration = Duration::from_secs(1);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

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
    state: watch::Sender<Option<AccountState>>,
    outcomes: mpsc::Sender<IntentOutcome>,
) {
    // No overall timeout: the response body is the stream.
    let http = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .expect("default HTTP client configuration is valid");
    loop {
        if let End::Lost(reason) = connection(&http, &url, &state, &outcomes).await {
            state.send_replace(None);
            warn!(%reason, "account stream lost");
            sleep(RETRY).await;
        }
    }
}

async fn connection(
    http: &reqwest::Client,
    url: &str,
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
    let resync_at = Instant::now() + RESYNC;
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
                    .and_then(|frame| apply_frame(held.as_ref(), frame).map_err(|e| e.to_string()));
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
