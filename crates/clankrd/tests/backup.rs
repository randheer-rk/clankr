//! Restore drill (PRD §37, DR-01): back up a live cluster while it's being written to, restore
//! into a fresh database, prove the copy is exact and usable, and refuse the unsafe cases.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clankr_engine::{Decider, Decision, Engine, RunStatus};
use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_store::secrets::MasterKey;
use clankr_types::{RequestCtx, Resource};
use clankrd::backup::{self, BackupOpts, RestoreOpts};
use serde_json::{Value, json};

const NS: &str = "drill";

fn ctx(actor: &str) -> RequestCtx {
    RequestCtx { actor: actor.into(), request_id: "drill".into() }
}

fn res(kind: &str, name: &str, spec: Value) -> Resource {
    serde_json::from_value(json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn backup_restore_drill() {
    let pg = DevPostgres::start(None).await.unwrap();
    let bin = pg.bin_dir();
    let url = pg.database("clankr").await.unwrap();
    let store = Store::connect(&url).await.unwrap();
    let master = MasterKey::generate();
    let data = tempfile::tempdir().unwrap();
    let ca = clankr_tls::Ca::load_or_create(&data.path().join("pki")).unwrap();

    // state worth restoring: a workflow paused at an approval, a secret, an audit trail
    store
        .create(&ctx("owner"), res("Workflow", "ship", json!({"entrypoint": "gate", "nodes": {"gate": {"type": "approval", "approverGroup": "eds", "next": "go"}, "go": {"type": "agent", "agentRef": "shipper"}}})))
        .await
        .unwrap();
    store.secret_put(&ctx("owner"), &master, NS, "llm", &BTreeMap::from([("apiKey".into(), "sk-drill".into())])).await.unwrap();
    let engine = Engine::new(store.clone());
    let run = engine.start(&ctx("serviceaccount:drill/ci"), NS, "ship", json!({"topic": "drill"}), None).await.unwrap().run_id;

    // writers keep going while the backup runs; the snapshot must still be exact
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (store, stop) = (store.clone(), stop.clone());
        tokio::spawn(async move {
            let mut i = 0;
            while !stop.load(Ordering::Relaxed) {
                store.create(&ctx("owner"), res("AgentConfig", &format!("c{i}"), json!({"n": i}))).await.unwrap();
                i += 1;
            }
            i
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let out = data.path().join("backup-1");
    let m = backup::backup(BackupOpts { database_url: &url, out: &out, master: &master, data_dir: data.path(), include_keys: false, pg_bin: Some(&bin) }).await.unwrap();
    stop.store(true, Ordering::Relaxed);
    let written = writer.await.unwrap();
    assert!(written > 0 && m.rows["resources"] < 1 + written as i64 + 1, "backup landed mid-stream ({} of {written} configs)", m.rows["resources"]);
    assert!(!out.join("master.key").exists() && out.join("ca.crt").exists(), "keys excluded by default, CA cert included");
    assert_eq!(m.schema_version, Store::schema_version());

    // the backup alone verifies against the live DB only at the snapshot; live has moved on
    let live = backup::verify(&url, &master, &m).await.unwrap();
    assert!(!live.ok(), "live database has diverged from the snapshot");

    // refusals: wrong key, damaged dump, non-empty target
    let target = pg.database("restored").await.unwrap();
    let restore_dir = tempfile::tempdir().unwrap();
    let opts = |dir, master| RestoreOpts { dir, database_url: &target, master, data_dir: restore_dir.path(), pg_bin: Some(&bin) };
    let wrong = MasterKey::generate();
    let e = backup::restore(opts(&out, &wrong)).await.unwrap_err();
    assert!(e.to_string().contains("not the one the backup was taken with"), "{e}");
    let damaged = data.path().join("damaged");
    std::fs::create_dir(&damaged).unwrap();
    for f in ["manifest.json", "db.dump"] {
        std::fs::copy(out.join(f), damaged.join(f)).unwrap();
    }
    let mut bytes = std::fs::read(damaged.join("db.dump")).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xff;
    std::fs::write(damaged.join("db.dump"), bytes).unwrap();
    let e = backup::restore(opts(&damaged, &master)).await.unwrap_err();
    assert!(e.to_string().contains("checksum"), "{e}");
    let e = backup::restore(RestoreOpts { dir: &out, database_url: &url, master: &master, data_dir: restore_dir.path(), pg_bin: Some(&bin) }).await.unwrap_err();
    assert!(e.to_string().contains("not empty"), "{e}");

    // the real restore: exact, verified, and carrying the CA over
    let report = backup::restore(opts(&out, &master)).await.unwrap();
    assert!(report.ok(), "{report:#?}");
    let names: Vec<&str> = report.checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["schema", "revision", "audit head", "row counts", "audit chain", "secrets", "run replay"]);
    assert_eq!(std::fs::read_to_string(restore_dir.path().join("pki/ca.crt")).unwrap(), ca.cert_pem);
    assert!(restore_dir.path().join("master.key").exists(), "escrowed key installed for the restored daemon");

    // and usable: the restored run is still waiting, an editor approves, the secret decrypts
    let restored = Store::connect(&target).await.unwrap();
    let engine2 = Engine::new(restored.clone());
    let pending = engine2.approvals(NS, Some("pending"), 10).await.unwrap();
    assert_eq!(pending.len(), 1);
    let ed = ctx("user:ed");
    let groups = vec!["eds".to_owned()];
    engine2.decide_approval(&Decider { ctx: &ed, groups: &groups, break_glass: false }, NS, pending[0].id, Decision::Approve, &pending[0].digest, "after restore").await.unwrap();
    let after = engine2.get(NS, run).await.unwrap();
    assert_eq!(after.status, RunStatus::Running);
    assert!(after.state.tasks.values().any(|t| t.node == "go"), "run moved past the gate on the restored database");
    assert_eq!(restored.secret_read(&ctx("owner"), &master, NS, "llm", "apiKey", "drill").await.unwrap(), "sk-drill");

    // tampering with history in the restored copy is caught
    sqlx::query("alter table audit_events disable trigger user").execute(restored.pool()).await.unwrap();
    sqlx::query("update audit_events set actor = 'mallory' where seq = 2").execute(restored.pool()).await.unwrap();
    let r = backup::verify(&target, &master, &m).await.unwrap();
    let chain = r.checks.iter().find(|c| c.name == "audit chain").unwrap();
    assert!(!chain.ok && chain.detail.contains("seq 2"), "{chain:?}");
}
