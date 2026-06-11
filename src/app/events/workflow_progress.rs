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

use std::path::PathBuf;

use crate::agent::events::ClientEvent;
use crate::agent::workflow_tail::{
    self, ActionStatus, FinalizeKind, RunTarget, WorkflowProgress,
};
use crate::app::{
    App, MessageBlock, ToolCallInfo, WorkflowActionCompletion, WorkflowActionRow,
    WorkflowActionStatus, WorkflowFinalizeKind, WorkflowFinalizeRow, WorkflowProgressState,
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
        let Some(MessageBlock::ToolCall(tc)) =
            app.messages.get(mi).and_then(|m| m.blocks.get(bi))
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

    // Resolve the events file. With an explicit --run-id we know it exactly;
    // otherwise watch the cwd for the newest dir created after launch.
    let events_path = resolve_events_path(&target);

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

    let event_tx = app.event_tx.clone();
    let id = tool_call_id.to_owned();
    tracing::info!(
        target: crate::logging::targets::APP_COMMAND,
        event_name = "workflow_tail_started",
        message = "started tailing workflow events.jsonl",
        outcome = "start",
        tool_call_id = %id,
        run_id = target.run_id.as_deref().unwrap_or("<newest-dir>"),
        events_path = %events_path.display(),
    );

    // The emit closure captures a non-Send mpsc sender (ClientEvent is !Send),
    // so this must run on the LocalSet — same pattern as update_check.
    tokio::task::spawn_local(async move {
        workflow_tail::tail_events(events_path, stop_rx, move |update| {
            let _ = event_tx.send(ClientEvent::WorkflowProgress {
                tool_call_id: id.clone(),
                update,
            });
        })
        .await;
    });
}

/// Resolve the events.jsonl path for a [`RunTarget`]. When the run id is
/// explicit the path is known; otherwise we fall back to the cwd-level
/// `events.jsonl` (the engine writes one per run dir, and the newest-dir
/// resolution is a best-effort future enhancement — until then an explicit
/// `--run-id`, which op-gen always passes, gives the precise path).
fn resolve_events_path(target: &RunTarget) -> PathBuf {
    target.events_path()
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

/// Mutate `state` for one update. Returns whether anything visible changed.
fn apply_to_state(state: &mut WorkflowProgressState, update: WorkflowProgress) -> bool {
    match update {
        WorkflowProgress::Started { workflow } => {
            let new = (!workflow.is_empty()).then_some(workflow);
            if state.workflow == new {
                return false;
            }
            state.workflow = new;
            true
        }
        WorkflowProgress::ActionStarted { action_id, kind, name } => {
            if state.actions.iter().any(|a| a.action_id == action_id) {
                return false; // dedup re-delivery
            }
            state.actions.push(WorkflowActionRow {
                action_id,
                kind,
                name,
                completed: None,
            });
            true
        }
        WorkflowProgress::ActionCompleted { action_id, status, outcome } => {
            let completion = WorkflowActionCompletion {
                status: map_status(status),
                outcome,
            };
            if let Some(row) = state.actions.iter_mut().find(|a| a.action_id == action_id) {
                row.completed = Some(completion);
            } else {
                // Completion without a prior start (e.g. tail attached late):
                // synthesize a row so the outcome is still visible.
                state.actions.push(WorkflowActionRow {
                    action_id,
                    kind: String::new(),
                    name: String::new(),
                    completed: Some(completion),
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

    #[test]
    fn started_sets_workflow_once() {
        let mut s = empty_state();
        assert!(apply_to_state(&mut s, WorkflowProgress::Started { workflow: "gen".into() }));
        assert_eq!(s.workflow.as_deref(), Some("gen"));
        // Re-delivery of identical start is a no-op.
        assert!(!apply_to_state(&mut s, WorkflowProgress::Started { workflow: "gen".into() }));
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
            },
        );
        assert_eq!(s.actions.len(), 1);
        assert!(s.actions[0].completed.is_none());

        apply_to_state(
            &mut s,
            WorkflowProgress::ActionCompleted {
                action_id: "a1".into(),
                status: ActionStatus::Ok,
                outcome: "clean".into(),
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
            },
        );
        assert_eq!(s.actions.len(), 1);
        assert_eq!(s.actions[0].action_id, "late");
        assert_eq!(
            s.actions[0].completed.as_ref().unwrap().status,
            WorkflowActionStatus::Fail
        );
    }

    #[test]
    fn finalize_sets_summary_and_stops_tail() {
        let mut s = empty_state();
        assert!(s.stop.is_some());
        apply_to_state(
            &mut s,
            WorkflowProgress::Finalized {
                kind: FinalizeKind::Done,
                summary: "done (PASS)".into(),
            },
        );
        let fin = s.finalized.as_ref().unwrap();
        assert_eq!(fin.kind, WorkflowFinalizeKind::Done);
        assert_eq!(fin.summary, "done (PASS)");
        assert!(s.stop.is_none(), "tail stop signal consumed on finalize");
    }
}
