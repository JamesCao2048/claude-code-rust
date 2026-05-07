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
