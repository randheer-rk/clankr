//! `clankrd rotate-master-key`: every secret is re-encrypted under the new key, the old key is
//! kept for older backups, and a rotation interrupted before or after its commit is settled.

use std::collections::BTreeMap;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::Command;

use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_store::secrets::MasterKey;
use clankr_types::RequestCtx;

fn ctx() -> RequestCtx {
    RequestCtx { actor: "owner".into(), request_id: "rotate".into() }
}

fn write_key(p: &Path, k: &MasterKey) {
    use std::io::Write;
    let _ = std::fs::remove_file(p);
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(p).unwrap().write_all(k.to_hex().as_bytes()).unwrap();
}

fn key(p: &Path) -> MasterKey {
    MasterKey::from_hex(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn rotate(url: &str, dir: &Path) {
    let o = Command::new(env!("CARGO_BIN_EXE_clankrd")).arg("--data-dir").arg(dir).arg("rotate-master-key").env("DATABASE_URL", url).env_remove("CLANKR_MASTER_KEY").output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}

#[tokio::test(flavor = "multi_thread")]
async fn rotation_rekeys_and_recovers() {
    let pg = DevPostgres::start(None).await.unwrap();
    let url = pg.database("clankr").await.unwrap();
    let store = Store::connect(&url).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let master = dir.path().join("master.key");
    let k1 = MasterKey::generate();
    write_key(&master, &k1);
    for (ns, name) in [("a", "llm"), ("b", "db")] {
        store.secret_put(&ctx(), &k1, ns, name, &BTreeMap::from([("v".into(), format!("{ns}-{name}"))])).await.unwrap();
    }
    let read = |k: MasterKey| {
        let store = store.clone();
        async move { store.secret_read(&ctx(), &k, "b", "db", "v", "test").await }
    };

    rotate(&url, dir.path());
    let k2 = key(&master);
    assert_eq!(read(k2.clone()).await.unwrap(), "b-db");
    assert!(read(k1.clone()).await.is_err(), "the old key no longer decrypts");
    let old = std::fs::read_dir(dir.path()).unwrap().filter_map(|e| e.ok()).find(|e| e.file_name().to_string_lossy().ends_with(".old")).expect("old key kept");
    assert_eq!(key(&old.path()).to_hex(), k1.to_hex());

    // died after the commit, before the swap: the next run finishes it (then rotates again)
    let k3 = MasterKey::generate();
    write_key(&dir.path().join("master.key.new"), &k3);
    store.secrets_rekey(&ctx(), &k2, &k3).await.unwrap();
    rotate(&url, dir.path());
    let k4 = key(&master);
    assert_eq!(read(k4.clone()).await.unwrap(), "b-db");
    assert!(read(k3).await.is_err());

    // died before the commit: the pending key is discarded and the current one still works
    write_key(&dir.path().join("master.key.new"), &MasterKey::generate());
    rotate(&url, dir.path());
    assert!(!dir.path().join("master.key.new").exists());
    assert_eq!(read(key(&master)).await.unwrap(), "b-db");
    assert_eq!(store.secrets_check(&key(&master)).await.unwrap(), (2, vec![]));
}
