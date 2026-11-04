//! A2A (ADR 0038): a service has an agent card; SendMessage reaches it and returns its reply
//! as a Message, or a Task that GetTask resolves; an `a2a` tool calls another A2A agent, here
//! the very same control plane, so both ends are exercised.

use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_store::mailbox::SendOpts;
use clankr_types::{RequestCtx, Resource};
use reqwest::Method as M;
use serde_json::{Value, json};

const NS: &str = "ext";
const TOKEN: &str = "a2a-tok";

fn res(kind: &str, name: &str, spec: Value) -> Resource {
    serde_json::from_value(json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})).unwrap()
}

/// Acts as the serving agent: answers the next request with `reply`.
async fn answer(store: &Store, reply: Value) -> Value {
    for _ in 0..300 {
        if let Some(m) = store.mailbox_receive(NS, "svc:translator", "t", 1, 60.0).await.unwrap().pop() {
            let to = m.reply_to.clone().unwrap();
            let opts = SendOpts { sender: "agent/translator", correlation_id: m.correlation_id.as_deref(), dedup_key: Some("reply"), ttl_secs: 60, max_deliveries: 1, reply_to: None };
            store.mailbox_send(NS, &to, &reply, opts).await.unwrap();
            store.mailbox_ack(NS, "svc:translator", m.id, m.lease_epoch).await.unwrap();
            return m.payload;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("no request reached the service")
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_card_send_message_get_task_and_the_a2a_tool() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pg = DevPostgres::start(None).await.unwrap();
    let store = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    let state = clankrd::AppState::new(store.clone(), TOKEN.into(), clankr_store::secrets::MasterKey::generate()).await.unwrap();
    let (gateway, master) = (state.gateway.clone(), state.master.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, clankrd::router(state)).await.unwrap() });
    let ctx = RequestCtx { actor: "owner".into(), request_id: "t".into() };
    let http = reqwest::Client::new();
    let rpc = |method: &str, params: Value| {
        let rb = http.request(M::POST, format!("{base}/a2a/{NS}/translator")).bearer_auth(TOKEN).json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}));
        async move { rb.send().await.unwrap().json::<Value>().await.unwrap() }
    };

    store.create(&ctx, res("Agent", "translator", json!({"runtime": {"type": "process", "command": ["true"]}}))).await.unwrap();
    store.create(&ctx, res("AgentService", "translator", json!({"agent": "translator", "description": "Translates text", "skills": [{"id": "translate", "name": "Translate", "description": "en → fr", "tags": ["nlp"]}]}))).await.unwrap();

    // the card: authenticated, and pointing back at this endpoint
    let r = http.get(format!("{base}/a2a/{NS}/translator/.well-known/a2a/agent-card.json")).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let card: Value = r.json().await.unwrap();
    assert_eq!(card["protocolVersion"], "1.0");
    assert_eq!(card["url"], format!("{}/a2a/{NS}/translator", base.replace("http://", "https://")), "https unless a proxy says otherwise");
    assert_eq!(card["skills"][0]["id"], "translate");
    assert_eq!(card["security"], json!([{"bearer": []}]));
    assert_eq!(http.get(format!("{base}/a2a/{NS}/translator/.well-known/a2a/agent-card.json")).send().await.unwrap().status(), 401, "cards aren't public");

    // SendMessage: text and data parts reach the service; the reply is a Message
    let send = rpc("SendMessage", json!({"message": {"messageId": "m1", "contextId": "c1", "role": "ROLE_USER", "parts": [{"text": "bonjour"}, {"data": {"to": "en"}}]}}));
    let (reply, got) = tokio::join!(send, answer(&store, json!({"text": "hello", "confidence": 0.9})));
    assert_eq!(got["text"], "bonjour");
    assert_eq!(got["data"], json!({"to": "en"}));
    assert_eq!(got["from"], "a2a:owner");
    assert_eq!(reply["result"]["role"], "ROLE_AGENT", "{reply}");
    assert_eq!(reply["result"]["parts"], json!([{"text": "hello"}, {"data": {"confidence": 0.9}}]));
    assert_eq!(reply["result"]["contextId"], "c1");

    // returnImmediately: a Task that GetTask follows to completion
    let t = rpc("SendMessage", json!({"message": {"messageId": "m2", "role": "ROLE_USER", "parts": [{"text": "merci"}]}, "configuration": {"returnImmediately": true}})).await;
    let id = t["result"]["id"].as_str().unwrap().to_owned();
    assert_eq!(t["result"]["status"]["state"], "TASK_STATE_SUBMITTED");
    assert_eq!(rpc("GetTask", json!({"id": id})).await["result"]["status"]["state"], "TASK_STATE_SUBMITTED");
    answer(&store, json!("thanks")).await;
    let done = rpc("GetTask", json!({"id": id})).await;
    assert_eq!(done["result"]["status"]["state"], "TASK_STATE_COMPLETED", "{done}");
    assert_eq!(done["result"]["status"]["message"]["parts"], json!([{"text": "thanks"}]));
    assert_eq!(rpc("GetTask", json!({"id": "nope"})).await["error"]["code"], -32001);
    assert_eq!(rpc("CancelTask", json!({"id": id})).await["error"]["code"], -32601);

    // the a2a tool: an agent in this cluster calls an A2A agent, which happens to be us
    store.secret_put(&ctx, &master, NS, "peer", &[("token".to_owned(), TOKEN.to_owned())].into()).await.unwrap();
    store.create(&ctx, res("Tool", "peer", json!({"version": "1", "timeoutSeconds": 20, "executor": {"type": "a2a", "url": format!("{base}/a2a/{NS}/translator"), "secretRef": {"name": "peer", "key": "token"}}, "methods": {"translate": {"input": {"type": "object"}}}}))).await.unwrap();
    store.create(&ctx, res("ToolClaim", "peer", json!({"allow": [{"tool": "peer"}]}))).await.unwrap();
    let call = serde_json::from_value(json!({"tool": "peer", "method": "translate", "input": {"text": "salut", "to": "en"}, "idempotencyKey": "k1"})).unwrap();
    let (result, got) = tokio::join!(gateway.call_tool(&ctx, NS, "peer", call), answer(&store, json!({"text": "hi"})));
    assert_eq!(got["text"], "salut");
    assert_eq!(got["data"], json!({"to": "en"}));
    let result = result.unwrap();
    assert_eq!(result.output["text"], "hi");
    assert_eq!(result.output["parts"], json!([{"text": "hi"}]));
}
