use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_types::ErrorCode;
use serde_json::json;

async fn store() -> (DevPostgres, Store) {
    let pg = DevPostgres::start(None).await.unwrap();
    let s = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    (pg, s)
}

#[tokio::test]
async fn versioned_writes_and_cas() {
    let (_pg, s) = store().await;
    let a = s.context_put("ns", "mem", "research/progress", &json!({"step": 1}), Some(0), "agent-a").await.unwrap();
    assert_eq!(a.version, 1);
    assert_eq!(s.context_put("ns", "mem", "research/progress", &json!({}), Some(0), "b").await.unwrap_err().code, ErrorCode::Conflict);

    let b = s.context_put("ns", "mem", "research/progress", &json!({"step": 2}), Some(1), "agent-b").await.unwrap();
    assert_eq!((b.version, b.writer.as_str()), (2, "agent-b"));
    assert_eq!(s.context_put("ns", "mem", "research/progress", &json!({"step": 9}), Some(1), "late").await.unwrap_err().code, ErrorCode::Conflict);
    assert_eq!(s.context_get("ns", "mem", "research/progress").await.unwrap().value, json!({"step": 2}));

    // unconditional write still versions
    assert_eq!(s.context_put("ns", "mem", "research/progress", &json!(3), None, "x").await.unwrap().version, 3);

    // claims and namespaces are separate keyspaces
    assert_eq!(s.context_get("ns", "other", "research/progress").await.unwrap_err().code, ErrorCode::NotFound);
    assert_eq!(s.context_get("ns2", "mem", "research/progress").await.unwrap_err().code, ErrorCode::NotFound);

    s.context_put("ns", "mem", "research/notes", &json!("n"), None, "x").await.unwrap();
    s.context_put("ns", "mem", "other", &json!("o"), None, "x").await.unwrap();
    let keys: Vec<_> = s.context_list("ns", "mem", "research/", 10).await.unwrap().into_iter().map(|o| o.key).collect();
    assert_eq!(keys, ["research/notes", "research/progress"]);

    assert_eq!(s.context_delete("ns", "mem", "other", Some(7)).await.unwrap_err().code, ErrorCode::Conflict);
    s.context_delete("ns", "mem", "other", Some(1)).await.unwrap();
    assert_eq!(s.context_delete("ns", "mem", "other", None).await.unwrap_err().code, ErrorCode::NotFound);

    let big = json!("x".repeat(clankr_store::context::MAX_VALUE_BYTES));
    assert_eq!(s.context_put("ns", "mem", "big", &big, None, "x").await.unwrap_err().code, ErrorCode::QuotaExceeded);
}

/// CL-30-07: concurrent agents updating a shared blackboard get CAS failures, never lost updates.
#[tokio::test]
async fn concurrent_increments_lose_nothing() {
    let (_pg, s) = store().await;
    s.context_put("ns", "board", "counter", &json!(0), Some(0), "init").await.unwrap();
    let agents: Vec<_> = (0..8)
        .map(|a| {
            let s = s.clone();
            tokio::spawn(async move {
                let mut conflicts = 0;
                for _ in 0..10 {
                    loop {
                        let cur = s.context_get("ns", "board", "counter").await.unwrap();
                        let next = json!(cur.value.as_i64().unwrap() + 1);
                        match s.context_put("ns", "board", "counter", &next, Some(cur.version), &format!("a{a}")).await {
                            Ok(_) => break,
                            Err(e) if e.code == ErrorCode::Conflict => conflicts += 1,
                            Err(e) => panic!("{e}"),
                        }
                    }
                }
                conflicts
            })
        })
        .collect();
    let mut conflicts = 0;
    for a in agents {
        conflicts += a.await.unwrap();
    }
    let c = s.context_get("ns", "board", "counter").await.unwrap();
    assert_eq!(c.value, json!(80));
    assert_eq!(c.version, 81);
    eprintln!("{conflicts} CAS conflicts retried");
}
