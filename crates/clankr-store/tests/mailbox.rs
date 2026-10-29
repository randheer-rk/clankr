use std::collections::BTreeSet;

use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_store::mailbox::SendOpts;
use clankr_types::ErrorCode;
use serde_json::json;

async fn store() -> (DevPostgres, Store) {
    let pg = DevPostgres::start(None).await.unwrap();
    let s = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    (pg, s)
}

fn opts<'a>(dedup: Option<&'a str>, max: i32) -> SendOpts<'a> {
    SendOpts { sender: "agent/planner", correlation_id: Some("req-1"), dedup_key: dedup, ttl_secs: 3600, max_deliveries: max }
}

#[tokio::test]
async fn send_dedupe_receive_ack_fencing() {
    let (_pg, s) = store().await;
    let (a, dup) = s.mailbox_send("ns", "mb", &json!({"n": 1}), opts(Some("k1"), 3)).await.unwrap();
    assert!(!dup);
    let (b, dup) = s.mailbox_send("ns", "mb", &json!({"n": 999}), opts(Some("k1"), 3)).await.unwrap();
    assert!(dup && a.id == b.id && b.payload == json!({"n": 1}), "same dedup key -> same message, first payload wins");
    s.mailbox_send("ns", "mb", &json!({"n": 2}), opts(None, 3)).await.unwrap();
    s.mailbox_send("ns", "other", &json!({"n": 3}), opts(None, 3)).await.unwrap();

    let got = s.mailbox_receive("ns", "mb", "w1", 10, 30.0).await.unwrap();
    assert_eq!(got.iter().map(|m| m.payload["n"].as_i64().unwrap()).collect::<Vec<_>>(), [1, 2], "FIFO within a mailbox, other mailboxes separate");
    assert_eq!(got[0].sender, "agent/planner");
    assert!(s.mailbox_receive("ns", "mb", "w2", 10, 30.0).await.unwrap().is_empty(), "leased messages are invisible");

    s.mailbox_ack("ns", "mb", got[0].id, got[0].lease_epoch).await.unwrap();
    s.mailbox_ack("ns", "mb", got[0].id, got[0].lease_epoch).await.unwrap(); // duplicate ack: fine
    assert_eq!(s.mailbox_ack("ns", "mb", got[1].id, got[1].lease_epoch + 5).await.unwrap_err().code, ErrorCode::StaleLease);

    // the second delivery lapses: redelivered with a new epoch; the old holder is fenced
    sqlx::query("update mailbox_messages set lease_expires = now() - interval '1 second' where state = 'leased'").execute(s.pool()).await.unwrap();
    assert_eq!(s.mailbox_tick().await.unwrap().lapsed, 1);
    let again = s.mailbox_receive("ns", "mb", "w2", 10, 30.0).await.unwrap();
    assert_eq!((again[0].id, again[0].lease_epoch, again[0].deliveries), (got[1].id, got[1].lease_epoch + 1, 2));
    assert_eq!(s.mailbox_ack("ns", "mb", got[1].id, got[1].lease_epoch).await.unwrap_err().code, ErrorCode::StaleLease);
    s.mailbox_ack("ns", "mb", again[0].id, again[0].lease_epoch).await.unwrap();

    let big = json!("x".repeat(clankr_store::mailbox::MAX_PAYLOAD_BYTES));
    assert_eq!(s.mailbox_send("ns", "mb", &big, opts(None, 3)).await.unwrap_err().code, ErrorCode::QuotaExceeded);
}

#[tokio::test]
async fn nack_redelivers_then_dead_letters_and_ttl_expires() {
    let (_pg, s) = store().await;
    let (m, _) = s.mailbox_send("ns", "mb", &json!("poison"), opts(None, 2)).await.unwrap();
    let d1 = s.mailbox_receive("ns", "mb", "w", 1, 30.0).await.unwrap().remove(0);
    let back = s.mailbox_nack("ns", "mb", m.id, d1.lease_epoch, 0.0, "bad input").await.unwrap();
    assert_eq!(back.state, "ready");
    let d2 = s.mailbox_receive("ns", "mb", "w", 1, 30.0).await.unwrap().remove(0);
    let dead = s.mailbox_nack("ns", "mb", m.id, d2.lease_epoch, 0.0, "bad input").await.unwrap();
    assert_eq!((dead.state.as_str(), dead.dead_reason.as_deref()), ("dead", Some("max deliveries reached: bad input")));
    assert!(s.mailbox_receive("ns", "mb", "w", 1, 30.0).await.unwrap().is_empty());

    // operator requeues from the dead-letter list with a fresh budget
    s.mailbox_requeue("ns", "mb", m.id, 3600).await.unwrap();
    assert_eq!(s.mailbox_receive("ns", "mb", "w", 1, 30.0).await.unwrap()[0].deliveries, 1);

    // delayed redelivery honours not_before
    let (n, _) = s.mailbox_send("ns", "delay", &json!(1), opts(None, 5)).await.unwrap();
    let d = s.mailbox_receive("ns", "delay", "w", 1, 30.0).await.unwrap().remove(0);
    s.mailbox_nack("ns", "delay", n.id, d.lease_epoch, 3600.0, "later").await.unwrap();
    assert!(s.mailbox_receive("ns", "delay", "w", 1, 30.0).await.unwrap().is_empty());

    // TTL: expired messages are dead-lettered, not delivered
    s.mailbox_send("ns", "ttl", &json!(1), SendOpts { ttl_secs: 1, ..opts(None, 5) }).await.unwrap();
    sqlx::query("update mailbox_messages set expires_at = now() - interval '1 second' where mailbox = 'ttl'").execute(s.pool()).await.unwrap();
    assert!(s.mailbox_receive("ns", "ttl", "w", 1, 30.0).await.unwrap().is_empty());
    assert_eq!(s.mailbox_tick().await.unwrap().expired, 1);
    assert_eq!(s.mailbox_list("ns", "ttl", Some("dead"), 10).await.unwrap()[0].dead_reason.as_deref(), Some("ttl expired"));
}

/// No message is delivered to two receivers at once.
#[tokio::test]
async fn concurrent_receivers_never_share_a_message() {
    let (_pg, s) = store().await;
    for i in 0..60 {
        s.mailbox_send("ns", "mb", &json!(i), opts(None, 3)).await.unwrap();
    }
    let workers: Vec<_> = (0..6)
        .map(|w| {
            let s = s.clone();
            tokio::spawn(async move {
                let mut got = vec![];
                loop {
                    let batch = s.mailbox_receive("ns", "mb", &format!("w{w}"), 4, 30.0).await.unwrap();
                    if batch.is_empty() {
                        break got;
                    }
                    for m in batch {
                        s.mailbox_ack("ns", "mb", m.id, m.lease_epoch).await.unwrap();
                        got.push(m.payload.as_i64().unwrap());
                    }
                }
            })
        })
        .collect();
    let mut all = vec![];
    for w in workers {
        all.extend(w.await.unwrap());
    }
    assert_eq!(all.len(), 60);
    assert_eq!(all.into_iter().collect::<BTreeSet<_>>().len(), 60);
}
