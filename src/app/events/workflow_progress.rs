// Copyright 2025 Simon Peter Rothgang
// SPDX-License-Identifier: Apache-2.0

//! Wiring for live `lingxi-ascendc run` progress.
//!
//! Two halves:
//! - [`maybe_start_workflow_tail`]: called when a Bash tool call is observed; if
//!   the command is a `lingxi-ascendc run`, it attaches a [`WorkflowProgressState`]
//!   to that tool call and spawns a `LocalSet` task that tails the run's
//!   `events.jsonl`, forwarding each update as a [`ClientEvent::WorkflowProgress`].
//! - [`apply_workflow_progress`]: routes one such update onto the tool call's
//!   child rows and marks it for redraw.

use crate::agent::events::ClientEvent;
use crate::agent::workflow_tail::{
    self, ActionStatus, FinalizeKind, SubagentPhase, SubagentToolEvent, WorkflowProgress,
};
use crate::app::{
    App, MessageBlock, ToolCallInfo, WorkflowActionCompletion, WorkflowActionRow,
    WorkflowActionStatus, WorkflowFinalizeKind, WorkflowFinalizeRow, WorkflowProgressState,
    WorkflowSubagentToolRow,
};

/// Inspect a freshly-built Bash tool call. If its command is a fresh
/// `lingxi-ascendc run`, attach live-progress state and start the tail task.
///
/// `command` is taken from the tool call's terminal command or its
/// `raw_input.command` (the SDK Bash tool's input). `cwd` is the app's working
/// directory — the engine resolves the run dir relative to it.
pub(super) fn maybe_start_workflow_tail(app: &mut App, tool_call_id: &str) {
    // Look up the just-inserted tool call.
    let Some((mi, bi)) = app.lookup_tool_call(tool_call_id) else {
        return;
    };
    let cwd = app.cwd_raw.clone();
    let command = {
        let Some(MessageBlock::ToolCall(tc)) = app.messages.get(mi).and_then(|m| m.blocks.get(bi))
        else {
            return;
        };
        if !tc.is_execute_tool() || tc.workflow_progress.is_some() {
            return; // not a shell call, or tail already attached
        }
        command_of(tc)
    };
    let Some(command) = command else {
        return;
    };
    if cwd.is_empty() {
        return;
    }
    let Some(target) = workflow_tail::extract_run_target(&command, std::path::Path::new(&cwd))
    else {
        return;
    };

    // Live progress needs a deterministic run dir, i.e. an explicit --run-id
    // (the workflow command templates always pass one). Without it we cannot
    // know which <cwd>/<run-id>/events.jsonl to tail — tailing <cwd>/events.jsonl
    // would wait forever on a file that never appears — so skip live progress
    // and let the Bash call render its normal output.
    if target.run_id.is_none() {
        tracing::debug!(
            target: crate::logging::targets::APP_COMMAND,
            event_name = "workflow_tail_skipped_no_run_id",
            message = "lingxi-ascendc run without --run-id; skipping live progress",
            outcome = "skip",
        );
        return;
    }
    let events_path = target.events_path();

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    // Attach state to the tool call so the render path can show child rows.
    if let Some(MessageBlock::ToolCall(tc)) =
        app.messages.get_mut(mi).and_then(|m| m.blocks.get_mut(bi))
    {
        tc.workflow_progress = Some(WorkflowProgressState::new(stop_tx));
        tc.mark_tool_call_render_dirty();
    } else {
        return;
    }

    let stream_path = target.agent_stream_path();
    tracing::info!(
        target: crate::logging::targets::APP_COMMAND,
        event_name = "workflow_tail_started",
        message = "started tailing workflow events.jsonl",
        outcome = "start",
        tool_call_id = %tool_call_id,
        run_id = target.run_id.as_deref().unwrap_or("<newest-dir>"),
        events_path = %events_path.display(),
    );

    // The emit closures capture a non-Send mpsc sender (ClientEvent is !Send),
    // so these must run on the LocalSet — same pattern as update_check.
    let event_tx = app.event_tx.clone();
    let id = tool_call_id.to_owned();
    let stop_rx_events = stop_rx.clone();
    tokio::task::spawn_local(async move {
        workflow_tail::tail_events(events_path, stop_rx_events, move |update| {
            let _ =
                event_tx.send(ClientEvent::WorkflowProgress { tool_call_id: id.clone(), update });
        })
        .await;
    });

    // Phase 2: also tail the sibling agent_stream.jsonl for subagent tool
    // calls. The engine does not emit this file today, so this is a silent
    // no-op until it appears (poll-until-exists); it shares the events tail's
    // stop signal so finalize/abort stops both.
    let event_tx2 = app.event_tx.clone();
    let id2 = tool_call_id.to_owned();
    tokio::task::spawn_local(async move {
        workflow_tail::tail_agent_stream(stream_path, stop_rx, move |event| {
            let _ = event_tx2
                .send(ClientEvent::WorkflowSubagentEvent { tool_call_id: id2.clone(), event });
        })
        .await;
    });
}

/// Pull the shell command for a tool call: prefer the captured terminal
/// command; fall back to `raw_input.command` (SDK Bash input).
fn command_of(tc: &ToolCallInfo) -> Option<String> {
    if let Some(cmd) = tc.terminal_command.as_ref().filter(|c| !c.trim().is_empty()) {
        return Some(cmd.clone());
    }
    tc.raw_input
        .as_ref()
        .and_then(|v| v.get("command"))
        .and_then(|v| v.as_str())
        .filter(|c| !c.trim().is_empty())
        .map(ToOwned::to_owned)
}

/// Apply one [`WorkflowProgress`] update to the named tool call's child rows.
pub fn apply_workflow_progress(app: &mut App, tool_call_id: &str, update: WorkflowProgress) {
    let Some((mi, bi)) = app.lookup_tool_call(tool_call_id) else {
        return;
    };
    let mut changed = false;
    if let Some(MessageBlock::ToolCall(tc)) =
        app.messages.get_mut(mi).and_then(|m| m.blocks.get_mut(bi))
    {
        let Some(state) = tc.workflow_progress.as_mut() else {
            return;
        };
        changed = apply_to_state(state, update);
        if changed {
            tc.mark_tool_call_layout_dirty();
        }
    }
    if changed {
        app.sync_render_cache_slot(mi, bi);
        app.recompute_message_retained_bytes(mi);
        app.invalidate_layout(crate::app::InvalidationLevel::MessageChanged(mi));
    }
}

/// Apply one subagent tool event (Phase 2) to the named tool call: nest it
/// under the action row whose `action_id` matches.
pub fn apply_workflow_subagent_event(app: &mut App, tool_call_id: &str, event: &SubagentToolEvent) {
    let Some((mi, bi)) = app.lookup_tool_call(tool_call_id) else {
        return;
    };
    let mut changed = false;
    if let Some(MessageBlock::ToolCall(tc)) =
        app.messages.get_mut(mi).and_then(|m| m.blocks.get_mut(bi))
        && let Some(state) = tc.workflow_progress.as_mut()
    {
        changed = apply_subagent_to_state(state, event);
        if changed {
            tc.mark_tool_call_layout_dirty();
        }
    }
    if changed {
        app.sync_render_cache_slot(mi, bi);
        app.recompute_message_retained_bytes(mi);
        app.invalidate_layout(crate::app::InvalidationLevel::MessageChanged(mi));
    }
}

/// Nest a subagent tool event under its parent action row. A `tool_use` adds
/// (or refreshes) a row; a `tool_result` marks the matching row completed.
/// Returns whether anything visible changed. Events whose `action_id` has no
/// matching action row yet are dropped (the `action_started` event arrives
/// first in practice; a late attach simply misses early subagent tools).
fn apply_subagent_to_state(state: &mut WorkflowProgressState, event: &SubagentToolEvent) -> bool {
    let Some(row) = state.actions.iter_mut().find(|a| a.action_id == event.action_id) else {
        return false;
    };
    match event.phase {
        SubagentPhase::ToolUse => {
            if let Some(existing) =
                row.subagent_tools.iter().find(|t| t.tool_call_id == event.tool_call_id)
            {
                // Re-delivery of the same tool_use is a no-op unless it would
                // change the name.
                if existing.name == event.name {
                    return false;
                }
            }
            row.subagent_tools.push(WorkflowSubagentToolRow {
                tool_call_id: event.tool_call_id.clone(),
                name: event.name.clone(),
                detail: event.detail.clone(),
                completed: false,
                status: None,
            });
            true
        }
        SubagentPhase::ToolResult => {
            if let Some(t) =
                row.subagent_tools.iter_mut().find(|t| t.tool_call_id == event.tool_call_id)
            {
                t.completed = true;
                t.status.clone_from(&event.status);
            } else {
                // Result without a prior use: synthesize a completed row.
                row.subagent_tools.push(WorkflowSubagentToolRow {
                    tool_call_id: event.tool_call_id.clone(),
                    name: event.name.clone(),
                    detail: event.detail.clone(),
                    completed: true,
                    status: event.status.clone(),
                });
            }
            true
        }
    }
}

/// Mutate `state` for one update. Returns whether anything visible changed.
fn apply_to_state(state: &mut WorkflowProgressState, update: WorkflowProgress) -> bool {
    match update {
        WorkflowProgress::Started { workflow, params, verbose } => {
            let new = (!workflow.is_empty()).then_some(workflow);
            let changed =
                state.workflow != new || state.params != params || state.verbose != verbose;
            if !changed {
                return false;
            }
            state.workflow = new;
            state.params = params;
            state.verbose = verbose;
            true
        }
        WorkflowProgress::ActionStarted { action_id, kind, name, detail } => {
            if state.actions.iter().any(|a| a.action_id == action_id) {
                return false; // dedup re-delivery
            }
            state.actions.push(WorkflowActionRow {
                action_id,
                kind,
                name,
                detail,
                completed: None,
                subagent_tools: Vec::new(),
            });
            true
        }
        WorkflowProgress::ActionCompleted { action_id, status, outcome, detail } => {
            let completion = WorkflowActionCompletion { status: map_status(status), outcome };
            if let Some(row) = state.actions.iter_mut().find(|a| a.action_id == action_id) {
                if row.detail.is_none() {
                    row.detail = detail;
                }
                row.completed = Some(completion);
            } else {
                // Completion without a prior start (e.g. tail attached late):
                // synthesize a row so the outcome is still visible.
                state.actions.push(WorkflowActionRow {
                    action_id,
                    kind: String::new(),
                    name: String::new(),
                    detail,
                    completed: Some(completion),
                    subagent_tools: Vec::new(),
                });
            }
            true
        }
        WorkflowProgress::Finalized { kind, summary } => {
            state.finalized = Some(WorkflowFinalizeRow { kind: map_finalize(kind), summary });
            // Terminal event: the tail task self-exits, but defensively stop it.
            state.stop_tail();
            true
        }
    }
}

fn map_status(status: ActionStatus) -> WorkflowActionStatus {
    match status {
        ActionStatus::Ok => WorkflowActionStatus::Ok,
        ActionStatus::Retry => WorkflowActionStatus::Retry,
        ActionStatus::Fail => WorkflowActionStatus::Fail,
    }
}

fn map_finalize(kind: FinalizeKind) -> WorkflowFinalizeKind {
    match kind {
        FinalizeKind::Done => WorkflowFinalizeKind::Done,
        FinalizeKind::Aborted => WorkflowFinalizeKind::Aborted,
        FinalizeKind::Escalated => WorkflowFinalizeKind::Escalated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_state() -> WorkflowProgressState {
        let (tx, _rx) = tokio::sync::watch::channel(false);
        WorkflowProgressState::new(tx)
    }

    fn started(workflow: &str) -> WorkflowProgress {
        WorkflowProgress::Started { workflow: workflow.into(), params: Vec::new(), verbose: false }
    }

    #[test]
    fn started_sets_workflow_once() {
        let mut s = empty_state();
        assert!(apply_to_state(&mut s, started("gen")));
        assert_eq!(s.workflow.as_deref(), Some("gen"));
        // Re-delivery of identical start is a no-op.
        assert!(!apply_to_state(&mut s, started("gen")));
    }

    #[test]
    fn started_stores_params_and_verbose() {
        let mut s = empty_state();
        assert!(!s.verbose);
        assert!(s.params.is_empty());
        let upd = WorkflowProgress::Started {
            workflow: "generate_ascendc".into(),
            params: vec![("op".into(), "3_Add".into()), ("via".into(), "cannbot".into())],
            verbose: true,
        };
        assert!(apply_to_state(&mut s, upd));
        assert_eq!(s.workflow.as_deref(), Some("generate_ascendc"));
        assert!(s.verbose);
        assert_eq!(
            s.params,
            vec![("op".to_owned(), "3_Add".to_owned()), ("via".to_owned(), "cannbot".to_owned())]
        );
        // Re-delivery of the identical Started is a no-op even with params.
        let same = WorkflowProgress::Started {
            workflow: "generate_ascendc".into(),
            params: vec![("op".into(), "3_Add".into()), ("via".into(), "cannbot".into())],
            verbose: true,
        };
        assert!(!apply_to_state(&mut s, same));
    }

    #[test]
    fn action_start_then_complete_updates_same_row() {
        let mut s = empty_state();
        apply_to_state(
            &mut s,
            WorkflowProgress::ActionStarted {
                action_id: "a1".into(),
                kind: "spawn_agent".into(),
                name: "worker".into(),
                detail: Some("scope=full".into()),
            },
        );
        assert_eq!(s.actions.len(), 1);
        assert_eq!(s.actions[0].detail.as_deref(), Some("scope=full"));
        assert!(s.actions[0].completed.is_none());

        apply_to_state(
            &mut s,
            WorkflowProgress::ActionCompleted {
                action_id: "a1".into(),
                status: ActionStatus::Ok,
                outcome: "clean".into(),
                detail: None,
            },
        );
        assert_eq!(s.actions.len(), 1, "completion updates in place");
        let done = s.actions[0].completed.as_ref().unwrap();
        assert_eq!(done.status, WorkflowActionStatus::Ok);
        assert_eq!(done.outcome, "clean");
    }

    #[test]
    fn duplicate_action_started_is_deduped() {
        let mut s = empty_state();
        let mk = || WorkflowProgress::ActionStarted {
            action_id: "a1".into(),
            kind: "verify".into(),
            name: "verify".into(),
            detail: None,
        };
        assert!(apply_to_state(&mut s, mk()));
        assert!(!apply_to_state(&mut s, mk()));
        assert_eq!(s.actions.len(), 1);
    }

    #[test]
    fn completion_without_start_synthesizes_row() {
        let mut s = empty_state();
        apply_to_state(
            &mut s,
            WorkflowProgress::ActionCompleted {
                action_id: "late".into(),
                status: ActionStatus::Fail,
                outcome: "boom".into(),
                detail: Some("args:target=ascendc".into()),
            },
        );
        assert_eq!(s.actions.len(), 1);
        assert_eq!(s.actions[0].action_id, "late");
        assert_eq!(s.actions[0].detail.as_deref(), Some("args:target=ascendc"));
        assert_eq!(s.actions[0].completed.as_ref().unwrap().status, WorkflowActionStatus::Fail);
    }

    fn started_action(s: &mut WorkflowProgressState, id: &str) {
        apply_to_state(
            s,
            WorkflowProgress::ActionStarted {
                action_id: id.into(),
                kind: "spawn_agent".into(),
                name: "worker".into(),
                detail: None,
            },
        );
    }

    fn sub_event(action_id: &str, tool_call_id: &str, phase: SubagentPhase) -> SubagentToolEvent {
        SubagentToolEvent {
            action_id: action_id.into(),
            agent_name: "worker".into(),
            tool_call_id: tool_call_id.into(),
            phase,
            name: "Bash".into(),
            detail: matches!(phase, SubagentPhase::ToolUse)
                .then(|| "deploy + build kernel".to_owned()),
            status: matches!(phase, SubagentPhase::ToolResult).then(|| "ok".to_owned()),
        }
    }

    #[test]
    fn subagent_tool_use_then_result_nests_and_completes() {
        let mut s = empty_state();
        started_action(&mut s, "a1");
        assert!(apply_subagent_to_state(&mut s, &sub_event("a1", "t1", SubagentPhase::ToolUse)));
        assert_eq!(s.actions[0].subagent_tools.len(), 1);
        assert!(!s.actions[0].subagent_tools[0].completed);
        // detail from the tool_use is carried onto the row.
        assert_eq!(s.actions[0].subagent_tools[0].detail.as_deref(), Some("deploy + build kernel"));

        assert!(apply_subagent_to_state(&mut s, &sub_event("a1", "t1", SubagentPhase::ToolResult)));
        assert_eq!(s.actions[0].subagent_tools.len(), 1, "result updates in place");
        assert!(s.actions[0].subagent_tools[0].completed);
        assert_eq!(s.actions[0].subagent_tools[0].status.as_deref(), Some("ok"));
    }

    #[test]
    fn subagent_event_for_unknown_action_is_dropped() {
        let mut s = empty_state();
        started_action(&mut s, "a1");
        // Event for a different action id -> no row anywhere, no change.
        assert!(!apply_subagent_to_state(
            &mut s,
            &sub_event("other", "t1", SubagentPhase::ToolUse)
        ));
        assert!(s.actions[0].subagent_tools.is_empty());
    }

    #[test]
    fn subagent_result_without_use_synthesizes_completed_row() {
        let mut s = empty_state();
        started_action(&mut s, "a1");
        assert!(apply_subagent_to_state(
            &mut s,
            &sub_event("a1", "late", SubagentPhase::ToolResult)
        ));
        assert_eq!(s.actions[0].subagent_tools.len(), 1);
        assert!(s.actions[0].subagent_tools[0].completed);
    }

    #[test]
    fn finalize_sets_summary_and_stops_tail() {
        let mut s = empty_state();
        assert!(s.stop.is_some());
        apply_to_state(
            &mut s,
            WorkflowProgress::Finalized { kind: FinalizeKind::Done, summary: "done (PASS)".into() },
        );
        let fin = s.finalized.as_ref().unwrap();
        assert_eq!(fin.kind, WorkflowFinalizeKind::Done);
        assert_eq!(fin.summary, "done (PASS)");
        assert!(s.stop.is_none(), "tail stop signal consumed on finalize");
    }
}
