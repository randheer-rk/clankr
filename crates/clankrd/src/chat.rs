//! The `ChatConnector` controller (ADR 0035). One replica runs it (leader lease `chat`); it
//! keeps one task per connector, which:
//!
//! - turns each inbound chat message into a request to the connector's AgentService (sender
//!   `<platform>:<chat>`, dedup key = the platform's message id, so a redelivered update never
//!   double-sends) and posts the service's reply back to the chat;
//! - posts the namespace's pending approvals for its group as cards with Approve/Reject
//!   buttons, and decides them when an allowed user presses one (recorded as
//!   `<platform>:<user>`, through the same path as `clank approve`);
//! - refuses chats not in `allowedChats`, with an audit denial.
//!
//! Telegram is long-polled (`getUpdates`); Slack uses Socket Mode. Neither needs a public URL,
//! like nodes, the connector dials out. Progress (the Telegram offset, which cards were
//! posted) lives in the connector's status, so a restart or failover carries on.

use std::collections::HashMap;
use std::time::Duration;

use clankr_engine::{Decider, Decision};
use clankr_gateway::chat::{self, Button, Sent};
use clankr_types::chat::{ChatConnectorSpec, ChatKind, admit_chat_connector};
use clankr_types::{ApiError, ErrorCode, RequestCtx, Resource};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::AppState;
use crate::services::{service_reply, service_request};

const LEASE: &str = "chat";
const KIND: &str = "ChatConnector";

/// Supervises connector tasks for as long as this replica holds the lease.
pub fn spawn(state: AppState, holder: String, every: Duration) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut running: HashMap<(String, String), (i64, JoinHandle<()>)> = HashMap::new();
        let mut tick = tokio::time::interval(every);
        loop {
            tick.tick().await;
            let leader = match state.store.acquire_leadership(LEASE, &holder, (every.as_secs_f64() * 4.0).max(10.0)).await {
                Ok(l) => l.is_some(),
                Err(e) => {
                    tracing::warn!(error = %e, "chat lease");
                    false
                }
            };
            let wanted: HashMap<(String, String), Resource> = if leader {
                match state.store.list(KIND, None, &Default::default()).await {
                    Ok((items, _)) => items.into_iter().map(|r| ((r.metadata.namespace.clone(), r.metadata.name.clone()), r)).collect(),
                    Err(e) => {
                        tracing::warn!(error = %e.message, "list chat connectors");
                        continue;
                    }
                }
            } else {
                HashMap::new()
            };
            // stop what's gone or changed (a new generation restarts with the new spec)
            running.retain(|key, (generation, task)| {
                let keep = wanted.get(key).is_some_and(|r| r.metadata.generation == *generation) && !task.is_finished();
                if !keep {
                    task.abort();
                }
                keep
            });
            for (key, r) in wanted {
                if running.contains_key(&key) {
                    continue;
                }
                let generation = r.metadata.generation;
                let st = state.clone();
                running.insert(key, (generation, tokio::spawn(async move { run(st, r).await })));
            }
        }
    })
}

/// Runs one connector until aborted; a failure is recorded in status and retried after a pause.
async fn run(state: AppState, r: Resource) {
    let (ns, name) = (r.metadata.namespace.clone(), r.metadata.name.clone());
    let spec = match admit_chat_connector(&r.spec) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(namespace = %ns, connector = %name, error = %e.message, "chat connector spec");
            return;
        }
    };
    let http = reqwest::Client::builder().timeout(Duration::from_secs(45)).build().expect("http client");
    let mut c = Connector { state, ns, name, spec, http, token: String::new(), status: r.status.unwrap_or_else(|| json!({})) };
    loop {
        let result = match c.load_tokens().await {
            Err(e) => Err(e),
            Ok(app_token) => match c.spec.kind {
                ChatKind::Telegram => c.telegram().await,
                ChatKind::Slack => c.slack(&app_token).await,
            },
        };
        let e = result.err().map_or_else(|| "connection closed".to_owned(), |e| e.message);
        tracing::warn!(namespace = %c.ns, connector = %c.name, error = %e, "chat connector stopped; retrying");
        c.status["connected"] = json!(false);
        c.status["lastError"] = json!(e);
        c.save_status().await;
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

struct Connector {
    state: AppState,
    ns: String,
    name: String,
    spec: ChatConnectorSpec,
    http: reqwest::Client,
    token: String,
    status: Value,
}

impl Connector {
    fn ctx(&self) -> RequestCtx {
        RequestCtx { actor: format!("chatconnector/{}/{}", self.ns, self.name), request_id: Uuid::new_v4().to_string() }
    }

    fn platform(&self) -> &'static str {
        match self.spec.kind {
            ChatKind::Telegram => "telegram",
            ChatKind::Slack => "slack",
        }
    }

    /// The bot token (kept) and, for Slack, the app-level token (returned).
    async fn load_tokens(&mut self) -> Result<String, ApiError> {
        let purpose = format!("chat connector {}", self.name);
        let t = &self.spec.token_ref;
        self.token = self.state.store.secret_read(&self.ctx(), &self.state.master, &self.ns, &t.name, &t.key, &purpose).await?;
        match &self.spec.app_token_ref {
            Some(a) => self.state.store.secret_read(&self.ctx(), &self.state.master, &self.ns, &a.name, &a.key, &purpose).await,
            None => Ok(String::new()),
        }
    }

    async fn save_status(&mut self) {
        for _ in 0..3 {
            let Ok(cur) = self.state.store.get(&self.ns, KIND, &self.name).await else { return };
            if cur.status.as_ref() == Some(&self.status) {
                return;
            }
            match self.state.store.update_status(&self.ctx(), &self.ns, KIND, &self.name, cur.metadata.resource_version, self.status.clone()).await {
                Ok(_) => return,
                Err(e) if e.code == ErrorCode::Conflict => continue,
                Err(e) => {
                    tracing::warn!(error = %e.message, "chat connector status");
                    return;
                }
            }
        }
    }

    async fn send(&self, chat: &str, text: &str, buttons: &[Button]) -> Result<Sent, ApiError> {
        chat::send(&self.http, self.spec.kind, &self.spec.endpoint, &self.token, chat, text, buttons).await
    }

    /// An inbound message. The request is enqueued here (durable before the platform is told
    /// we've seen it); the reply is awaited on its own task so a slow agent never stalls polling.
    async fn inbound(&self, chat: &str, user: &str, user_name: &str, text: &str, message_id: &str) -> Result<(), ApiError> {
        let Some(svc) = self.spec.service.clone() else { return Ok(()) };
        let platform = self.platform();
        if !self.spec.allowed_chats.iter().any(|c| c == chat) {
            let ctx = RequestCtx { actor: format!("{platform}:{chat}"), request_id: Uuid::new_v4().to_string() };
            self.state.store.audit_deny(&ctx, &format!("chat message to {}/{}", self.ns, self.name), "chat is not in allowedChats").await?;
            self.send(chat, "This bot isn't set up for this chat.", &[]).await?;
            return Ok(());
        }
        let payload = json!({"text": text, "chat": chat, "from": {"id": user, "name": user_name}, "platform": platform, "messageId": message_id});
        let sender = format!("{platform}:{chat}");
        let timeout = self.spec.reply_timeout_seconds;
        let dedup = format!("{platform}:{chat}:{message_id}");
        let req = service_request(&self.state, &self.ns, &svc, &sender, &payload, timeout, Some(&dedup)).await?;
        let (state, ns, http, kind, endpoint, token, chat) = (self.state.clone(), self.ns.clone(), self.http.clone(), self.spec.kind, self.spec.endpoint.clone(), self.token.clone(), chat.to_owned());
        tokio::spawn(async move {
            let text = match service_reply(&state, &ns, &svc, req, timeout).await {
                Ok(r) => reply_text(&r["reply"]),
                Err(e) if e.code == ErrorCode::OutcomeUnknown => format!("No answer within {timeout}s. The request is still queued; the agent may answer later."),
                Err(e) => format!("Sorry, that failed: {}", e.message),
            };
            if let Err(e) = chat::send(&http, kind, &endpoint, &token, &chat, &text, &[]).await {
                tracing::warn!(error = %e.message, "chat reply");
            }
        });
        Ok(())
    }

    /// Posts cards for newly pending approvals and closes cards for decided ones.
    async fn approval_cards(&mut self) -> Result<(), ApiError> {
        let Some(cards) = self.spec.approvals.clone() else { return Ok(()) };
        let pending = self.state.engine.approvals(&self.ns, Some("pending"), 200).await?;
        let mut posted: HashMap<String, Value> = serde_json::from_value(self.status["posted"].clone()).unwrap_or_default();
        for a in pending.iter().filter(|a| a.approver_group == cards.group) {
            let id = a.id.to_string();
            if posted.contains_key(&id) {
                continue;
            }
            let subject = serde_json::to_string_pretty(&a.subject.0).unwrap_or_default();
            let subject: String = subject.chars().take(1500).collect();
            let text = format!(
                "Approval needed: {} (run {})\nRequested by: {}\nDeadline: {}\n\n{subject}",
                a.node,
                a.run_id,
                a.requested_by.as_deref().unwrap_or("-"),
                a.deadline.map_or("none".to_owned(), |d| d.to_rfc3339()),
            );
            let buttons = [
                Button { label: "Approve".into(), data: format!("apr:{id}:approve"), primary: true },
                Button { label: "Reject".into(), data: format!("apr:{id}:reject"), primary: false },
            ];
            let sent = self.send(&cards.chat, &text, &buttons).await?;
            posted.insert(id, json!({"chat": sent.chat, "message": sent.message}));
        }
        let still: Vec<String> = pending.iter().map(|a| a.id.to_string()).collect();
        for id in posted.keys().filter(|id| !still.contains(id)).cloned().collect::<Vec<_>>() {
            let Ok(uuid) = id.parse::<Uuid>() else {
                posted.remove(&id);
                continue;
            };
            let outcome = match self.state.engine.approval(&self.ns, uuid).await {
                Ok(a) => format!("{} {} by {}{}", a.node, a.state, a.decided_by.as_deref().unwrap_or("-"), a.comment.as_deref().filter(|c| !c.is_empty()).map_or(String::new(), |c| format!(": {c}"))),
                Err(_) => "approval no longer exists".to_owned(),
            };
            let p = &posted[&id];
            let sent = Sent { chat: p["chat"].as_str().unwrap_or_default().to_owned(), message: p["message"].as_str().unwrap_or_default().to_owned() };
            if let Err(e) = chat::edit(&self.http, self.spec.kind, &self.spec.endpoint, &self.token, &sent, &outcome).await {
                tracing::warn!(error = %e.message, "closing approval card");
            }
            posted.remove(&id);
        }
        self.status["posted"] = serde_json::to_value(posted).expect("serializable");
        self.save_status().await;
        Ok(())
    }

    /// A button press: `apr:<id>:approve|reject` by `user`. Returns what to tell the user.
    async fn decide(&self, user: &str, data: &str) -> String {
        let Some(cards) = &self.spec.approvals else { return "No approvals here.".into() };
        let parts: Vec<&str> = data.split(':').collect();
        let (Some(&"apr"), Some(id), Some(which)) = (parts.first(), parts.get(1), parts.get(2)) else { return "Unknown button.".into() };
        let actor = format!("{}:{user}", self.platform());
        let ctx = RequestCtx { actor: actor.clone(), request_id: Uuid::new_v4().to_string() };
        if !cards.users.iter().any(|u| u == user) {
            let _ = self.state.store.audit_deny(&ctx, &format!("approval {which} {id}"), "chat user is not in approvals.users").await;
            return "You're not on the list of people who can decide this.".into();
        }
        let decision = if *which == "approve" { Decision::Approve } else { Decision::Reject };
        let r = async {
            let id: Uuid = id.parse().map_err(|_| ApiError::new(ErrorCode::NotFound, "bad approval id"))?;
            let a = self.state.engine.approval(&self.ns, id).await?;
            let who = Decider { ctx: &ctx, groups: std::slice::from_ref(&cards.group), break_glass: false };
            self.state.engine.decide_approval(&who, &self.ns, id, decision, &a.digest, &format!("via {}", self.platform())).await
        }
        .await;
        match r {
            Ok(a) => format!("{} {}", a.node, a.state),
            Err(e) => e.message,
        }
    }

    // ── Telegram ──────────────────────────────────────────────────────────────

    async fn telegram(&mut self) -> Result<(), ApiError> {
        let base = chat::base(ChatKind::Telegram, &self.spec.endpoint);
        let mut offset = self.status["telegram"]["offset"].as_i64().unwrap_or(0);
        self.status["connected"] = json!(true);
        self.status["lastError"] = Value::Null;
        self.approval_cards().await?;
        let mut cards = tokio::time::interval(Duration::from_secs(10));
        cards.tick().await;
        loop {
            let updates = tokio::select! {
                u = chat::telegram::get_updates(&self.http, &base, &self.token, offset, 25) => u?,
                _ = cards.tick() => {
                    self.approval_cards().await?;
                    continue;
                }
            };
            for u in updates {
                offset = offset.max(u["update_id"].as_i64().unwrap_or(0) + 1);
                if let Some(m) = u.get("message").filter(|m| m["text"].is_string()) {
                    let chat = m["chat"]["id"].to_string();
                    let user = m["from"]["id"].to_string();
                    let name = m["from"]["username"].as_str().or(m["from"]["first_name"].as_str()).unwrap_or_default();
                    if let Err(e) = self.inbound(&chat, &user, name, m["text"].as_str().unwrap_or_default(), &m["message_id"].to_string()).await {
                        tracing::warn!(error = %e.message, "telegram inbound");
                    }
                } else if let Some(q) = u.get("callback_query") {
                    let answer = self.decide(&q["from"]["id"].to_string(), q["data"].as_str().unwrap_or_default()).await;
                    let _ = chat::telegram::answer_callback(&self.http, &base, &self.token, q["id"].as_str().unwrap_or_default(), &answer).await;
                    self.approval_cards().await?;
                }
            }
            if self.status["telegram"]["offset"].as_i64() != Some(offset) {
                self.status["telegram"] = json!({"offset": offset});
                self.save_status().await;
            }
        }
    }

    // ── Slack (Socket Mode) ───────────────────────────────────────────────────

    async fn slack(&mut self, app_token: &str) -> Result<(), ApiError> {
        let base = chat::base(ChatKind::Slack, &self.spec.endpoint);
        let url = chat::slack::connections_open(&self.http, &base, app_token).await?;
        let (ws, _) = tokio_tungstenite::connect_async(&url).await.map_err(|e| ApiError::new(ErrorCode::ProviderUnavailable, format!("slack socket: {e}")))?;
        let (mut tx, mut rx) = ws.split();
        self.status["connected"] = json!(true);
        self.status["lastError"] = Value::Null;
        self.approval_cards().await?;
        let mut cards = tokio::time::interval(Duration::from_secs(10));
        cards.tick().await;
        loop {
            let msg = tokio::select! {
                m = rx.next() => m,
                _ = cards.tick() => {
                    self.approval_cards().await?;
                    continue;
                }
            };
            let text = match msg {
                Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t))) => t.to_string(),
                Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(p))) => {
                    let _ = tx.send(tokio_tungstenite::tungstenite::Message::Pong(p)).await;
                    continue;
                }
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(ApiError::new(ErrorCode::ProviderUnavailable, format!("slack socket: {e}"))),
                None => return Ok(()),
            };
            let env: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            // the event is durable in our queue before Slack hears the ack, so it isn't lost
            // between the two; an ack after 3s makes Slack resend, which the dedup key absorbs
            let outcome = self.slack_envelope(&env).await;
            if let Some(id) = env["envelope_id"].as_str() {
                let _ = tx.send(tokio_tungstenite::tungstenite::Message::Text(json!({"envelope_id": id}).to_string().into())).await;
            }
            if let Err(e) = outcome {
                tracing::warn!(error = %e.message, "slack inbound");
            }
            if env["type"] == "disconnect" {
                return Ok(());
            }
        }
    }

    async fn slack_envelope(&mut self, env: &Value) -> Result<(), ApiError> {
        match env["type"].as_str() {
            Some("events_api") => {
                let ev = &env["payload"]["event"];
                let own = ev["bot_id"].is_string() || ev["subtype"].is_string();
                if (ev["type"] == "message" || ev["type"] == "app_mention") && !own && ev["text"].is_string() {
                    let id = env["payload"]["event_id"].as_str().unwrap_or(ev["ts"].as_str().unwrap_or_default());
                    self.inbound(ev["channel"].as_str().unwrap_or_default(), ev["user"].as_str().unwrap_or_default(), "", ev["text"].as_str().unwrap_or_default(), id).await?;
                }
            }
            Some("interactive") => {
                let p = &env["payload"];
                if p["type"] == "block_actions" {
                    let data = p["actions"][0]["value"].as_str().unwrap_or_default();
                    let answer = self.decide(p["user"]["id"].as_str().unwrap_or_default(), data).await;
                    let chat = p["channel"]["id"].as_str().unwrap_or_default();
                    if !chat.is_empty() {
                        let _ = self.send(chat, &answer, &[]).await;
                    }
                    self.approval_cards().await?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// What a service's reply reads as in a chat: a string as is, `{text}` by its text, anything
/// else as JSON.
fn reply_text(reply: &Value) -> String {
    match reply {
        Value::String(s) => s.clone(),
        v if v["text"].is_string() => v["text"].as_str().unwrap_or_default().to_owned(),
        v => v.to_string(),
    }
}

