//! Integration tests for the headless `-p` mode.
//!
//! These spawn the real `lingxi-ascendc` binary against a stub bridge
//! (`tests/fixtures/headless/stub_bridge.mjs`) that emits scripted JSON.
//! Each test crafts a small "script" of EMIT/SLEEP/IGNORE_SHUTDOWN/EXIT_NOW
//! directives and asserts on the binary's exit code, stdout, and stderr.
//!
//! Tests are skipped if `node` is not on PATH (mirroring the production
//! bridge requirement).

use assert_cmd::Command;
use std::io::Write as _;
use tempfile::NamedTempFile;

/// One stub-bridge script. Compose with the helper constructors below for
/// readability at the call site.
pub struct StubScript(pub Vec<String>);

impl StubScript {
    pub fn new(lines: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self(lines.into_iter().map(Into::into).collect())
    }
    pub fn write_to_temp(&self) -> NamedTempFile {
        let mut f = NamedTempFile::new().expect("tempfile");
        f.write_all(self.0.join("\n").as_bytes()).expect("write");
        f.write_all(b"\n").expect("write trailing");
        f
    }
}

pub fn node_available() -> bool {
    which::which("node").is_ok()
}

/// Build a `Command` for the headless binary, wired to invoke the stub bridge
/// against the given script. Caller chains `.assert()` for expectations.
pub fn cmd_for_stub(extra_args: &[&str], script: &StubScript) -> (Command, NamedTempFile) {
    let stub_js = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/headless/stub_bridge.mjs");
    let script_file = script.write_to_temp();
    let mut cmd = Command::cargo_bin("lingxi-ascendc").expect("cargo bin");
    cmd.args(["--bridge-script", stub_js.to_str().expect("utf8 path")])
        .args(extra_args)
        .env("STUB_SCRIPT", script_file.path())
        .env("LINGXI_HEADLESS_REQUIRE_EXPLICIT_PERMISSION_MODE", "0");
    (cmd, script_file)
}

// Canned event payloads — keep these as String so callers can format session ids etc.
pub fn ev_initialized() -> String {
    r#"{"event":"initialized","result":{"agent_name":"stub","agent_version":"0.0","auth_methods":[],"capabilities":{"prompt_image":false,"prompt_embedded_context":false,"supports_session_listing":false,"supports_resume_session":false}}}"#.to_owned()
}

pub fn ev_connected(session_id: &str) -> String {
    // CurrentModel requires the full field set — see `crate::agent::types::CurrentModel`.
    // We hand-roll a minimal but spec-compliant payload here.
    let model = r#"{"requested_id":null,"resolved_id":"stub","display_name_short":"Stub","display_name_long":"Stub Model","catalog_id":null,"supports_effort":false,"supported_effort_levels":[],"supports_fast_mode":null,"supports_auto_mode":null,"supports_adaptive_thinking":null,"is_authoritative":true}"#;
    format!(
        r#"{{"event":"connected","session_id":"{session_id}","cwd":"/tmp","current_model":{model},"available_models":[],"mode":null,"history_updates":null}}"#
    )
}

pub fn ev_turn_complete(session_id: &str, terminal_reason: Option<&str>) -> String {
    match terminal_reason {
        Some(r) => format!(r#"{{"event":"turn_complete","session_id":"{session_id}","terminal_reason":"{r}"}}"#),
        None => format!(r#"{{"event":"turn_complete","session_id":"{session_id}"}}"#),
    }
}

pub fn ev_permission_request(session_id: &str) -> String {
    format!(
        r#"{{"event":"permission_request","session_id":"{session_id}","request":{{"tool_call_id":"tc1","tool_name":"Bash","input":{{}},"explanation":"stub","preview":null}}}}"#
    )
}

// ---------------------------------------------------------------------------
// Smoke test — verifies the harness wiring itself.
// (Full T16-T21 scenarios append below.)
// ---------------------------------------------------------------------------

#[test]
fn harness_smoke_happy_path_returns_zero() {
    if !node_available() {
        eprintln!("skip: node not on PATH");
        return;
    }
    let script = StubScript::new(vec![
        format!("EMIT {}", ev_initialized()),
        format!("EMIT {}", ev_connected("s-smoke")),
        format!("EMIT {}", ev_turn_complete("s-smoke", Some("completed"))),
    ]);
    let (mut cmd, _scriptfile) = cmd_for_stub(
        &["-p", "smoke"],
        &script,
    );
    cmd.assert().success();
}
