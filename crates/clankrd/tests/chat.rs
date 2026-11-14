//! Chat connectors (ADR 0035): Telegram and Slack messages become service requests whose
//! replies go back to the chat; unknown chats are refused and audited; pending approvals are
//! posted as cards and decided by their buttons; agents send through a `chat` tool.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::routing::post;
use axum::{Json, Router};
use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_store::mailbox::SendOpts;
use clankr_types::{RequestCtx, Resource};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};

const NS: &str = "shop";

fn ctx() -> RequestCtx {
    RequestCtx { actor: "owner".into(), request_id: "t".into() }
}

fn res(kind: &str, name: &str, spec: Value) -> Resource {
    serde_json::from_value(json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})).unwrap()
}

/// What the fake platform saw and what it has queued for the connector.
#[derive(Default)]
struct Fake {
    updates: VecDeque<Value>,
    calls: Vec<(String, Value)>,
    next_id: i64,
}

type Shared = Arc<Mutex<Fake>>;

async fn wait_for<F: Fn(&[(String, Value)]) -> bool>(fake: &Shared, what: &str, f: F) -> Vec<(String, Value)> {
    for _ in 0..300 {
        let calls = fake.lock().unwrap().calls.clone();
        if f(&calls) {
            return calls;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}: {:?}", fake.lock().unwrap().calls);
}

async fn setup() -> (clankrd::AppState, Store) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pg = DevPostgres::start(None).await.unwrap();
    let store = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    std::mem::forget(pg);
    let state = clankrd::AppState::new(store.clone(), "tok".into(), clankr_store::secrets::MasterKey::generate()).await.unwrap();
    store.create(&ctx(), res("Agent", "helper", json!({"runtime": {"type": "process", "command": ["true"]}}))).await.unwrap();
    store.create(&ctx(), res("AgentService", "assistant", json!({"agent": "helper"}))).await.unwrap();
    store.create(&ctx(), res("Workflow", "publish", json!({"entrypoint": "review", "nodes": {"review": {"type": "approval", "approverGroup": "editors"}}}))).await.unwrap();
    (state, store)
}

/// Acts as the serving agent: takes the next request and answers it.
async fn answer(store: &Store, reply: Value) -> Value {
    for _ in 0..300 {
        if let Some(m) = store.mailbox_receive(NS, "svc:assistant", "helper", 1, 60.0).await.unwrap().pop() {
            let to = m.reply_to.clone().unwrap();
            let opts = SendOpts { sender: "agent/helper", correlation_id: m.correlation_id.as_deref(), dedup_key: Some("reply"), ttl_secs: 60, max_deliveries: 1, reply_to: None };
            store.mailbox_send(NS, &to, &reply, opts).await.unwrap();
            store.mailbox_ack(NS, "svc:assistant", m.id, m.lease_epoch).await.unwrap();
            return json!({"payload": m.payload, "sender": m.sender});
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("no request reached the service");
}

#[tokio::test(flavor = "multi_thread")]
async fn telegram_messages_approvals_and_outbound_tool() {
    let (state, store) = setup().await;
    // the fake Telegram API: updates are handed out once; every call is recorded
    let fake: Shared = Default::default();
    async fn tg(State(f): State<Shared>, Path((bot, method)): Path<(String, String)>, Json(body): Json<Value>) -> Json<Value> {
        assert_eq!(bot, "bot123:ABC", "the bot token comes from the secret");
        if method == "getUpdates" {
            for _ in 0..10 {
                if let Some(u) = f.lock().unwrap().updates.pop_front() {
                    return Json(json!({"ok": true, "result": [u]}));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            return Json(json!({"ok": true, "result": []}));
        }
        let mut g = f.lock().unwrap();
        g.next_id += 1;
        let id = g.next_id;
        g.calls.push((method, body.clone()));
        Json(json!({"ok": true, "result": {"message_id": id, "chat": {"id": body["chat_id"]}}}))
    }
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let app = Router::new().route("/{bot}/{method}", post(tg)).with_state(fake.clone());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

    store.secret_put(&ctx(), &state.master, NS, "tg", &[("token".to_owned(), "123:ABC".to_owned())].into()).await.unwrap();
    let spec = json!({"kind": "telegram", "tokenRef": {"name": "tg", "key": "token"}, "endpoint": base, "service": "assistant", "allowedChats": ["42"],
        "replyTimeoutSeconds": 10, "approvals": {"chat": "42", "group": "editors", "users": ["7"]}});
    store.create(&ctx(), res("ChatConnector", "tg", spec)).await.unwrap();
    clankrd::chat::spawn(state.clone(), "test".into(), Duration::from_millis(300));

    // a message from an allowed chat reaches the service as that chat, and the reply comes back
    let msg = |id: i64, chat: i64, text: &str| json!({"update_id": id, "message": {"message_id": id, "chat": {"id": chat}, "from": {"id": 7, "username": "ana"}, "text": text}});
    fake.lock().unwrap().updates.push_back(msg(1, 42, "how many blue mugs?"));
    let got = answer(&store, json!({"text": "4 left"})).await;
    assert_eq!(got["sender"], "telegram:42");
    assert_eq!(got["payload"]["text"], "how many blue mugs?");
    assert_eq!(got["payload"]["from"]["name"], "ana");
    wait_for(&fake, "the reply", |c| c.iter().any(|(m, b)| m == "sendMessage" && b["text"] == "4 left" && b["chat_id"] == "42")).await;

    // a chat that isn't allowed is refused and the refusal is in the audit log
    fake.lock().unwrap().updates.push_back(msg(2, 99, "hi"));
    wait_for(&fake, "the refusal", |c| c.iter().any(|(m, b)| m == "sendMessage" && b["chat_id"] == "99")).await;
    let denied = store.audit_latest(20).await.unwrap().into_iter().find(|e| e.actor == "telegram:99").expect("audit denial");
    assert_eq!(denied.decision, "deny");
    assert!(store.mailbox_receive(NS, "svc:assistant", "helper", 1, 1.0).await.unwrap().is_empty(), "nothing was queued for the stranger");

    // a pending approval becomes a card; the button decides it as the person who pressed it
    let run = state.engine.start(&ctx(), NS, "publish", json!({"post": "spring"}), None).await.unwrap();
    let calls = wait_for(&fake, "the approval card", |c| c.iter().any(|(m, b)| m == "sendMessage" && b["reply_markup"].is_object())).await;
    let card = &calls.iter().find(|(m, b)| m == "sendMessage" && b["reply_markup"].is_object()).unwrap().1;
    let approve = card["reply_markup"]["inline_keyboard"][0][0]["callback_data"].as_str().unwrap().to_owned();
    assert!(approve.starts_with("apr:") && approve.ends_with(":approve"), "{approve}");
    assert!(card["text"].as_str().unwrap().contains("spring"), "the card shows what's being approved");
    let press = |id: i64, user: i64, data: &str| json!({"update_id": id, "callback_query": {"id": format!("cb{id}"), "from": {"id": user}, "data": data, "message": {"chat": {"id": 42}, "message_id": 1}}});
    // someone not on the list gets told so, and nothing changes
    fake.lock().unwrap().updates.push_back(press(3, 8, &approve));
    wait_for(&fake, "the refusal toast", |c| c.iter().any(|(m, b)| m == "answerCallbackQuery" && b["text"].as_str().unwrap().contains("not on the list"))).await;
    assert_eq!(state.engine.approvals(NS, Some("pending"), 10).await.unwrap().len(), 1);
    fake.lock().unwrap().updates.push_back(press(4, 7, &approve));
    wait_for(&fake, "the decision toast", |c| c.iter().any(|(m, b)| m == "answerCallbackQuery" && b["text"] == "review approved")).await;
    let a = &state.engine.approvals(NS, Some("approved"), 10).await.unwrap()[0];
    assert_eq!(a.decided_by.as_deref(), Some("telegram:7"));
    assert!(state.engine.get(NS, run.run_id).await.unwrap().status.is_terminal(), "the run moved on");
    wait_for(&fake, "the closed card", |c| c.iter().any(|(m, b)| m == "editMessageText" && b["text"].as_str().unwrap().contains("approved by telegram:7"))).await;

    // an agent sends through a chat tool; the token stays on the control plane
    store.create(&ctx(), res("Tool", "notify", json!({"version": "1", "executor": {"type": "chat", "connector": "tg"}, "methods": {"send": {"input": {"type": "object", "required": ["chat", "text"]}, "risk": "low"}}}))).await.unwrap();
    store.create(&ctx(), res("ToolClaim", "notify", json!({"allow": [{"tool": "notify"}]}))).await.unwrap();
    let call = serde_json::from_value(json!({"tool": "notify", "method": "send", "input": {"chat": "42", "text": "restock ordered"}, "idempotencyKey": "k1"})).unwrap();
    let r = state.gateway.call_tool(&ctx(), NS, "notify", call).await.unwrap();
    assert_eq!(r.output["chat"], "42");
    assert!(fake.lock().unwrap().calls.iter().any(|(m, b)| m == "sendMessage" && b["text"] == "restock ordered"));

    // progress is in status, so a restart carries on from the last update
    for _ in 0..50 {
        let st = store.get(NS, "ChatConnector", "tg").await.unwrap().status.unwrap_or_default();
        if st["telegram"]["offset"] == 5 && st["connected"] == true {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("status wasn't saved: {:?}", store.get(NS, "ChatConnector", "tg").await.unwrap().status);
}

#[tokio::test(flavor = "multi_thread")]
async fn slack_socket_mode_round_trip() {
    let (state, store) = setup().await;
    let fake: Shared = Default::default();
    // the websocket the connector dials: hello, then one message event, then a button press
    let ws = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_url = format!("ws://{}/link", ws.local_addr().unwrap());
    let acks: Arc<Mutex<Vec<String>>> = Default::default();
    {
        let acks = acks.clone();
        tokio::spawn(async move {
            let (sock, _) = ws.accept().await.unwrap();
            let mut s = tokio_tungstenite::accept_async(sock).await.unwrap();
            let text = |v: Value| tokio_tungstenite::tungstenite::Message::Text(v.to_string().into());
            s.send(text(json!({"type": "hello"}))).await.unwrap();
            s.send(text(json!({"type": "events_api", "envelope_id": "e1", "payload": {"event_id": "Ev1", "event": {"type": "message", "channel": "C42", "user": "U7", "text": "status?", "ts": "1.1"}}}))).await.unwrap();
            // a bot's own message must be ignored
            s.send(text(json!({"type": "events_api", "envelope_id": "e2", "payload": {"event_id": "Ev2", "event": {"type": "message", "channel": "C42", "bot_id": "B1", "text": "echo", "ts": "1.2"}}}))).await.unwrap();
            while let Some(Ok(m)) = s.next().await {
                if let tokio_tungstenite::tungstenite::Message::Text(t) = m {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    acks.lock().unwrap().push(v["envelope_id"].as_str().unwrap_or_default().to_owned());
                    if v["envelope_id"] == "e2" {
                        break;
                    }
                }
            }
            // wait for the approval card, then press its button
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if let Ok(data) = std::env::var("CLANKR_TEST_PRESS") {
                    s.send(text(json!({"type": "interactive", "envelope_id": "e3", "payload": {"type": "block_actions", "user": {"id": "U7"}, "channel": {"id": "C42"}, "actions": [{"action_id": data, "value": data}]}}))).await.unwrap();
                    break;
                }
            }
            while let Some(Ok(_)) = s.next().await {}
        });
    }
    async fn slack(State(f): State<Shared>, Path(method): Path<String>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Json<Value> {
        let auth = headers.get("authorization").unwrap().to_str().unwrap().to_owned();
        let mut g = f.lock().unwrap();
        g.calls.push((method.clone(), body.clone()));
        g.next_id += 1;
        Json(match method.as_str() {
            "apps.connections.open" => {
                assert_eq!(auth, "Bearer xapp-1", "socket mode uses the app token");
                json!({"ok": true, "url": g.updates.pop_front().unwrap()["url"]})
            }
            _ => {
                assert_eq!(auth, "Bearer xoxb-1", "messages use the bot token");
                json!({"ok": true, "channel": body["channel"], "ts": format!("{}.0", g.next_id)})
            }
        })
    }
    fake.lock().unwrap().updates.push_back(json!({"url": ws_url}));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let app = Router::new().route("/{method}", post(slack)).with_state(fake.clone());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

    store.secret_put(&ctx(), &state.master, NS, "slack", &[("bot".to_owned(), "xoxb-1".to_owned()), ("app".to_owned(), "xapp-1".to_owned())].into()).await.unwrap();
    let spec = json!({"kind": "slack", "tokenRef": {"name": "slack", "key": "bot"}, "appTokenRef": {"name": "slack", "key": "app"}, "endpoint": base, "service": "assistant",
        "allowedChats": ["C42"], "replyTimeoutSeconds": 10, "approvals": {"chat": "C42", "group": "editors", "users": ["U7"]}});
    store.create(&ctx(), res("ChatConnector", "slack", spec)).await.unwrap();
    clankrd::chat::spawn(state.clone(), "test".into(), Duration::from_millis(300));

    let got = answer(&store, json!("all good")).await;
    assert_eq!(got["sender"], "slack:C42");
    assert_eq!(got["payload"]["text"], "status?");
    wait_for(&fake, "the reply", |c| c.iter().any(|(m, b)| m == "chat.postMessage" && b["channel"] == "C42" && b["text"] == "all good")).await;
    for _ in 0..50 {
        if acks.lock().unwrap().len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(acks.lock().unwrap().as_slice(), ["e1", "e2"], "every envelope is acked");
    assert!(store.mailbox_receive(NS, "svc:assistant", "helper", 1, 1.0).await.unwrap().is_empty(), "the bot's own message wasn't forwarded");

    // an approval card with Block Kit buttons, decided by a block_actions interaction
    state.engine.start(&ctx(), NS, "publish", json!({"post": "spring"}), None).await.unwrap();
    let calls = wait_for(&fake, "the card", |c| c.iter().any(|(m, b)| m == "chat.postMessage" && b["blocks"][1]["type"] == "actions")).await;
    let card = &calls.iter().find(|(m, b)| m == "chat.postMessage" && b["blocks"][1]["type"] == "actions").unwrap().1;
    let approve = card["blocks"][1]["elements"][0]["value"].as_str().unwrap().to_owned();
    assert_eq!(card["blocks"][1]["elements"][0]["style"], "primary");
    // SAFETY: a test-only handoff to the fake socket task, set once
    unsafe { std::env::set_var("CLANKR_TEST_PRESS", &approve) };
    wait_for(&fake, "the decision", |c| c.iter().any(|(m, b)| m == "chat.postMessage" && b["text"] == "review approved")).await;
    assert_eq!(state.engine.approvals(NS, Some("approved"), 10).await.unwrap()[0].decided_by.as_deref(), Some("slack:U7"));
    wait_for(&fake, "the closed card", |c| c.iter().any(|(m, b)| m == "chat.update" && b["text"].as_str().unwrap().contains("approved by slack:U7"))).await;
}
