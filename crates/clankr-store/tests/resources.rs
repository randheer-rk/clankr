use std::collections::BTreeMap;

use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_types::{ErrorCode, RequestCtx, Resource, WatchEventType};
use serde_json::json;

async fn store() -> (DevPostgres, Store) {
    let pg = DevPostgres::start(None).await.unwrap();
    let url = pg.database("clankr").await.unwrap();
    let store = Store::connect(&url).await.unwrap();
    (pg, store)
}

fn ctx() -> RequestCtx {
    RequestCtx { actor: "owner".into(), request_id: "req-1".into() }
}

fn agent(name: &str) -> Resource {
    serde_json::from_value(json!({
        "apiVersion": "clankr.io/v1alpha1", "kind": "Agent",
        "metadata": {"name": name, "namespace": "research", "labels": {"team": "a"}},
        "spec": {"image": "x@sha256:1"}
    }))
    .unwrap()
}

#[tokio::test]
async fn crud_cas_generation_and_status() {
    let (_pg, s) = store().await;

    let a = s.create(&ctx(), agent("one")).await.unwrap();
    assert_eq!(a.metadata.generation, 1);
    assert!(a.metadata.uid.is_some());
    assert_eq!(s.create(&ctx(), agent("one")).await.unwrap_err().code, ErrorCode::AlreadyExists);

    // label-only change keeps generation; spec change bumps it
    let mut b = a.clone();
    b.metadata.labels.insert("tier".into(), "worker".into());
    let b = s.update(&ctx(), b).await.unwrap();
    assert_eq!(b.metadata.generation, 1);
    assert!(b.metadata.resource_version > a.metadata.resource_version);
    let mut c = b.clone();
    c.spec = json!({"image": "x@sha256:2"});
    let c = s.update(&ctx(), c).await.unwrap();
    assert_eq!(c.metadata.generation, 2);

    // stale writer loses
    let mut stale = b.clone();
    stale.spec = json!({"image": "x@sha256:3"});
    assert_eq!(s.update(&ctx(), stale).await.unwrap_err().code, ErrorCode::Conflict);

    // status only through the status path, and a spec update can't clobber it
    let st = s.update_status(&ctx(), "research", "Agent", "one", c.metadata.resource_version, json!({"phase": "Ready"})).await.unwrap();
    let mut d = st.clone();
    d.status = Some(json!({"phase": "Hacked"}));
    let d = s.update(&ctx(), d).await.unwrap();
    assert_eq!(d.status, Some(json!({"phase": "Ready"})));

    let (list, _) = s.list("Agent", Some("research"), &BTreeMap::from([("tier".into(), "worker".into())])).await.unwrap();
    assert_eq!(list.len(), 1);
    let (list, _) = s.list("Agent", None, &BTreeMap::from([("tier".into(), "nope".into())])).await.unwrap();
    assert!(list.is_empty());

    s.delete(&ctx(), "research", "Agent", "one", None).await.unwrap();
    assert_eq!(s.get("research", "Agent", "one").await.unwrap_err().code, ErrorCode::NotFound);
}

#[tokio::test]
async fn finalizers_hold_deletion() {
    let (_pg, s) = store().await;
    let mut r = agent("held");
    r.metadata.finalizers = vec!["clankr.io/volumes".into()];
    s.create(&ctx(), r).await.unwrap();

    let t = s.delete(&ctx(), "research", "Agent", "held", None).await.unwrap();
    assert!(t.metadata.deletion_timestamp.is_some());
    let mut t = s.get("research", "Agent", "held").await.unwrap();
    assert!(t.metadata.deletion_timestamp.is_some());

    t.metadata.finalizers.clear();
    s.update(&ctx(), t).await.unwrap();
    assert_eq!(s.get("research", "Agent", "held").await.unwrap_err().code, ErrorCode::NotFound);
}

#[tokio::test]
async fn watch_resumes_from_list_revision_and_expires_after_compaction() {
    let (_pg, s) = store().await;
    s.create(&ctx(), agent("a")).await.unwrap();
    let (_, rev) = s.list("Agent", Some("research"), &BTreeMap::new()).await.unwrap();

    let b = s.create(&ctx(), agent("b")).await.unwrap();
    s.delete(&ctx(), "research", "Agent", "a", None).await.unwrap();

    let ch = s.changes_since(rev, Some("Agent"), Some("research"), 100).await.unwrap();
    let got: Vec<_> = ch.iter().map(|(_, e)| (e.kind, e.object.metadata.name.as_str())).collect();
    assert_eq!(got, [(WatchEventType::Added, "b"), (WatchEventType::Deleted, "a")]);
    assert!(ch.windows(2).all(|w| w[0].0 < w[1].0));

    s.compact(b.metadata.resource_version).await.unwrap();
    assert_eq!(s.changes_since(rev, None, None, 100).await.unwrap_err().code, ErrorCode::HistoryExpired);
    assert_eq!(s.changes_since(b.metadata.resource_version, None, None, 100).await.unwrap().len(), 1);
}

/// CL-19-03: N concurrent writers on the same prior version -> exactly one wins.
#[tokio::test]
async fn concurrent_cas_has_exactly_one_winner() {
    let (_pg, s) = store().await;
    let base = s.create(&ctx(), agent("race")).await.unwrap();
    let tasks: Vec<_> = (0..16)
        .map(|i| {
            let (s, mut r) = (s.clone(), base.clone());
            r.spec = json!({"image": format!("x@sha256:{i}")});
            tokio::spawn(async move { s.update(&ctx(), r).await })
        })
        .collect();
    let mut ok = 0;
    for t in tasks {
        match t.await.unwrap() {
            Ok(_) => ok += 1,
            Err(e) => assert_eq!(e.code, ErrorCode::Conflict),
        }
    }
    assert_eq!(ok, 1);
    assert_eq!(s.get("research", "Agent", "race").await.unwrap().metadata.generation, 2);
}

/// Watch cursor must never skip: concurrent writers produce a gap-free revision sequence.
#[tokio::test]
async fn revisions_are_gap_free_under_concurrency() {
    let (_pg, s) = store().await;
    let tasks: Vec<_> = (0..32).map(|i| {
        let s = s.clone();
        tokio::spawn(async move { s.create(&ctx(), agent(&format!("n{i}"))).await.unwrap() })
    }).collect();
    for t in tasks {
        t.await.unwrap();
    }
    let revs: Vec<i64> = s.changes_since(0, None, None, 1000).await.unwrap().into_iter().map(|(r, _)| r).collect();
    assert_eq!(revs, (1..=32).collect::<Vec<_>>());
}

#[tokio::test]
async fn every_mutation_is_audited_in_a_verifiable_chain() {
    let (_pg, s) = store().await;
    let a = s.create(&ctx(), agent("aud")).await.unwrap();
    s.update_status(&ctx(), "research", "Agent", "aud", a.metadata.resource_version, json!({"phase": "Ready"})).await.unwrap();
    // a failed write leaves no audit row
    assert!(s.create(&ctx(), agent("aud")).await.is_err());
    s.delete(&ctx(), "research", "Agent", "aud", None).await.unwrap();
    s.audit_deny(&RequestCtx { actor: "anonymous".into(), request_id: "r2".into() }, "authenticate", "bad token").await.unwrap();

    let log = s.audit_since(0, 100).await.unwrap();
    let got: Vec<_> = log.iter().map(|e| (e.action.as_str(), e.decision.as_str())).collect();
    assert_eq!(got, [("create", "allow"), ("update_status", "allow"), ("delete", "allow"), ("authenticate", "deny")]);
    assert_eq!(log[0].uid, a.metadata.uid);
    assert_eq!(s.verify_audit_chain().await.unwrap(), None);

    // append-only at the DB level
    assert!(sqlx::query("delete from audit_events").execute(s.pool()).await.is_err());
    assert!(sqlx::query("update audit_events set actor = 'x'").execute(s.pool()).await.is_err());

    // tampering (bypassing the trigger as a superuser would) is detected
    sqlx::query("alter table audit_events disable trigger audit_events_no_update").execute(s.pool()).await.unwrap();
    sqlx::query("update audit_events set actor = 'mallory' where seq = 2").execute(s.pool()).await.unwrap();
    assert_eq!(s.verify_audit_chain().await.unwrap(), Some(2));
}
