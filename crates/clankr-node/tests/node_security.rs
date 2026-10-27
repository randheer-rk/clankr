//! mTLS node identity and workload-identity tokens (SEC-04, CL-31-02, CL-31-03).

mod common;

use std::time::Duration;

use clankr_node::client::{Client, NodeTls};
use clankr_types::{RequestCtx, Resource};
use reqwest::StatusCode;
use serde_json::{Value, json};

fn res(kind: &str, name: &str, spec: Value) -> Resource {
    serde_json::from_value(json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": "research"}, "spec": spec})).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn nodes_authenticate_with_certs_and_act_only_under_live_workload_tokens() {
    let cp = common::start("owner-token", "join-secret", None).await;
    let ca = reqwest::Certificate::from_pem(cp.ca_pem.as_bytes()).unwrap();
    let cap = json!({"memoryMiB": 2048, "cpus": 2.0, "runtimes": ["process"]});

    let e = Client::join(&cp.base, &ca, "n1", "wrong", &cap, &json!({})).await.unwrap_err();
    assert_eq!(e.code, clankr_types::ErrorCode::Unauthenticated);
    let (id1, _) = Client::join(&cp.base, &ca, "n1", "join-secret", &cap, &json!({})).await.unwrap();
    Client::join(&cp.base, &ca, "n2", "join-secret", &cap, &json!({})).await.unwrap();
    let n1 = NodeTls { ca: ca.clone(), identity: Some(id1) };
    let http = n1.client(Duration::from_secs(10));

    // place research/hosted on n1 only
    let ctx = RequestCtx { actor: "owner".into(), request_id: "t".into() };
    for (k, n, spec) in [
        ("Agent", "hosted", json!({"runtime": {"type": "process", "command": ["true"]}, "resources": {"memoryMiB": 256}})),
        ("Agent", "other", json!({"runtime": {"type": "process", "command": ["true"]}})),
        ("AgentDeployment", "hosted", json!({"agent": "hosted", "replicas": 1, "nodeSelector": {}})),
    ] {
        cp.store.create(&ctx, res(k, n, spec)).await.unwrap();
    }
    sqlx::query("update nodes set ready = false where name = 'n2'").execute(cp.store.pool()).await.unwrap();
    let lead = cp.store.acquire_leadership("scheduler", "t", 30.0).await.unwrap().unwrap();
    clankrd::scheduler::reconcile(&cp.store, &lead).await.unwrap();

    let assignments = Client::node_heartbeat(&cp.base, &n1, "n1").await.unwrap();
    assert_eq!(assignments.len(), 1);
    let token = assignments[0].identity.clone();
    let url = |p: &str| format!("{}{p}", cp.base);
    let with = |rb: reqwest::RequestBuilder, t: &str| rb.header("x-clankr-workload", t.to_owned());

    // identity is the certificate: no bearer, no cert -> unauthenticated
    let anon = reqwest::Client::builder().tls_certs_only([ca.clone()]).build().unwrap();
    assert_eq!(anon.get(url("/v1/namespaces/research/agents/hosted")).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    assert_eq!(http.post(url("/v1/nodes/n2/heartbeat")).send().await.unwrap().status(), StatusCode::FORBIDDEN, "can't heartbeat as another node");

    // agent calls need the workload token, and it's bound to its agent
    assert_eq!(http.get(url("/v1/namespaces/research/agents/hosted")).send().await.unwrap().status(), StatusCode::FORBIDDEN);
    assert_eq!(with(http.get(url("/v1/namespaces/research/agents/hosted")), &token).send().await.unwrap().status(), StatusCode::OK);
    assert_eq!(with(http.get(url("/v1/namespaces/research/agents/other")), &token).send().await.unwrap().status(), StatusCode::FORBIDDEN);
    let lease = |q: &str| with(http.post(url("/v1/namespaces/research/tasks/lease")), &token).json(&json!({"queue": q, "owner": "x"})).send();
    assert_eq!(lease("agent/hosted").await.unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(lease("agent/other").await.unwrap().status(), StatusCode::FORBIDDEN);
    assert_eq!(lease("tool/publisher").await.unwrap().status(), StatusCode::FORBIDDEN);
    assert_eq!(with(http.get(url("/v1/namespaces/research/secrets")), &token).send().await.unwrap().status(), StatusCode::FORBIDDEN, "outside the node API");
    assert_eq!(with(http.get(url("/v1/nodes")), &token).send().await.unwrap().status(), StatusCode::FORBIDDEN);

    // mailboxes: the workload token isn't enough, the Agent spec must list the mailbox
    cp.store.create(&ctx, res("AgentMailbox", "jobs", json!({}))).await.unwrap();
    let send = with(http.post(url("/v1/namespaces/research/mailboxes/jobs/messages")), &token).json(&json!({"payload": 1})).send().await.unwrap();
    assert_eq!(send.status(), StatusCode::FORBIDDEN);
    assert_eq!(with(http.get(url("/v1/namespaces/research/mailboxes/jobs/messages")), &token).send().await.unwrap().status(), StatusCode::FORBIDDEN, "listing is owner-only");

    // a token from another signer is worthless
    let forged = clankrd::identity::Signer::derive("not-the-cluster-key").issue(&clankrd::identity::Claims {
        ns: "research".into(), agent: "hosted".into(), instance: assignments[0].id.clone(), node: "n1".into(), epoch: assignments[0].epoch, exp: i64::MAX,
    });
    assert_eq!(with(http.get(url("/v1/namespaces/research/agents/hosted")), &forged).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);

    // the instance moves (epoch bump): the old token is refused immediately, before it expires
    sqlx::query("update instances set epoch = epoch + 1").execute(cp.store.pool()).await.unwrap();
    let r = with(http.get(url("/v1/namespaces/research/agents/hosted")), &token).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CONFLICT);
    assert_eq!(r.json::<Value>().await.unwrap()["code"], "StaleLease");

    // renewal: own certificate only; the new one replaces the old immediately
    assert_eq!(Client::renew(&cp.base, &n1, "n2").await.unwrap_err().code, clankr_types::ErrorCode::Forbidden);
    let (renewed, pem) = Client::renew(&cp.base, &n1, "n1").await.unwrap();
    assert!(clankr_tls::not_after_pem(&pem).unwrap() > chrono::Utc::now().timestamp() + 29 * 86_400);
    assert_eq!(http.post(url("/v1/nodes/n1/heartbeat")).send().await.unwrap().status(), StatusCode::OK, "old cert overlaps briefly");
    sqlx::query("update nodes set prev_valid_until = now() - interval '1 second'").execute(cp.store.pool()).await.unwrap();
    assert_eq!(http.post(url("/v1/nodes/n1/heartbeat")).send().await.unwrap().status(), StatusCode::UNAUTHORIZED, "and is rejected once the overlap ends");
    let n1 = NodeTls { ca: ca.clone(), identity: Some(renewed) };
    let http = n1.client(Duration::from_secs(10));

    // re-joining issues a new certificate; the old one stops working
    let (id1b, _) = Client::join(&cp.base, &ca, "n1", "join-secret", &cap, &json!({})).await.unwrap();
    assert_eq!(http.post(url("/v1/nodes/n1/heartbeat")).send().await.unwrap().status(), StatusCode::UNAUTHORIZED, "old cert revoked");
    let fresh = NodeTls { ca, identity: Some(id1b) };
    assert_eq!(Client::node_heartbeat(&cp.base, &fresh, "n1").await.unwrap().len(), 1);

    let denials = cp.store.audit_since(0, 1000).await.unwrap().into_iter().filter(|e| e.decision == "deny").count();
    assert!(denials >= 3, "denials are audited");
}
