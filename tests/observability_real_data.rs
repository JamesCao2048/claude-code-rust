//! End-to-end check: feed REAL engine output through the shipped workflow-tail
//! parser and assert it produces the live-progress rows the TUI renders.
//!
//! Fixtures under `tests/fixtures/real_abs4_*.jsonl` were captured from a real
//! `generate_ascendc_direct` run of `NPUKernelBench` `4_Abs` on an `Ascend910B3`
//! (compiled `AscendC` kernel + on-NPU eval). This locks the TUI parser to the
//! actual event / agent-stream schema the Python engine emits.

// Test crate: expect/unwrap are the idiom for asserting fixture-parse outcomes.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use lingxi_ascendc::agent::workflow_tail::{
    SubagentPhase, WorkflowProgress, extract_run_target, map_agent_stream_line, map_event_line,
};

fn fixture(name: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    std::fs::read_to_string(p).expect("fixture readable")
}

#[test]
fn real_run_command_is_detected_and_paths_resolved() {
    let cmd =
        "lingxi-ascendc run --workflow generate_ascendc_direct --select L1/4 --run-id abs4 -v";
    let t = extract_run_target(cmd, Path::new("/runs")).expect("run detected");
    assert_eq!(t.run_id.as_deref(), Some("abs4"));
    assert_eq!(t.events_path(), PathBuf::from("/runs/abs4/events.jsonl"));
    assert_eq!(t.agent_stream_path(), PathBuf::from("/runs/abs4/agent_stream.jsonl"));
}

#[test]
fn real_events_jsonl_parses_into_progress_rows() {
    let text = fixture("real_abs4_events.jsonl");
    let mut started = None;
    let mut n_started = 0;
    let mut n_completed = 0;
    let mut details: Vec<String> = Vec::new();
    for line in text.lines() {
        match map_event_line(line) {
            Some(WorkflowProgress::Started { workflow, verbose, params }) => {
                started = Some((workflow, verbose, params.len()));
            }
            Some(WorkflowProgress::ActionStarted { detail, .. }) => {
                n_started += 1;
                details.extend(detail);
            }
            Some(WorkflowProgress::ActionCompleted { detail, outcome, .. }) => {
                n_completed += 1;
                details.extend(detail);
                assert!(!outcome.is_empty(), "completed row carries an outcome string");
            }
            _ => {}
        }
    }
    let (wf, verbose, nparams) = started.expect("run_started mapped");
    assert_eq!(wf, "generate_ascendc_direct");
    assert!(verbose, "verbose=true carried through to the header");
    assert!(nparams >= 1, "header params present");
    assert!(n_started >= 1, "at least one action_started row");
    assert!(n_completed >= 1, "at least one action_completed row");
    // action_detail() pulled the structured task fields out of the real event.
    assert!(details.iter().any(|d| d.contains("scope=full")), "action detail: {details:?}");
    println!(
        "events -> header(wf={wf}, verbose={verbose}, params={nparams}), started={n_started}, completed={n_completed}"
    );
    for d in &details {
        println!("  action detail row: [{d}]");
    }
}

#[test]
fn real_agent_stream_parses_into_nested_subagent_rows() {
    let text = fixture("real_abs4_agent_stream.jsonl");
    let mut uses = 0;
    let mut results = 0;
    let mut action_ids: BTreeSet<String> = BTreeSet::new();
    let mut sample = None;
    for line in text.lines() {
        if let Some(ev) = map_agent_stream_line(line) {
            action_ids.insert(ev.action_id.clone());
            match ev.phase {
                SubagentPhase::ToolUse => {
                    uses += 1;
                    if sample.is_none() {
                        sample = Some(ev.clone());
                    }
                }
                SubagentPhase::ToolResult => {
                    results += 1;
                    assert!(ev.status.is_some(), "tool_result carries a status");
                }
            }
        }
    }
    assert!(uses > 0, "tool_use rows present");
    assert!(results > 0, "tool_result rows present");
    assert!(!action_ids.is_empty(), "events key to action rows via action_id");
    let s = sample.expect("a tool_use sample");
    assert!(!s.agent_name.is_empty(), "subagent name present");
    assert!(!s.name.is_empty(), "tool name present");
    println!("agent_stream -> tool_use={uses}, tool_result={results}, action_ids={action_ids:?}");
    println!("  sample nested row: agent={} tool={} detail={:?}", s.agent_name, s.name, s.detail);
}
