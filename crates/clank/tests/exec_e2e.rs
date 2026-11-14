//! `clank exec` end to end (ADR 0030): a user with admin runs a command inside an agent's
//! container on a fleet node and gets its output, stdin and exit status back; a viewer is
//! refused; the exec is audited. Skips when kennel or node:24-slim isn't available.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use clankr_node::fleet::{FleetConfig, run_fleet};
use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use futures_util::StreamExt;
use serde_json::{Value, json};

const OWNER: &str = "owner-token";
const NS: &str = "ex";
const NODE_BASE: &str = "docker.io/library/node@sha256:a43f7fc3e52e87d12a905f46eea0275def84ca8f2981a5b53fed44d7fb398ee0";

fn kennel(bin: &Path, root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new(bin).arg("--root").arg(root).args(args).output().ok().filter(|o| o.status.success())?;
    Some(String::from_utf8(out.stdout).ok()?.trim().to_owned())
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_into_an_agent_container() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    let kbin = std::env::var_os("KENNEL").map(PathBuf::from).unwrap_or_else(|| repo.join("../kennel/target/debug/kennel"));
    let kroot = repo.join("target/kennel-test-store");
    if !kbin.exists() || (kennel(&kbin, &kroot, &["inspect", NODE_BASE]).is_none() && kennel(&kbin, &kroot, &["pull", NODE_BASE]).is_none()) {
        eprintln!("SKIPPED: kennel or node:24-slim unavailable");
        return;
    }
    let base = kennel(&kbin, &kroot, &["build", "-t", "clankr/agent-node:test", repo.join("sdk/typescript").to_str().unwrap()]).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let ctx_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&ctx_dir).unwrap();
    std::fs::write(ctx_dir.join("agent.ts"), "import { AgentApp } from \"@clankr/sdk\"\nnew AgentApp().run()\n").unwrap();
    std::fs::write(ctx_dir.join("Kennelfile"), format!("FROM clankr/agent-node@{}\nCOPY agent.ts /app/agent.ts\nAGENT_ENTRYPOINT [\"node\", \"/app/agent.ts\"]\n", base.rsplit_once('@').unwrap().1)).unwrap();
    let image = kennel(&kbin, &kroot, &["build", "-t", "ex/idle:test", ctx_dir.to_str().unwrap()]).unwrap();

    let _ = tracing_subscriber::fmt().with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "clankr_node=info,clankrd=warn".into())).try_init();
    // control plane over TLS (nodes join with mTLS)
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pg = DevPostgres::start(None).await.unwrap();
    let store = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    let master = clankr_store::secrets::MasterKey::generate();
    // two replicas on one database: the node uses one, the user the other, so every session
    // crosses between them
    let pki = tempfile::tempdir().unwrap();
    let ca = Arc::new(clankr_tls::Ca::load_or_create(pki.path()).unwrap());
    let (cert, key) = ca.server_cert(&["localhost".into(), "127.0.0.1".into()]).unwrap();
    let tls = ca.server_config(&cert, &key).unwrap();
    let ca_pem = ca.cert_pem.clone();
    let ca_file = pki.path().join("ca.pem");
    std::fs::write(&ca_file, &ca_pem).unwrap();
    clankrd::scheduler::spawn(store.clone(), "test-scheduler".into(), Duration::from_millis(300));
    let mut replicas = vec![];
    for _ in 0..2 {
        let mut state = clankrd::AppState::with_join_token(store.clone(), OWNER.into(), master.clone(), "join".into()).await.unwrap();
        state.ca = Some(ca.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        replicas.push(format!("https://127.0.0.1:{}", l.local_addr().unwrap().port()));
        let (app, tls) = (clankrd::router(state), tls.clone());
        tokio::spawn(async move { clankrd::tls::serve_tls(l, app, tls).await.unwrap() });
    }
    let (addr, node_addr) = (replicas[0].clone(), replicas[1].clone());

    let clank = {
        let (addr, ca_file) = (addr.clone(), ca_file.clone());
        move |token: &str, args: &[&str], stdin: &str| {
            let mut c = Command::new(env!("CARGO_BIN_EXE_clank"));
            c.args(args).env("CLANKR_SERVER", &addr).env("CLANKR_TOKEN", token).env("CLANKR_CA", &ca_file);
            c.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
            let mut child = c.spawn().unwrap();
            use std::io::Write;
            child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
            child.wait_with_output().unwrap()
        }
    };
    let res = |kind: &str, name: &str, spec: Value| json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec});
    let rctx = clankr_types::RequestCtx { actor: "owner".into(), request_id: "ex".into() };
    for r in [
        res("Agent", "idle", json!({"runtime": {"type": "oci", "image": format!("ex/idle@{}", image.rsplit_once('@').unwrap().1)}, "resources": {"memoryMiB": 256, "cpus": 0.5, "pids": 64}})),
        res("AgentDeployment", "idle", json!({"agent": "idle", "replicas": 1})),
        res("ServiceAccount", "dev", json!({})),
        res("ServiceAccount", "watcher", json!({})),
        res("RoleBinding", "dev-admin", json!({"role": "admin", "subjects": ["serviceaccount:ex/dev"]})),
        res("RoleBinding", "watcher-view", json!({"role": "viewer", "subjects": ["serviceaccount:ex/watcher"]})),
    ] {
        store.create(&rctx, serde_json::from_value(r).unwrap()).await.unwrap();
    }
    let (stop, rx) = tokio::sync::watch::channel(false);
    let node = tokio::spawn(run_fleet(
        FleetConfig {
            server: node_addr,
            ca_pem,
            name: "exec-node".into(),
            join_token: "join".into(),
            capacity: json!({"memoryMiB": 2048, "cpus": 2.0, "runtimes": ["oci"]}),
            labels: json!({}),
            work_dir: tmp.path().join("node"),
            kennel: kbin,
            kennel_root: Some(kroot),
            heartbeat: Duration::from_millis(500),
            fence_after: Duration::from_secs(30),
            renew_before: Duration::from_secs(86_400),
        },
        rx,
    ));

    let http = reqwest::Client::builder().add_root_certificate(reqwest::Certificate::from_pem(&std::fs::read(&ca_file).unwrap()).unwrap()).build().unwrap();
    let token = |sa: &str| {
        let (http, addr) = (http.clone(), addr.clone());
        let url = format!("{addr}/v1/namespaces/{NS}/serviceaccounts/{sa}/tokens");
        async move { http.post(url).bearer_auth(OWNER).json(&json!({})).send().await.unwrap().json::<Value>().await.unwrap()["token"].as_str().unwrap().to_owned() }
    };
    let (dev, watcher) = (token("dev").await, token("watcher").await);
    let mut ready = false;
    for _ in 0..240 {
        let v: Value = http.get(format!("{addr}/v1/namespaces/{NS}/instances")).bearer_auth(&dev).send().await.unwrap().json().await.unwrap();
        if v["items"].as_array().is_some_and(|i| i.iter().any(|i| i["ready"] == true)) {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(ready, "the agent instance never became ready");

    let (ok, viewer) = tokio::task::spawn_blocking({
        let clank = clank.clone();
        move || {
            let ok = clank(&dev, &["exec", "-n", NS, "idle", "--", "sh", "-c", "read l; echo \"got $l as $(id -u)\"; ls /app; exit 7"], "hello\n");
            let viewer = clank(&watcher, &["exec", "-n", NS, "idle", "--", "true"], "");
            (ok, viewer)
        }
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&ok.stdout);
    assert_eq!(ok.status.code(), Some(7), "exit status passes through: {stdout} {}", String::from_utf8_lossy(&ok.stderr));
    assert!(stdout.contains("got hello as 65534") && stdout.contains("agent.ts"), "{stdout}");
    assert!(!viewer.status.success() && String::from_utf8_lossy(&viewer.stderr).contains("lacks InstancesExec"), "viewer refused: {}", String::from_utf8_lossy(&viewer.stderr));

    // a terminal session resized after it starts: the new size reaches the sandbox's pty
    let dev = token("dev").await;
    let inst = {
        let v: Value = http.get(format!("{addr}/v1/namespaces/{NS}/instances")).bearer_auth(&dev).send().await.unwrap().json().await.unwrap();
        v["items"][0]["id"].as_str().unwrap().to_owned()
    };
    let r: Value = http.post(format!("{addr}/v1/namespaces/{NS}/instances/{inst}/exec")).bearer_auth(&dev).json(&json!({"command": ["sh", "-c", "sleep 1; stty size"], "tty": true, "rows": 24, "cols": 80})).send().await.unwrap().json().await.unwrap();
    let session = r["session"].as_str().unwrap().to_owned();
    let input = {
        let (http, url, dev) = (http.clone(), format!("{addr}/v1/namespaces/{NS}/exec/{session}/input"), dev.clone());
        // the resize, then hold the input open while the command runs
        let frames = futures_util::stream::iter([Ok::<_, std::io::Error>(vec![4u8, 0, 0, 0, 4, 0, 40, 0, 100])]).chain(futures_util::stream::once(async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(vec![])
        }));
        tokio::spawn(async move { http.post(url).bearer_auth(dev).body(reqwest::Body::wrap_stream(frames)).send().await })
    };
    let out = http.get(format!("{addr}/v1/namespaces/{NS}/exec/{session}/output")).bearer_auth(&dev).send().await.unwrap().bytes().await.unwrap();
    input.abort();
    let (mut text, mut code, mut rest) = (String::new(), None, &out[..]);
    while rest.len() >= 5 {
        let n = u32::from_be_bytes([rest[1], rest[2], rest[3], rest[4]]) as usize;
        match rest[0] {
            1 => text.push_str(&String::from_utf8_lossy(&rest[5..5 + n])),
            2 => code = Some(i32::from_be_bytes(rest[5..9].try_into().unwrap())),
            _ => {}
        }
        rest = &rest[5 + n..];
    }
    assert_eq!((text.trim(), code), ("40 100", Some(0)), "resize reached the sandbox's terminal");

    let audit: Value = http.get(format!("{addr}/v1/audit")).bearer_auth(OWNER).send().await.unwrap().json().await.unwrap();
    let row = audit.as_array().unwrap().iter().find(|e| e["action"].as_str().is_some_and(|a| a.starts_with("exec "))).expect("exec audited");
    assert_eq!(row["actor"], "serviceaccount:ex/dev", "{row}");
    assert!(row["action"].as_str().unwrap().contains("read l"), "{row}");

    stop.send(true).unwrap();
    let _ = node.await;
}

