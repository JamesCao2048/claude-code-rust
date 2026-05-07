//! Headless output emitters (stream-json + text). See T5/T6 in the implementation plan.
//!
//! `StreamJsonEmitter` writes one JSON object per line to the wrapped writer,
//! flushing after every event. The schema versioning field `v` is `1`. The
//! event-type mapping follows design §4.1 with v1 simplifications:
//!
//! - `Initialized` → `system.init`
//! - `Connected` / `SessionReplaced` → `system.session`
//! - `TurnComplete{Completed|None}` → `result.success`
//! - `TurnComplete{other}` → `result.terminated`
//! - `TurnError` → `result.error`
//! - `AuthRequired` → `result.auth_required`
//! - `ConnectionFailed` → `result.connection_failed`
//! - `SlashError` / `RuntimeReloadFailed` → `error_event` (with `kind`)
//! - `SessionUpdate` → `session_update` (full passthrough; see TODO)
//! - permission/question/elicitation: intercepted by the driver; emitter
//!   carries a defensive fallthrough as `session_update`.

use crate::agent::types::TerminalReason;
use crate::agent::wire::BridgeEvent;
use serde_json::json;
use std::io::Write;

pub trait Emitter: Send {
    fn emit_event(&mut self, ev: &BridgeEvent) -> std::io::Result<()>;
    fn emit_warning(&mut self, msg: &str) -> std::io::Result<()>;
    fn emit_error(&mut self, msg: &str) -> std::io::Result<()>;
    fn finish(&mut self) -> std::io::Result<()>;
}

pub struct StreamJsonEmitter<W: Write> {
    out: W,
}

impl<W: Write> StreamJsonEmitter<W> {
    pub fn new(out: W) -> Self {
        Self { out }
    }

    fn write_line(&mut self, value: &serde_json::Value) -> std::io::Result<()> {
        let line = serde_json::to_string(value).map_err(std::io::Error::other)?;
        debug_assert!(
            !line.contains('\n'),
            "stream-json line must not embed newlines"
        );
        self.out.write_all(line.as_bytes())?;
        self.out.write_all(b"\n")?;
        self.out.flush()
    }
}

/// Map a `BridgeEvent` to the v1 stream-json envelope. Pure function so we keep
/// `emit_event` short and test the mapping without I/O.
fn event_to_json(ev: &BridgeEvent) -> serde_json::Value {
    match ev {
        BridgeEvent::Initialized { result } => json!({
            "type": "system.init",
            "v": 1,
            "agent_name": result.agent_name,
            "agent_version": result.agent_version,
            "capabilities": result.capabilities,
        }),
        BridgeEvent::Connected {
            session_id,
            cwd,
            current_model,
            ..
        }
        | BridgeEvent::SessionReplaced {
            session_id,
            cwd,
            current_model,
            ..
        } => json!({
            "type": "system.session",
            "v": 1,
            "session_id": session_id,
            "cwd": cwd,
            "model": current_model,
        }),
        // TODO(v1.1): refine session_update mapping per design §4.1 table
        BridgeEvent::SessionUpdate { session_id, update } => json!({
            "type": "session_update",
            "v": 1,
            "session_id": session_id,
            "update": update,
        }),
        BridgeEvent::SlashError {
            session_id,
            message,
        } => json!({
            "type": "error_event",
            "v": 1,
            "kind": "slash_error",
            "session_id": session_id,
            "message": message,
        }),
        BridgeEvent::RuntimeReloadFailed {
            session_id,
            message,
        } => json!({
            "type": "error_event",
            "v": 1,
            "kind": "runtime_reload_failed",
            "session_id": session_id,
            "message": message,
        }),
        BridgeEvent::TurnComplete {
            session_id,
            terminal_reason,
        } => turn_complete_to_json(session_id, *terminal_reason),
        BridgeEvent::TurnError {
            session_id,
            message,
            error_kind,
            terminal_reason,
            ..
        } => json!({
            "type": "result.error",
            "v": 1,
            "session_id": session_id,
            "message": message,
            "error_kind": error_kind,
            "terminal_reason": terminal_reason.map(TerminalReason::as_stored),
        }),
        BridgeEvent::AuthRequired {
            method_name,
            method_description,
        } => json!({
            "type": "result.auth_required",
            "v": 1,
            "method_name": method_name,
            "method_description": method_description,
        }),
        BridgeEvent::ConnectionFailed { message } => json!({
            "type": "result.connection_failed",
            "v": 1,
            "message": message,
        }),
        // permission/question/elicitation requests are intercepted by the
        // driver before reaching the emitter — defense-in-depth fallthrough:
        other => json!({
            "type": "session_update",
            "v": 1,
            "raw_event": other.event_name(),
        }),
    }
}

fn turn_complete_to_json(
    session_id: &str,
    terminal_reason: Option<TerminalReason>,
) -> serde_json::Value {
    let reason_str = terminal_reason.map_or("completed", TerminalReason::as_stored);
    let kind = match terminal_reason {
        Some(TerminalReason::Completed) | None => "result.success",
        _ => "result.terminated",
    };
    json!({
        "type": kind,
        "v": 1,
        "session_id": session_id,
        "terminal_reason": reason_str,
    })
}

impl<W: Write + Send> Emitter for StreamJsonEmitter<W> {
    fn emit_event(&mut self, ev: &BridgeEvent) -> std::io::Result<()> {
        let value = event_to_json(ev);
        self.write_line(&value)
    }

    fn emit_warning(&mut self, _msg: &str) -> std::io::Result<()> {
        // Warnings are routed to stderr by the caller, not into the NDJSON stream.
        Ok(())
    }

    fn emit_error(&mut self, _msg: &str) -> std::io::Result<()> {
        // Errors are routed to stderr by the caller, not into the NDJSON stream.
        Ok(())
    }

    fn finish(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::types::{
        AgentCapabilities, AuthMethod, InitializeResult, TerminalReason,
    };

    fn lines(buf: &[u8]) -> Vec<serde_json::Value> {
        std::str::from_utf8(buf)
            .unwrap()
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("valid json"))
            .collect()
    }

    fn sample_initialize_result() -> InitializeResult {
        InitializeResult {
            agent_name: "lingxi".into(),
            agent_version: "0.1".into(),
            auth_methods: Vec::<AuthMethod>::new(),
            capabilities: AgentCapabilities {
                prompt_image: false,
                prompt_embedded_context: false,
                supports_session_listing: false,
                supports_resume_session: false,
            },
        }
    }

    #[test]
    fn stream_json_initialized_emits_system_init_v1() {
        let mut buf = Vec::new();
        let mut e = StreamJsonEmitter::new(&mut buf);
        e.emit_event(&BridgeEvent::Initialized {
            result: sample_initialize_result(),
        })
        .unwrap();
        let l = lines(&buf);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0]["type"], "system.init");
        assert_eq!(l[0]["v"], 1);
        assert_eq!(l[0]["agent_name"], "lingxi");
    }

    #[test]
    fn turn_complete_completed_emits_result_success() {
        let mut buf = Vec::new();
        StreamJsonEmitter::new(&mut buf)
            .emit_event(&BridgeEvent::TurnComplete {
                session_id: "s1".into(),
                terminal_reason: Some(TerminalReason::Completed),
            })
            .unwrap();
        let l = lines(&buf);
        assert_eq!(l[0]["type"], "result.success");
        assert_eq!(l[0]["terminal_reason"], "completed");
    }

    #[test]
    fn turn_complete_max_turns_emits_result_terminated() {
        let mut buf = Vec::new();
        StreamJsonEmitter::new(&mut buf)
            .emit_event(&BridgeEvent::TurnComplete {
                session_id: "s1".into(),
                terminal_reason: Some(TerminalReason::MaxTurns),
            })
            .unwrap();
        let l = lines(&buf);
        assert_eq!(l[0]["type"], "result.terminated");
        assert_eq!(l[0]["terminal_reason"], "max_turns");
    }

    #[test]
    fn slash_error_emits_error_event_kind_slash_error() {
        let mut buf = Vec::new();
        StreamJsonEmitter::new(&mut buf)
            .emit_event(&BridgeEvent::SlashError {
                session_id: "s1".into(),
                message: "bad".into(),
            })
            .unwrap();
        let l = lines(&buf);
        assert_eq!(l[0]["type"], "error_event");
        assert_eq!(l[0]["kind"], "slash_error");
    }

    #[test]
    fn lines_never_embed_newlines() {
        let mut buf = Vec::new();
        StreamJsonEmitter::new(&mut buf)
            .emit_event(&BridgeEvent::SlashError {
                session_id: "s1".into(),
                message: "line1\nline2".into(),
            })
            .unwrap();
        // exactly one newline (the line terminator)
        let newlines = buf.iter().fold(0u32, |acc, &b| acc + u32::from(b == b'\n'));
        assert_eq!(newlines, 1);
    }
}

// ---------------------------------------------------------------------------
// T6: `TextEmitter` — human-readable output for `--output-format text`.
//
// Routing rules (design §4.2, simplified for v1):
// - Assistant text chunks (`SessionUpdate::AgentMessageChunk` with
//   `ContentBlock::Text`) → stdout, written as-is (no trailing newline; the
//   model emits its own newlines, and the final terminator from
//   `result.success` ensures the line ends).
// - Thinking chunks (`SessionUpdate::AgentThoughtChunk`) → suppressed by
//   default (`show_thinking == false`). Plan defers `--show-thinking` to v1.5.
// - Tool calls (`SessionUpdate::ToolCall`) → stderr line `● {tool}({short})`
//   where `{tool}` is the tool kind and `{short}` is a one-line preview of
//   `tool_call.title` (truncated to 60 chars).
// - Tool call updates (`SessionUpdate::ToolCallUpdate`) → stderr line:
//     `  ✓` for `status == "completed"`,
//     `  ✗ {reason}` for `status == "failed"` (reason from raw_output or
//     fields.title fallback). Other statuses are dropped.
// - `result.success` (`TurnComplete{Completed|None}`) → stdout terminator
//   newline, so callers piping to `read` always see a clean EOL.
// - `result.error`-like events (`TurnError`, `ConnectionFailed`) → stderr
//   `error: {msg}`.
// - Warnings/errors via `emit_warning`/`emit_error` → stderr.
//
// No color: TTY/no-TTY decision lives in the caller. We just emit plain text.
// ---------------------------------------------------------------------------

pub struct TextEmitter<W1: Write, W2: Write> {
    stdout: W1,
    stderr: W2,
    show_thinking: bool,
}

impl<W1: Write, W2: Write> TextEmitter<W1, W2> {
    pub fn new(stdout: W1, stderr: W2) -> Self {
        Self {
            stdout,
            stderr,
            show_thinking: false,
        }
    }

    /// Returns the short (≤60 char) one-line preview used in `● {tool}(...)`.
    fn short_title(title: &str) -> String {
        const MAX: usize = 60;
        let single_line: String = title.lines().next().unwrap_or("").to_owned();
        if single_line.chars().count() <= MAX {
            single_line
        } else {
            let truncated: String = single_line.chars().take(MAX).collect();
            format!("{truncated}…")
        }
    }

    fn write_assistant_text(&mut self, text: &str) -> std::io::Result<()> {
        self.stdout.write_all(text.as_bytes())?;
        self.stdout.flush()
    }

    fn write_tool_call(
        &mut self,
        tool_call: &crate::agent::types::ToolCall,
    ) -> std::io::Result<()> {
        let kind = if tool_call.kind.is_empty() {
            "tool"
        } else {
            tool_call.kind.as_str()
        };
        let short = Self::short_title(&tool_call.title);
        writeln!(self.stderr, "● {kind}({short})")?;
        self.stderr.flush()
    }

    fn write_tool_update(
        &mut self,
        update: &crate::agent::types::ToolCallUpdate,
    ) -> std::io::Result<()> {
        match update.fields.status.as_deref() {
            Some("completed") => {
                writeln!(self.stderr, "  ✓")?;
                self.stderr.flush()
            }
            Some("failed") => {
                let reason = update
                    .fields
                    .raw_output
                    .as_deref()
                    .or(update.fields.title.as_deref())
                    .unwrap_or("failed")
                    .lines()
                    .next()
                    .unwrap_or("failed");
                writeln!(self.stderr, "  ✗ {reason}")?;
                self.stderr.flush()
            }
            _ => Ok(()),
        }
    }
}

impl<W1: Write + Send, W2: Write + Send> Emitter for TextEmitter<W1, W2> {
    fn emit_event(&mut self, ev: &BridgeEvent) -> std::io::Result<()> {
        use crate::agent::types::{ContentBlock, SessionUpdate};
        match ev {
            BridgeEvent::SessionUpdate { update, .. } => match update {
                SessionUpdate::AgentMessageChunk {
                    content: ContentBlock::Text { text },
                } => self.write_assistant_text(text),
                SessionUpdate::AgentThoughtChunk { .. } => {
                    // Reserved for v1.5 `--show-thinking`; default suppresses.
                    // Both branches currently no-op; structure preserved so the
                    // future `show_thinking == true` arm slots in cleanly.
                    let _ = self.show_thinking;
                    Ok(())
                }
                SessionUpdate::ToolCall { tool_call } => self.write_tool_call(tool_call),
                SessionUpdate::ToolCallUpdate { tool_call_update } => {
                    self.write_tool_update(tool_call_update)
                }
                _ => Ok(()),
            },
            BridgeEvent::TurnComplete {
                terminal_reason, ..
            } => match terminal_reason {
                Some(TerminalReason::Completed) | None => {
                    self.stdout.write_all(b"\n")?;
                    self.stdout.flush()
                }
                Some(other) => {
                    writeln!(self.stderr, "terminated: {}", other.as_stored())?;
                    self.stderr.flush()
                }
            },
            BridgeEvent::TurnError { message, .. } | BridgeEvent::ConnectionFailed { message } => {
                writeln!(self.stderr, "error: {message}")?;
                self.stderr.flush()
            }
            // Other events (system.init, system.session, etc.) are silent
            // in text mode — they're plumbing, not user-visible content.
            _ => Ok(()),
        }
    }

    fn emit_warning(&mut self, msg: &str) -> std::io::Result<()> {
        writeln!(self.stderr, "warning: {msg}")?;
        self.stderr.flush()
    }

    fn emit_error(&mut self, msg: &str) -> std::io::Result<()> {
        writeln!(self.stderr, "error: {msg}")?;
        self.stderr.flush()
    }

    fn finish(&mut self) -> std::io::Result<()> {
        self.stdout.flush()?;
        self.stderr.flush()
    }
}

#[cfg(test)]
mod text_tests {
    use super::*;
    use crate::agent::types::{
        ContentBlock, SessionUpdate, TerminalReason, ToolCall, ToolCallUpdate, ToolCallUpdateFields,
    };

    fn sample_tool_call(kind: &str, title: &str) -> ToolCall {
        ToolCall {
            tool_call_id: "tc1".into(),
            title: title.into(),
            kind: kind.into(),
            status: "in_progress".into(),
            content: Vec::new(),
            raw_input: None,
            raw_output: None,
            output_metadata: None,
            task_metadata: None,
            locations: Vec::new(),
            meta: None,
        }
    }

    fn agent_message_event(text: &str) -> BridgeEvent {
        BridgeEvent::SessionUpdate {
            session_id: "s1".into(),
            update: SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text {
                    text: text.to_owned(),
                },
            },
        }
    }

    fn agent_thought_event(text: &str) -> BridgeEvent {
        BridgeEvent::SessionUpdate {
            session_id: "s1".into(),
            update: SessionUpdate::AgentThoughtChunk {
                content: ContentBlock::Text {
                    text: text.to_owned(),
                },
            },
        }
    }

    #[test]
    fn text_assistant_chunk_writes_to_stdout_only() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        TextEmitter::new(&mut out, &mut err)
            .emit_event(&agent_message_event("hi"))
            .unwrap();
        assert_eq!(std::str::from_utf8(&out).unwrap(), "hi");
        assert!(err.is_empty(), "stderr unexpectedly populated: {err:?}");
    }

    #[test]
    fn text_thinking_suppressed_by_default() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        TextEmitter::new(&mut out, &mut err)
            .emit_event(&agent_thought_event("inner musing"))
            .unwrap();
        assert!(out.is_empty(), "stdout unexpectedly populated: {out:?}");
        assert!(err.is_empty(), "stderr unexpectedly populated: {err:?}");
    }

    #[test]
    fn text_tool_call_writes_marker_to_stderr() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        TextEmitter::new(&mut out, &mut err)
            .emit_event(&BridgeEvent::SessionUpdate {
                session_id: "s1".into(),
                update: SessionUpdate::ToolCall {
                    tool_call: sample_tool_call("BashTool", "ls -la /tmp"),
                },
            })
            .unwrap();
        assert!(out.is_empty());
        let stderr_text = std::str::from_utf8(&err).unwrap();
        assert_eq!(stderr_text, "● BashTool(ls -la /tmp)\n");
    }

    #[test]
    fn text_tool_update_completed_writes_check_to_stderr() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let update = ToolCallUpdate {
            tool_call_id: "tc1".into(),
            fields: ToolCallUpdateFields {
                status: Some("completed".into()),
                ..Default::default()
            },
        };
        TextEmitter::new(&mut out, &mut err)
            .emit_event(&BridgeEvent::SessionUpdate {
                session_id: "s1".into(),
                update: SessionUpdate::ToolCallUpdate {
                    tool_call_update: update,
                },
            })
            .unwrap();
        assert!(out.is_empty());
        assert_eq!(std::str::from_utf8(&err).unwrap(), "  ✓\n");
    }

    #[test]
    fn text_tool_update_failed_writes_cross_with_reason() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let update = ToolCallUpdate {
            tool_call_id: "tc1".into(),
            fields: ToolCallUpdateFields {
                status: Some("failed".into()),
                raw_output: Some("permission denied".into()),
                ..Default::default()
            },
        };
        TextEmitter::new(&mut out, &mut err)
            .emit_event(&BridgeEvent::SessionUpdate {
                session_id: "s1".into(),
                update: SessionUpdate::ToolCallUpdate {
                    tool_call_update: update,
                },
            })
            .unwrap();
        assert!(out.is_empty());
        assert_eq!(std::str::from_utf8(&err).unwrap(), "  ✗ permission denied\n");
    }

    #[test]
    fn text_result_success_writes_terminator_newline_to_stdout() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        TextEmitter::new(&mut out, &mut err)
            .emit_event(&BridgeEvent::TurnComplete {
                session_id: "s1".into(),
                terminal_reason: Some(TerminalReason::Completed),
            })
            .unwrap();
        assert_eq!(std::str::from_utf8(&out).unwrap(), "\n");
        assert!(err.is_empty());
    }

    #[test]
    fn text_turn_error_writes_error_prefix_to_stderr() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        TextEmitter::new(&mut out, &mut err)
            .emit_event(&BridgeEvent::TurnError {
                session_id: "s1".into(),
                message: "boom".into(),
                error_kind: None,
                sdk_result_subtype: None,
                assistant_error: None,
                terminal_reason: None,
            })
            .unwrap();
        assert!(out.is_empty());
        assert_eq!(std::str::from_utf8(&err).unwrap(), "error: boom\n");
    }
}
