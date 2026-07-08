// Copyright 2025 Simon Peter Rothgang
// SPDX-License-Identifier: Apache-2.0

//! Live progress for `lingxi-ascendc run` workflow invocations launched via the
//! Bash tool.
//!
//! The Bash tool only surfaces a subprocess's stdout/stderr when the process
//! exits, so a long workflow run shows nothing but a spinner until it finishes.
//! The Python engine, however, streams structured progress to a JSONL event
//! file (`<bash_cwd>/<run_id>/events.jsonl`). This module:
//!
//! 1. parses a Bash command string into a [`RunTarget`] (the events file path),
//! 2. tails that file line-by-line as the engine appends events,
//! 3. maps each engine event to a [`WorkflowProgress`] update the UI renders as
//!    child rows under the originating Bash tool call.
//!
//! The tail task is pure I/O plus parsing — it owns no UI state. The caller
//! wires the emitted [`WorkflowProgress`] stream to a specific tool call.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};

/// How often the tail loop re-checks for the events file before it exists, and
/// re-polls for appended bytes once EOF is hit on an existing file.
const POLL_INTERVAL: Duration = Duration::from_millis(150);

/// Where a `lingxi-ascendc run` invocation will write its `events.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunTarget {
    /// The run id parsed from `--run-id`, or the synthesized fallback marker
    /// when absent (`None` here means "watch cwd for a newly created dir";
    /// `Some` means we resolved an explicit run dir).
    pub run_id: Option<String>,
    /// Directory that will contain `events.jsonl`. For an explicit `--run-id`
    /// this is `<cwd>/<run_id>`; otherwise it is `<cwd>` and the caller must
    /// resolve the newest child directory before tailing.
    pub run_dir: PathBuf,
}

impl RunTarget {
    /// Path to the events file, valid only when [`Self::run_id`] is `Some`.
    /// When the run id is absent the run dir is not yet known (a timestamped
    /// dir is created by the engine at startup) and the caller resolves it.
    #[must_use]
    pub fn events_path(&self) -> PathBuf {
        self.run_dir.join("events.jsonl")
    }

    /// Sibling subagent stream file (Phase 2). Same dir as `events.jsonl`.
    #[must_use]
    pub fn agent_stream_path(&self) -> PathBuf {
        self.run_dir.join("agent_stream.jsonl")
    }
}

/// Detect a `lingxi-ascendc run` or scenario invocation in a Bash command string
/// and resolve where its event file will live, relative to the Bash tool's
/// working dir.
///
/// Returns `None` for any command that is not a fresh Lingxi workflow launch
/// (including `resume`/`status` subcommands and unrelated commands), so the
/// caller can cheaply gate every Bash launch.
#[must_use]
pub fn extract_run_target(command: &str, cwd: &Path) -> Option<RunTarget> {
    let tokens = shell_split(command);
    match parse_run_invocation(&tokens)? {
        RunDetect::WithId(id) => {
            let run_dir = cwd.join(&id);
            Some(RunTarget { run_id: Some(id), run_dir })
        }
        RunDetect::WithoutId => Some(RunTarget { run_id: None, run_dir: cwd.to_path_buf() }),
    }
}

/// Outcome of scanning a command for a fresh `lingxi-ascendc run` invocation.
enum RunDetect {
    /// Explicit `--run-id <id>`.
    WithId(String),
    /// A `run` with no run id (caller falls back to newest-dir watching).
    WithoutId,
}

/// Inspect already-split tokens for a `lingxi-ascendc run` invocation or one of
/// the user-facing scenario commands. Returns `None` for anything that is not a
/// fresh workflow launch.
fn parse_run_invocation(tokens: &[String]) -> Option<RunDetect> {
    // Find the binary token (allow an absolute/relative path ending in the
    // binary name, e.g. `./.local/bin/lingxi-ascendc`).
    let bin_idx = tokens.iter().position(|t| is_lingxi_binary(t))?;
    let rest = &tokens[bin_idx + 1..];

    // The first non-flag token after the binary must be the `run` subcommand
    // or one of the scenario entrypoints that delegates to `run`.
    let subcommand = rest.iter().find(|t| !t.starts_with('-'))?;
    let is_run = subcommand == "run";
    let is_scenario = matches!(subcommand.as_str(), "generate" | "debug" | "optimize" | "research");
    if !is_run && !is_scenario {
        return None;
    }

    // Reject run resume/status flags that imply an existing run dir we
    // shouldn't relabel.
    if is_run && (flag_present(rest, "--resume") || flag_present(rest, "--status")) {
        return None;
    }

    match flag_value(rest, "--run-id") {
        Some(id) => Some(RunDetect::WithId(id)),
        // The explicit `run` subcommand can synthesize a timestamped run id,
        // but the TUI cannot know that directory before the engine prints it.
        // Keep the legacy WithoutId marker so callers can skip live progress.
        None if is_run => Some(RunDetect::WithoutId),
        // Scenario commands only become live-tailable when the demo passes an
        // explicit --run-id; otherwise there is no deterministic target path.
        None => None,
    }
}

fn is_lingxi_binary(token: &str) -> bool {
    let name = token.rsplit(['/', '\\']).next().unwrap_or(token);
    name == "lingxi-ascendc"
}

fn flag_present(tokens: &[String], flag: &str) -> bool {
    tokens.iter().any(|t| t == flag || t.starts_with(&format!("{flag}=")))
}

/// Read the value of `--flag value` or `--flag=value`.
fn flag_value(tokens: &[String], flag: &str) -> Option<String> {
    let eq_prefix = format!("{flag}=");
    let mut iter = tokens.iter();
    while let Some(tok) = iter.next() {
        if let Some(value) = tok.strip_prefix(&eq_prefix) {
            return Some(unquote(value));
        }
        if tok == flag {
            return iter.next().map(|v| unquote(v));
        }
    }
    None
}

fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        value[1..value.len() - 1].to_owned()
    } else {
        value.to_owned()
    }
}

/// Minimal shell tokenizer: splits on unquoted whitespace, honoring single and
/// double quotes and backslash escapes. Good enough to recover the binary,
/// subcommand, and flag values from the kinds of commands the agent emits; it
/// is not a full POSIX parser (no expansion, no operators).
fn shell_split(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut chars = command.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut started = false;

    while let Some(c) = chars.next() {
        match c {
            '\\' if !in_single => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                    started = true;
                }
            }
            '\'' if !in_double => {
                in_single = !in_single;
                started = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                started = true;
            }
            ws if ws.is_whitespace() && !in_single && !in_double => {
                if started {
                    tokens.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            other => {
                cur.push(other);
                started = true;
            }
        }
    }
    if started {
        tokens.push(cur);
    }
    tokens
}

/// A single live-progress update derived from one engine event. The caller maps
/// these onto child rows attached to the originating Bash tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowProgress {
    /// `run_started`: workflow name, the ordered header `params` (key/value
    /// pairs the engine emits for the header line), and the `verbose` flag
    /// (gates nested subagent tool rows; default false).
    Started { workflow: String, params: Vec<(String, String)>, verbose: bool },
    /// `action_started`: an action (agent spawn / nested workflow / verify)
    /// began. `action_id` keys later updates; `kind`/`name` label the row.
    /// `detail` is a compact action-level metadata summary from events.jsonl.
    ActionStarted { action_id: String, kind: String, name: String, detail: Option<String> },
    /// `action_completed`: an action finished. `outcome` is a short
    /// human-facing status derived from the result (marker outcome, error
    /// category, or success/fail), and `status` classifies it for coloring.
    ActionCompleted {
        action_id: String,
        status: ActionStatus,
        outcome: String,
        detail: Option<String>,
    },
    /// A terminal workflow decision. `summary` collapses the run to one line;
    /// `kind` distinguishes done/aborted/escalated for coloring.
    Finalized { kind: FinalizeKind, summary: String },
}

/// Coarse status of a completed action, used purely for UI coloring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionStatus {
    Ok,
    Retry,
    Fail,
}

/// Terminal workflow outcome class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalizeKind {
    Done,
    Aborted,
    Escalated,
}

/// A single subagent tool-call event from a run's sibling `agent_stream.jsonl`.
/// Nested under the parent action row keyed by `action_id`.
///
/// The engine writes this file via its `AgentStreamWriter` (one line per
/// subagent `tool_use` / `tool_result`); the tail poll-waits for it and is a
/// silent no-op only while the file is absent (e.g. an older engine). Schema:
/// `{ts, action_id, agent_name, tool_call_id, kind:"tool_use"|"tool_result",
/// name, detail?, status?}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentToolEvent {
    /// Parent action this subagent belongs to (matches an `action_started`
    /// `action_id` from events.jsonl).
    pub action_id: String,
    /// Subagent name (e.g. `aog-kernel-worker`).
    pub agent_name: String,
    /// Tool call id within the subagent; pairs a `tool_use` with its result.
    pub tool_call_id: String,
    /// `tool_use` (call started) or `tool_result` (call finished).
    pub phase: SubagentPhase,
    /// Tool name (e.g. `Bash`, `Edit`).
    pub name: String,
    /// Short a5_ops-style label derived by the engine from the tool input
    /// (Bash description, file basename, grep pattern, skill/subagent name,
    /// url). Carried on `tool_use`; empty/absent renders just the name.
    pub detail: Option<String>,
    /// Optional status string carried on a `tool_result`.
    pub status: Option<String>,
}

/// Whether a subagent event is a tool invocation or its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentPhase {
    ToolUse,
    ToolResult,
}

/// Raw engine event as it appears on a line of `events.jsonl`. Only the fields
/// the UI consumes are modeled; unknown fields are ignored so engine-side
/// schema additions never break the tail.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum RawEvent {
    #[serde(rename = "run_started")]
    RunStarted {
        #[serde(default)]
        workflow: String,
        /// Ordered header params. Insertion order is preserved by
        /// [`OrderedParams`]'s visitor (the default `serde_json::Map` is a
        /// `BTreeMap` and would re-sort keys alphabetically).
        #[serde(default)]
        params: OrderedParams,
        #[serde(default)]
        verbose: bool,
    },
    #[serde(rename = "action_started")]
    ActionStarted {
        #[serde(default)]
        action_id: String,
        #[serde(default)]
        action: RawAction,
    },
    #[serde(rename = "action_completed")]
    ActionCompleted {
        #[serde(default)]
        action_id: String,
        #[serde(default)]
        action: RawAction,
        #[serde(default)]
        result: serde_json::Value,
    },
    #[serde(rename = "workflow_done")]
    WorkflowDone {
        #[serde(default)]
        reason: String,
        #[serde(default)]
        outcome: Option<serde_json::Value>,
    },
    #[serde(rename = "workflow_aborted")]
    WorkflowAborted {
        #[serde(default)]
        category: String,
        #[serde(default)]
        reason: String,
        #[serde(default)]
        user_remediation: Option<String>,
    },
    #[serde(rename = "workflow_escalated")]
    WorkflowEscalated {
        #[serde(default)]
        reason: String,
        #[serde(default)]
        outcome: Option<serde_json::Value>,
    },
    /// Any other event type (`internal_event`, `workflow_route_intake`, ...) is
    /// parsed but produces no UI update.
    #[serde(other)]
    Other,
}

#[derive(Debug, Default, Deserialize)]
struct RawAction {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    skill_args: serde_json::Value,
    #[serde(default)]
    task: Option<RawTask>,
    #[serde(default)]
    expected_artifacts: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawTask {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    task_scope: String,
    #[serde(default)]
    max_retry_rounds: Option<u64>,
}

/// Ordered key/value header params parsed from `run_started.params`. The JSON
/// object's entries are visited in document order and pushed into a `Vec`, so
/// the engine-emitted order is preserved (unlike `serde_json::Map`, a sorted
/// `BTreeMap` by default). Scalar values (string/number/bool) are stringified;
/// the contract is short strings, but coercing keeps the tail forgiving.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct OrderedParams(Vec<(String, String)>);

impl<'de> serde::Deserialize<'de> for OrderedParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ParamsVisitor;

        impl<'de> serde::de::Visitor<'de> for ParamsVisitor {
            type Value = OrderedParams;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an object of header params")
            }

            fn visit_map<M>(self, mut map: M) -> Result<OrderedParams, M::Error>
            where
                M: serde::de::MapAccess<'de>,
            {
                let mut out = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, serde_json::Value>()? {
                    if let Some(s) = scalar_to_string(&v) {
                        out.push((k, s));
                    }
                }
                Ok(OrderedParams(out))
            }
        }

        deserializer.deserialize_map(ParamsVisitor)
    }
}

/// Stringify a scalar JSON value for a header param; non-scalars (array/object/
/// null) are dropped (return `None`) so only short displayable values land in
/// the header.
fn scalar_to_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

const MAX_ACTION_DETAIL_CHARS: usize = 110;

fn action_detail(action: &RawAction) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();

    if let Some(task) = action.task.as_ref() {
        if !task.task_scope.trim().is_empty() {
            parts.push(format!("scope={}", task.task_scope.trim()));
        }
        if let Some(rounds) = task.max_retry_rounds {
            parts.push(format!("rounds={rounds}"));
        }
        if !task.kind.trim().is_empty() {
            parts.push(format!("task={}", task.kind.trim()));
        }
    }

    if let Some(args) = compact_object_fields(&action.skill_args, 4) {
        parts.push(format!("args:{args}"));
    }

    if !action.expected_artifacts.is_empty() {
        if action.expected_artifacts.len() <= 2 {
            parts.push(format!("expected={}", action.expected_artifacts.join(",")));
        } else {
            parts.push(format!("expected={} artifacts", action.expected_artifacts.len()));
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(truncate_to(&parts.join(" "), MAX_ACTION_DETAIL_CHARS))
    }
}

fn compact_object_fields(value: &serde_json::Value, max_fields: usize) -> Option<String> {
    let obj = value.as_object()?;
    if obj.is_empty() {
        return None;
    }
    let mut fields = Vec::new();
    for (idx, (k, v)) in obj.iter().enumerate() {
        if idx >= max_fields {
            fields.push(format!("+{}", obj.len() - max_fields));
            break;
        }
        if let Some(s) = scalar_to_string(v) {
            fields.push(format!("{k}={s}"));
        }
    }
    if fields.is_empty() { None } else { Some(fields.join(" ")) }
}

/// Raw subagent tool event line from `agent_stream.jsonl` (Phase 2).
#[derive(Debug, Deserialize)]
struct RawSubagentEvent {
    #[serde(default)]
    action_id: String,
    #[serde(default)]
    agent_name: String,
    #[serde(default)]
    tool_call_id: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

/// Map one parsed JSON line to a [`WorkflowProgress`], or `None` for events the
/// UI does not surface (or unparseable lines).
#[must_use]
pub fn map_event_line(line: &str) -> Option<WorkflowProgress> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let event: RawEvent = serde_json::from_str(trimmed).ok()?;
    map_event(event)
}

/// Map one `agent_stream.jsonl` line to a [`SubagentToolEvent`], or `None` for
/// blank/malformed lines or unknown `kind` values. Requires a non-empty
/// `action_id` so the event can be attached to a parent action row.
#[must_use]
pub fn map_agent_stream_line(line: &str) -> Option<SubagentToolEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let raw: RawSubagentEvent = serde_json::from_str(trimmed).ok()?;
    if raw.action_id.is_empty() {
        return None;
    }
    let phase = match raw.kind.as_str() {
        "tool_use" => SubagentPhase::ToolUse,
        "tool_result" => SubagentPhase::ToolResult,
        _ => return None,
    };
    Some(SubagentToolEvent {
        action_id: raw.action_id,
        agent_name: raw.agent_name,
        tool_call_id: raw.tool_call_id,
        phase,
        name: raw.name,
        detail: raw.detail.filter(|s| !s.is_empty()),
        status: raw.status.filter(|s| !s.is_empty()),
    })
}

fn map_event(event: RawEvent) -> Option<WorkflowProgress> {
    match event {
        RawEvent::RunStarted { workflow, params, verbose } => {
            Some(WorkflowProgress::Started { workflow, params: params.0, verbose })
        }
        RawEvent::ActionStarted { action_id, action } => {
            let detail = action_detail(&action);
            Some(WorkflowProgress::ActionStarted {
                action_id,
                kind: action.kind,
                name: action.name,
                detail,
            })
        }
        RawEvent::ActionCompleted { action_id, action, result } => {
            let (status, outcome) = classify_result(&action.kind, &result);
            let detail = action_detail(&action);
            Some(WorkflowProgress::ActionCompleted { action_id, status, outcome, detail })
        }
        RawEvent::WorkflowDone { reason, outcome } => Some(WorkflowProgress::Finalized {
            kind: FinalizeKind::Done,
            summary: finalize_summary("done", &reason, outcome.as_ref()),
        }),
        RawEvent::WorkflowAborted { category, reason, user_remediation } => {
            let mut summary = format!("aborted [{category}]");
            let detail =
                first_nonempty([reason.as_str(), user_remediation.as_deref().unwrap_or("")]);
            if let Some(detail) = detail {
                summary.push_str(": ");
                summary.push_str(detail);
            }
            Some(WorkflowProgress::Finalized { kind: FinalizeKind::Aborted, summary })
        }
        RawEvent::WorkflowEscalated { reason, outcome } => Some(WorkflowProgress::Finalized {
            kind: FinalizeKind::Escalated,
            summary: finalize_summary("escalated", &reason, outcome.as_ref()),
        }),
        RawEvent::Other => None,
    }
}

/// Derive an [`ActionStatus`] + short outcome string from an action result.
///
/// Recognized result shapes (checked in order):
/// - `result.error` / `result.error_category` (string) → Fail.
/// - `result.missing_artifacts` (non-empty array) → Fail.
/// - `result.marker.outcome` (string) → status inferred from the outcome word.
/// - `result.success` (bool) → Ok/Fail.
fn classify_result(_kind: &str, result: &serde_json::Value) -> (ActionStatus, String) {
    if let Some(err) = result.get("error").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        return (ActionStatus::Fail, truncate(err));
    }
    if let Some(cat) =
        result.get("error_category").and_then(|v| v.as_str()).filter(|s| !s.is_empty())
    {
        return (ActionStatus::Fail, truncate(cat));
    }
    if let Some(missing) = result.get("missing_artifacts").and_then(|v| v.as_array())
        && !missing.is_empty()
    {
        let names: Vec<String> =
            missing.iter().filter_map(|v| v.as_str().map(ToOwned::to_owned)).collect();
        return (ActionStatus::Fail, format!("missing: {}", names.join(", ")));
    }
    // Prefer the structured `marker.outcome` word to classify the *status* — a
    // free-text `summary` like "no failures detected" must not be miscolored
    // Fail by the substring scan. The summary is still preferred as the display
    // *text* when present; fall back to scanning it only when no marker exists.
    let marker_outcome = result
        .get("marker")
        .and_then(|m| m.get("outcome"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let summary = result.get("summary").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
    if let Some(text) = summary.or(marker_outcome) {
        let status = status_for_outcome_word(marker_outcome.unwrap_or(text));
        return (status, truncate(text));
    }
    match result.get("success").and_then(serde_json::Value::as_bool) {
        Some(true) => (ActionStatus::Ok, "ok".to_owned()),
        Some(false) => (ActionStatus::Fail, "failed".to_owned()),
        None => (ActionStatus::Ok, "done".to_owned()),
    }
}

/// Classify a marker outcome word. Outcomes containing retry/abort/fail signals
/// map to Retry/Fail; everything else (clean, success, directive, ...) is Ok.
fn status_for_outcome_word(outcome: &str) -> ActionStatus {
    let lower = outcome.to_ascii_lowercase();
    if lower.contains("abort") || lower.contains("fail") || lower.contains("error") {
        ActionStatus::Fail
    } else if lower.contains("retry") || lower.contains("escalat") {
        ActionStatus::Retry
    } else {
        ActionStatus::Ok
    }
}

fn finalize_summary(verb: &str, reason: &str, outcome: Option<&serde_json::Value>) -> String {
    let mut summary = verb.to_owned();
    if let Some(outcome) = outcome.and_then(outcome_label).filter(|s| !s.is_empty()) {
        summary.push_str(" (");
        summary.push_str(&outcome);
        summary.push(')');
    }
    if !reason.trim().is_empty() {
        summary.push_str(": ");
        summary.push_str(&truncate(reason));
    }
    summary
}

/// Best-effort one-word label for an `outcome` field (string or object with an
/// `outcome`/`marker` field).
fn outcome_label(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(_) => value
            .get("outcome")
            .or_else(|| value.get("marker"))
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned),
        _ => None,
    }
}

fn first_nonempty<'a, I: IntoIterator<Item = &'a str>>(items: I) -> Option<&'a str> {
    items.into_iter().map(str::trim).find(|s| !s.is_empty())
}

const MAX_OUTCOME_CHARS: usize = 80;

fn truncate(s: &str) -> String {
    truncate_to(s, MAX_OUTCOME_CHARS)
}

fn truncate_to(s: &str, max_chars: usize) -> String {
    let s = s.trim();
    if s.chars().count() > max_chars {
        let head: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{head}\u{2026}")
    } else {
        s.to_owned()
    }
}

/// Tail `events_path` and forward every mapped [`WorkflowProgress`] update to
/// `emit`, until the channel closes, the [`tokio::sync::watch`] stop signal
/// fires, or a terminal workflow event is observed.
///
/// The file may not exist yet (the engine creates it shortly after launch); the
/// loop poll-waits for it. Once open it follows appended lines, re-polling at
/// EOF. Partial trailing lines (no newline yet) are buffered until complete.
///
/// `emit` is an `FnMut` so callers can route updates into a non-`Send` channel
/// (this task is intended to run on a `LocalSet` via `spawn_local`).
pub async fn tail_events<F>(
    events_path: PathBuf,
    stop: tokio::sync::watch::Receiver<bool>,
    mut emit: F,
) where
    F: FnMut(WorkflowProgress),
{
    follow_file(events_path, stop, |line| match map_event_line(line) {
        Some(update) => {
            let terminal = matches!(update, WorkflowProgress::Finalized { .. });
            emit(update);
            if terminal { LineAction::Stop } else { LineAction::Continue }
        }
        None => LineAction::Continue,
    })
    .await;
}

/// Tail a run's sibling `agent_stream.jsonl` and forward every
/// [`SubagentToolEvent`] to `emit`. The engine writes this file via its
/// `AgentStreamWriter`; the tail poll-waits for it (a silent no-op while it is
/// absent, e.g. an older engine), then follows appended lines like
/// [`tail_events`]. There is no terminal event for the subagent stream; it runs
/// until the stop signal fires.
pub async fn tail_agent_stream<F>(
    stream_path: PathBuf,
    stop: tokio::sync::watch::Receiver<bool>,
    mut emit: F,
) where
    F: FnMut(SubagentToolEvent),
{
    follow_file(stream_path, stop, |line| {
        if let Some(event) = map_agent_stream_line(line) {
            emit(event);
        }
        LineAction::Continue
    })
    .await;
}

/// Whether the follow loop should keep reading or stop after a line.
enum LineAction {
    Continue,
    Stop,
}

/// Poll-wait for `path`, then follow appended whole lines, invoking `on_line`
/// for each. Buffers partial trailing lines. Returns when `on_line` yields
/// [`LineAction::Stop`], the stop signal fires, or a read error occurs.
async fn follow_file<F>(path: PathBuf, mut stop: tokio::sync::watch::Receiver<bool>, mut on_line: F)
where
    F: FnMut(&str) -> LineAction,
{
    // Phase A: wait for the file to appear.
    let file = loop {
        if *stop.borrow() {
            return;
        }
        match tokio::fs::File::open(&path).await {
            Ok(f) => break f,
            Err(_) => {
                if wait_or_stop(&mut stop).await {
                    return;
                }
            }
        }
    };

    // Phase B: follow appended lines.
    let mut reader = BufReader::new(file);
    let mut pending = String::new();
    loop {
        let mut chunk = String::new();
        match reader.read_line(&mut chunk).await {
            Ok(0) => {
                // EOF: no more bytes yet. Wait, then retry from the same
                // position (BufReader preserves its offset across read_line).
                if *stop.borrow() || wait_or_stop(&mut stop).await {
                    return;
                }
            }
            Ok(_) => {
                pending.push_str(&chunk);
                if !pending.ends_with('\n') {
                    // Partial line (writer mid-append); keep buffering.
                    continue;
                }
                let line = std::mem::take(&mut pending);
                if matches!(on_line(&line), LineAction::Stop) {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

/// Sleep one poll interval or return `true` early if the stop signal fired.
async fn wait_or_stop(stop: &mut tokio::sync::watch::Receiver<bool>) -> bool {
    tokio::select! {
        () = tokio::time::sleep(POLL_INTERVAL) => *stop.borrow(),
        changed = stop.changed() => changed.is_err() || *stop.borrow(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cwd() -> PathBuf {
        PathBuf::from("/runs")
    }

    #[test]
    fn extract_run_target_explicit_run_id() {
        let t = extract_run_target("lingxi-ascendc run --workflow gen --run-id op42", &cwd())
            .expect("should detect run");
        assert_eq!(t.run_id.as_deref(), Some("op42"));
        assert_eq!(t.events_path(), PathBuf::from("/runs/op42/events.jsonl"));
        assert_eq!(t.agent_stream_path(), PathBuf::from("/runs/op42/agent_stream.jsonl"));
    }

    #[test]
    fn extract_run_target_run_id_equals_form() {
        let t = extract_run_target("lingxi-ascendc run --run-id=op7 --workflow gen", &cwd())
            .expect("should detect run");
        assert_eq!(t.run_id.as_deref(), Some("op7"));
        assert_eq!(t.run_dir, PathBuf::from("/runs/op7"));
    }

    #[test]
    fn extract_run_target_absolute_binary_path() {
        let t =
            extract_run_target("./.local/bin/lingxi-ascendc run --workflow w --run-id r1", &cwd())
                .expect("should detect run via path-qualified binary");
        assert_eq!(t.run_id.as_deref(), Some("r1"));
    }

    #[test]
    fn extract_run_target_scenario_with_run_id() {
        let t = extract_run_target(
            "lingxi-ascendc generate --via cannbot --run-id demo1 --bench NPUKernelBench --select L1/1",
            &cwd(),
        )
        .expect("scenario with explicit run id should resolve");
        assert_eq!(t.run_id.as_deref(), Some("demo1"));
        assert_eq!(t.events_path(), PathBuf::from("/runs/demo1/events.jsonl"));
    }

    #[test]
    fn extract_run_target_scenario_without_run_id_is_skipped() {
        assert!(
            extract_run_target(
                "lingxi-ascendc generate --via cannbot --bench NPUKernelBench --select L1/1",
                &cwd(),
            )
            .is_none()
        );
    }

    #[test]
    fn extract_run_target_with_leading_env_and_pipe_still_finds_binary() {
        let t = extract_run_target(
            "PATH=/x:$PATH lingxi-ascendc run --run-id op9 --workflow gen",
            &cwd(),
        )
        .expect("env-prefixed command should still resolve");
        assert_eq!(t.run_id.as_deref(), Some("op9"));
    }

    #[test]
    fn extract_run_target_no_run_id_falls_back_to_cwd_watch() {
        let t = extract_run_target("lingxi-ascendc run --workflow gen", &cwd())
            .expect("run without --run-id is still a run");
        assert_eq!(t.run_id, None);
        assert_eq!(t.run_dir, cwd());
    }

    #[test]
    fn extract_run_target_quoted_run_id() {
        let t = extract_run_target("lingxi-ascendc run --run-id \"my run\" --workflow w", &cwd())
            .expect("quoted run id");
        assert_eq!(t.run_id.as_deref(), Some("my run"));
    }

    #[test]
    fn extract_run_target_rejects_non_run_subcommands() {
        assert!(extract_run_target("lingxi-ascendc verify-env --target ascendc", &cwd()).is_none());
        assert!(
            extract_run_target("lingxi-ascendc batch --workflow w --select x", &cwd()).is_none()
        );
        assert!(extract_run_target("lingxi-ascendc --version", &cwd()).is_none());
    }

    #[test]
    fn extract_run_target_rejects_resume_and_status() {
        assert!(
            extract_run_target("lingxi-ascendc run --resume op1 --workflow w", &cwd()).is_none()
        );
        assert!(extract_run_target("lingxi-ascendc run --status --workflow w", &cwd()).is_none());
    }

    #[test]
    fn extract_run_target_rejects_unrelated_commands() {
        assert!(extract_run_target("echo hello && ls -la", &cwd()).is_none());
        assert!(extract_run_target("python run.py --run-id x", &cwd()).is_none());
    }

    #[test]
    fn map_run_started() {
        let u = map_event_line(r#"{"type":"run_started","workflow":"gen","cwd":"/x","seq":1}"#)
            .expect("run_started maps");
        // No params/verbose present -> defaults (empty params, verbose false).
        assert_eq!(
            u,
            WorkflowProgress::Started {
                workflow: "gen".to_owned(),
                params: Vec::new(),
                verbose: false,
            }
        );
    }

    #[test]
    fn map_run_started_parses_params_in_order_and_verbose() {
        // Keys intentionally NOT alphabetical: op, via, run_mode, model. A
        // BTreeMap would resort them; the visitor must preserve emit order.
        let line = r#"{"type":"run_started","workflow":"generate_ascendc",
            "params":{"op":"3_Add","via":"cannbot","run_mode":"user","model":"model.py"},
            "verbose":true}"#;
        match map_event_line(line).expect("run_started maps") {
            WorkflowProgress::Started { workflow, params, verbose } => {
                assert_eq!(workflow, "generate_ascendc");
                assert!(verbose);
                assert_eq!(
                    params,
                    vec![
                        ("op".to_owned(), "3_Add".to_owned()),
                        ("via".to_owned(), "cannbot".to_owned()),
                        ("run_mode".to_owned(), "user".to_owned()),
                        ("model".to_owned(), "model.py".to_owned()),
                    ]
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn map_run_started_coerces_scalar_param_values_and_drops_nonscalars() {
        let line = r#"{"type":"run_started","workflow":"gen",
            "params":{"op":"5_Mul","retries":3,"deep":false,"junk":[1,2],"nil":null}}"#;
        match map_event_line(line).expect("maps") {
            WorkflowProgress::Started { params, verbose, .. } => {
                assert!(!verbose, "verbose defaults to false when absent");
                // Scalars stringified, in order; array/null dropped.
                assert_eq!(
                    params,
                    vec![
                        ("op".to_owned(), "5_Mul".to_owned()),
                        ("retries".to_owned(), "3".to_owned()),
                        ("deep".to_owned(), "false".to_owned()),
                    ]
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn map_action_started_reads_nested_action() {
        let line = r#"{"type":"action_started","action_id":"act_1",
            "action":{"kind":"spawn_agent","name":"aog-kernel-worker","skill_args":{}}}"#;
        let u = map_event_line(line).expect("action_started maps");
        assert_eq!(
            u,
            WorkflowProgress::ActionStarted {
                action_id: "act_1".to_owned(),
                kind: "spawn_agent".to_owned(),
                name: "aog-kernel-worker".to_owned(),
                detail: None,
            }
        );
    }

    #[test]
    fn map_action_started_extracts_action_detail() {
        let line = r#"{"type":"action_started","action_id":"act_1",
            "action":{
                "kind":"spawn_agent",
                "name":"cannbot-producer",
                "task":{"kind":"generate_ascendc_cannbot_split","task_scope":"full","max_retry_rounds":20},
                "expected_artifacts":["full_task/model_new_ascendc.py","verification.json","trace/manifest.json"]
            }}"#;
        let u = map_event_line(line).expect("action_started maps");
        match u {
            WorkflowProgress::ActionStarted { detail, .. } => {
                let detail = detail.expect("detail");
                assert!(detail.contains("scope=full"), "detail={detail}");
                assert!(detail.contains("rounds=20"), "detail={detail}");
                assert!(detail.contains("expected=3 artifacts"), "detail={detail}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn map_action_completed_marker_outcome_ok() {
        let line = r#"{"type":"action_completed","action_id":"act_1",
            "action":{"kind":"spawn_agent","name":"w"},
            "result":{"marker":{"outcome":"clean"}}}"#;
        let u = map_event_line(line).expect("completed maps");
        match u {
            WorkflowProgress::ActionCompleted { action_id, status, outcome, .. } => {
                assert_eq!(action_id, "act_1");
                assert_eq!(status, ActionStatus::Ok);
                assert_eq!(outcome, "clean");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn map_action_completed_error_category_is_fail() {
        let line = r#"{"type":"action_completed","action_id":"a2",
            "action":{"kind":"verify","name":"verify"},
            "result":{"error_category":"compile_error"}}"#;
        match map_event_line(line).expect("maps") {
            WorkflowProgress::ActionCompleted { status, outcome, .. } => {
                assert_eq!(status, ActionStatus::Fail);
                assert_eq!(outcome, "compile_error");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn map_action_completed_missing_artifacts_is_fail() {
        let line = r#"{"type":"action_completed","action_id":"a3",
            "action":{"kind":"run_workflow","name":"sub"},
            "result":{"missing_artifacts":["verification.json"]}}"#;
        match map_event_line(line).expect("maps") {
            WorkflowProgress::ActionCompleted { status, outcome, .. } => {
                assert_eq!(status, ActionStatus::Fail);
                assert!(outcome.contains("verification.json"), "outcome={outcome}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn map_action_completed_success_bool() {
        let line = r#"{"type":"action_completed","action_id":"a4",
            "action":{"kind":"spawn_agent","name":"w"},"result":{"success":true}}"#;
        match map_event_line(line).expect("maps") {
            WorkflowProgress::ActionCompleted { status, .. } => {
                assert_eq!(status, ActionStatus::Ok);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn map_action_completed_retry_outcome() {
        let line = r#"{"type":"action_completed","action_id":"a5",
            "action":{"kind":"spawn_agent","name":"w"},
            "result":{"marker":{"outcome":"needs_retry"}}}"#;
        match map_event_line(line).expect("maps") {
            WorkflowProgress::ActionCompleted { status, .. } => {
                assert_eq!(status, ActionStatus::Retry);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn map_action_completed_benign_summary_not_miscolored_fail() {
        // A benign summary containing the substring "fail" must be colored by the
        // structured marker outcome (Ok), not scanned as Fail; the summary text
        // is still what gets displayed.
        let line = r#"{"type":"action_completed","action_id":"a6",
            "action":{"kind":"verify","name":"verify"},
            "result":{"summary":"no failures detected","marker":{"outcome":"clean"}}}"#;
        match map_event_line(line).expect("maps") {
            WorkflowProgress::ActionCompleted { status, outcome, .. } => {
                assert_eq!(status, ActionStatus::Ok, "benign summary must not be Fail");
                assert_eq!(outcome, "no failures detected", "summary is the display text");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn map_workflow_done_with_outcome() {
        let line = r#"{"type":"workflow_done","reason":"all checks pass","outcome":"PASS"}"#;
        match map_event_line(line).expect("maps") {
            WorkflowProgress::Finalized { kind, summary } => {
                assert_eq!(kind, FinalizeKind::Done);
                assert!(summary.contains("PASS"), "summary={summary}");
                assert!(summary.contains("all checks pass"), "summary={summary}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn map_workflow_aborted_carries_category() {
        let line = r#"{"type":"workflow_aborted","category":"env","reason":"no NPU",
            "user_remediation":"free a slot","details":null}"#;
        match map_event_line(line).expect("maps") {
            WorkflowProgress::Finalized { kind, summary } => {
                assert_eq!(kind, FinalizeKind::Aborted);
                assert!(summary.contains("env"), "summary={summary}");
                assert!(summary.contains("no NPU"), "summary={summary}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn map_workflow_escalated() {
        let line = r#"{"type":"workflow_escalated","reason":"await user","outcome":null}"#;
        match map_event_line(line).expect("maps") {
            WorkflowProgress::Finalized { kind, .. } => assert_eq!(kind, FinalizeKind::Escalated),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn map_ignores_internal_and_unknown_events() {
        assert!(map_event_line(r#"{"type":"internal_event","action_name":"x"}"#).is_none());
        assert!(map_event_line(r#"{"type":"workflow_route_intake","missing":[]}"#).is_none());
        assert!(map_event_line(r#"{"type":"brand_new_future_event"}"#).is_none());
    }

    #[test]
    fn map_ignores_blank_and_malformed_lines() {
        assert!(map_event_line("").is_none());
        assert!(map_event_line("   ").is_none());
        assert!(map_event_line("not json").is_none());
        assert!(map_event_line("{ partial").is_none());
    }

    #[test]
    fn map_agent_stream_tool_use() {
        let line = r#"{"ts":1,"action_id":"act_1","agent_name":"aog-kernel-worker",
            "tool_call_id":"tc_9","kind":"tool_use","name":"Bash"}"#;
        let e = map_agent_stream_line(line).expect("tool_use maps");
        assert_eq!(e.action_id, "act_1");
        assert_eq!(e.agent_name, "aog-kernel-worker");
        assert_eq!(e.tool_call_id, "tc_9");
        assert_eq!(e.phase, SubagentPhase::ToolUse);
        assert_eq!(e.name, "Bash");
        assert!(e.detail.is_none());
        assert!(e.status.is_none());
    }

    #[test]
    fn map_agent_stream_tool_use_carries_detail() {
        let line = r#"{"ts":1,"action_id":"act_1","agent_name":"w","tool_call_id":"tc_9",
            "kind":"tool_use","name":"Bash","detail":"deploy + build kernel"}"#;
        let e = map_agent_stream_line(line).expect("tool_use maps");
        assert_eq!(e.detail.as_deref(), Some("deploy + build kernel"));
    }

    #[test]
    fn map_agent_stream_empty_detail_becomes_none() {
        let line = r#"{"ts":1,"action_id":"act_1","agent_name":"w","tool_call_id":"tc_9",
            "kind":"tool_use","name":"Read","detail":""}"#;
        let e = map_agent_stream_line(line).expect("tool_use maps");
        assert!(e.detail.is_none(), "empty detail should normalize to None");
    }

    #[test]
    fn map_agent_stream_tool_result_with_status() {
        let line = r#"{"ts":2,"action_id":"act_1","agent_name":"w","tool_call_id":"tc_9",
            "kind":"tool_result","name":"Bash","status":"ok"}"#;
        let e = map_agent_stream_line(line).expect("tool_result maps");
        assert_eq!(e.phase, SubagentPhase::ToolResult);
        assert_eq!(e.status.as_deref(), Some("ok"));
    }

    #[test]
    fn map_agent_stream_rejects_unknown_kind_and_missing_action_id() {
        assert!(
            map_agent_stream_line(r#"{"action_id":"a","kind":"thinking","name":"x"}"#).is_none()
        );
        assert!(map_agent_stream_line(r#"{"kind":"tool_use","name":"Bash"}"#).is_none());
        assert!(map_agent_stream_line("").is_none());
        assert!(map_agent_stream_line("{bad").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tail_agent_stream_is_noop_when_file_absent_then_stops() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let dir = std::env::temp_dir().join(format!("wf_substream_{}", std::process::id()));
                let _ = tokio::fs::remove_dir_all(&dir).await;
                let stream = dir.join("agent_stream.jsonl"); // never created

                let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                let handle = tokio::task::spawn_local(async move {
                    tail_agent_stream(stream, stop_rx, move |e| {
                        let _ = tx.send(e);
                    })
                    .await;
                });
                tokio::time::sleep(Duration::from_millis(60)).await;
                // No file => no events emitted.
                assert!(rx.try_recv().is_err());
                stop_tx.send(true).unwrap();
                let res = tokio::time::timeout(Duration::from_secs(2), handle).await;
                assert!(res.is_ok(), "agent_stream tail did not stop on signal");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tail_follows_appended_lines_until_terminal() {
        // The tail task is designed to run on a LocalSet (non-Send emit
        // closure), so drive it through one here — mirroring the app's
        // `spawn_local` integration point.
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let dir = std::env::temp_dir().join(format!("wf_tail_{}", std::process::id()));
                let _ = tokio::fs::remove_dir_all(&dir).await;
                tokio::fs::create_dir_all(&dir).await.unwrap();
                let events = dir.join("events.jsonl");

                let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
                let (updates_tx, mut updates_rx) = tokio::sync::mpsc::unbounded_channel();

                let events_for_task = events.clone();
                let handle = tokio::task::spawn_local(async move {
                    tail_events(events_for_task, stop_rx, move |u| {
                        let _ = updates_tx.send(u);
                    })
                    .await;
                });

                // File does not exist yet -> task should be poll-waiting.
                tokio::time::sleep(Duration::from_millis(50)).await;

                // Append run start, an action pair, then a terminal event,
                // simulating the engine writing one full line at a time.
                append_line(&events, r#"{"type":"run_started","workflow":"gen","cwd":"/x"}"#)
                    .await;
                append_line(
                    &events,
                    r#"{"type":"action_started","action_id":"a1","action":{"kind":"spawn_agent","name":"w"}}"#,
                )
                .await;
                append_line(
                    &events,
                    r#"{"type":"action_completed","action_id":"a1","action":{"kind":"spawn_agent","name":"w"},"result":{"marker":{"outcome":"clean"}}}"#,
                )
                .await;
                append_line(&events, r#"{"type":"workflow_done","reason":"ok","outcome":"PASS"}"#)
                    .await;

                let mut got = Vec::new();
                // The task self-terminates on the Finalized event.
                while let Some(u) = recv_with_timeout(&mut updates_rx).await {
                    let terminal = matches!(u, WorkflowProgress::Finalized { .. });
                    got.push(u);
                    if terminal {
                        break;
                    }
                }
                let _ = stop_tx.send(true);
                let _ = handle.await;
                let _ = tokio::fs::remove_dir_all(&dir).await;

                assert_eq!(got.len(), 4, "got = {got:?}");
                assert!(matches!(got[0], WorkflowProgress::Started { .. }));
                assert!(matches!(got[1], WorkflowProgress::ActionStarted { .. }));
                assert!(matches!(got[2], WorkflowProgress::ActionCompleted { .. }));
                assert!(matches!(
                    got[3],
                    WorkflowProgress::Finalized { kind: FinalizeKind::Done, .. }
                ));
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tail_stops_on_signal_when_file_never_appears() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let dir =
                    std::env::temp_dir().join(format!("wf_tail_nostart_{}", std::process::id()));
                let _ = tokio::fs::remove_dir_all(&dir).await;
                let events = dir.join("events.jsonl"); // never created

                let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
                let handle = tokio::task::spawn_local(async move {
                    tail_events(events, stop_rx, |_| {}).await;
                });
                tokio::time::sleep(Duration::from_millis(60)).await;
                stop_tx.send(true).unwrap();
                // Should return promptly after the stop signal rather than hang.
                let res = tokio::time::timeout(Duration::from_secs(2), handle).await;
                assert!(res.is_ok(), "tail task did not stop on signal");
            })
            .await;
    }

    async fn append_line(path: &Path, line: &str) {
        use tokio::io::AsyncWriteExt;
        let mut f =
            tokio::fs::OpenOptions::new().create(true).append(true).open(path).await.unwrap();
        f.write_all(line.as_bytes()).await.unwrap();
        f.write_all(b"\n").await.unwrap();
        f.flush().await.unwrap();
        // Give the tail loop a couple poll cycles to pick it up.
        tokio::time::sleep(Duration::from_millis(40)).await;
    }

    async fn recv_with_timeout(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<WorkflowProgress>,
    ) -> Option<WorkflowProgress> {
        tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.ok().flatten()
    }
}
