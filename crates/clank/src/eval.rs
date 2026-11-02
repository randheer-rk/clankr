//! `clank eval`: run a dataset of cases through a workflow, score the outputs, and fail on
//! threshold misses or regressions against a baseline report (CL-34-08, OBS-07). Pair it with
//! a `replay` ProviderConnection so model answers are deterministic and the score measures the
//! agent and workflow, not sampling noise.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Api, ok};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Suite {
    pub name: String,
    #[serde(default = "default_ns")]
    pub namespace: String,
    pub workflow: String,
    /// Approve every approval the run reaches, as `--approver-token` (a principal with the
    /// approver role and group that did not start the run).
    #[serde(default)]
    pub auto_approve: bool,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub thresholds: Thresholds,
    pub cases: Vec<Case>,
}

fn default_ns() -> String {
    "default".into()
}
fn default_timeout() -> u64 {
    300
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Case {
    pub name: String,
    #[serde(default)]
    pub input: Value,
    /// Terminal status the run must reach; safety cases expect `Failed`.
    #[serde(default = "default_status")]
    pub status: String,
    #[serde(default)]
    pub expect: Vec<Assertion>,
}

fn default_status() -> String {
    "Succeeded".into()
}

/// A check on the run's output (`state.data`), addressed with `$.node.field[0]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Assertion {
    pub path: String,
    #[serde(default)]
    pub equals: Option<Value>,
    #[serde(default)]
    pub contains: Option<String>,
    #[serde(default)]
    pub not_contains: Option<String>,
    #[serde(default)]
    pub gte: Option<f64>,
    #[serde(default)]
    pub lte: Option<f64>,
    #[serde(default)]
    pub exists: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Thresholds {
    #[serde(default = "one")]
    pub pass_rate: f64,
    pub max_avg_cost_usd: Option<f64>,
    pub max_p95_seconds: Option<f64>,
    /// Against a baseline: allowed drop in pass rate (absolute) and rise in average cost (fraction).
    #[serde(default)]
    pub max_pass_rate_drop: f64,
    #[serde(default = "twenty_percent")]
    pub max_cost_increase: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self { pass_rate: 1.0, max_avg_cost_usd: None, max_p95_seconds: None, max_pass_rate_drop: 0.0, max_cost_increase: 0.2 }
    }
}

fn one() -> f64 {
    1.0
}
fn twenty_percent() -> f64 {
    0.2
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaseResult {
    pub name: String,
    pub run_id: Option<String>,
    pub status: String,
    pub passed: bool,
    pub failures: Vec<String>,
    pub cost_usd: f64,
    pub tokens: i64,
    pub seconds: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub suite: String,
    pub workflow: String,
    pub at: String,
    pub cases: Vec<CaseResult>,
    pub pass_rate: f64,
    pub avg_cost_usd: f64,
    pub p95_seconds: f64,
}

/// `$.a.b[2].c` → value, if present.
pub fn lookup<'a>(data: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = data;
    let rest = path.strip_prefix('$')?;
    for part in rest.split('.').filter(|p| !p.is_empty()) {
        let (key, idx) = match part.find('[') {
            Some(i) => (&part[..i], Some(&part[i..])),
            None => (part, None),
        };
        if !key.is_empty() {
            cur = cur.get(key)?;
        }
        for ix in idx.into_iter().flat_map(|s| s.split(['[', ']']).filter(|x| !x.is_empty())) {
            cur = cur.get(ix.parse::<usize>().ok()?)?;
        }
    }
    Some(cur)
}

/// Failure messages for `a` against `data` (empty = passed).
pub fn check(a: &Assertion, data: &Value) -> Vec<String> {
    let v = lookup(data, &a.path);
    let mut out = vec![];
    let fail = |m: String| format!("{}: {m}", a.path);
    if let Some(want) = a.exists {
        if v.is_some() != want {
            out.push(fail(if want { "missing".into() } else { "should not exist".into() }));
        }
        if !want {
            return out;
        }
    }
    let Some(v) = v else {
        out.push(fail("missing".into()));
        return out;
    };
    let text = v.as_str().map_or_else(|| v.to_string(), str::to_owned);
    if let Some(want) = &a.equals
        && v != want
    {
        out.push(fail(format!("expected {want}, got {v}")));
    }
    if let Some(c) = &a.contains
        && !text.contains(c.as_str())
    {
        out.push(fail(format!("doesn't contain {c:?}")));
    }
    if let Some(c) = &a.not_contains
        && text.contains(c.as_str())
    {
        out.push(fail(format!("contains forbidden {c:?}")));
    }
    if a.gte.is_some() || a.lte.is_some() {
        match v.as_f64() {
            Some(n) => {
                if a.gte.is_some_and(|g| n < g) {
                    out.push(fail(format!("{n} < {}", a.gte.unwrap())));
                }
                if a.lte.is_some_and(|l| n > l) {
                    out.push(fail(format!("{n} > {}", a.lte.unwrap())));
                }
            }
            None => out.push(fail(format!("not a number: {v}"))),
        }
    }
    out
}

fn p95(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let mut v = xs.to_vec();
    v.sort_by(f64::total_cmp);
    v[((v.len() as f64 * 0.95).ceil() as usize).clamp(1, v.len()) - 1]
}

pub fn summarize(suite: &Suite, cases: Vec<CaseResult>) -> Report {
    let n = cases.len().max(1) as f64;
    Report {
        suite: suite.name.clone(),
        workflow: suite.workflow.clone(),
        at: chrono::Utc::now().to_rfc3339(),
        pass_rate: cases.iter().filter(|c| c.passed).count() as f64 / n,
        avg_cost_usd: cases.iter().map(|c| c.cost_usd).sum::<f64>() / n,
        p95_seconds: p95(&cases.iter().map(|c| c.seconds).collect::<Vec<_>>()),
        cases,
    }
}

/// Threshold misses and regressions against `baseline`; empty means the gate passes.
pub fn gate(t: &Thresholds, r: &Report, baseline: Option<&Report>) -> Vec<String> {
    let mut out = vec![];
    if r.pass_rate + 1e-9 < t.pass_rate {
        out.push(format!("pass rate {:.0}% is below the required {:.0}%", r.pass_rate * 100.0, t.pass_rate * 100.0));
    }
    if let Some(max) = t.max_avg_cost_usd
        && r.avg_cost_usd > max
    {
        out.push(format!("average cost ${:.4} is above ${max:.4}", r.avg_cost_usd));
    }
    if let Some(max) = t.max_p95_seconds
        && r.p95_seconds > max
    {
        out.push(format!("p95 duration {:.1}s is above {max:.1}s", r.p95_seconds));
    }
    if let Some(b) = baseline {
        if r.pass_rate + t.max_pass_rate_drop + 1e-9 < b.pass_rate {
            out.push(format!("pass rate dropped from {:.0}% to {:.0}%", b.pass_rate * 100.0, r.pass_rate * 100.0));
        }
        let newly: Vec<&str> = r
            .cases
            .iter()
            .filter(|c| !c.passed && b.cases.iter().any(|bc| bc.name == c.name && bc.passed))
            .map(|c| c.name.as_str())
            .collect();
        if !newly.is_empty() {
            out.push(format!("cases that passed in the baseline now fail: {}", newly.join(", ")));
        }
        if b.avg_cost_usd > 0.0 && r.avg_cost_usd > b.avg_cost_usd * (1.0 + t.max_cost_increase) {
            out.push(format!("average cost rose from ${:.4} to ${:.4} (allowed +{:.0}%)", b.avg_cost_usd, r.avg_cost_usd, t.max_cost_increase * 100.0));
        }
    }
    out
}

fn run_case(api: &Api, approver: &Api, s: &Suite, c: &Case, nonce: &str) -> Result<CaseResult> {
    use reqwest::Method as M;
    let base = format!("/v1/namespaces/{}", s.namespace);
    let body = json!({"input": c.input, "idempotencyKey": format!("eval:{}:{}:{nonce}", s.name, c.name)});
    let run: Value = ok(api.req(M::POST, &format!("{base}/workflows/{}/runs", s.workflow)).json(&body).send()?)?.json()?;
    let id = run["runId"].as_str().context("no runId")?.to_owned();
    let deadline = Instant::now() + Duration::from_secs(s.timeout_seconds);
    let run = loop {
        let r: Value = ok(api.req(M::GET, &format!("{base}/runs/{id}")).send()?)?.json()?;
        if r["status"] != "Running" {
            break r;
        }
        if s.auto_approve && r["state"]["waits"].as_object().is_some_and(|w| w.keys().any(|k| k.starts_with("approve."))) {
            let pending: Vec<Value> = ok(approver.req(M::GET, &format!("{base}/approvals?state=pending")).send()?)?.json()?;
            for a in pending.iter().filter(|a| a["runId"] == id.as_str()) {
                let path = format!("{base}/approvals/{}/approve", a["id"].as_str().unwrap_or_default());
                ok(approver.req(M::POST, &path).json(&json!({"digest": a["digest"], "comment": format!("auto-approved by eval {}", s.name)})).send()?)
                    .context("auto-approve: use --approver-token for a principal with the approver role and group that didn't start the run")?;
            }
        }
        if Instant::now() > deadline {
            let _ = api.req(M::POST, &format!("{base}/runs/{id}/cancel")).json(&json!({"reason": format!("eval {} timed out", s.name)})).send();
            break json!({"status": "TimedOut", "state": r["state"], "createdAt": r["createdAt"], "updatedAt": chrono::Utc::now().to_rfc3339()});
        }
        std::thread::sleep(Duration::from_millis(300));
    };
    let spend: Value = ok(api.req(M::GET, &format!("{base}/runs/{id}/spend")).send()?)?.json()?;
    let status = run["status"].as_str().unwrap_or("?").to_owned();
    let mut failures = vec![];
    if status != c.status {
        let why = run["state"]["error"].as_str().map(|e| format!(" ({e})")).unwrap_or_default();
        failures.push(format!("status {status}, expected {}{why}", c.status));
    }
    let data = &run["state"]["data"];
    for a in &c.expect {
        failures.extend(check(a, data));
    }
    let secs = |k: &str| chrono::DateTime::parse_from_rfc3339(run[k].as_str().unwrap_or_default()).map(|t| t.timestamp_millis() as f64 / 1000.0).unwrap_or(0.0);
    Ok(CaseResult {
        name: c.name.clone(),
        run_id: Some(id),
        status,
        passed: failures.is_empty(),
        failures,
        cost_usd: spend["costUSD"].as_f64().unwrap_or(0.0),
        tokens: spend["inputTokens"].as_i64().unwrap_or(0) + spend["outputTokens"].as_i64().unwrap_or(0),
        seconds: (secs("updatedAt") - secs("createdAt")).max(0.0),
    })
}

pub fn run(api: &Api, approver: &Api, suite_path: &Path, report_path: Option<&Path>, baseline_path: Option<&Path>) -> Result<()> {
    let suite: Suite = serde_yaml_ng::from_str(&std::fs::read_to_string(suite_path).with_context(|| format!("read {}", suite_path.display()))?)
        .with_context(|| format!("parse {}", suite_path.display()))?;
    if suite.cases.is_empty() {
        bail!("suite {} has no cases", suite.name);
    }
    let baseline: Option<Report> = match baseline_path {
        Some(p) if p.exists() => Some(serde_json::from_slice(&std::fs::read(p)?).with_context(|| format!("parse baseline {}", p.display()))?),
        Some(p) => {
            eprintln!("no baseline at {} yet; this run's report can become it", p.display());
            None
        }
        None => None,
    };
    let nonce = uuid_like();
    // ponytail: cases run one after another; add a --parallel pool when suites get slow
    let mut results = vec![];
    for c in &suite.cases {
        let r = run_case(api, approver, &suite, c, &nonce).unwrap_or_else(|e| CaseResult {
            name: c.name.clone(),
            run_id: None,
            status: "Error".into(),
            passed: false,
            failures: vec![format!("{e:#}")],
            cost_usd: 0.0,
            tokens: 0,
            seconds: 0.0,
        });
        println!("{} {:<28} {:<10} ${:<8.4} {:>6.1}s{}", if r.passed { "pass" } else { "FAIL" }, r.name, r.status, r.cost_usd, r.seconds, r.failures.first().map(|f| format!("  {f}")).unwrap_or_default());
        results.push(r);
    }
    let report = summarize(&suite, results);
    println!(
        "\n{}: {}/{} passed ({:.0}%) · avg ${:.4} per case · p95 {:.1}s",
        suite.name,
        report.cases.iter().filter(|c| c.passed).count(),
        report.cases.len(),
        report.pass_rate * 100.0,
        report.avg_cost_usd,
        report.p95_seconds
    );
    if let Some(p) = report_path {
        std::fs::write(p, serde_json::to_vec_pretty(&report)?)?;
        println!("report written to {}", p.display());
    }
    let problems = gate(&suite.thresholds, &report, baseline.as_ref());
    if !problems.is_empty() {
        for p in &problems {
            eprintln!("gate: {p}");
        }
        bail!("evaluation gate failed");
    }
    println!("gate: passed");
    Ok(())
}

fn uuid_like() -> String {
    format!("{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(path: &str, f: impl FnOnce(&mut Assertion)) -> Assertion {
        let mut x = Assertion { path: path.into(), equals: None, contains: None, not_contains: None, gte: None, lte: None, exists: None };
        f(&mut x);
        x
    }

    #[test]
    fn paths_and_assertions() {
        let d = json!({"research": {"sources": 12, "items": [{"t": "a"}, {"t": "b"}], "summary": "survives worker loss"}, "verify": {"ok": true}});
        assert_eq!(lookup(&d, "$.research.items[1].t"), Some(&json!("b")));
        assert_eq!(lookup(&d, "$.nope"), None);
        assert!(check(&a("$.verify.ok", |x| x.equals = Some(json!(true))), &d).is_empty());
        assert!(check(&a("$.research.sources", |x| x.gte = Some(10.0)), &d).is_empty());
        assert_eq!(check(&a("$.research.sources", |x| x.lte = Some(5.0)), &d), ["$.research.sources: 12 > 5"]);
        assert!(check(&a("$.research.summary", |x| x.contains = Some("worker loss".into())), &d).is_empty());
        assert_eq!(check(&a("$.research.summary", |x| x.not_contains = Some("loss".into())), &d).len(), 1);
        assert!(check(&a("$.secret", |x| x.exists = Some(false)), &d).is_empty());
        assert_eq!(check(&a("$.missing", |x| x.equals = Some(json!(1))), &d), ["$.missing: missing"]);
    }

    fn case(name: &str, passed: bool, cost: f64, secs: f64) -> CaseResult {
        CaseResult { name: name.into(), run_id: None, status: "Succeeded".into(), passed, failures: vec![], cost_usd: cost, tokens: 0, seconds: secs }
    }

    #[test]
    fn gates_and_regressions() {
        let suite: Suite = serde_yaml_ng::from_str("{name: s, workflow: w, cases: [], thresholds: {passRate: 0.5, maxP95Seconds: 10}}").unwrap();
        let base = summarize(&suite, vec![case("a", true, 0.10, 1.0), case("b", true, 0.10, 2.0)]);
        assert!(gate(&suite.thresholds, &base, None).is_empty());
        // pass rate still meets 50%, but a case that used to pass now fails: regression
        let worse = summarize(&suite, vec![case("a", true, 0.10, 1.0), case("b", false, 0.10, 2.0)]);
        let g = gate(&suite.thresholds, &worse, Some(&base));
        assert!(g.iter().any(|x| x.contains("now fail: b")), "{g:?}");
        assert!(g.iter().any(|x| x.contains("dropped from 100% to 50%")), "{g:?}");
        // cost creep beyond 20% and slow tail
        let pricey = summarize(&suite, vec![case("a", true, 0.13, 1.0), case("b", true, 0.13, 30.0)]);
        let g = gate(&suite.thresholds, &pricey, Some(&base));
        assert!(g.iter().any(|x| x.contains("average cost rose")) && g.iter().any(|x| x.contains("p95")), "{g:?}");
        assert_eq!(p95(&[1.0, 2.0, 3.0, 4.0, 100.0]), 100.0);
    }
}
