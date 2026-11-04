//! `GET /metrics`: the cluster's state as Prometheus text (ADR 0039). Everything comes from
//! the tables the control plane already keeps, so every replica answers the same numbers and
//! nothing is counted in memory. Needs the audit-read permission (it's a cluster-wide view).

use std::fmt::Write;

use axum::extract::State;
use clankr_store::db;
use sqlx::Row;

use crate::{ApiResult, AppState};

fn label(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

struct Out(String);

impl Out {
    fn head(&mut self, name: &str, kind: &str, help: &str) {
        let _ = writeln!(self.0, "# HELP {name} {help}\n# TYPE {name} {kind}");
    }
    fn row(&mut self, name: &str, labels: &[(&str, &str)], value: impl std::fmt::Display) {
        let ls: Vec<String> = labels.iter().map(|(k, v)| format!("{k}=\"{}\"", label(v))).collect();
        if ls.is_empty() {
            let _ = writeln!(self.0, "{name} {value}");
        } else {
            let _ = writeln!(self.0, "{name}{{{}}} {value}", ls.join(","));
        }
    }
}

/// One grouped count: `select a, b, count(*) …` becomes rows with the given label names.
async fn grouped(s: &AppState, out: &mut Out, name: &str, help: &str, sql: &'static str, labels: &[&str]) -> Result<(), clankr_types::ApiError> {
    out.head(name, "gauge", help);
    for r in sqlx::query(sql).fetch_all(s.store.pool()).await.map_err(db)? {
        let vals: Vec<String> = (0..labels.len()).map(|i| r.try_get::<Option<String>, _>(i).ok().flatten().unwrap_or_default()).collect();
        let pairs: Vec<(&str, &str)> = labels.iter().copied().zip(vals.iter().map(String::as_str)).collect();
        let v: i64 = r.try_get(labels.len()).unwrap_or(0);
        out.row(name, &pairs, v);
    }
    Ok(())
}

pub async fn metrics(State(s): State<AppState>) -> ApiResult<([(&'static str, &'static str); 1], String)> {
    let mut out = Out(String::new());
    out.head("clankr_build_info", "gauge", "The control plane's version.");
    out.row("clankr_build_info", &[("version", env!("CARGO_PKG_VERSION"))], 1);
    grouped(&s, &mut out, "clankr_runs", "Workflow runs by status.", "select namespace, status, count(*) from workflow_runs group by 1, 2", &["namespace", "status"]).await?;
    grouped(&s, &mut out, "clankr_tasks", "Agent tasks by state (ready = queue depth).", "select namespace, state, count(*) from tasks group by 1, 2", &["namespace", "state"]).await?;
    grouped(&s, &mut out, "clankr_approvals", "Approval requests by state.", "select namespace, state, count(*) from approvals group by 1, 2", &["namespace", "state"]).await?;
    grouped(&s, &mut out, "clankr_instances", "Agent instances placed, by agent.", "select namespace, agent, count(*) from instances group by 1, 2", &["namespace", "agent"]).await?;
    grouped(&s, &mut out, "clankr_nodes", "Nodes by readiness.", "select ready::text, count(*) from nodes group by 1", &["ready"]).await?;
    grouped(&s, &mut out, "clankr_mailbox_messages", "Messages in queues by state (dead = dead letters).", "select namespace, state, count(*) from mailbox_messages group by 1, 2", &["namespace", "state"]).await?;
    grouped(&s, &mut out, "clankr_model_calls_total", "Model calls by claim and outcome.", "select namespace, claim, outcome, count(*) from model_calls group by 1, 2, 3", &["namespace", "claim", "outcome"]).await?;
    grouped(&s, &mut out, "clankr_model_input_tokens_total", "Input tokens by claim.", "select namespace, claim, sum(input_tokens)::bigint from model_calls group by 1, 2", &["namespace", "claim"]).await?;
    grouped(&s, &mut out, "clankr_model_output_tokens_total", "Output tokens by claim.", "select namespace, claim, sum(output_tokens)::bigint from model_calls group by 1, 2", &["namespace", "claim"]).await?;
    grouped(&s, &mut out, "clankr_model_cost_micro_usd_total", "Model spend in micro-USD by claim (needs pricing).", "select namespace, claim, coalesce(sum(cost_micro_usd), 0)::bigint from model_calls group by 1, 2", &["namespace", "claim"]).await?;
    grouped(&s, &mut out, "clankr_budget_used", "Budget counters: tokens or micro-USD used per bucket and window.", "select namespace, claim, scope, used from model_usage", &["namespace", "bucket", "window"]).await?;
    grouped(&s, &mut out, "clankr_budget_reserved", "Budget counters: in-flight reservations.", "select namespace, claim, scope, reserved from model_usage", &["namespace", "bucket", "window"]).await?;
    grouped(&s, &mut out, "clankr_tool_calls_total", "Tool calls by tool and state (failed calls aren't kept).", "select namespace, tool, state, count(*) from tool_calls group by 1, 2, 3", &["namespace", "tool", "state"]).await?;
    grouped(&s, &mut out, "clankr_leader", "Which replica holds each leader lease.", "select name, holder, 1::bigint from leader_leases where expires_at > now()", &["lease", "holder"]).await?;
    let seq: i64 = sqlx::query_scalar("select coalesce(max(seq), 0) from audit_events").fetch_one(s.store.pool()).await.map_err(db)?;
    out.head("clankr_audit_seq", "gauge", "The newest audit record.");
    out.row("clankr_audit_seq", &[], seq);
    out.head("clankr_audit_export_lag", "gauge", "Audit records not yet shipped to each export destination.");
    for r in sqlx::query("select dest, seq from audit_exports").fetch_all(s.store.pool()).await.map_err(db)? {
        let (dest, sent): (String, i64) = (r.get(0), r.get(1));
        out.row("clankr_audit_export_lag", &[("dest", &crate::audit_export::redact(&dest))], seq - sent);
    }
    Ok(([("content-type", "text/plain; version=0.0.4; charset=utf-8")], out.0))
}
