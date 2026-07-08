// Copyright 2025 Simon Peter Rothgang
// SPDX-License-Identifier: Apache-2.0

//! Two-layer rendering for Execute/Bash tool calls: content is cached
//! (width-independent), borders are applied at render time.

use crate::agent::model;
use crate::app::{ToolCallInfo, WorkflowActionStatus, WorkflowFinalizeKind, WorkflowProgressState};
use crate::ui::highlight;
use crate::ui::theme;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::errors::failed_execute_first_line;
use super::interactions::{render_permission_lines, render_question_lines};
use super::{
    ToolCallRenderContext, markdown_inline_spans, spans_width, status_icon, tool_display_title,
    tool_output_badge_spans, truncate_spans_to_width,
};

/// Max visible output lines for Execute/Bash tool calls.
/// Total box height = 1 (title) + 1 (command) + this + 1 (bottom border) = 15.
pub(super) const TERMINAL_MAX_LINES: usize = 12;

/// Render Execute/Bash content lines WITHOUT any border decoration.
/// This is width-independent and safe to cache across resizes.
/// Returns: command line + output lines + permission lines (no border prefixes).
pub(super) fn render_execute_content(tc: &ToolCallInfo) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Command line (no border prefix)
    if let Some(ref cmd) = tc.terminal_command {
        let mut spans = vec![Span::styled(
            "$ ",
            Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
        )];
        let mut command_spans = highlight::highlight_shell_command(cmd);
        if command_spans.is_empty() {
            command_spans.push(Span::styled(cmd.clone(), Style::default().fg(Color::Yellow)));
        }
        spans.extend(command_spans);
        lines.push(Line::from(spans));
    }

    // Live workflow progress (lingxi-ascendc run): structured child rows in
    // place of the subprocess stdout, which only arrives once the run exits.
    // Only replaces stdout once at least one event has landed — until then the
    // raw output stays visible, so a run whose events.jsonl never appears still
    // shows its subprocess output instead of a permanently empty progress box.
    if let Some(progress) = tc.workflow_progress.as_ref().filter(|p| p.has_content()) {
        lines.extend(render_workflow_progress(progress));
        // Inline permission/question controls may still apply (e.g. an
        // await_user_decision escalation surfaces a permission); keep them.
        if let Some(ref perm) = tc.pending_permission {
            lines.extend(render_permission_lines(tc, perm));
        }
        if let Some(ref question) = tc.pending_question {
            lines.extend(render_question_lines(question));
        }
        return lines;
    }

    // Output lines (capped, no border prefix)
    let mut body_lines: Vec<Line<'static>> = Vec::new();

    if let Some(ref output) = tc.terminal_output {
        let stripped_output = highlight::strip_ansi(output);
        if matches!(tc.status, model::ToolCallStatus::Failed | model::ToolCallStatus::Killed)
            && let Some(first_line) = failed_execute_first_line(&stripped_output)
        {
            body_lines.push(Line::from(Span::styled(
                first_line,
                Style::default().fg(theme::STATUS_ERROR),
            )));
        } else {
            let raw_lines = highlight::render_terminal_output(output);

            let total = raw_lines.len();
            if total > TERMINAL_MAX_LINES {
                let skipped = total - TERMINAL_MAX_LINES;
                body_lines.push(Line::from(Span::styled(
                    format!("... {skipped} lines hidden ..."),
                    Style::default().fg(theme::DIM),
                )));
                body_lines.extend(raw_lines.into_iter().skip(skipped));
            } else {
                body_lines = raw_lines;
            }
        }
    } else if matches!(tc.status, model::ToolCallStatus::InProgress) {
        body_lines.push(Line::from(Span::styled("running...", Style::default().fg(theme::DIM))));
    }

    lines.extend(body_lines);

    // Inline permission controls (no border prefix)
    if let Some(ref perm) = tc.pending_permission {
        lines.extend(render_permission_lines(tc, perm));
    }
    if let Some(ref question) = tc.pending_question {
        lines.extend(render_question_lines(question));
    }

    lines
}

/// Apply Execute/Bash box borders around pre-rendered content lines.
/// This is called at render time with the current width, so borders always
/// fill the terminal correctly even after resize.
pub(super) fn render_execute_with_borders(
    tc: &ToolCallInfo,
    render_context: ToolCallRenderContext<'_>,
    content: &[Line<'static>],
    width: u16,
    spinner_frame: usize,
) -> Vec<Line<'static>> {
    let border = Style::default().fg(theme::DIM);
    let inner_w = (width as usize).saturating_sub(2);
    let mut out = Vec::with_capacity(content.len() + 2);

    // Top border with status icon and title
    let (status_icon_str, icon_color) = status_icon(tc.status, spinner_frame);
    let (_tool_icon, tool_label) = theme::tool_name_label(&tc.sdk_tool_name);
    let line_budget = width as usize;
    let left_prefix = vec![
        Span::styled("  \u{256D}\u{2500}", border),
        Span::styled(format!(" {status_icon_str} "), Style::default().fg(icon_color)),
        Span::styled(
            format!("{tool_label} "),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
    ];
    let badge_spans = tool_output_badge_spans(tc);
    let prefix_w = spans_width(&left_prefix);
    let badges_w = spans_width(&badge_spans);
    let right_border_w = 1; // "right-corner"
    // Reserve at least one fill char so the border looks continuous.
    let title_max_w = line_budget.saturating_sub(prefix_w + badges_w + right_border_w + 1);
    let display_title = tool_display_title(tc, render_context);
    let title_spans =
        truncate_spans_to_width(markdown_inline_spans(display_title.as_ref()), title_max_w);
    let title_w = spans_width(&title_spans);
    let fill_w = line_budget.saturating_sub(prefix_w + title_w + badges_w + right_border_w);
    let top_fill: String = "\u{2500}".repeat(fill_w);

    let mut top = left_prefix;
    top.extend(title_spans);
    top.extend(badge_spans);
    top.push(Span::styled(format!("{top_fill}\u{256E}"), border));
    out.push(Line::from(top));

    // Content lines with left border prefix
    for line in content {
        let mut spans = vec![Span::styled("  \u{2502} ", border)];
        spans.extend(line.spans.iter().cloned());
        out.push(Line::from(spans));
    }

    // Bottom border
    let bottom_fill: String = "\u{2500}".repeat(inner_w.saturating_sub(2));
    out.push(Line::from(Span::styled(format!("  \u{2570}{bottom_fill}\u{256F}"), border)));

    out
}

/// Render live workflow-progress rows (no border prefix; the border is applied
/// by [`render_execute_with_borders`]).
///
/// While running: a header (`run <workflow> ...`) plus one row per action,
/// `glyph kind:name  outcome`. Once finalized: collapse to the summary line.
pub(super) fn render_workflow_progress(progress: &WorkflowProgressState) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Header: workflow name + running/finalized state.
    let workflow = progress.workflow.as_deref().unwrap_or("workflow");
    if let Some(ref fin) = progress.finalized {
        let (glyph, color) = match fin.kind {
            WorkflowFinalizeKind::Done => ("\u{2713}", Color::Green),
            WorkflowFinalizeKind::Aborted => ("\u{2717}", theme::STATUS_ERROR),
            WorkflowFinalizeKind::Escalated => ("\u{26a0}", theme::STATUS_WARNING),
        };
        let mut header = vec![
            Span::styled(format!("{glyph} "), Style::default().fg(color)),
            Span::styled(
                format!("{workflow} "),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
        ];
        if let Some(params) = params_span(&progress.params) {
            header.push(params);
        }
        header.push(Span::styled(fin.summary.clone(), Style::default().fg(color)));
        lines.push(Line::from(header));
        // Keep per-action rows visible under the summary so the breakdown that
        // led to the outcome is not lost. Nested subagent tool rows only when
        // verbose, same gating as the running path.
        for row in &progress.actions {
            push_action_row(&mut lines, row, progress.verbose);
        }
        return lines;
    }

    let mut header = vec![Span::styled(
        format!("\u{25b8} {workflow} "),
        Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
    )];
    if let Some(params) = params_span(&progress.params) {
        header.push(params);
    }
    header.push(Span::styled("running", Style::default().fg(theme::DIM)));
    lines.push(Line::from(header));
    for row in &progress.actions {
        push_action_row(&mut lines, row, progress.verbose);
    }
    lines
}

/// Render the ordered header params as a single dim ` k=v k=v ` span, or `None`
/// when there are no params. Trailing space separates it from the `running`
/// marker that follows.
fn params_span(params: &[(String, String)]) -> Option<Span<'static>> {
    if params.is_empty() {
        return None;
    }
    let text = params.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(" ");
    Some(Span::styled(format!("{text}  "), Style::default().fg(theme::DIM)))
}

/// Max nested subagent rows shown per action (Phase 2), to keep the box bounded.
const MAX_SUBAGENT_ROWS: usize = 6;

/// Append an action row plus, when `verbose`, its nested subagent tool rows.
/// Default (non-verbose) shows only the action row; the nested Bash/Read/...
/// rows from `agent_stream.jsonl` render only under `-v`/`--verbose`.
fn push_action_row(
    lines: &mut Vec<Line<'static>>,
    row: &crate::app::WorkflowActionRow,
    verbose: bool,
) {
    lines.push(render_action_row(row));
    if !verbose {
        return;
    }
    // Nested subagent tool calls (from agent_stream.jsonl). Indented one level
    // under the action.
    let total = row.subagent_tools.len();
    let shown = total.min(MAX_SUBAGENT_ROWS);
    if total > MAX_SUBAGENT_ROWS {
        lines.push(Line::from(Span::styled(
            format!("      \u{2026} {} earlier tool calls", total - shown),
            Style::default().fg(theme::DIM),
        )));
    }
    for sub in row.subagent_tools.iter().skip(total - shown) {
        lines.push(render_subagent_row(sub));
    }
}

fn render_action_row(row: &crate::app::WorkflowActionRow) -> Line<'static> {
    let label = action_label(&row.kind, &row.name);
    match &row.completed {
        None => {
            let mut spans = vec![
                Span::styled("  \u{2219} ", Style::default().fg(theme::DIM)),
                Span::styled(label, Style::default().fg(Color::White)),
            ];
            push_action_detail(&mut spans, row.detail.as_deref());
            spans.push(Span::styled("  running", Style::default().fg(theme::DIM)));
            Line::from(spans)
        }
        Some(done) => {
            let (glyph, color) = match done.status {
                WorkflowActionStatus::Ok => ("\u{2713}", Color::Green),
                WorkflowActionStatus::Retry => ("\u{21bb}", theme::STATUS_WARNING),
                WorkflowActionStatus::Fail => ("\u{2717}", theme::STATUS_ERROR),
            };
            let mut spans = vec![
                Span::styled(format!("  {glyph} "), Style::default().fg(color)),
                Span::styled(label, Style::default().fg(Color::White)),
            ];
            push_action_detail(&mut spans, row.detail.as_deref());
            if !done.outcome.is_empty() {
                spans.push(Span::styled(
                    format!("  {}", done.outcome),
                    Style::default().fg(theme::DIM),
                ));
            }
            Line::from(spans)
        }
    }
}

fn push_action_detail(spans: &mut Vec<Span<'static>>, detail: Option<&str>) {
    if let Some(detail) = detail.map(str::trim).filter(|s| !s.is_empty()) {
        spans.push(Span::styled(format!("  [{detail}]"), Style::default().fg(theme::DIM)));
    }
}

fn render_subagent_row(sub: &crate::app::WorkflowSubagentToolRow) -> Line<'static> {
    let label = subagent_label(&sub.name, sub.detail.as_deref());
    if sub.completed {
        let mut spans = vec![
            Span::styled("      \u{2713} ", Style::default().fg(Color::Green)),
            Span::styled(label, Style::default().fg(theme::DIM)),
        ];
        if let Some(status) = sub.status.as_deref().filter(|s| !s.is_empty()) {
            spans.push(Span::styled(format!("  {status}"), Style::default().fg(theme::DIM)));
        }
        Line::from(spans)
    } else {
        Line::from(vec![
            Span::styled("      \u{2219} ", Style::default().fg(theme::DIM)),
            Span::styled(label, Style::default().fg(theme::DIM)),
        ])
    }
}

/// Compose a subagent tool-row label: `"<name>: <detail>"`, or just `"<name>"`
/// when the detail is empty/None. Falls back to `"tool"` for an empty name.
fn subagent_label(name: &str, detail: Option<&str>) -> String {
    let name = if name.is_empty() { "tool" } else { name };
    match detail.map(str::trim).filter(|d| !d.is_empty()) {
        Some(detail) => format!("{name}: {detail}"),
        None => name.to_owned(),
    }
}

/// Compose an action label from kind + name, tolerating either being empty
/// (e.g. a synthesized completion-only row).
fn action_label(kind: &str, name: &str) -> String {
    match (kind.is_empty(), name.is_empty()) {
        (false, false) => format!("{kind}: {name}"),
        (true, false) => name.to_owned(),
        (false, true) => kind.to_owned(),
        (true, true) => "action".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{
        BlockCache, WorkflowActionCompletion, WorkflowActionRow, WorkflowFinalizeRow,
        WorkflowSubagentToolRow,
    };

    fn render_text(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect()
    }

    fn bash_with_progress(progress: WorkflowProgressState) -> ToolCallInfo {
        ToolCallInfo {
            id: "tc1".to_owned(),
            title: "Bash".to_owned(),
            sdk_tool_name: "Bash".to_owned(),
            raw_input: None,
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status: model::ToolCallStatus::InProgress,
            content: Vec::new(),
            hidden: false,
            terminal_id: Some("term1".to_owned()),
            terminal_command: Some("lingxi-ascendc run --workflow gen --run-id op1".to_owned()),
            terminal_output: Some("this raw stdout should be suppressed".to_owned()),
            terminal_output_len: 0,
            terminal_bytes_seen: 0,
            terminal_snapshot_mode: crate::app::TerminalSnapshotMode::AppendOnly,
            render_epoch: 0,
            layout_epoch: 0,
            last_measured_width: 0,
            last_measured_height: 0,
            last_measured_layout_epoch: 0,
            last_measured_layout_generation: 0,
            cache: BlockCache::default(),
            pending_permission: None,
            pending_question: None,
            workflow_progress: Some(progress),
        }
    }

    fn state() -> WorkflowProgressState {
        let (tx, _rx) = tokio::sync::watch::channel(false);
        WorkflowProgressState::new(tx)
    }

    #[test]
    fn empty_progress_keeps_raw_stdout_visible() {
        // Before any event lands the progress box is empty; the raw Bash stdout
        // must stay visible instead of being hidden behind an empty box (e.g.
        // when events.jsonl never appears because the run dir did not match).
        let tc = bash_with_progress(state());
        let joined = render_text(&render_execute_content(&tc)).join("\n");
        assert!(
            joined.contains("this raw stdout should be suppressed"),
            "empty progress must not hide stdout:\n{joined}"
        );
    }

    #[test]
    fn running_progress_replaces_stdout_with_child_rows() {
        let mut s = state();
        s.workflow = Some("generate_ascendc".to_owned());
        s.actions.push(WorkflowActionRow {
            action_id: "a1".to_owned(),
            kind: "spawn_agent".to_owned(),
            name: "aog-kernel-worker".to_owned(),
            detail: Some("scope=full rounds=20 expected=3 artifacts".to_owned()),
            completed: Some(WorkflowActionCompletion {
                status: WorkflowActionStatus::Ok,
                outcome: "clean".to_owned(),
            }),
            subagent_tools: Vec::new(),
        });
        s.actions.push(WorkflowActionRow {
            action_id: "a2".to_owned(),
            kind: "verify".to_owned(),
            name: "verify".to_owned(),
            detail: None,
            completed: None,
            subagent_tools: Vec::new(),
        });

        let tc = bash_with_progress(s);
        let text = render_text(&render_execute_content(&tc));
        let joined = text.join("\n");

        // Command line is preserved; raw stdout is suppressed.
        assert!(joined.contains("lingxi-ascendc run"), "cmd missing:\n{joined}");
        assert!(
            !joined.contains("this raw stdout should be suppressed"),
            "raw stdout leaked:\n{joined}"
        );
        // Workflow header + both action rows present.
        assert!(joined.contains("generate_ascendc"), "header missing:\n{joined}");
        assert!(joined.contains("spawn_agent: aog-kernel-worker"), "row1:\n{joined}");
        assert!(joined.contains("scope=full rounds=20"), "detail:\n{joined}");
        assert!(joined.contains("clean"), "outcome:\n{joined}");
        assert!(joined.contains("verify: verify"), "row2:\n{joined}");
        assert!(joined.contains("running"), "running marker:\n{joined}");
    }

    #[test]
    fn finalized_progress_shows_summary_line() {
        let mut s = state();
        s.workflow = Some("generate_ascendc".to_owned());
        s.actions.push(WorkflowActionRow {
            action_id: "a1".to_owned(),
            kind: "spawn_agent".to_owned(),
            name: "w".to_owned(),
            detail: None,
            completed: Some(WorkflowActionCompletion {
                status: WorkflowActionStatus::Ok,
                outcome: "clean".to_owned(),
            }),
            subagent_tools: Vec::new(),
        });
        s.finalized = Some(WorkflowFinalizeRow {
            kind: WorkflowFinalizeKind::Done,
            summary: "done (PASS): all checks pass".to_owned(),
        });

        let tc = bash_with_progress(s);
        let joined = render_text(&render_execute_content(&tc)).join("\n");
        assert!(joined.contains("done (PASS)"), "summary missing:\n{joined}");
        assert!(joined.contains("all checks pass"), "reason missing:\n{joined}");
        // Action breakdown is still shown under the summary.
        // (continued below)
        assert!(joined.contains("spawn_agent: w"), "action kept:\n{joined}");
    }

    fn action_with_subtools() -> WorkflowActionRow {
        WorkflowActionRow {
            action_id: "a1".to_owned(),
            kind: "spawn_agent".to_owned(),
            name: "worker".to_owned(),
            detail: None,
            completed: None,
            subagent_tools: vec![
                WorkflowSubagentToolRow {
                    tool_call_id: "t1".to_owned(),
                    name: "Read".to_owned(),
                    detail: Some("op_kernel.cpp".to_owned()),
                    completed: true,
                    status: Some("ok".to_owned()),
                },
                WorkflowSubagentToolRow {
                    tool_call_id: "t2".to_owned(),
                    name: "Bash".to_owned(),
                    detail: Some("deploy + build kernel".to_owned()),
                    completed: false,
                    status: None,
                },
                WorkflowSubagentToolRow {
                    tool_call_id: "t3".to_owned(),
                    name: "Bash".to_owned(),
                    detail: None,
                    completed: false,
                    status: None,
                },
            ],
        }
    }

    #[test]
    fn nested_subagent_tool_rows_render_under_action_when_verbose() {
        let mut s = state();
        s.workflow = Some("gen".to_owned());
        s.verbose = true;
        s.actions.push(action_with_subtools());

        let tc = bash_with_progress(s);
        let joined = render_text(&render_execute_content(&tc)).join("\n");
        assert!(joined.contains("spawn_agent: worker"), "parent action:\n{joined}");
        // "<name>: <detail>" rendering.
        assert!(joined.contains("Read: op_kernel.cpp"), "subagent w/ detail:\n{joined}");
        assert!(joined.contains("Bash: deploy + build kernel"), "bash detail:\n{joined}");
        // detail None -> just the bare name.
        assert!(joined.contains("\u{2219} Bash"), "bare name row:\n{joined}");
    }

    #[test]
    fn nested_subagent_tool_rows_hidden_when_not_verbose() {
        let mut s = state();
        s.workflow = Some("gen".to_owned());
        // verbose defaults to false.
        s.actions.push(action_with_subtools());

        let tc = bash_with_progress(s);
        let joined = render_text(&render_execute_content(&tc)).join("\n");
        // Action row still shows.
        assert!(joined.contains("spawn_agent: worker"), "parent action:\n{joined}");
        // Nested tool rows are suppressed by default.
        assert!(!joined.contains("op_kernel.cpp"), "detail leaked:\n{joined}");
        assert!(!joined.contains("deploy + build kernel"), "detail leaked:\n{joined}");
    }

    #[test]
    fn header_renders_ordered_params_after_workflow_name() {
        let mut s = state();
        s.workflow = Some("generate_ascendc".to_owned());
        s.params = vec![
            ("op".to_owned(), "3_Add".to_owned()),
            ("via".to_owned(), "cannbot".to_owned()),
            ("run_mode".to_owned(), "user".to_owned()),
        ];
        s.actions.push(WorkflowActionRow {
            action_id: "a1".to_owned(),
            kind: "verify".to_owned(),
            name: "verify".to_owned(),
            detail: None,
            completed: None,
            subagent_tools: Vec::new(),
        });

        let tc = bash_with_progress(s);
        let lines = render_text(&render_execute_content(&tc));
        // Header line is the first workflow-progress line (after the $ cmd line).
        let header = lines.iter().find(|l| l.contains("generate_ascendc")).expect("header");
        assert!(header.contains("op=3_Add"), "param op:\n{header}");
        assert!(header.contains("via=cannbot"), "param via:\n{header}");
        assert!(header.contains("run_mode=user"), "param run_mode:\n{header}");
        assert!(header.contains("running"), "running marker:\n{header}");
        // Order: workflow name precedes params, params precede `running`.
        let w = header.find("generate_ascendc").unwrap();
        let p = header.find("op=3_Add").unwrap();
        let r = header.find("running").unwrap();
        assert!(w < p && p < r, "ordering wrong:\n{header}");
    }
}
