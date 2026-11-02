//! The TypeScript and Go SDKs against a real node and control plane: handshake, delivery,
//! context with optimistic versions, models, tools (derived idempotency keys), capability
//! denial, and Permanent failures. Each part skips when its toolchain (node / go) is missing.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use clankr_node::client::Client;
use clankr_node::daemon::{Config, run_instance};
use clankr_store::Store;
use clankr_store::embedded::DevPostgres;
use clankr_types::{RequestCtx, Resource};
use serde_json::{Value, json};

const TOKEN: &str = "sdk-test-token";
const NS: &str = "sdks";

fn ctx() -> RequestCtx {
    RequestCtx { actor: "owner".into(), request_id: "sdk".into() }
}

fn sdk(lang: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sdk").join(lang).canonicalize().unwrap()
}

fn have(bin: &str, arg: &str) -> bool {
    Command::new(bin).arg(arg).output().is_ok_and(|o| o.status.success())
}

const TS_AGENT: &str = r#"
import { AgentApp, ClankrError, Permanent } from "SDK/src/index.ts"
const app = new AgentApp()
app.task("work", async (task, ctx) => {
  const mem = ctx.context("memory")
  const prev = await mem.get("n")
  const saved = await mem.put("n", (prev?.value ?? 0) + 1, { ifVersion: prev?.version ?? 0 })
  const reply = await ctx.models.generate("default", [{ role: "user", content: task.input.input.topic }])
  const tool = await ctx.tools.call("t", "search", "query", { q: "x" })
  let denied = null
  try { await ctx.models.generate("nope", [{ role: "user", content: "x" }]) } catch (e) { denied = (e as ClankrError).code }
  return { sdk: "ts", text: reply.text, version: saved.version, tool: tool.output, denied }
})
app.task("boom", async () => { throw new Permanent("bad input") })
app.run()
"#;

const GO_AGENT: &str = r#"
package main

import (
	"context"
	"encoding/json"
	"errors"

	clankr "clankr.dev/sdk"
)

func main() {
	app := clankr.New()
	app.Task("work", func(ctx context.Context, t *clankr.Task, c *clankr.Client) (any, error) {
		var in struct {
			Input struct {
				Topic string `json:"topic"`
			} `json:"input"`
		}
		_ = json.Unmarshal(t.Input, &in)
		mem := c.Context("memory")
		prev, err := mem.Get(ctx, "n")
		if err != nil {
			return nil, err
		}
		n, ifv := 0, int64(0)
		if prev != nil {
			_ = json.Unmarshal(prev.Value, &n)
			ifv = prev.Version
		}
		saved, err := mem.Put(ctx, "n", n+1, &ifv)
		if err != nil {
			return nil, err
		}
		reply, err := c.Generate(ctx, "default", []clankr.Message{{Role: "user", Content: in.Input.Topic}}, nil)
		if err != nil {
			return nil, err
		}
		tool, err := c.CallTool(ctx, "t", "search", "query", map[string]any{"q": "x"}, nil)
		if err != nil {
			return nil, err
		}
		var denied *string
		var ce *clankr.Error
		if _, err := c.Generate(ctx, "nope", []clankr.Message{{Role: "user", Content: "x"}}, nil); errors.As(err, &ce) {
			denied = &ce.Code
		}
		return map[string]any{"sdk": "go", "text": reply.Text, "version": saved.Version, "tool": tool.Output, "denied": denied}, nil
	})
	app.Task("boom", func(ctx context.Context, t *clankr.Task, c *clankr.Client) (any, error) {
		return nil, clankr.Permanent(errors.New("bad input"))
	})
	app.Run()
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn typescript_and_go_agents_run_workflows_through_the_node() {
    let (has_node, has_go) = (have("node", "--version"), have("go", "version"));
    if !has_node && !has_go {
        eprintln!("SKIPPED: neither node nor go is installed");
        return;
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pg = DevPostgres::start(None).await.unwrap();
    let store = Store::connect(&pg.database("clankr").await.unwrap()).await.unwrap();
    let state = clankrd::AppState::new(store.clone(), TOKEN.into(), clankr_store::secrets::MasterKey::generate()).await.unwrap();
    clankrd::spawn_ticker(state.engine.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, clankrd::router(state)).await.unwrap() });
    let tmp = tempfile::tempdir().unwrap();
    let apply = |kind: &str, name: &str, spec: Value| {
        let (store, r) = (store.clone(), serde_json::from_value::<Resource>(json!({"apiVersion": "clankr.io/v1alpha1", "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec})).unwrap());
        async move { store.create(&ctx(), r).await.unwrap() }
    };
    apply("ProviderConnection", "mock", json!({"provider": "mock", "allowedModels": ["mock-1"]})).await;
    apply("ModelClaim", "default", json!({"providerConnection": "mock", "model": "mock-1"})).await;
    apply("ModelClaim", "nope", json!({"providerConnection": "mock", "model": "mock-1"})).await;
    apply("ContextClaim", "memory", json!({})).await;
    apply("Tool", "search", json!({"version": "1", "executor": {"type": "mock", "responses": {"query": {"hits": 3}}}, "methods": {"query": {"sideEffects": "readOnly"}}})).await;
    apply("ToolClaim", "t", json!({"allow": [{"tool": "search"}]})).await;

    let mut agents = vec![];
    if has_node {
        let path = tmp.path().join("agent.ts");
        std::fs::write(&path, TS_AGENT.replace("SDK", sdk("typescript").to_str().unwrap())).unwrap();
        agents.push(("ts", json!(["node", path])));
    } else {
        eprintln!("SKIPPED (TypeScript): node not installed");
    }
    if has_go {
        let dir = tmp.path().join("goagent");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.go"), GO_AGENT).unwrap();
        std::fs::write(dir.join("go.mod"), format!("module agent\n\ngo 1.24\n\nrequire clankr.dev/sdk v0.0.0\n\nreplace clankr.dev/sdk => {}\n", sdk("go").display())).unwrap();
        let bin = dir.join("agent");
        let out = Command::new("go").arg("build").arg("-o").arg(&bin).arg(".").current_dir(&dir).env("GOFLAGS", "-mod=mod").env("GOPROXY", "off").output().unwrap();
        assert!(out.status.success(), "go build: {}", String::from_utf8_lossy(&out.stderr));
        agents.push(("go", json!([bin])));
    } else {
        eprintln!("SKIPPED (Go): go not installed");
    }

    let engine = clankr_engine::Engine::new(store.clone());
    let wait = |run_id: uuid::Uuid| {
        let engine = engine.clone();
        async move {
            for _ in 0..300 {
                let r = engine.get(NS, run_id).await.unwrap();
                if r.status.is_terminal() {
                    return r;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            panic!("run {run_id} did not finish");
        }
    };
    let mut stops = vec![];
    for (i, (lang, cmd)) in agents.iter().enumerate() {
        let agent = format!("{lang}-agent");
        apply("Agent", &agent, json!({"runtime": {"type": "process", "command": cmd}, "models": ["default"], "contexts": ["memory"], "tools": ["t"], "taskTtlSeconds": 5})).await;
        apply("Workflow", &format!("wf-{lang}"), json!({"entrypoint": "work", "nodes": {"work": {"type": "agent", "agentRef": agent, "next": "boom"}, "boom": {"type": "agent", "agentRef": agent}}})).await;
        let (tx, rx) = tokio::sync::watch::channel(false);
        let cfg = Config {
            agent: agent.clone(),
            namespace: NS.into(),
            work_dir: tmp.path().join("node"),
            instance: format!("{agent}-i1"),
            kennel: "kennel".into(),
            kennel_root: None,
            ready: None,
            spec: None,
            hibernate: None,
            checkpoint: None,
        };
        tokio::spawn(run_instance(Client::new(&base, TOKEN, NS), cfg, rx));
        stops.push(tx);

        let run = engine.start(&ctx(), NS, &format!("wf-{lang}"), json!({"topic": "sdks"}), None).await.unwrap();
        let r = wait(run.run_id).await;
        let work = &r.state.data["work"];
        assert_eq!(work["sdk"], *lang, "{lang}: {work}");
        assert!(!work["text"].as_str().unwrap_or_default().is_empty(), "{lang}: model reply: {work}");
        assert_eq!(work["version"], (i + 1) as i64, "{lang}: context versions advance across agents");
        assert_eq!(work["tool"], json!({"hits": 3}), "{lang}");
        assert_eq!(work["denied"], "Forbidden", "{lang}: unbound model claim refused by the node");
        assert_eq!(r.status, clankr_engine::RunStatus::Failed, "{lang}: boom is Permanent");
        let err = r.state.error.unwrap_or_default();
        assert!(err.contains("after 1 attempt(s)") && err.contains("bad input"), "{lang}: no retry for Permanent: {err}");
    }
    let calls: i64 = sqlx::query_scalar("select count(*) from tool_calls where namespace = $1 and idempotency_key like '%:search.query:1'").bind(NS).fetch_one(store.pool()).await.unwrap();
    assert!(!agents.is_empty());
    assert_eq!(calls, agents.len() as i64, "tool keys are derived from the task, like the Python SDK");
    eprintln!("checked SDKs: {:?}", agents.iter().map(|a| a.0).collect::<Vec<_>>());
    for s in stops {
        let _ = s.send(true);
    }
}
