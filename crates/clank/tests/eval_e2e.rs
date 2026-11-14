//! `clank eval` end to end (CL-34-08): a suite passes against a good agent and writes a report;
//! the same suite fails the gate, naming the regressed case, when the agent gets worse.

use std::path::Path;
use std::process::{Child, Command, Output};

use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use serde_json::{Value, json};

const OWNER: &str = "owner-token";
const NS: &str = "eval";

struct Cluster {
    base: String,
    _pg: DevPostgres,
    _rt: tokio::runtime::Runtime,
}

fn cluster() -> Cluster {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (pg, base) = rt.block_on(async {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let pg = DevPostgres::start(None).await.unwrap();
        let store = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
        let state = clankrd::AppState::new(store, OWNER.into(), clankr_store::secrets::MasterKey::generate()).await.unwrap();
        clankrd::spawn_ticker(state.engine.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = clankrd::router(state);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (pg, base)
    });
    Cluster { base, _pg: pg, _rt: rt }
}

fn clank(c: &Cluster, token: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_clank"));
    cmd.args(args).env("CLANKR_SERVER", &c.base).env("CLANKR_TOKEN", token).env_remove("CLANKR_EVAL_APPROVER_TOKEN");
    cmd
}

fn ok(o: Output) -> String {
    assert!(o.status.success(), "clank failed:\n{}\n{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    String::from_utf8(o.stdout).unwrap()
}

/// A dev worker answering the queue with a small python program (`good` picks which).
fn worker(c: &Cluster, token: &str, good: bool) -> Child {
    let prog = if good {
        r#"import json,sys; i=json.load(sys.stdin)["input"]; print(json.dumps({"answer": i["q"].upper(), "sources": 3}))"#
    } else {
        r#"import json,sys; i=json.load(sys.stdin)["input"]; print(json.dumps({"answer": "I cannot help with that", "sources": 0}))"#
    };
    clank(c, token, &["worker", "-n", NS, "-q", "agent/answerer", "--exec", &format!("python3 -c '{prog}'"), "--max-tasks", "3"]).spawn().unwrap()
}

#[test]
fn eval_passes_then_gates_a_regression() {
    let c = cluster();
    let dir = tempfile::tempdir().unwrap();
    // the runner (ci) starts runs; a separate reviewer approves them
    let setup = format!(
        "apiVersion: clankr.io/v1alpha1\nkind: Workflow\nmetadata: {{name: answer, namespace: {NS}}}\nspec:\n  entrypoint: answer\n  nodes:\n    answer: {{type: agent, agentRef: answerer, next: review}}\n    review: {{type: approval, approverGroup: reviewers}}\n---\n\
         apiVersion: clankr.io/v1alpha1\nkind: ServiceAccount\nmetadata: {{name: ci, namespace: {NS}}}\nspec: {{}}\n---\n\
         apiVersion: clankr.io/v1alpha1\nkind: ServiceAccount\nmetadata: {{name: reviewer, namespace: {NS}}}\nspec: {{groups: [reviewers]}}\n---\n\
         apiVersion: clankr.io/v1alpha1\nkind: RoleBinding\nmetadata: {{name: ci, namespace: {NS}}}\nspec: {{role: editor, subjects: ['serviceaccount:{NS}/ci', 'serviceaccount:{NS}/reviewer']}}\n---\n\
         apiVersion: clankr.io/v1alpha1\nkind: RoleBinding\nmetadata: {{name: reviewers, namespace: {NS}}}\nspec: {{role: approver, subjects: ['group:reviewers']}}\n"
    );
    std::fs::write(dir.path().join("setup.yaml"), setup).unwrap();
    ok(clank(&c, OWNER, &["apply", "-f", dir.path().join("setup.yaml").to_str().unwrap()]).output().unwrap());
    let ci = ok(clank(&c, OWNER, &["token", "-n", NS, "create", "ci"]).output().unwrap()).trim().to_owned();
    let reviewer = ok(clank(&c, OWNER, &["token", "-n", NS, "create", "reviewer"]).output().unwrap()).trim().to_owned();

    let suite = dir.path().join("suite.yaml");
    std::fs::write(
        &suite,
        format!(
            r#"name: answers
namespace: {NS}
workflow: answer
autoApprove: true
timeoutSeconds: 60
thresholds: {{passRate: 0.6, maxP95Seconds: 30}}
cases:
  - name: shouts-back
    input: {{q: "hello"}}
    expect:
      - {{path: "$.answer.answer", equals: "HELLO"}}
      - {{path: "$.answer.sources", gte: 1}}
  - name: cites-sources
    input: {{q: "why"}}
    expect:
      - {{path: "$.answer.sources", gte: 2}}
  - name: no-refusal
    input: {{q: "help"}}
    expect:
      - {{path: "$.answer.answer", notContains: "cannot"}}
"#
        ),
    )
    .unwrap();
    let (report, eval) = (dir.path().join("baseline.json"), |token: &str, extra: &[&str]| {
        let mut args = vec!["eval", "run", suite.to_str().unwrap(), "--approver-token", &reviewer];
        args.extend_from_slice(extra);
        clank(&c, token, &args).output().unwrap()
    });

    // running as the reviewer itself must not be able to approve its own runs
    let mut w = worker(&c, &ci, true);
    let own = clank(&c, &reviewer, &["eval", "run", suite.to_str().unwrap()]).output().unwrap();
    assert!(!own.status.success());
    assert!(String::from_utf8_lossy(&own.stdout).contains("auto-approve"), "{}", String::from_utf8_lossy(&own.stdout));
    let _ = w.kill();
    let _ = w.wait();

    // good agent: all pass, report written
    let mut w = worker(&c, &ci, true);
    let out = ok(eval(&ci, &["--report", report.to_str().unwrap()]));
    let _ = w.wait();
    assert!(out.contains("3/3 passed") && out.contains("gate: passed"), "{out}");
    let base: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert_eq!(base["passRate"], json!(1.0));
    assert!(base["cases"].as_array().unwrap().iter().all(|c| c["runId"].is_string()));

    // regressed agent: the threshold (60%) still could be met by luck, but the baseline catches it
    let mut w = worker(&c, &ci, false);
    let bad = eval(&ci, &["--baseline", report.to_str().unwrap()]);
    let _ = w.wait();
    assert!(!bad.status.success(), "regression must fail the gate");
    let (stdout, stderr) = (String::from_utf8_lossy(&bad.stdout), String::from_utf8_lossy(&bad.stderr));
    assert!(stdout.contains("FAIL shouts-back") && stdout.contains("0/3 passed"), "{stdout}");
    assert!(stderr.contains("now fail: shouts-back, cites-sources, no-refusal") && stderr.contains("dropped from 100% to 0%"), "{stderr}");
    assert!(Path::new(&report).exists());
}
