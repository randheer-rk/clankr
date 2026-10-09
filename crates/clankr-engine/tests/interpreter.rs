use chrono::{DateTime, TimeZone, Utc};
use clankr_engine::run::{Event, Reject, RunStatus, approval_signal};
use clankr_engine::{RunState, WorkflowSpec, decide, replay};
use proptest::prelude::*;
use serde_json::{Value, json};

/// Drives one run through `decide`, keeping the full history, and checks after every
/// step that replaying history reproduces the live state (the core durability invariant).
struct Sim {
    g: WorkflowSpec,
    state: RunState,
    history: Vec<Event>,
    now: DateTime<Utc>,
}

impl Sim {
    fn new(yaml: &str) -> Self {
        let g: WorkflowSpec = serde_yaml_ng::from_str(yaml).unwrap();
        g.validate().unwrap();
        Self { g, state: RunState::default(), history: vec![], now: Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap() }
    }

    fn input(&mut self, e: Event) -> Result<Vec<Event>, Reject> {
        let evs = decide(&self.g, &self.state, e, self.now)?;
        for ev in &evs {
            self.state = clankr_engine::evolve(std::mem::take(&mut self.state), ev);
        }
        self.history.extend(evs.iter().cloned());
        assert_eq!(replay(&self.history), self.state, "replay diverged from live state");
        Ok(evs)
    }

    fn start(&mut self, input: Value) {
        self.input(Event::RunStarted { input }).unwrap();
    }

    fn complete(&mut self, task: &str, result: Value) -> Result<Vec<Event>, Reject> {
        self.input(Event::TaskCompleted { task: task.into(), result })
    }

    fn fail(&mut self, task: &str, retryable: bool) -> Result<Vec<Event>, Reject> {
        self.input(Event::TaskFailed { task: task.into(), error: "boom".into(), retryable })
    }

    fn pending(&self) -> Vec<String> {
        self.state.tasks.keys().cloned().collect()
    }

    fn count(&self, pred: impl Fn(&Event) -> bool) -> usize {
        self.history.iter().filter(|e| pred(e)).count()
    }
}

const RESEARCH: &str = "
entrypoint: plan
nodes:
  plan: {type: agent, agentRef: planner, next: [research, verify]}
  research: {type: agent, agentRef: researcher, next: gather}
  verify: {type: agent, agentRef: verifier, next: gather}
  gather: {type: join, next: review}
  review: {type: approval, approverGroup: editors, next: publish}
  publish: {type: activity, toolRef: publisher}";

#[test]
fn research_graph_end_to_end() {
    let mut s = Sim::new(RESEARCH);
    s.start(json!({"topic": "rust"}));
    assert_eq!(s.pending(), ["plan#1.1"]);
    match &s.history[2] {
        Event::TaskScheduled { queue, .. } => assert_eq!(queue, "agent/planner"),
        e => panic!("unexpected {e:?}"),
    }

    s.complete("plan#1.1", json!({"topics": 2})).unwrap();
    assert_eq!(s.pending(), ["research#1.1", "verify#1.1"]);
    s.complete("verify#1.1", json!({"ok": true})).unwrap();
    assert!(s.state.waits.is_empty(), "join must wait for both branches");
    s.complete("research#1.1", json!({"notes": "x"})).unwrap();
    assert_eq!(s.state.waits.keys().collect::<Vec<_>>(), [&approval_signal("review")]);

    // duplicate delivery of an already-applied result is refused, not double-applied
    assert_eq!(s.complete("research#1.1", json!({})), Err(Reject::UnknownTask("research#1.1".into())));
    // a signal nobody waits for is refused
    assert!(matches!(s.input(Event::SignalReceived { signal: "nope".into(), payload: json!({}) }), Err(Reject::NotWaiting(_))));

    s.input(Event::SignalReceived { signal: approval_signal("review"), payload: json!({"approved": true, "by": "ed"}) }).unwrap();
    s.complete("publish#1.1", json!({"url": "https://x"})).unwrap();
    assert_eq!(s.state.status, RunStatus::Succeeded);
    assert_eq!(s.state.data["review"]["by"], "ed");
    assert_eq!(s.count(|e| matches!(e, Event::RunCompleted)), 1);
    assert_eq!(s.complete("publish#1.1", json!({})), Err(Reject::Finished(RunStatus::Succeeded)));
}

proptest! {
    /// CL-21-04: join outcome doesn't depend on branch completion order.
    #[test]
    fn fan_out_join_is_order_independent(order in Just(vec![0usize, 1, 2, 3]).prop_shuffle()) {
        let mut s = Sim::new("
entrypoint: plan
nodes:
  plan: {type: agent, agentRef: p, next: [a, b, c, d]}
  a: {type: agent, agentRef: w, next: j}
  b: {type: agent, agentRef: w, next: j}
  c: {type: agent, agentRef: w, next: j}
  d: {type: agent, agentRef: w, next: j}
  j: {type: join, next: done}
  done: {type: activity, toolRef: t}");
        s.start(json!(null));
        s.complete("plan#1.1", json!(1)).unwrap();
        for i in order {
            let n = ["a", "b", "c", "d"][i];
            s.complete(&format!("{n}#1.1"), json!(n)).unwrap();
        }
        prop_assert_eq!(s.pending(), vec!["done#1.1".to_string()]);
        prop_assert_eq!(s.count(|e| matches!(e, Event::JoinFired { .. })), 1);
        s.complete("done#1.1", json!(null)).unwrap();
        prop_assert_eq!(s.state.status, RunStatus::Succeeded);
    }
}

#[test]
fn retries_with_deterministic_backoff_then_gives_up() {
    let g = "
entrypoint: a
retryPolicy: {maxAttempts: 3, initialBackoff: 1s, maxBackoff: 60s}
nodes:
  a: {type: agent, agentRef: x}";
    let mut s = Sim::new(g);
    s.start(json!(null));
    let evs = s.fail("a#1.1", true).unwrap();
    let Event::TaskScheduled { task, attempt, not_before, .. } = &evs[1] else { panic!("{evs:?}") };
    assert_eq!((task.as_str(), *attempt, *not_before - s.now), ("a#1.2", 2, chrono::Duration::seconds(1)));
    let evs = s.fail("a#1.2", true).unwrap();
    let Event::TaskScheduled { not_before, .. } = &evs[1] else { panic!() };
    assert_eq!(*not_before - s.now, chrono::Duration::seconds(2));
    s.fail("a#1.3", true).unwrap();
    assert_eq!(s.state.status, RunStatus::Failed);
    assert!(s.state.error.as_deref().unwrap().contains("after 3 attempt(s)"));

    // non-retryable fails immediately
    let mut s = Sim::new(g);
    s.start(json!(null));
    s.fail("a#1.1", false).unwrap();
    assert_eq!(s.state.status, RunStatus::Failed);
}

const LOOP: &str = "
entrypoint: work
nodes:
  work: {type: agent, agentRef: w, next: check}
  check: {type: condition, path: $.work.done, equals: true, then: ship, else: work, maxIterations: 3}
  ship: {type: activity, toolRef: t}";

#[test]
fn loops_run_until_condition_or_bound() {
    let mut s = Sim::new(LOOP);
    s.start(json!(null));
    s.complete("work#1.1", json!({"done": false})).unwrap();
    s.complete("work#2.1", json!({"done": true})).unwrap();
    assert_eq!(s.pending(), ["ship#1.1"]);

    let mut s = Sim::new(LOOP);
    s.start(json!(null));
    // check may run 3 times; its 3rd pass loops back once more, the 4th entry trips the bound
    for i in 1..=4 {
        s.complete(&format!("work#{i}.1"), json!({"done": false})).unwrap();
    }
    assert_eq!(s.state.status, RunStatus::Failed);
    assert_eq!(s.state.error.as_deref(), Some("node check exceeded maxIterations 3"));
}

#[test]
fn max_steps_bounds_a_run() {
    let mut s = Sim::new(&format!("maxSteps: 4\n{LOOP}").replace("maxIterations: 3", "maxIterations: 100"));
    s.start(json!(null));
    s.complete("work#1.1", json!({"done": false})).unwrap();
    s.complete("work#2.1", json!({"done": false})).unwrap();
    assert_eq!(s.state.status, RunStatus::Failed);
    assert!(s.state.error.as_deref().unwrap().starts_with("maxSteps 4 exceeded"));
}

#[test]
fn any_join_fires_once_and_absorbs_late_branches() {
    let mut s = Sim::new("
entrypoint: go
nodes:
  go: {type: agent, agentRef: p, next: [fast, slow]}
  fast: {type: agent, agentRef: w, next: first}
  slow: {type: agent, agentRef: w, next: first}
  first: {type: join, mode: any, next: done}
  done: {type: activity, toolRef: t}");
    s.start(json!(null));
    s.complete("go#1.1", json!(null)).unwrap();
    s.complete("fast#1.1", json!(1)).unwrap();
    assert_eq!(s.pending(), ["done#1.1", "slow#1.1"]);
    s.complete("slow#1.1", json!(2)).unwrap();
    assert_eq!(s.pending(), ["done#1.1"], "late branch must not re-fire the join");
    s.complete("done#1.1", json!(null)).unwrap();
    assert_eq!(s.state.status, RunStatus::Succeeded);
    assert_eq!(s.count(|e| matches!(e, Event::JoinFired { .. })), 1);
}

#[test]
fn approval_rejection_routes_or_fails() {
    let g = "
entrypoint: review
nodes:
  review: {type: approval, approverGroup: eds, next: ship, onReject: fix}
  ship: {type: activity, toolRef: t}
  fix: {type: agent, agentRef: f}";
    let mut s = Sim::new(g);
    s.start(json!(null));
    s.input(Event::SignalReceived { signal: approval_signal("review"), payload: json!({"approved": false}) }).unwrap();
    assert_eq!(s.pending(), ["fix#1.1"]);

    let mut s = Sim::new(&g.replace(", onReject: fix", "").replace("\n  fix: {type: agent, agentRef: f}", ""));
    s.start(json!(null));
    s.input(Event::SignalReceived { signal: approval_signal("review"), payload: json!({}) }).unwrap();
    assert_eq!(s.state.status, RunStatus::Failed);
}

#[test]
fn timers_and_cancel() {
    let mut s = Sim::new("
entrypoint: wait
nodes:
  wait: {type: timer, duration: 30s, next: go}
  go: {type: agent, agentRef: g}");
    s.start(json!(null));
    let Some(Event::TimerScheduled { timer, due_at, .. }) = s.history.last().cloned() else { panic!() };
    assert_eq!(due_at - s.now, chrono::Duration::seconds(30));
    s.input(Event::TimerFired { timer: timer.clone() }).unwrap();
    assert_eq!(s.input(Event::TimerFired { timer }), Err(Reject::UnknownTimer("wait#1".into())));
    assert_eq!(s.pending(), ["go#1.1"]);

    s.input(Event::RunCancelled { reason: "operator".into() }).unwrap();
    assert_eq!(s.state.status, RunStatus::Cancelled);
    assert!(s.state.is_idle());
    assert_eq!(s.complete("go#1.1", json!(null)), Err(Reject::Finished(RunStatus::Cancelled)));
}

#[test]
fn consequence_events_cannot_be_injected() {
    let mut s = Sim::new(RESEARCH);
    s.start(json!(null));
    assert_eq!(s.input(Event::RunCompleted), Err(Reject::NotAnInput));
}
