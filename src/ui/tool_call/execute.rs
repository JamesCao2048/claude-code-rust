// Copyright 2025 Simon Peter Rothgang
// SPDX-License-Identifier: Apache-2.0

//! Two-layer rendering for Execute/Bash tool calls: content is cached
//! (width-independent), borders are applied at render time.

use crate::agent::model;
use crate::app::{
    ToolCallInfo, WorkflowActionStatus, WorkflowFinalizeKind, WorkflowProgressState,
};
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
    if let Some(ref progress) = tc.workflow_progress {
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
        lines.push(Line::from(vec![
            Span::styled(format!("{glyph} "), Style::default().fg(color)),
            Span::styled(
                format!("{workflow} "),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
            Span::styled(fin.summary.clone(), Style::default().fg(color)),
        ]));
        // Keep per-action rows visible under the summary so the breakdown that
        // led to the outcome is not lost.
        for row in &progress.actions {
            lines.push(render_action_row(row));
        }
        return lines;
    }

    lines.push(Line::from(vec![
        Span::styled(
            format!("\u{25b8} {workflow} "),
            Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
        ),
        Span::styled("running", Style::default().fg(theme::DIM)),
    ]));
    for row in &progress.actions {
        push_action_row(&mut lines, row);
    }
    lines
}

/// Max nested subagent rows shown per action (Phase 2), to keep the box bounded.
const MAX_SUBAGENT_ROWS: usize = 6;

/// Append an action row plus its nested subagent tool rows (Phase 2).
fn push_action_row(lines: &mut Vec<Line<'static>>, row: &crate::app::WorkflowActionRow) {
    lines.push(render_action_row(row));
    // Nested subagent tool calls (from agent_stream.jsonl). Indented one level
    // under the action. Absent for current runs (engine emits no stream yet).
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
        None => Line::from(vec![
            Span::styled("  \u{2219} ", Style::default().fg(theme::DIM)),
            Span::styled(label, Style::default().fg(Color::White)),
            Span::styled("  running", Style::default().fg(theme::DIM)),
        ]),
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

fn render_subagent_row(sub: &crate::app::WorkflowSubagentToolRow) -> Line<'static> {
    let name = if sub.name.is_empty() { "tool" } else { sub.name.as_str() };
    if sub.completed {
        let mut spans = vec![
            Span::styled("      \u{2713} ", Style::default().fg(Color::Green)),
            Span::styled(name.to_owned(), Style::default().fg(theme::DIM)),
        ];
        if let Some(status) = sub.status.as_deref().filter(|s| !s.is_empty()) {
            spans.push(Span::styled(
                format!("  {status}"),
                Style::default().fg(theme::DIM),
            ));
        }
        Line::from(spans)
    } else {
        Line::from(vec![
            Span::styled("      \u{2219} ", Style::default().fg(theme::DIM)),
            Span::styled(name.to_owned(), Style::default().fg(theme::DIM)),
        ])
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
    fn running_progress_replaces_stdout_with_child_rows() {
        let mut s = state();
        s.workflow = Some("generate_ascendc".to_owned());
        s.actions.push(WorkflowActionRow {
            action_id: "a1".to_owned(),
            kind: "spawn_agent".to_owned(),
            name: "aog-kernel-worker".to_owned(),
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

    #[test]
    fn nested_subagent_tool_rows_render_under_action() {
        let mut s = state();
        s.workflow = Some("gen".to_owned());
        s.actions.push(WorkflowActionRow {
            action_id: "a1".to_owned(),
            kind: "spawn_agent".to_owned(),
            name: "worker".to_owned(),
            completed: None,
            subagent_tools: vec![
                WorkflowSubagentToolRow {
                    tool_call_id: "t1".to_owned(),
                    name: "Edit".to_owned(),
                    completed: true,
                    status: Some("ok".to_owned()),
                },
                WorkflowSubagentToolRow {
                    tool_call_id: "t2".to_owned(),
                    name: "Bash".to_owned(),
                    completed: false,
                    status: None,
                },
            ],
        });

        let tc = bash_with_progress(s);
        let joined = render_text(&render_execute_content(&tc)).join("\n");
        assert!(joined.contains("spawn_agent: worker"), "parent action:\n{joined}");
        assert!(joined.contains("Edit"), "subagent tool 1:\n{joined}");
        assert!(joined.contains("Bash"), "subagent tool 2:\n{joined}");
    }
}
