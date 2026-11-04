//! Inbound webhooks (ADR 0036): `POST /hooks/{ns}/{name}`. The route takes no bearer token;
//! the delivery's HMAC signature, checked against the Webhook's secret, is the authentication.
//! A good delivery is published to the Webhook's topic as `{body, headers}` with a dedup key
//! from the sender's delivery id, so a retried delivery never starts a second run. A bad one
//! is refused with 401 and an audit denial (budgeted, like other anonymous denials).

use std::collections::BTreeMap;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use clankr_types::webhook::{Signature, WebhookSpec, admit_webhook};
use clankr_types::{ApiError, ErrorCode, RequestCtx};
use hmac::{KeyInit, Mac};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{AppState, HttpError};

pub fn routes() -> Router<AppState> {
    Router::new().route("/hooks/{ns}/{name}", post(deliver))
}

async fn deliver(State(s): State<AppState>, Path((ns, name)): Path<(String, String)>, headers: HeaderMap, body: Bytes) -> axum::response::Response {
    let ctx = RequestCtx { actor: format!("webhook/{ns}/{name}"), request_id: Uuid::new_v4().to_string() };
    match deliver_inner(&s, &ctx, &ns, &name, &headers, &body).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => {
            if e.code == ErrorCode::Unauthenticated {
                let (record, suppressed) = crate::anon_audit_budget(std::time::Instant::now());
                if suppressed > 0 {
                    let _ = s.store.audit_deny(&ctx, "webhook deliveries", &format!("{suppressed} more rejected deliveries not recorded individually")).await;
                }
                if record {
                    let _ = s.store.audit_deny(&ctx, "webhook delivery", &e.message).await;
                }
            }
            HttpError(e).into_response()
        }
    }
}

async fn deliver_inner(s: &AppState, ctx: &RequestCtx, ns: &str, name: &str, headers: &HeaderMap, body: &Bytes) -> Result<Value, ApiError> {
    // an unknown hook is indistinguishable from a bad signature to the outside
    let spec = match s.store.get(ns, "Webhook", name).await {
        Ok(r) => admit_webhook(&r.spec)?,
        Err(e) if e.code == ErrorCode::NotFound => return Err(ApiError::new(ErrorCode::Unauthenticated, "no such webhook or bad signature")),
        Err(e) => return Err(e),
    };
    if body.len() > spec.max_body_bytes {
        return Err(ApiError::new(ErrorCode::InvalidSpec, format!("body over {} bytes", spec.max_body_bytes)));
    }
    let secret = s.store.secret_read(ctx, &s.master, ns, &spec.secret_ref.name, &spec.secret_ref.key, &format!("webhook {name}")).await?;
    let header = |h: &str| headers.get(h).and_then(|v| v.to_str().ok()).map(str::to_owned);
    verify(&spec, &secret, headers, body, chrono::Utc::now().timestamp()).map_err(|m| ApiError::new(ErrorCode::Unauthenticated, m))?;

    let parsed: Value = serde_json::from_slice(body).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(body).into_owned()));
    let dedup = match spec.signature {
        Signature::Github => header("x-github-delivery"),
        Signature::Stripe => parsed["id"].as_str().map(str::to_owned),
        Signature::Generic => spec.dedup_header.as_deref().and_then(header),
    }
    .unwrap_or_else(|| hex::encode(Sha256::digest(body)));
    // what the sender said about the delivery, minus anything that looks like a credential
    let kept: BTreeMap<String, String> = headers
        .iter()
        .filter(|(k, _)| k.as_str() == "content-type" || k.as_str() == "user-agent" || (k.as_str().starts_with("x-") && !k.as_str().contains("signature") && !k.as_str().contains("token")))
        .filter_map(|(k, v)| Some((k.to_string(), v.to_str().ok()?.to_owned())))
        .collect();
    let payload = json!({"body": parsed, "headers": kept, "webhook": name});
    let sender = format!("webhook/{name}");
    crate::services::publish_topic(s, ctx, ns, &spec.topic, &sender, &payload, Some(&format!("hook:{name}:{dedup}"))).await
}

fn hmac_hex(secret: &str, msg: &[u8]) -> String {
    let mut m = hmac::Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    m.update(msg);
    hex::encode(m.finalize().into_bytes())
}

fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Checks the delivery's signature. Errors are what the sender did wrong, never the secret.
pub fn verify(spec: &WebhookSpec, secret: &str, headers: &HeaderMap, body: &[u8], now: i64) -> Result<(), String> {
    let header = |h: &str| headers.get(h).and_then(|v| v.to_str().ok()).ok_or_else(|| format!("missing {h} header"));
    match spec.signature {
        Signature::Github => {
            let sig = header("x-hub-signature-256")?.strip_prefix("sha256=").ok_or("X-Hub-Signature-256 isn't sha256=…")?;
            if !ct_eq(&sig.to_ascii_lowercase(), &hmac_hex(secret, body)) {
                return Err("bad signature".into());
            }
        }
        Signature::Stripe => {
            let h = header("stripe-signature")?;
            let field = |k: &str| h.split(',').find_map(|p| p.trim().strip_prefix(k)).map(str::to_owned);
            let t = field("t=").and_then(|t| t.parse::<i64>().ok()).ok_or("Stripe-Signature has no t=")?;
            if (now - t).abs() > 300 {
                return Err("Stripe-Signature timestamp is more than 5 minutes off".into());
            }
            let mut signed = format!("{t}.").into_bytes();
            signed.extend_from_slice(body);
            let want = hmac_hex(secret, &signed);
            if !h.split(',').filter_map(|p| p.trim().strip_prefix("v1=")).any(|v| ct_eq(&v.to_ascii_lowercase(), &want)) {
                return Err("bad signature".into());
            }
        }
        Signature::Generic => {
            let v = header(&spec.header.to_ascii_lowercase())?;
            let sig = v.strip_prefix(spec.prefix.as_str()).ok_or_else(|| format!("{} doesn't start with {:?}", spec.header, spec.prefix))?;
            if !ct_eq(&sig.to_ascii_lowercase(), &hmac_hex(secret, body)) {
                return Err("bad signature".into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(signature: Signature) -> WebhookSpec {
        admit_webhook(&json!({"topic": "t", "secretRef": {"name": "s", "key": "k"}, "signature": signature, "header": "X-Sig", "prefix": "sha256="})).unwrap()
    }

    #[test]
    fn signatures() {
        let body = br#"{"id":"evt_1","hello":"world"}"#;
        let mut h = HeaderMap::new();
        h.insert("x-hub-signature-256", format!("sha256={}", hmac_hex("secret", body)).parse().unwrap());
        assert!(verify(&spec(Signature::Github), "secret", &h, body, 0).is_ok());
        assert_eq!(verify(&spec(Signature::Github), "other", &h, body, 0).unwrap_err(), "bad signature");
        assert!(verify(&spec(Signature::Github), "secret", &HeaderMap::new(), body, 0).unwrap_err().contains("missing"));

        let t = 1_700_000_000;
        let mut signed = format!("{t}.").into_bytes();
        signed.extend_from_slice(body);
        let mut h = HeaderMap::new();
        h.insert("stripe-signature", format!("t={t},v1={},v0=junk", hmac_hex("whsec", &signed)).parse().unwrap());
        assert!(verify(&spec(Signature::Stripe), "whsec", &h, body, t + 10).is_ok());
        assert!(verify(&spec(Signature::Stripe), "whsec", &h, body, t + 1000).unwrap_err().contains("5 minutes"));
        assert_eq!(verify(&spec(Signature::Stripe), "nope", &h, body, t).unwrap_err(), "bad signature");

        let mut h = HeaderMap::new();
        h.insert("x-sig", format!("sha256={}", hmac_hex("k", body).to_ascii_uppercase()).parse().unwrap());
        assert!(verify(&spec(Signature::Generic), "k", &h, body, 0).is_ok(), "hex case doesn't matter");
        h.insert("x-sig", "nope".parse().unwrap());
        assert!(verify(&spec(Signature::Generic), "k", &h, body, 0).unwrap_err().contains("doesn't start with"));
    }
}
