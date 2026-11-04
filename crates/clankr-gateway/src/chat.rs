//! Chat platform adapters (Telegram, Slack): the few HTTP calls a connector needs. Tokens go
//! in the URL (Telegram) or a header (Slack) and never appear in errors.

use clankr_types::chat::ChatKind;
use clankr_types::{ApiError, ErrorCode};
use serde_json::{Value, json};

/// A message sent to a chat: an id the platform gave it, so it can be edited later.
#[derive(Debug, Clone, PartialEq)]
pub struct Sent {
    pub chat: String,
    pub message: String,
}

/// A button under a message; pressing it comes back as `data`.
#[derive(Debug, Clone)]
pub struct Button {
    pub label: String,
    pub data: String,
    pub primary: bool,
}

fn err(platform: &str, what: impl std::fmt::Display) -> ApiError {
    ApiError::new(ErrorCode::ProviderUnavailable, format!("{platform}: {what}"))
}

pub fn base(kind: ChatKind, endpoint: &str) -> String {
    let d = match kind {
        ChatKind::Telegram => "https://api.telegram.org",
        ChatKind::Slack => "https://slack.com/api",
    };
    (if endpoint.is_empty() { d } else { endpoint }).trim_end_matches('/').to_owned()
}

/// Sends `text` to `chat`, with optional buttons.
pub async fn send(http: &reqwest::Client, kind: ChatKind, endpoint: &str, token: &str, chat: &str, text: &str, buttons: &[Button]) -> Result<Sent, ApiError> {
    match kind {
        ChatKind::Telegram => telegram::send_message(http, &base(kind, endpoint), token, chat, text, buttons).await,
        ChatKind::Slack => slack::post_message(http, &base(kind, endpoint), token, chat, text, buttons).await,
    }
}

/// Replaces a sent message's text and drops its buttons (a decided approval card).
pub async fn edit(http: &reqwest::Client, kind: ChatKind, endpoint: &str, token: &str, sent: &Sent, text: &str) -> Result<(), ApiError> {
    match kind {
        ChatKind::Telegram => telegram::edit_message(http, &base(kind, endpoint), token, sent, text).await,
        ChatKind::Slack => slack::update_message(http, &base(kind, endpoint), token, sent, text).await,
    }
}

pub mod telegram {
    use super::*;

    /// Telegram's `{ok, result}` envelope; errors never echo the URL (it holds the token).
    async fn call(http: &reqwest::Client, base: &str, token: &str, method: &str, body: Option<Value>) -> Result<Value, ApiError> {
        let url = format!("{base}/bot{token}/{method}");
        let rb = match body {
            Some(b) => http.post(url).json(&b),
            None => http.get(url),
        };
        let resp = rb.send().await.map_err(|e| err("telegram", e.without_url()))?;
        let v: Value = resp.json().await.map_err(|e| err("telegram", e.without_url()))?;
        if v["ok"] == true {
            Ok(v["result"].clone())
        } else {
            Err(err("telegram", format!("{} ({})", v["description"].as_str().unwrap_or("error"), v["error_code"])))
        }
    }

    fn keyboard(buttons: &[Button]) -> Value {
        json!({"inline_keyboard": [buttons.iter().map(|b| json!({"text": b.label, "callback_data": b.data})).collect::<Vec<_>>()]})
    }

    pub async fn send_message(http: &reqwest::Client, base: &str, token: &str, chat: &str, text: &str, buttons: &[Button]) -> Result<Sent, ApiError> {
        let mut body = json!({"chat_id": chat, "text": text});
        if !buttons.is_empty() {
            body["reply_markup"] = keyboard(buttons);
        }
        let r = call(http, base, token, "sendMessage", Some(body)).await?;
        Ok(Sent { chat: chat.to_owned(), message: r["message_id"].to_string() })
    }

    pub async fn edit_message(http: &reqwest::Client, base: &str, token: &str, sent: &Sent, text: &str) -> Result<(), ApiError> {
        call(http, base, token, "editMessageText", Some(json!({"chat_id": sent.chat, "message_id": sent.message.parse::<i64>().unwrap_or(0), "text": text}))).await.map(drop)
    }

    pub async fn answer_callback(http: &reqwest::Client, base: &str, token: &str, id: &str, text: &str) -> Result<(), ApiError> {
        call(http, base, token, "answerCallbackQuery", Some(json!({"callback_query_id": id, "text": text}))).await.map(drop)
    }

    /// Long-polls for updates after `offset`, for up to `timeout` seconds.
    pub async fn get_updates(http: &reqwest::Client, base: &str, token: &str, offset: i64, timeout: u64) -> Result<Vec<Value>, ApiError> {
        let body = json!({"offset": offset, "timeout": timeout, "allowed_updates": ["message", "callback_query"]});
        Ok(call(http, base, token, "getUpdates", Some(body)).await?.as_array().cloned().unwrap_or_default())
    }
}

pub mod slack {
    use super::*;

    async fn call(http: &reqwest::Client, base: &str, token: &str, method: &str, body: Value) -> Result<Value, ApiError> {
        let resp = http.post(format!("{base}/{method}")).bearer_auth(token).json(&body).send().await.map_err(|e| err("slack", e.without_url()))?;
        let v: Value = resp.json().await.map_err(|e| err("slack", e.without_url()))?;
        if v["ok"] == true { Ok(v) } else { Err(err("slack", v["error"].as_str().unwrap_or("error"))) }
    }

    fn blocks(text: &str, buttons: &[Button]) -> Value {
        let mut b = vec![json!({"type": "section", "text": {"type": "mrkdwn", "text": text}})];
        if !buttons.is_empty() {
            let els: Vec<Value> = buttons
                .iter()
                .map(|x| {
                    let mut e = json!({"type": "button", "text": {"type": "plain_text", "text": x.label}, "action_id": x.data, "value": x.data});
                    if x.primary {
                        e["style"] = "primary".into();
                    }
                    e
                })
                .collect();
            b.push(json!({"type": "actions", "elements": els}));
        }
        Value::Array(b)
    }

    pub async fn post_message(http: &reqwest::Client, base: &str, token: &str, channel: &str, text: &str, buttons: &[Button]) -> Result<Sent, ApiError> {
        let r = call(http, base, token, "chat.postMessage", json!({"channel": channel, "text": text, "blocks": blocks(text, buttons)})).await?;
        Ok(Sent { chat: r["channel"].as_str().unwrap_or(channel).to_owned(), message: r["ts"].as_str().unwrap_or_default().to_owned() })
    }

    pub async fn update_message(http: &reqwest::Client, base: &str, token: &str, sent: &Sent, text: &str) -> Result<(), ApiError> {
        call(http, base, token, "chat.update", json!({"channel": sent.chat, "ts": sent.message, "text": text, "blocks": blocks(text, &[])})).await.map(drop)
    }

    /// Socket Mode: the websocket URL to connect to (app-level token).
    pub async fn connections_open(http: &reqwest::Client, base: &str, app_token: &str) -> Result<String, ApiError> {
        let r = call(http, base, app_token, "apps.connections.open", json!({})).await?;
        r["url"].as_str().map(str::to_owned).ok_or_else(|| err("slack", "apps.connections.open returned no url"))
    }
}
