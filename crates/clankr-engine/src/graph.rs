//! Workflow graph spec and admission validation (CL-21-01, CL-21-06).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type NodeId = String;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowSpec {
    pub entrypoint: NodeId,
    pub nodes: BTreeMap<NodeId, Node>,
    /// Hard bound on node entries per run.
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
    #[serde(default)]
    pub retry_policy: RetryPolicy,
}

fn default_max_steps() -> u32 {
    100
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    #[serde(flatten)]
    pub kind: NodeKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "one_or_many")]
    pub next: Vec<NodeId>,
    /// Required somewhere on every cycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_policy: Option<RetryPolicy>,
    /// `unsafe`: if a worker's lease lapses mid-task the outcome is unknown, so the task is not
    /// blindly redelivered; it waits for the late worker's answer or an operator (CL-22-07/08).
    #[serde(default, skip_serializing_if = "SideEffects::is_default")]
    pub side_effects: SideEffects,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SideEffects {
    /// Safe to redeliver after a lost lease.
    #[default]
    Idempotent,
    Unsafe,
}

impl SideEffects {
    fn is_default(&self) -> bool {
        *self == SideEffects::Idempotent
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum NodeKind {
    /// Runs on queue `agent/<agentRef>`.
    Agent { agent_ref: String },
    /// Runs on queue `tool/<toolRef>`.
    Activity { tool_ref: String },
    /// Routes on `path` (e.g. `$.verify.ok`) equal to `equals`.
    Condition {
        path: String,
        equals: Value,
        then: NodeId,
        #[serde(rename = "else")]
        otherwise: NodeId,
    },
    /// Waits for all (or any) incoming branches.
    Join {
        #[serde(default)]
        mode: JoinMode,
    },
    Timer { duration: String },
    /// Waits for signal `approve.<node>` with payload `{"approved": bool, ...}`.
    Approval {
        approver_group: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on_reject: Option<NodeId>,
        /// Undecided after this long ("30m", "2d"): rejected as timed out.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<String>,
    },
    /// Waits for a named signal.
    Signal { signal: String },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JoinMode {
    #[default]
    All,
    Any,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub initial_backoff: String,
    pub max_backoff: String,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_attempts: 3, initial_backoff: "1s".into(), max_backoff: "60s".into() }
    }
}

impl RetryPolicy {
    /// Backoff before `attempt` (2 = first retry): initial * 2^(attempt-2), capped. No jitter,
    /// so the schedule is part of deterministic history.
    pub fn backoff(&self, attempt: u32) -> Duration {
        let init = parse_duration(&self.initial_backoff).unwrap_or(Duration::from_secs(1));
        let max = parse_duration(&self.max_backoff).unwrap_or(Duration::from_secs(60));
        init.saturating_mul(1 << attempt.saturating_sub(2).min(20)).min(max)
    }
}

fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<NodeId>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(NodeId),
        Many(Vec<NodeId>),
    }
    Ok(match Option::<OneOrMany>::deserialize(d)? {
        None => vec![],
        Some(OneOrMany::One(n)) => vec![n],
        Some(OneOrMany::Many(v)) => v,
    })
}

/// `"500ms" | "30s" | "5m" | "2h" | "1d"`
pub fn parse_duration(s: &str) -> Option<Duration> {
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: u64 = num.parse().ok()?;
    Some(match unit {
        "ms" => Duration::from_millis(n),
        "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n * 60),
        "h" => Duration::from_secs(n * 3600),
        "d" => Duration::from_secs(n * 86400),
        _ => return None,
    })
}

/// Reads a `$.a.b.c` path from a JSON object.
pub fn lookup<'a>(data: &'a Value, path: &str) -> Option<&'a Value> {
    let rest = path.strip_prefix('$')?;
    rest.split('.').filter(|p| !p.is_empty()).try_fold(data, |v, key| v.get(key))
}

/// A validation failure with the JSON path of the offending field.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphError {
    pub field: String,
    pub message: String,
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (at {})", self.message, self.field)
    }
}

impl WorkflowSpec {
    pub fn successors(&self, id: &str) -> Vec<&NodeId> {
        let n = &self.nodes[id];
        let mut out: Vec<&NodeId> = n.next.iter().collect();
        match &n.kind {
            NodeKind::Condition { then, otherwise, .. } => out.extend([then, otherwise]),
            NodeKind::Approval { on_reject: Some(r), .. } => out.push(r),
            _ => {}
        }
        out
    }

    pub fn in_degree(&self, id: &str) -> u32 {
        self.nodes.keys().flat_map(|n| self.successors(n)).filter(|s| *s == id).count() as u32
    }

    /// Admission checks. Returns every problem, not just the first.
    pub fn validate(&self) -> Result<(), Vec<GraphError>> {
        let mut errs = vec![];
        let mut err = |field: String, message: String| errs.push(GraphError { field, message });

        if !self.nodes.contains_key(&self.entrypoint) {
            err("spec.entrypoint".into(), format!("entrypoint {:?} is not a node", self.entrypoint));
        }
        if self.max_steps == 0 {
            err("spec.maxSteps".into(), "must be > 0".into());
        }
        let check_policy = |p: &RetryPolicy, field: &str, err: &mut dyn FnMut(String, String)| {
            if p.max_attempts == 0 {
                err(format!("{field}.maxAttempts"), "must be >= 1".into());
            }
            for (k, v) in [("initialBackoff", &p.initial_backoff), ("maxBackoff", &p.max_backoff)] {
                if parse_duration(v).is_none() {
                    err(format!("{field}.{k}"), format!("bad duration {v:?}"));
                }
            }
        };
        check_policy(&self.retry_policy, "spec.retryPolicy", &mut err);

        for (id, n) in &self.nodes {
            let at = |f: &str| format!("spec.nodes.{id}.{f}");
            if !is_node_id(id) {
                err(format!("spec.nodes.{id}"), "node ids are [a-zA-Z0-9_-]+".into());
            }
            for (i, t) in n.next.iter().enumerate() {
                if !self.nodes.contains_key(t) {
                    err(at(&format!("next[{i}]")), format!("unknown node {t:?}"));
                }
            }
            if let Some(p) = &n.retry_policy {
                check_policy(p, &at("retryPolicy"), &mut err);
            }
            match &n.kind {
                NodeKind::Condition { path, then, otherwise, .. } => {
                    if !n.next.is_empty() {
                        err(at("next"), "condition routes with then/else, not next".into());
                    }
                    if !path.starts_with('$') {
                        err(at("path"), "path must start with $".into());
                    }
                    for (f, t) in [("then", then), ("else", otherwise)] {
                        if !self.nodes.contains_key(t) {
                            err(at(f), format!("unknown node {t:?}"));
                        }
                    }
                }
                NodeKind::Approval { on_reject: Some(t), .. } if !self.nodes.contains_key(t) => {
                    err(at("onReject"), format!("unknown node {t:?}"));
                }
                NodeKind::Approval { timeout: Some(t), .. } if parse_duration(t).is_none() => {
                    err(at("timeout"), format!("bad duration {t:?}"));
                }
                NodeKind::Approval { approver_group, .. } if approver_group.is_empty() => {
                    err(at("approverGroup"), "approverGroup is required".into());
                }
                NodeKind::Timer { duration } if parse_duration(duration).is_none() => {
                    err(at("duration"), format!("bad duration {duration:?}"));
                }
                NodeKind::Join { .. } if self.in_degree(id) < 2 => {
                    err(format!("spec.nodes.{id}"), "join needs at least two incoming edges".into());
                }
                NodeKind::Agent { agent_ref: r } | NodeKind::Activity { tool_ref: r } if r.is_empty() => {
                    err(format!("spec.nodes.{id}"), "missing agentRef/toolRef".into());
                }
                _ => {}
            }
        }
        if !errs.is_empty() {
            return Err(errs);
        }

        // unreachable nodes are almost always a typo
        let mut seen = BTreeSet::from([self.entrypoint.as_str()]);
        let mut stack = vec![self.entrypoint.as_str()];
        while let Some(n) = stack.pop() {
            for s in self.successors(n) {
                if seen.insert(s) {
                    stack.push(s);
                }
            }
        }
        for id in self.nodes.keys().filter(|id| !seen.contains(id.as_str())) {
            errs.push(GraphError { field: format!("spec.nodes.{id}"), message: "unreachable from entrypoint".into() });
        }

        // CL-21-06: every cycle needs a bound. Drop bounded nodes; anything still cyclic is unbounded.
        if let Some(n) = self.unbounded_cycle_node() {
            errs.push(GraphError {
                field: format!("spec.nodes.{n}"),
                message: "node is on a cycle with no maxIterations; set maxIterations on a node in the loop".into(),
            });
        }
        if errs.is_empty() { Ok(()) } else { Err(errs) }
    }

    fn unbounded_cycle_node(&self) -> Option<&NodeId> {
        let bounded = |n: &str| self.nodes[n].max_iterations.is_some();
        // Kahn's algorithm over the unbounded subgraph: whatever can't be peeled off is on a cycle.
        let live: BTreeSet<&NodeId> = self.nodes.keys().filter(|n| !bounded(n)).collect();
        let mut indeg: BTreeMap<&NodeId, usize> = live.iter().map(|n| (*n, 0)).collect();
        for n in &live {
            for s in self.successors(n).into_iter().filter(|s| live.contains(s)) {
                *indeg.get_mut(s).unwrap() += 1;
            }
        }
        let mut ready: Vec<&NodeId> = indeg.iter().filter(|(_, d)| **d == 0).map(|(n, _)| *n).collect();
        while let Some(n) = ready.pop() {
            indeg.remove(n);
            for s in self.successors(n).into_iter().filter(|s| live.contains(s)) {
                if let Some(d) = indeg.get_mut(s) {
                    *d -= 1;
                    if *d == 0 {
                        ready.push(s);
                    }
                }
            }
        }
        indeg.into_keys().next()
    }
}

fn is_node_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 63 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(yaml: &str) -> WorkflowSpec {
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    fn fields(r: Result<(), Vec<GraphError>>) -> Vec<String> {
        r.unwrap_err().into_iter().map(|e| e.field).collect()
    }

    #[test]
    fn prd_style_graph_parses_and_validates() {
        let s = spec(
            "entrypoint: plan
nodes:
  plan: {type: agent, agentRef: planner, next: [research, verify]}
  research: {type: agent, agentRef: researcher, next: gather}
  verify: {type: agent, agentRef: verifier, next: gather}
  gather: {type: join, next: review}
  review: {type: approval, approverGroup: editors, next: publish}
  publish: {type: activity, toolRef: publisher}",
        );
        s.validate().unwrap();
        assert_eq!(s.in_degree("gather"), 2);
        assert_eq!(s.nodes["plan"].next, ["research", "verify"]);
    }

    #[test]
    fn rejects_dangling_refs_unreachable_and_bad_durations() {
        let s = spec(
            "entrypoint: a
nodes:
  a: {type: timer, duration: 5 parsecs, next: ghost}
  orphan: {type: agent, agentRef: x}",
        );
        assert_eq!(fields(s.validate()), ["spec.nodes.a.next[0]", "spec.nodes.a.duration"]);
        let s = spec("entrypoint: a\nnodes:\n  a: {type: agent, agentRef: x}\n  orphan: {type: agent, agentRef: x}");
        assert_eq!(fields(s.validate()), ["spec.nodes.orphan"]);
    }

    #[test]
    fn loops_need_max_iterations() {
        let looped = "entrypoint: work
nodes:
  work: {type: agent, agentRef: w, next: check}
  check: {type: condition, path: $.work.done, equals: true, then: end, else: work}
  end: {type: activity, toolRef: t}";
        assert_eq!(fields(spec(looped).validate()), ["spec.nodes.check"]);
        let bounded = looped.replace("else: work}", "else: work, maxIterations: 3}");
        spec(&bounded).validate().unwrap();
    }

    #[test]
    fn join_needs_two_inputs_and_unknown_fields_fail_to_parse() {
        let s = spec("entrypoint: a\nnodes:\n  a: {type: agent, agentRef: x, next: j}\n  j: {type: join}");
        assert_eq!(fields(s.validate()), ["spec.nodes.j"]);
        assert!(serde_yaml_ng::from_str::<WorkflowSpec>("entrypoint: a\nnodes: {}\nmaxStep: 3").is_err());
    }

    #[test]
    fn helpers() {
        assert_eq!(parse_duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("h"), None);
        let p = RetryPolicy::default();
        assert_eq!([2, 3, 4, 9].map(|a| p.backoff(a).as_secs()), [1, 2, 4, 60]);
        let v = serde_json::json!({"a": {"b": true}});
        assert_eq!(lookup(&v, "$.a.b"), Some(&Value::Bool(true)));
        assert_eq!(lookup(&v, "$.a.zz"), None);
    }
}
