//! The relayer and state API behind the official web app's origin.

use std::collections::BTreeSet;
use std::time::Duration;

use alloy::primitives::{Address, U256};
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::deployment::ORIGIN;
use super::intents::SignedIntent;
use super::wire::{
    IntentFailure, IntentReceipt, MintCurve, ProtocolSummary, QueueSummary, Trade, TradePage, TradingState,
};

const TIMEOUT: Duration = Duration::from_secs(10);

/// The observed frontend's history request asks for this many rows per page.
pub const TRADE_PAGE_LIMIT: usize = 75;

/// Rejections the relayer states explicitly: the intent was not taken in.
const REJECTION_CODES: [&str; 7] = [
    "invalid_intent",
    "deadline_expired",
    "unauthorized_intent",
    "intent_conflict",
    "admission_rejected",
    "intake_action_paused",
    "rate_limited",
];

#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    /// The relayer refused the intent. It will not execute.
    #[error("rejected: {code}{}", reason.as_deref().map(|r| format!(" ({r})")).unwrap_or_default())]
    Rejected { code: String, reason: Option<String> },
    /// The request may or may not have reached the relayer. The intent can still
    /// execute until its deadline, so it must not be signed again before then.
    #[error("outcome unknown: {0}")]
    Unknown(String),
}

#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    origin: String,
}

impl Client {
    /// The production deployment.
    pub fn new() -> reqwest::Result<Self> {
        Self::with_origin(ORIGIN)
    }

    /// Any server that speaks the same protocol, such as the test simulator.
    pub fn with_origin(origin: &str) -> reqwest::Result<Self> {
        Ok(Self { http: reqwest::Client::builder().timeout(TIMEOUT).build()?, origin: origin.to_owned() })
    }

    pub fn account_stream_url(&self, wallet: Address) -> String {
        format!("{}/state/user/live?wallet={wallet:#x}", self.origin)
    }

    pub async fn trading_state(&self) -> reqwest::Result<TradingState> {
        self.get("/state/trading").await
    }

    /// Total the LP owes to queued winners, 18-decimal USD.
    pub async fn queue_total(&self) -> reqwest::Result<U256> {
        Ok(self.get::<QueueSummary>("/query/protocol/queue-summary").await?.total.0)
    }

    /// Where the PAPER mint curve stands, from the protocol summary.
    pub async fn mint_curve(&self) -> reqwest::Result<MintCurve> {
        Ok(self.get::<ProtocolSummary>("/query/protocol/summary").await?.paper)
    }

    /// The wallet's entire settled-position history, newest first. No partial
    /// result is returned if a later page fails or a cursor stops progressing.
    pub async fn trades(&self, wallet: Address) -> anyhow::Result<Vec<Trade>> {
        let url = reqwest::Url::parse(&format!("{}/query/accounts/{wallet:#x}/trades", self.origin))?;
        let limit = TRADE_PAGE_LIMIT.to_string();
        let mut cursor: Option<String> = None;
        let mut seen = BTreeSet::new();
        let mut trades = Vec::new();
        loop {
            // Preserve the API's canonical filter, limit, cursor order, and
            // let reqwest encode the opaque cursor rather than interpolating it.
            let mut page_url = url.clone();
            {
                let mut parameters = page_url.query_pairs_mut();
                parameters.append_pair("filter", "all").append_pair("limit", &limit);
                if let Some(cursor) = cursor.as_deref() {
                    parameters.append_pair("cursor", cursor);
                }
            }
            let page = self.http.get(page_url).send().await?.error_for_status()?.json::<TradePage>().await?;
            anyhow::ensure!(page.items.len() <= 200, "history returned more than 200 rows in a page");
            trades.extend(page.items);
            let Some(next) = page.next_cursor else { return Ok(trades) };
            anyhow::ensure!(seen.insert(next.clone()), "history returned a repeated cursor");
            cursor = Some(next);
        }
    }

    /// Sends one signed intent, once. Never retries: see [`SubmitError::Unknown`].
    pub async fn submit_intent(&self, intent: &SignedIntent) -> Result<IntentReceipt, SubmitError> {
        let unknown = |detail: String| SubmitError::Unknown(detail);
        let response = self
            .http
            .post(format!("{}/intents", self.origin))
            .json(&intent.body())
            .send()
            .await
            .map_err(|error| unknown(error.to_string()))?;
        let status = response.status();
        let text = response.text().await.map_err(|error| unknown(error.to_string()))?;
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);

        let failure: IntentFailure = serde_json::from_value(body.clone()).unwrap_or_default();
        let code = failure
            .code
            .or(failure.error)
            .or_else(|| (status.as_u16() == 429).then(|| "rate_limited".to_owned()));
        match code {
            Some(code) if REJECTION_CODES.contains(&code.as_str()) => {
                Err(SubmitError::Rejected { code, reason: failure.reason })
            }
            None if status.is_success() => {
                let receipt: IntentReceipt = serde_json::from_value(body)
                    .map_err(|error| unknown(format!("HTTP {status}, unreadable receipt: {error}")))?;
                if receipt.intent_id != intent.intent_id {
                    return Err(unknown(format!(
                        "HTTP {status}, receipt identifies {}, expected {}; the submitted intent may still execute",
                        receipt.intent_id, intent.intent_id,
                    )));
                }
                Ok(receipt)
            }
            _ => Err(unknown(format!("HTTP {status} {text}"))),
        }
    }

    /// Asks the relayer to look at the wallet's deposit address now, so that
    /// funds which have arrived there are credited without waiting for its own
    /// round. Returns the address the relayer checked, for the log only: where
    /// to send funds is never taken from here.
    pub async fn check_deposit(&self, user: Address) -> reqwest::Result<Option<Address>> {
        let reply: Value = self
            .http
            .post(format!("{}/funding/deposits/check", self.origin))
            .json(&serde_json::json!({ "user": format!("{user:#x}") }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(reply.get("proxy").and_then(Value::as_str).and_then(|proxy| proxy.parse().ok()))
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> reqwest::Result<T> {
        self.http.get(format!("{}{path}", self.origin)).send().await?.error_for_status()?.json().await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use axum::extract::{OriginalUri, State};
    use axum::http::StatusCode;
    use axum::routing::{get, post};
    use serde_json::json;

    use super::*;

    struct History {
        pages: VecDeque<(StatusCode, Value)>,
        requests: Vec<String>,
    }

    async fn page(
        State(history): State<Arc<Mutex<History>>>,
        OriginalUri(uri): OriginalUri,
    ) -> (StatusCode, axum::Json<Value>) {
        let mut history = history.lock().unwrap();
        history.requests.push(uri.query().unwrap_or_default().to_owned());
        let (status, body) = history.pages.pop_front().expect("unexpected extra history request");
        (status, axum::Json(body))
    }

    async fn history(
        pages: Vec<(StatusCode, Value)>,
    ) -> (Client, Arc<Mutex<History>>, tokio::task::JoinHandle<()>) {
        let state = Arc::new(Mutex::new(History { pages: pages.into(), requests: Vec::new() }));
        let app =
            axum::Router::new().route("/query/accounts/{wallet}/trades", get(page)).with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (Client::with_origin(&origin).unwrap(), state, server)
    }

    fn trade(position_id: u64) -> Value {
        json!({
            "timestampMs": 1_791_700_000_000u64,
            "txHash": format!("0x{}", "cd".repeat(32)),
            "positionId": position_id.to_string(), "symbol": "BTC",
            "priceScaleRaw": "10", "isLong": true,
            "entryPriceRaw": "827935", "marginRaw": "500000000000000000000", "leverage": 20,
            "kind": "PositionClosed", "exitPriceRaw": "819656",
            "adjustedPnlRaw": "-100000000000000000000", "userPaidFeeRaw": "0",
            "paperMintedRaw": "9800000000000000000000", "paperMintBasisRaw": "98000000000000000000"
        })
    }

    #[tokio::test]
    async fn a_receipt_for_another_intent_is_unknown_and_the_expected_intent_stays_tracked() {
        use crate::protocol::intents::Action;
        use crate::strategy::decide::Side;
        use crate::strategy::inflight::{Inflight, Kind, track};
        use crate::testing::{NOW, account};
        use alloy::primitives::B256;

        let app = axum::Router::new().route(
            "/intents",
            post(|| async {
                axum::Json(json!({
                    "intentId": format!("{:#x}", B256::repeat_byte(0x22)),
                    "action": "withdrawToCore", "state": "queued", "queueRevision": "7",
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::with_origin(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let expected = B256::repeat_byte(0x11);
        let intent = SignedIntent {
            action: Action::WithdrawToCore,
            intent_id: expected,
            user: Address::ZERO,
            deadline: NOW + 120,
            signed: json!({}),
            withdrawal: Some(json!({ "route": "hypercore" })),
        };
        let result = client.submit_intent(&intent).await;
        server.abort();
        assert!(
            matches!(result, Err(SubmitError::Unknown(ref detail))
            if detail.contains("expected") && detail.contains(&expected.to_string())),
            "{result:?}"
        );

        // An unknown receipt provides no acceptance revision and no replacement
        // identity. Tracking the request that was actually sent keeps its deadline.
        let pending = Inflight {
            side: Side::Long,
            kind: Kind::Withdraw,
            intent_id: expected,
            deadline: intent.deadline,
            sent_at_ms: 0,
            queue_revision: None,
            seen_queued: false,
            failed: false,
        };
        let observed = account();
        assert_eq!(track(pending.clone(), Some(&observed)).unwrap().intent_id, expected);
        let expired = crate::protocol::account::AccountState {
            indexed_through_timestamp: intent.deadline + 1,
            ..observed
        };
        assert!(track(pending, Some(&expired)).is_none());
    }

    #[tokio::test]
    async fn history_fetches_past_75_rows_until_an_explicit_last_page_and_encodes_the_cursor() {
        let first: Vec<Value> = (1..=75).map(trade).collect();
        let cursor = "opaque+/=&?";
        let (client, history, server) = history(vec![
            (StatusCode::OK, json!({ "items": first, "nextCursor": cursor })),
            (StatusCode::OK, json!({ "items": [trade(76)], "nextCursor": null })),
        ])
        .await;
        let trades = client.trades(Address::ZERO).await.unwrap();
        server.abort();
        assert_eq!(trades.len(), 76);
        assert_eq!(trades.last().unwrap().position_id.0, U256::from(76));
        let requests = &history.lock().unwrap().requests;
        assert_eq!(requests[0], "filter=all&limit=75");
        assert_eq!(requests[1], "filter=all&limit=75&cursor=opaque%2B%2F%3D%26%3F");
    }

    #[tokio::test]
    async fn history_does_not_return_a_partial_first_page_if_a_later_request_fails() {
        let (client, history, server) = history(vec![
            (StatusCode::OK, json!({ "items": [trade(1)], "nextCursor": "next" })),
            (StatusCode::SERVICE_UNAVAILABLE, json!({ "error": "unavailable" })),
        ])
        .await;
        let result = client.trades(Address::ZERO).await;
        server.abort();
        assert!(result.is_err());
        assert_eq!(history.lock().unwrap().requests.len(), 2);
    }

    #[tokio::test]
    async fn history_rejects_a_cursor_cycle_instead_of_looping_or_returning_duplicates() {
        let (client, history, server) = history(vec![
            (StatusCode::OK, json!({ "items": [trade(1)], "nextCursor": "a" })),
            (StatusCode::OK, json!({ "items": [trade(2)], "nextCursor": "b" })),
            (StatusCode::OK, json!({ "items": [trade(3)], "nextCursor": "a" })),
        ])
        .await;
        let result = client.trades(Address::ZERO).await;
        server.abort();
        assert!(result.unwrap_err().to_string().contains("repeated cursor"));
        assert_eq!(history.lock().unwrap().requests.len(), 3);
    }
}
