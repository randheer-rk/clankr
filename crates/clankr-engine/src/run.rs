//! Event-sourced run interpreter (PRD §21).
//!
//! `evolve(state, event)` is the only thing that changes run state, and it never looks at the
//! graph, the clock or the outside world, so `replay(history)` is a pure fold. `decide` takes
//! one external input (start, task result, timer, signal, cancel) and returns that input
//! followed by every event it causes. The store appends them in one transaction, together with
//! the task and timer rows they imply (CL-21-10).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::graph::{JoinMode, NodeId, NodeKind, WorkflowSpec, lookup, parse_duration};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum Event {
    // inputs
    RunStarted { input: Value },
    TaskCompleted { task: String, result: Value },
    TaskFailed { task: String, error: String, retryable: bool },
    TimerFired { timer: String },
    /// An `unsafe` task's lease lapsed: it may or may not have taken effect.
    TaskOutcomeUnknown { task: String },
    SignalReceived { signal: String, payload: Value },
    RunCancelled { reason: String },
    // consequences
    NodeEntered { node: NodeId },
    JoinArrived { node: NodeId },
    JoinFired { node: NodeId, swallow: u32 },
    TaskScheduled { task: String, node: NodeId, attempt: u32, queue: String, not_before: DateTime<Utc> },
    TimerScheduled { timer: String, node: NodeId, due_at: DateTime<Utc> },
    WaitStarted { signal: String, node: NodeId },
    RunCompleted,
    RunFailed { reason: String },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    #[default]
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn is_terminal(self) -> bool {
        self != RunStatus::Running
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingTask {
    pub node: NodeId,
    pub attempt: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunState {
    pub status: RunStatus,
    /// `{"input": ..., "<node>": <result>, ...}`; what tasks see and conditions test.
    pub data: Map<String, Value>,
    pub steps: u32,
    pub visits: BTreeMap<NodeId, u32>,
    pub tasks: BTreeMap<String, PendingTask>,
    pub timers: BTreeMap<String, NodeId>,
    pub waits: BTreeMap<String, NodeId>,
    pub join_arrivals: BTreeMap<NodeId, u32>,
    /// For `any` joins: late arrivals of an already-fired wave to absorb.
    pub join_swallow: BTreeMap<NodeId, u32>,
    /// Tasks with an ambiguous outcome, awaiting `resolve.<task>`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unknown: BTreeMap<String, PendingTask>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RunState {
    pub fn is_idle(&self) -> bool {
        self.tasks.is_empty() && self.timers.is_empty() && self.waits.is_empty()
    }
}

/// Applies one event. Graph-free and clock-free by design.
pub fn evolve(mut s: RunState, e: &Event) -> RunState {
    match e {
        Event::RunStarted { input } => {
            s.data.insert("input".into(), input.clone());
        }
        Event::TaskCompleted { task, result } => {
            if let Some(t) = s.tasks.remove(task) {
                s.data.insert(t.node, result.clone());
            }
        }
        Event::TaskFailed { task, .. } => {
            s.tasks.remove(task);
        }
        Event::TimerFired { timer } => {
            s.timers.remove(timer);
        }
        Event::TaskOutcomeUnknown { task } => {
            if let Some(t) = s.tasks.remove(task) {
                s.waits.insert(resolve_signal(task), t.node.clone());
                s.unknown.insert(task.clone(), t);
            }
        }
        Event::SignalReceived { signal, payload } => {
            if let Some(node) = s.waits.remove(signal) {
                match signal.strip_prefix("resolve.").and_then(|t| s.unknown.remove(t)) {
                    Some(_) if payload["outcome"] == "completed" => {
                        s.data.insert(node, payload["result"].clone());
                    }
                    Some(_) => {}
                    None => {
                        s.data.insert(node, payload.clone());
                    }
                }
            }
        }
        Event::RunCancelled { reason } => {
            s.status = RunStatus::Cancelled;
            s.error = Some(reason.clone());
            clear_pending(&mut s);
        }
        Event::NodeEntered { node } => {
            s.steps += 1;
            *s.visits.entry(node.clone()).or_default() += 1;
        }
        Event::JoinArrived { node } => {
            match s.join_swallow.get_mut(node) {
                Some(n) if *n > 0 => *n -= 1,
                _ => *s.join_arrivals.entry(node.clone()).or_default() += 1,
            }
        }
        Event::JoinFired { node, swallow } => {
            s.join_arrivals.remove(node);
            s.join_swallow.insert(node.clone(), *swallow);
        }
        Event::TaskScheduled { task, node, attempt, .. } => {
            s.tasks.insert(task.clone(), PendingTask { node: node.clone(), attempt: *attempt });
        }
        Event::TimerScheduled { timer, node, .. } => {
            s.timers.insert(timer.clone(), node.clone());
        }
        Event::WaitStarted { signal, node } => {
            s.waits.insert(signal.clone(), node.clone());
        }
        Event::RunCompleted => s.status = RunStatus::Succeeded,
        Event::RunFailed { reason } => {
            s.status = RunStatus::Failed;
            s.error = Some(reason.clone());
            clear_pending(&mut s);
        }
    }
    s
}

fn clear_pending(s: &mut RunState) {
    s.unknown.clear();
    s.tasks.clear();
    s.timers.clear();
    s.waits.clear();
}

pub fn replay<'a>(events: impl IntoIterator<Item = &'a Event>) -> RunState {
    events.into_iter().fold(RunState::default(), evolve)
}

/// Why an input was refused. Nothing is recorded for a refused input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reject {
    /// Run already finished; late results are dropped.
    Finished(RunStatus),
    UnknownTask(String),
    UnknownTimer(String),
    NotWaiting(String),
    NotAnInput,
}

impl std::fmt::Display for Reject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reject::Finished(s) => write!(f, "run already finished ({s:?})"),
            Reject::UnknownTask(t) => write!(f, "task {t} is not pending"),
            Reject::UnknownTimer(t) => write!(f, "timer {t} is not pending"),
            Reject::NotWaiting(sig) => write!(f, "run is not waiting for signal {sig:?}"),
            Reject::NotAnInput => write!(f, "only input events can be submitted"),
        }
    }
}

pub const APPROVAL_PREFIX: &str = "approve.";

pub fn approval_signal(node: &str) -> String {
    format!("{APPROVAL_PREFIX}{node}")
}

/// Resolves an `OutcomeUnknown` task. Payload: `{"outcome": "completed", "result": ...}`,
/// `{"outcome": "retry"}` or `{"outcome": "failed", "error": "..."}`.
pub fn resolve_signal(task: &str) -> String {
    format!("resolve.{task}")
}

pub fn task_queue(kind: &NodeKind) -> Option<String> {
    match kind {
        NodeKind::Agent { agent_ref } => Some(format!("agent/{agent_ref}")),
        NodeKind::Activity { tool_ref } => Some(format!("tool/{tool_ref}")),
        _ => None,
    }
}

/// Validates `input` against the current state and returns `[input, consequences...]`.
pub fn decide(g: &WorkflowSpec, state: &RunState, input: Event, now: DateTime<Utc>) -> Result<Vec<Event>, Reject> {
    if state.status.is_terminal() {
        return Err(Reject::Finished(state.status));
    }
    let mut d = Decider { g, s: state.clone(), out: vec![], now };
    match &input {
        Event::RunStarted { .. } => {
            d.emit(input.clone());
            d.enter(&g.entrypoint);
        }
        Event::TaskCompleted { task, .. } => {
            let t = state.tasks.get(task).ok_or_else(|| Reject::UnknownTask(task.clone()))?.clone();
            d.emit(input.clone());
            d.advance(&t.node);
        }
        Event::TaskFailed { task, error, retryable } => {
            let t = state.tasks.get(task).ok_or_else(|| Reject::UnknownTask(task.clone()))?.clone();
            d.emit(input.clone());
            let policy = g.nodes[&t.node].retry_policy.as_ref().unwrap_or(&g.retry_policy);
            if *retryable && t.attempt < policy.max_attempts {
                let attempt = t.attempt + 1;
                let backoff = chrono::Duration::from_std(policy.backoff(attempt)).unwrap_or_default();
                d.schedule_task(&t.node, attempt, now + backoff);
            } else {
                d.fail(format!("node {} failed after {} attempt(s): {error}", t.node, t.attempt));
            }
        }
        Event::TimerFired { timer } => {
            let node = state.timers.get(timer).ok_or_else(|| Reject::UnknownTimer(timer.clone()))?.clone();
            d.emit(input.clone());
            d.advance(&node);
        }
        Event::TaskOutcomeUnknown { task } => {
            state.tasks.get(task).ok_or_else(|| Reject::UnknownTask(task.clone()))?;
            d.emit(input.clone());
        }
        Event::SignalReceived { signal, payload } => {
            let node = state.waits.get(signal).ok_or_else(|| Reject::NotWaiting(signal.clone()))?.clone();
            let unknown = signal.strip_prefix("resolve.").and_then(|t| state.unknown.get(t)).cloned();
            d.emit(input.clone());
            if let Some(t) = unknown {
                match payload["outcome"].as_str() {
                    Some("completed") => d.advance(&node),
                    Some("retry") => d.schedule_task(&node, t.attempt + 1, now),
                    _ => d.fail(format!("node {node}: outcome unknown, resolved as failed: {}", payload["error"].as_str().unwrap_or("no reason given"))),
                }
            } else {
                match &g.nodes[&node].kind {
                    NodeKind::Approval { on_reject, .. } if payload.get("approved") != Some(&Value::Bool(true)) => match on_reject {
                        Some(r) => d.enter(r),
                        None => d.fail(format!("approval {node} rejected")),
                    },
                    _ => d.advance(&node),
                }
            }
        }
        Event::RunCancelled { .. } => d.emit(input.clone()),
        _ => return Err(Reject::NotAnInput),
    }
    if d.s.status == RunStatus::Running && d.s.is_idle() {
        d.emit(Event::RunCompleted);
    }
    Ok(d.out)
}

struct Decider<'a> {
    g: &'a WorkflowSpec,
    s: RunState,
    out: Vec<Event>,
    now: DateTime<Utc>,
}

impl Decider<'_> {
    fn emit(&mut self, e: Event) {
        self.s = evolve(std::mem::take(&mut self.s), &e);
        self.out.push(e);
    }

    fn running(&self) -> bool {
        self.s.status == RunStatus::Running
    }

    fn fail(&mut self, reason: String) {
        if self.running() {
            self.emit(Event::RunFailed { reason });
        }
    }

    fn advance(&mut self, node: &NodeId) {
        for n in self.g.nodes[node].next.clone() {
            self.enter(&n);
        }
    }

    fn enter(&mut self, id: &NodeId) {
        if !self.running() {
            return;
        }
        let node = &self.g.nodes[id];
        if self.s.steps >= self.g.max_steps {
            return self.fail(format!("maxSteps {} exceeded entering {id}", self.g.max_steps));
        }
        if let Some(max) = node.max_iterations
            && self.s.visits.get(id).copied().unwrap_or(0) >= max
        {
            return self.fail(format!("node {id} exceeded maxIterations {max}"));
        }
        self.emit(Event::NodeEntered { node: id.clone() });

        match &node.kind {
            NodeKind::Agent { .. } | NodeKind::Activity { .. } => self.schedule_task(id, 1, self.now),
            NodeKind::Condition { path, equals, then, otherwise } => {
                let data = Value::Object(self.s.data.clone());
                let target = if lookup(&data, path) == Some(equals) { then } else { otherwise };
                self.enter(target);
            }
            NodeKind::Join { mode } => {
                let swallowed = self.s.join_swallow.get(id).is_some_and(|n| *n > 0);
                self.emit(Event::JoinArrived { node: id.clone() });
                let arrived = self.s.join_arrivals.get(id).copied().unwrap_or(0);
                let need = self.g.in_degree(id);
                if !swallowed && (*mode == JoinMode::Any || arrived >= need) {
                    let swallow = if *mode == JoinMode::Any { need - 1 } else { 0 };
                    self.emit(Event::JoinFired { node: id.clone(), swallow });
                    self.advance(id);
                }
            }
            NodeKind::Timer { duration } => {
                let d = parse_duration(duration).expect("validated at admission");
                let timer = format!("{id}#{}", self.s.visits[id]);
                let due_at = self.now + chrono::Duration::from_std(d).unwrap_or_default();
                self.emit(Event::TimerScheduled { timer, node: id.clone(), due_at });
            }
            NodeKind::Approval { .. } => self.emit(Event::WaitStarted { signal: approval_signal(id), node: id.clone() }),
            NodeKind::Signal { signal } => self.emit(Event::WaitStarted { signal: signal.clone(), node: id.clone() }),
        }
    }

    fn schedule_task(&mut self, node: &NodeId, attempt: u32, not_before: DateTime<Utc>) {
        // task ids are deterministic: node, visit and attempt
        let task = format!("{node}#{}.{attempt}", self.s.visits[node]);
        let queue = task_queue(&self.g.nodes[node].kind).expect("task node");
        self.emit(Event::TaskScheduled { task, node: node.clone(), attempt, queue, not_before });
    }
}
