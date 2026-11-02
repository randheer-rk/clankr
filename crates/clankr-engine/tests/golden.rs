//! Golden execution fixtures (CL-34-08, PRD §38): each file pins a graph, the external inputs
//! fed to a run, and the exact event history and final state the interpreter must produce.
//! Any change in interpreter behaviour (an event added, reordered, renamed, a different task id
//! or backoff) fails here instead of surfacing as divergent replays of real runs.
//!
//! Regenerate after an *intended* change with `UPDATE_GOLDEN=1 cargo test -p clankr-engine --test golden`
//! and review the diff like code.

use chrono::{Duration, TimeZone, Utc};
use clankr_engine::run::Event;
use clankr_engine::{RunState, WorkflowSpec, decide, evolve, replay};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize)]
struct Fixture {
    description: String,
    graph: Value,
    /// External inputs in order; `after` advances the clock (seconds) before the input.
    inputs: Vec<Step>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expect: Option<Expect>,
}

#[derive(Serialize, Deserialize)]
struct Step {
    #[serde(default, skip_serializing_if = "is_zero")]
    after: i64,
    #[serde(flatten)]
    event: Event,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Expect {
    history: Vec<Event>,
    state: RunState,
}

fn run(f: &Fixture) -> Expect {
    let g: WorkflowSpec = serde_json::from_value(f.graph.clone()).expect("graph parses");
    g.validate().expect("graph valid");
    let mut now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let (mut state, mut history) = (RunState::default(), vec![]);
    for (i, s) in f.inputs.iter().enumerate() {
        now += Duration::seconds(s.after);
        let evs = decide(&g, &state, s.event.clone(), now).unwrap_or_else(|e| panic!("input {i} rejected: {e}"));
        for e in &evs {
            state = evolve(state, e);
        }
        history.extend(evs);
    }
    assert_eq!(replay(&history), state, "replay of the produced history diverges from live state");
    Expect { history, state }
}

#[test]
fn golden_fixtures() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|x| x == "yaml")).collect();
    files.sort();
    assert!(files.len() >= 6, "fixtures missing from {}", dir.display());
    let mut failures = vec![];
    for path in files {
        let mut f: Fixture = serde_yaml_ng::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let got = run(&f);
        if update {
            f.expect = Some(got);
            std::fs::write(&path, serde_yaml_ng::to_string(&f).unwrap()).unwrap();
            continue;
        }
        match &f.expect {
            Some(want) if *want == got => {}
            Some(want) => {
                let first = want.history.iter().zip(&got.history).position(|(a, b)| a != b).unwrap_or(want.history.len().min(got.history.len()));
                failures.push(format!(
                    "{}: history differs at event {first}\n  want {:?}\n  got  {:?}",
                    path.file_name().unwrap().to_string_lossy(),
                    want.history.get(first),
                    got.history.get(first)
                ));
            }
            None => failures.push(format!("{}: no expectation recorded (run with UPDATE_GOLDEN=1)", path.display())),
        }
    }
    assert!(failures.is_empty(), "golden fixtures changed (if intended: UPDATE_GOLDEN=1, then review the diff)\n{}", failures.join("\n"));
}
