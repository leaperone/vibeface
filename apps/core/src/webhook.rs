use crate::{clock::now_ms, crypto::hmac_sha256};
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const MAX_ATTEMPTS: u32 = 8;
pub const FIRST_BACKOFF_MS: i64 = 10_000;

/// `X-Vibeface-Signature` for a body sent at `timestamp` (Unix seconds).
pub fn signature(secret: &[u8], timestamp: i64, body: &str) -> String {
    format!(
        "v1={}",
        hex::encode(hmac_sha256(
            secret,
            format!("{timestamp}.{body}").as_bytes()
        ))
    )
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Pending,
    Delivered,
    Dead,
}

/// One webhook delivery, keyed by `event_id` so an event is queued once.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OutboxEntry {
    pub event_id: String,
    pub body: String,
    pub state: Delivery,
    pub attempts: u32,
    pub next_ms: i64,
}

impl OutboxEntry {
    pub fn new(event_id: &str, body: String, now_ms: i64) -> Self {
        Self {
            event_id: event_id.into(),
            body,
            state: Delivery::Pending,
            attempts: 0,
            next_ms: now_ms,
        }
    }

    /// Applies one attempt's result: an HTTP status, or `None` when the request failed.
    pub fn record(&mut self, status: Option<u16>, now_ms: i64) {
        self.attempts += 1;
        let retry = match status {
            Some(200..=299) => {
                self.state = Delivery::Delivered;
                return;
            }
            None | Some(408 | 429 | 500..=599) => true,
            Some(_) => false,
        };
        if !retry || self.attempts >= MAX_ATTEMPTS {
            self.state = Delivery::Dead;
            return;
        }
        self.next_ms = now_ms + FIRST_BACKOFF_MS * (1 << (self.attempts - 1).min(16));
    }
}

/// One signed POST; the status, or the transport error.
pub(crate) async fn post_webhook(
    url: &str,
    secret: &[u8],
    event_id: &str,
    body: &str,
) -> (Option<u16>, Option<String>) {
    let timestamp = now_ms() / 1000;
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(error) => return (None, Some(error.to_string())),
    };
    match client
        .post(url)
        .header("content-type", "application/json")
        .header("x-vibeface-event-id", event_id)
        .header("x-vibeface-timestamp", timestamp.to_string())
        .header("x-vibeface-signature", signature(secret, timestamp, body))
        .body(body.to_owned())
        .send()
        .await
    {
        Ok(response) => (Some(response.status().as_u16()), None),
        // Without the URL: it may carry a path the user treats as private.
        Err(error) => (None, Some(error.without_url().to_string())),
    }
}
