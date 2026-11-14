//! `Webhook`: a signed inbound HTTP endpoint that publishes each delivery to a topic, so
//! anything that can call a URL (GitHub, Stripe, Linear, Sentry, your own systems) can start
//! workflows and reach agents through EventTriggers and subscriptions.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ApiError, invalid};

/// How deliveries are signed. Every scheme is an HMAC-SHA256 of the body with the shared
/// secret; they differ in where the signature travels and what exactly is signed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Signature {
    /// `X-Hub-Signature-256: sha256=<hex>`; deliveries deduplicated by `X-GitHub-Delivery`.
    Github,
    /// `Stripe-Signature: t=<unix>,v1=<hex>` over `<t>.<body>`, within 5 minutes; deduplicated
    /// by the event's `id`.
    Stripe,
    /// `<header>: [<prefix>]<hex>` over the body (Linear, Sentry, most home-grown senders).
    Generic,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebhookSecretRef {
    pub name: String,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebhookSpec {
    /// Where deliveries are published (`{"body": …, "headers": {…}}`).
    pub topic: String,
    pub secret_ref: WebhookSecretRef,
    pub signature: Signature,
    /// Generic: the header carrying the signature.
    #[serde(default = "default_header")]
    pub header: String,
    /// Generic: text before the hex digest, e.g. `sha256=`.
    #[serde(default)]
    pub prefix: String,
    /// Generic: a header whose value identifies the delivery, so a retried delivery publishes
    /// once. Unset: the body's hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedup_header: Option<String>,
    #[serde(default = "default_max_body")]
    pub max_body_bytes: usize,
}

fn default_header() -> String {
    "x-signature-256".into()
}

fn default_max_body() -> usize {
    256 * 1024
}

pub fn admit_webhook(spec: &Value) -> Result<WebhookSpec, ApiError> {
    let s: WebhookSpec = serde_json::from_value(spec.clone()).map_err(|e| invalid("spec", format!("webhook: {e}")))?;
    if !crate::agent::valid_topic(&s.topic) {
        return Err(invalid("spec.topic", "topic: lowercase letters, digits, '.', '-', '_'"));
    }
    if s.header.is_empty() || s.header.contains(|c: char| c.is_whitespace()) {
        return Err(invalid("spec.header", "a header name"));
    }
    if !(1..=4 << 20).contains(&s.max_body_bytes) {
        return Err(invalid("spec.maxBodyBytes", "1..4MiB"));
    }
    Ok(s)
}
