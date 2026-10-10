//! The relayer and state API behind the official web app's origin.

use std::time::Duration;

use alloy::primitives::{Address, U256};
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::deployment::ORIGIN;
use super::intents::SignedIntent;
use super::wire::{IntentFailure, IntentReceipt, QueueSummary, Trade, TradePage, TradingState};

const TIMEOUT: Duration = Duration::from_secs(10);

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
    pub fn new() -> reqwest::Result<Self> {
        Ok(Self { http: reqwest::Client::builder().timeout(TIMEOUT).build()?, origin: ORIGIN.to_owned() })
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

    /// The wallet's most recent settled positions, newest first.
    pub async fn trades(&self, wallet: Address) -> reqwest::Result<Vec<Trade>> {
        // The API insists on its canonical parameter order.
        let path = format!("/query/accounts/{wallet:#x}/trades?filter=all&limit=75");
        Ok(self.get::<TradePage>(&path).await?.items)
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
            None if status.is_success() => serde_json::from_value(body)
                .map_err(|error| unknown(format!("HTTP {status}, unreadable receipt: {error}"))),
            _ => Err(unknown(format!("HTTP {status} {text}"))),
        }
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> reqwest::Result<T> {
        self.http.get(format!("{}{path}", self.origin)).send().await?.error_for_status()?.json().await
    }
}
